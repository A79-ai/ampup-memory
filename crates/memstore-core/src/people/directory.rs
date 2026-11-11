//! Orgs, teams and their members.
//!
//! A catalog holds one or more **orgs**; an org holds **teams**. Each org and each team is its own memory
//! scope (a namespace, see [`Scope`]), so a team's memory is isolated from the rest of its org by the
//! catalog itself. A user may belong to several orgs and to several teams in each; which memory they get
//! is the org or team they select (`memstore config set org|team`, or the `x-memstore-org` /
//! `x-memstore-team` headers), checked here against their memberships.
//!
//! The directory is an append-only table of changes (`memory_system.directory`), folded into the
//! current state on read: no row is ever rewritten, and the history says who changed what.
//!
//! Who may change it: whoever reaches the catalog directly (`memstore org …` without a server) or holds an
//! admin API key may do anything; an org's admins manage that org's teams and members.
//!
//! An org can also connect its own identity provider (`memstore auth setup`). People who sign in through it
//! may act in that org only (a provider vouches for its own people, never for another org's), its groups can
//! decide team membership (`memstore auth map-group`), and verified emails at the org's domains join it
//! without an invite (`memstore auth domain add`). Who someone is a member of is therefore worked out per
//! sign-in: [`Directory::access`].
//!
//! Service accounts (`memstore apikey create`) are API keys an org's admins create for CI and other
//! non-people. Only a key's sha256 is stored. A key reads its org's memory, or only its team's, never
//! more, and writes there only if its role is `writer`. An `ingest` key only writes: it reads nothing, so
//! a connector that feeds memory can't be used to read it back. A `recorder` key does nothing as itself: it
//! records runs and asks advice for a member of its org (or team) that it names ([`Directory::holds_member`]).
//!
//! Keys an operator kept in a keys file (`AMPUP_MEMORY_API_KEYS`, `memstore keys new`) move here with
//! `memstore apikey import`: each file row becomes a directory key with the same sha256, so its holder
//! keeps using the same key, and with no more than it had (an admin key, or one holding groups, is
//! refused rather than narrowed in silence or widened). Once a file key is in the directory, the
//! directory decides for it: revoking it there stops it, even while the file still lists it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{DEFAULT_ORG, Scope, Store, check_name};

/// One change, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// When it was made (µs since epoch); changes apply in this order.
    pub at: i64,
    pub actor: String,
    /// `org.create`, `org.delete`, `org.purged` (its memory is gone), `workspace.set` (the org is a workspace:
    /// [`crate::workspace`]), `team.create`, `team.delete`,
    /// `member.add` (or a role change), `member.remove`, `member.forget` (a tombstone: the person's
    /// memories were deleted), `member.request` (the person asked to join; an invite answers it),
    /// `member.decline` (an admin turned them away), `team.add` (a member joins a team), `team.remove`, `org.profile`,
    /// `auth.provider`, `org.auth0` (the org's Auth0 Organization), `domain.add`, `domain.remove`,
    /// `group.map`, `group.unmap`, `key.create`, `key.revoke`. Deployment-wide, with no org (`""`):
    /// `user.disable` and `user.enable` (every request as `user` refused, or let back in), `closure.set` (the
    /// groups added to and removed from `user`'s closure, JSON [`ClosureEdit`]; both empty clears it) and
    /// `team.csv` (the pilot team csv, the detail; no detail clears it).
    pub op: String,
    pub org: String,
    pub team: Option<String>,
    pub user: Option<String>,
    pub role: Option<String>,
    /// `org.profile`: the org's memory profile, JSON. `auth.provider`: its identity provider, JSON (none:
    /// removed). `org.auth0`: the Auth0 Organization's id (none: unbound). `domain.add|remove`: the domain. `group.map|unmap`: the provider's group. `key.create`:
    /// the key, JSON ([`KeyDetail`]; an imported file key names the row it came from). `key.revoke`: the
    /// key's name. `member.forget`: what was removed, as counts ([`Forgotten`]), never content.
    pub detail: Option<String>,
    /// The format of this change: 1 for every change an older engine folds right (a row from before
    /// versions is 1 too), else the version that gave it its meaning (2: a key's source prefixes, which an
    /// engine before them would ignore, letting the key write anything). A change of a version newer than
    /// [`EVENT_VERSION`] is never folded: see [`Folded::apply`].
    pub version: i64,
}

/// How long after a decline a person may ask to join again (µs): a day.
pub const ASK_AGAIN_AFTER: i64 = 86_400 * 1_000_000;
/// How many people may wait to join one org at once: past it, asking is refused until an admin answers some.
pub const MAX_PENDING: usize = 100;
/// How many other scopes one scope's readers may also search (`Settings::reads`): each is searched on every read.
const MAX_READS: usize = 8;
/// The most sources one compile round may take (`Settings::compile_batch`).
const MAX_COMPILE_BATCH: usize = 100;
/// The most rewarded runs `advice_personal_runs` may name: the server's own bar for a person's first map
/// (`AMPUP_MEMORY_ADVICE_MIN_NEW`) is 10 by default, and the setting only ever lowers bars.
const MAX_PERSONAL_RUNS: usize = 10;

/// What a scope's `advice_personal_runs` says ([`Settings::advice_personal_runs`]): a number of rewarded runs (`0`
/// unsets it), or `"off"`, which turns it off there whatever its org or the server says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonalRuns {
    Runs(usize),
    Off,
}

impl Serialize for PersonalRuns {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            PersonalRuns::Runs(n) => s.serialize_u64(*n as u64),
            PersonalRuns::Off => s.serialize_str("off"),
        }
    }
}

impl<'de> Deserialize<'de> for PersonalRuns {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) if n.as_u64().is_some() => {
                Ok(PersonalRuns::Runs(n.as_u64().unwrap_or_default() as usize))
            }
            Value::String(s) => s.parse().map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "advice_personal_runs is a number of rewarded runs (0 unsets it) or \"off\", not {other}"
            ))),
        }
    }
}

impl std::str::FromStr for PersonalRuns {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.trim() {
            "off" => Ok(PersonalRuns::Off),
            n => n.parse().map(PersonalRuns::Runs).map_err(|_| {
                format!(
                    "advice_personal_runs is a number of rewarded runs (0 unsets it) or \"off\", not {s:?}"
                )
            }),
        }
    }
}
/// What a scope's compile sources may be said to be (`Settings::compile_kind`); `""` unsets it.
pub const COMPILE_KINDS: &[&str] = &["", "errors"];
/// How advice routes over a scope's compiled skills (`Settings::advice_skills`); `""` unsets it.
pub const ADVICE_SKILLS: &[&str] = &["", "fallback", "together", "off"];
/// How the MCP instructions and the session tools' descriptions read (`Settings::instructions`); `""` unsets it.
pub const INSTRUCTIONS: &[&str] = &["", "default", "purpose"];

/// The newest directory change format this engine understands, and the one it writes. Bump it when a
/// change's meaning changes in a way an older engine would get wrong (a new field it would ignore, a
/// new meaning of an op it knows). Rolling deploys upgrade every reader before any writer writes the new
/// version: an older reader that meets it stops folding there (it serves the directory as of the change
/// before, signs no one in, refuses directory writes, and raises `memstore_directory_blocked`). Only a
/// change that needs it is written at it ([`SOURCES_VERSION`], [`PURGE_VERSION`], [`USERS_VERSION`]); every
/// other change stays at 1, so an older reader folds on until a change it would get wrong. An op this
/// engine doesn't know stops the fold too, whatever its version ([`OPS`]).
pub const EVENT_VERSION: i64 = 4;

/// The version of a change whose op is newer than this engine ([`OPS_VERSION`] and up). From this engine on,
/// every engine stops folding at an op it doesn't know, at any version; an engine before it reads at most
/// version 3 and skipped such an op, so a new op is written at this version for that engine to stop too.
pub const OPS_VERSION: i64 = 4;

/// Every op this engine folds (`member.forget` is a record only: it changes nothing). A change of any other
/// op stops the fold there, as a change of a newer version does ([`Folded::apply`]): an engine that skipped
/// it would serve a directory the change may have narrowed (a new kind of revoke or removal).
pub const OPS: &[&str] = &[
    "org.create",
    "org.delete",
    "org.purged",
    "org.profile",
    "org.settings",
    "org.auth0",
    "workspace.set",
    "team.create",
    "team.delete",
    "team.add",
    "team.remove",
    "member.add",
    "member.remove",
    "member.forget",
    "member.request",
    "member.decline",
    "auth.provider",
    "domain.add",
    "domain.remove",
    "group.map",
    "group.unmap",
    "key.create",
    "key.revoke",
    "key.sources",
    "user.disable",
    "user.enable",
    "closure.set",
    "team.csv",
];

/// Whether this engine folds `e`: a version it reads, of an op it knows.
pub fn readable(e: &Event) -> bool {
    e.version <= EVENT_VERSION && OPS.contains(&e.op.as_str())
}

/// Why the fold stopped at a change of `version` ([`Folded::blocked`]).
pub fn blocked_why(version: i64) -> String {
    if version > EVENT_VERSION {
        format!("a change of version {version}, newer than this engine reads ({EVENT_VERSION})")
    } else {
        format!(
            "a change of an op this engine doesn't know (at version {version}: a newer engine wrote it)"
        )
    }
}

/// The version of `org.delete`. An engine from before purging (it reads version 1 only) would let the name
/// be created again while its old memory is still there, and the new org would read it: that engine stops
/// at the delete instead, and plans nothing more. Every engine that reads version 2 knows purging.
pub const PURGE_VERSION: i64 = 2;

/// The version of a change that holds a key to source prefixes (`key.create` with sources, `key.sources`):
/// an engine before it would fold the key without them, free to write any source.
pub const SOURCES_VERSION: i64 = 2;

/// The version of a deployment-wide access change that narrows access (`user.disable`, `closure.set`,
/// `team.csv`): an engine before them kept these in its own process only, and folding on without them would
/// let a user disabled through another server in, or keep a group removed through it. (`user.enable` stays
/// at 1: an engine that misses it only refuses more.)
pub const USERS_VERSION: i64 = 3;

/// A user's closure edit (`closure.set`): groups added to, and removed from, what their token carries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClosureEdit {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl Event {
    pub fn new(actor: &str, op: &str, org: &str) -> Self {
        Event {
            at: 0,
            actor: actor.into(),
            op: op.into(),
            org: org.into(),
            team: None,
            user: None,
            role: None,
            detail: None,
            version: 1,
        }
    }
    fn team(mut self, team: &str) -> Self {
        self.team = Some(team.into());
        self
    }
    pub fn user(mut self, user: &str) -> Self {
        self.user = Some(user.into());
        self
    }
    fn role(mut self, role: Role) -> Self {
        self.role = Some(role.as_str().into());
        self
    }
    pub fn detail(mut self, detail: &str) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// One line for people: what changed.
    pub fn describe(&self) -> String {
        let team = self.team.as_deref().unwrap_or("");
        let user = self.user.as_deref().unwrap_or("");
        let role = self.role.as_deref().unwrap_or("member");
        let org = &self.org;
        match self.op.as_str() {
            "org.create" => format!("create org {org}"),
            "org.delete" => format!("delete org {org}"),
            "org.purged" => format!("purge the memory of org {org}"),
            "member.forget" => match self
                .detail
                .as_deref()
                .and_then(|d| serde_json::from_str::<Forgotten>(d).ok())
            {
                Some(f) => format!(
                    "forget {user} in {org}: {} episodes, {} facts, {} quotes and {} closures deleted",
                    f.episodes, f.facts, f.quotes, f.closures
                ),
                None => format!("forget {user} in {org}"),
            },
            "team.create" => format!("create team {org}/{team}"),
            "team.delete" => format!("delete team {org}/{team}"),
            "member.add" => format!("{user} is {role} of {org}"),
            "member.remove" => format!("remove {user} from {org}"),
            "member.request" => format!("{user} asked to join {org}"),
            "member.decline" => format!("turn {user} away from {org}"),
            "team.add" => format!("add {user} to {org}/{team}"),
            "team.remove" => format!("remove {user} from {org}/{team}"),
            "org.profile" => format!("set the memory profile of {org}"),
            "org.settings" => format!(
                "set {org}{} settings: {}",
                if team.is_empty() {
                    "'s".to_string()
                } else {
                    format!("/{team}'s")
                },
                self.detail.as_deref().unwrap_or("{}")
            ),
            "workspace.set" => format!("make {org} a workspace"),
            "auth.provider" => match self
                .detail
                .as_deref()
                .and_then(|d| serde_json::from_str::<Provider>(d).ok())
            {
                Some(p) => format!(
                    "{org} signs in with {} ({}){}",
                    p.kind,
                    p.issuer,
                    if p.required { ", required" } else { "" }
                ),
                None => format!("{org} signs in with the server's identity provider"),
            },
            "org.auth0" => match self.detail.as_deref() {
                Some(id) => format!("{org} is Auth0 organization {id}"),
                None => format!("{org} is bound to no Auth0 organization"),
            },
            "domain.add" => format!(
                "verified emails at {} join {org}{}",
                self.detail.as_deref().unwrap_or(""),
                if team.is_empty() {
                    String::new()
                } else {
                    format!(" and {org}/{team}")
                }
            ),
            "user.disable" => format!("disable {user}"),
            "user.enable" => format!("let {user} back in"),
            "closure.set" => format!(
                "set {user}'s closure edit: {}",
                self.detail.as_deref().unwrap_or("{}")
            ),
            "team.csv" if self.detail.is_none() => "clear the pushed team csv".into(),
            "team.csv" => "replace the team csv".into(),
            "domain.remove" => format!(
                "emails at {} no longer join {org}",
                self.detail.as_deref().unwrap_or("")
            ),
            "group.map" => format!(
                "group {} joins {org}/{team}",
                self.detail.as_deref().unwrap_or("")
            ),
            "group.unmap" => format!(
                "group {} no longer joins {org}/{team}",
                self.detail.as_deref().unwrap_or("")
            ),
            "key.create" => match self
                .detail
                .as_deref()
                .and_then(|d| serde_json::from_str::<KeyDetail>(d).ok())
            {
                Some(k) => {
                    format!(
                        "{} {} key {} for {}{}",
                        match k.imported_user() {
                            Some(u) => format!("import a file key acting as {u} as"),
                            None => "create".into(),
                        },
                        k.role.as_str(),
                        k.name,
                        k.team
                            .as_deref()
                            .map_or(org.clone(), |t| format!("{org}/{t}")),
                        k.expires_at
                            .map(|t| format!(", expiring at {t} (seconds since epoch)"))
                            .unwrap_or_default()
                    ) + &sources_note(&k.sources)
                }
                None => format!("create a key for {org}"),
            },
            "key.sources" => match self
                .detail
                .as_deref()
                .and_then(|d| serde_json::from_str::<KeySources>(d).ok())
            {
                Some(k) if k.sources.is_empty() => {
                    format!("key {} of {org} writes any source", k.name)
                }
                Some(k) => format!("key {} of {org}{}", k.name, sources_note(&k.sources)),
                None => format!("change a key's sources in {org}"),
            },
            "key.revoke" => format!(
                "revoke key {} of {org}",
                self.detail.as_deref().unwrap_or("")
            ),
            other => format!("{other} {org} {team} {user}"),
        }
    }
}

/// What `memstore users forget` removed from one org's memory: counts only, never content. The
/// `member.forget` tombstone's detail.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Forgotten {
    /// Episodes the person wrote (`remember_memory`), each gone with its body.
    pub episodes: usize,
    /// Facts that cited any of them.
    pub facts: usize,
    /// Backfilled quotes of those facts.
    pub quotes: usize,
    /// Closures whose evidence was one of them.
    pub closures: usize,
    /// Answers they logged (`log_answer`): their questions.
    #[serde(default)]
    pub answers: usize,
    /// Feedback they gave on facts (`feedback_memory`).
    #[serde(default)]
    pub feedback: usize,
}

impl Forgotten {
    pub fn add(&mut self, o: &Forgotten) {
        self.answers += o.answers;
        self.feedback += o.feedback;
        self.episodes += o.episodes;
        self.facts += o.facts;
        self.quotes += o.quotes;
        self.closures += o.closures;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Manages the org's teams and members.
    Admin,
    #[default]
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            _ => anyhow::bail!("role is admin or member, got {s:?}"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Org {
    pub members: BTreeMap<String, Role>,
    /// Each team's members.
    pub teams: BTreeMap<String, BTreeSet<String>>,
    /// What this org wants from memory (`memstore org init`), shown to agents that connect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// The org's own identity provider (`memstore auth setup`). Shown without its client secret.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "provider_view"
    )]
    pub provider: Option<Provider>,
    /// The Auth0 Organization (`org_…`) whose sign-ins through the platform's Auth0 tenant count here
    /// (`AMPUP_MEMORY_AUTH0_ISSUER`). Set by the operator only; one Auth0 Organization is one org's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth0_org: Option<String>,
    /// Email domains whose verified addresses join the org without an invite, each with the team they
    /// join too (if any).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub domains: BTreeMap<String, Option<String>>,
    /// The org provider's groups, each with the teams its members are in.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, BTreeSet<String>>,
    /// Its service accounts' live keys, by name (revoked ones are gone).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, ServiceKey>,
    /// Set when the org is a workspace (`memstore workspace create`): an isolated, disposable memory, with
    /// its limits and time to live ([`crate::workspace`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::Spec>,
    /// How the org's memory runs (`set_settings`), and each team's where its admins set it apart.
    #[serde(skip_serializing_if = "Settings::is_empty")]
    pub settings: Settings,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub team_settings: BTreeMap<String, Settings>,
    /// People an admin removed: a domain or a group doesn't bring them back (an invite does).
    #[serde(skip)]
    removed: BTreeSet<String>,
    /// People who asked to join (`request_join`, what `memstore join` sends when the directory doesn't hold
    /// them) and wait for an admin, each with when they asked (µs). An invite or `add_member` lets them in;
    /// `decline` turns them away. Nothing until then: a request grants no access.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub pending: BTreeMap<String, i64>,
    /// People an admin turned away, with when (µs): they ask again after [`ASK_AGAIN_AFTER`], not before, so
    /// a decline isn't answered with another mail to every admin at once. An invite clears it.
    #[serde(skip)]
    declined: BTreeMap<String, i64>,
    /// Which org of this name it is: the time (µs) the directory's log stamped on its `org.create` (0: the
    /// default org, or one created before the log). A name taken again after a delete is another org, with
    /// another incarnation; audit rows carry the one they were recorded for ([`crate::audit`]).
    #[serde(skip)]
    pub created_at: i64,
}

impl Org {
    /// The org as one of its members who isn't an admin sees it (`/admin {"op": "show"}`): themselves, with
    /// their role and teams, and the org's profile (which agents that connect are shown anyway). Not the
    /// other members, the keys, the provider, the domains or who asked to join: those are its admins'.
    pub fn member_view(&self, user: &str, access: &Access) -> Value {
        let user = user_id(user);
        let teams: BTreeMap<&String, [&String; 1]> =
            access.teams.iter().map(|t| (t, [&user])).collect();
        let mut v = json!({"members": {&user: access.role}, "teams": teams});
        if let Some(p) = &self.profile {
            v["profile"] = json!(p);
        }
        v
    }

    /// Whether an admin removed `user` (a [`user_id`]) and no invite brought them back.
    pub fn removed(&self, user: &str) -> bool {
        self.removed.contains(user)
    }
}

/// What a service key may do in its scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeyRole {
    #[default]
    Reader,
    /// Also writes (`remember_memory`).
    Writer,
    /// Only writes: every read (search, query, episodes, the org's profile) is refused.
    Ingest,
    /// Manages its org's membership and nothing else (a SCIM bridge's): adds members (never admins),
    /// removes members (never admins), and makes, deletes and fills teams through `/admin`, and reads the
    /// org's `show`. It reads and writes no memory. Org-wide only: never a team's.
    Directory,
    /// Acts for a member of its org (or its team) it names in `x-memstore-as`, on runs, rewards, advice and
    /// sessions only (a CI job recording a pull request's review as the request's author): the request is then
    /// that member's. As itself it reads and writes nothing.
    Recorder,
}

impl KeyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyRole::Reader => "reader",
            KeyRole::Writer => "writer",
            KeyRole::Ingest => "ingest",
            KeyRole::Directory => "directory",
            KeyRole::Recorder => "recorder",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "reader" => Ok(KeyRole::Reader),
            "writer" => Ok(KeyRole::Writer),
            "ingest" => Ok(KeyRole::Ingest),
            "directory" => Ok(KeyRole::Directory),
            "recorder" => Ok(KeyRole::Recorder),
            _ => anyhow::bail!(
                "a key's role is reader, writer, ingest, directory or recorder, got {s:?}"
            ),
        }
    }
}

/// A service account's key, as the directory holds it. Shown without its hash.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ServiceKey {
    /// Reads (and writes) this team only; none: the org's own memory.
    pub team: Option<String>,
    pub role: KeyRole,
    /// When it stops working, in seconds since the epoch.
    pub expires_at: Option<i64>,
    /// An ingest key writes (and retracts) only sources starting with one of these; none: any source.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    pub created_by: String,
    /// µs since the epoch, like every event's time.
    pub created_at: i64,
    /// sha256 of the key, hex: what a bearer is matched against. Never shown.
    #[serde(skip)]
    pub sha256: String,
    /// When it last authenticated, µs since the epoch: not part of the directory's log, filled in from
    /// the servers' usage records ([`Directory::with_usage`]) for views. None: never (or not looked up).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    /// The user an imported file key acted as ([`user_id`]): it reads its scope only while they are a
    /// member there, and removing them revokes it.
    #[serde(skip)]
    pub imported_user: Option<String>,
}

/// A `key.create` event's detail.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyDetail {
    pub name: String,
    pub sha256: String,
    pub team: Option<String>,
    pub role: KeyRole,
    pub expires_at: Option<i64>,
    /// The source prefixes an ingest key is held to ([`ServiceKey::sources`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    pub created_by: String,
    /// On imports from before `imported_user` was written: the row's id and the user it acted as
    /// (`ci (svc:ci)`). Read for [`KeyDetail::imported_user`] only, and never written.
    #[serde(default, skip_serializing)]
    pub imported_from: Option<String>,
    /// The user the imported file key acted as ([`user_id`]); on imports before it was written, read from
    /// `imported_from` ([`KeyDetail::imported_user`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_user: Option<String>,
}

impl KeyDetail {
    /// The user an imported key acted as: as written, or as `imported_from` names them (`<id> (<user>)`).
    pub fn imported_user(&self) -> Option<String> {
        if let Some(u) = &self.imported_user {
            return Some(user_id(u));
        }
        let from = self.imported_from.as_deref()?;
        let (_, user) = from.strip_suffix(')')?.rsplit_once(" (")?;
        Some(user_id(user))
    }
}

/// A `key.sources` event's detail: the key's source prefixes from now on (none: any source).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeySources {
    pub name: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// One row of a keys file (`AMPUP_MEMORY_API_KEYS`, `memstore keys new`), as `memstore apikey import` reads
/// it: only the key's sha256, never the key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileKey {
    pub id: String,
    pub sha256: String,
    pub user: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub admin: bool,
}

/// A keys file's rows: a JSON list of `{id, sha256, user, groups, admin}` (the keys file and
/// `AMPUP_MEMORY_API_KEYS` are the same format), or `{"keys": [...]}`.
pub fn parse_file_keys(text: &str) -> Result<Vec<FileKey>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum File {
        List(Vec<FileKey>),
        Wrapped { keys: Vec<FileKey> },
    }
    Ok(match serde_json::from_str::<File>(text)? {
        File::List(k) | File::Wrapped { keys: k } => k,
    })
}

/// The directory name a file key's id becomes: lower-case letters, digits and single underscores
/// (`Amy@Acme.com` is `amy_acme_com`). None if nothing is left.
pub fn key_name_of(id: &str) -> Option<String> {
    let mut out = String::new();
    for c in id.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    let mut out = out.trim_end_matches('_').to_string();
    out.truncate(48);
    let out = out.trim_end_matches('_').to_string();
    check_name("key", &out).ok().map(|_| out)
}

/// An org's own identity provider.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    /// `google`, `okta`, `entra` or `oidc`.
    pub kind: String,
    pub issuer: String,
    /// The client ids its ID tokens may be issued to; the first is the one `memstore auth login` uses.
    pub client_ids: Vec<String>,
    /// What kind of client the CLI's (the first) is. Only a public client's secret is ever handed out;
    /// none on providers set up before this was declared, which `memstore identity check` flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<ClientType>,
    /// The CLI client's secret, for public clients the provider gives one anyway (Google's installed
    /// apps): handed to anyone who asks how to sign in, but never kept in the directory, which anyone who
    /// can read the bucket reads. It is stored as `env:AMPUP_MEMORY_IDP_SECRET_<NAME>` or `sealed:…` (see
    /// [`crate::auth::secret`]); events written before that may hold it in plain text, which still works
    /// and `memstore identity check` flags. A confidential client's secret is refused: it is never served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The ID-token claim that lists a person's groups.
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    /// Members must sign in through this provider: a sign-in through the server's own is refused here.
    #[serde(default)]
    pub required: bool,
}

/// The kind of OAuth client (RFC 6749 §2.1) the CLI signs in with.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClientType {
    /// An installed app: PKCE, and no secret, or one the provider doesn't treat as confidential
    /// (Google's installed-app secrets). Its secret, if any, is handed to anyone who asks how to sign in.
    Public,
    /// A web app whose secret proves it is the client. The secret is never taken or served: the CLI
    /// signs in with PKCE alone, which a provider allows only if the client accepts it (otherwise the org
    /// registers a public client for the CLI).
    Confidential,
}

/// Why `p`'s client type and secret don't go together, if they don't. Checked before a secret is sealed,
/// and again when the change is planned.
pub fn client_type_problem(p: &Provider) -> Option<&'static str> {
    match (p.client_type, &p.client_secret) {
        (Some(ClientType::Confidential), Some(_)) => Some(
            "a confidential client's secret is never stored or handed out (anyone who asks how to sign in would get it): \
             leave out --client-secret, and the CLI signs in with PKCE alone; if the provider requires the \
             secret, register a public (installed-app) client for the CLI and pass --public-client",
        ),
        (None, Some(_)) => Some(
            "say what kind of client this is: --public-client (an installed app: its secret, if any, is handed to \
             anyone who asks how to sign in) or --confidential-client (its secret is never taken)",
        ),
        _ => None,
    }
}

/// An org's provider as views show it: never its client secret, sealed or not.
fn provider_view<S: serde::Serializer>(p: &Option<Provider>, s: S) -> Result<S::Ok, S::Error> {
    let shown = p.as_ref().map(|p| Provider {
        client_secret: p.client_secret.as_deref().map(crate::auth::secret::shown),
        ..p.clone()
    });
    shown.serialize(s)
}

fn default_groups_claim() -> String {
    "groups".into()
}

/// Provider kinds `memstore auth setup` knows.
pub const PROVIDER_KINDS: [&str; 4] = ["google", "okta", "entra", "oidc"];

/// Mail services anyone can sign up to: owning an address there says nothing about belonging to an org.
const PUBLIC_MAIL: [&str; 12] = [
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "yahoo.com",
    "icloud.com",
    "me.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
];

/// How a caller proved who they are, as far as the directory cares.
#[derive(Clone, Debug, Default)]
pub struct SignIn {
    /// Signed in through an identity provider (a person), not with an API key or an app's token.
    pub person: bool,
    /// Signed in through these orgs' own provider: they may act in these orgs only. Bound to none
    /// (`Some` of nothing): they act in no org, as a keys-file key, which reads its server's own scope only.
    pub orgs: Option<BTreeSet<String>>,
    /// The groups that provider says they are in.
    pub groups: Vec<String>,
    /// Their provider said their email is verified (what joining by domain rests on).
    pub verified_email: bool,
    /// Signed in with an org's service key: it holds its own scope and nothing else.
    pub key: Option<crate::auth::OrgKey>,
    /// Signed in through the platform's Auth0 tenant: only the org bound to the Auth0 Organization the
    /// token was issued for counts it ([`Directory::vouched`]); none, if it names none.
    pub auth0: bool,
    /// That Organization (the token's `org_id`).
    pub auth0_org: Option<String>,
    /// Signed in through the platform's Auth0 tenant for the operators' Organization
    /// (`AMPUP_MEMORY_AUTH0_OPERATORS_ORG`): they manage orgs (create them, bind them to Auth0, set their
    /// admins and members, see every org) and nothing else: [`Change::operators`].
    pub operator: bool,
}

impl SignIn {
    /// Whether this caller may write memory: anyone but a reader, directory or recorder key (a recorder
    /// writes as the member it acts for, never as itself).
    pub fn may_write(&self) -> bool {
        self.key.as_ref().is_none_or(|k| {
            !matches!(
                k.role,
                KeyRole::Reader | KeyRole::Directory | KeyRole::Recorder
            )
        })
    }

    /// Whether this caller may read memory: anyone but an ingest, directory or recorder key.
    pub fn may_read(&self) -> bool {
        self.key.as_ref().is_none_or(|k| {
            !matches!(
                k.role,
                KeyRole::Ingest | KeyRole::Directory | KeyRole::Recorder
            )
        })
    }

    /// The directory key this caller signed in with, if they did (`KeyRole::Directory`).
    pub fn directory_key(&self) -> Option<&crate::auth::OrgKey> {
        self.key.as_ref().filter(|k| k.role == KeyRole::Directory)
    }

    /// The recorder key this caller signed in with, if they did (`KeyRole::Recorder`).
    pub fn recorder_key(&self) -> Option<&crate::auth::OrgKey> {
        self.key.as_ref().filter(|k| k.role == KeyRole::Recorder)
    }
}

/// What someone holds in one org.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Access {
    pub role: Role,
    pub teams: BTreeSet<String>,
}

/// Whether `u` is an email address of letters, digits and `. _ + -` at a domain of letters, digits, `.` and `-`:
/// one safe to quote in a command or a page as it is.
pub fn plain_email(u: &str) -> bool {
    let Some((local, domain)) = u.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}

/// The domain of an email address.
fn domain_of(user: &str) -> Option<&str> {
    user.rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| !d.is_empty())
}

/// How an org's (or a team's) memory runs, as its admins set it (`set_settings`). A team's setting wins over
/// its org's; one set by neither takes its default.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Whether the engine compiles the scope's skill library and AIP graphs from its `compile_sources` as they
    /// come in (`crate::compile`): a model call per batch of sources, and documents written back into the
    /// scope. On unless set false, so it turns compiling off where sources are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile: Option<bool>,
    /// The sources the scope's skill library is compiled from: source prefixes (`kb:`, `github:`), or `*` for
    /// every source. Unset, nothing is compiled (or what the server's `AMPUP_MEMORY_COMPILE_SOURCES` names): a
    /// compile is the learning model reading every source, so it is chosen, never on by default. An empty
    /// list unsets it, back to the org's (or the server's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_sources: Option<Vec<String>>,
    /// Sources per compile round in the scope, instead of the server's `AMPUP_MEMORY_COMPILE_BATCH` (12). A round
    /// writes at most 8 skills, so where each source is a situation of its own (an error report) a batch of 8 or
    /// fewer leaves none without its skill. It doesn't change the compiler's id: nothing compiles again. 0 unsets
    /// it, back to the org's (or the server's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_batch: Option<usize>,
    /// What the scope's compile sources are, where the compile prompt should say so: `errors` (error reports, such
    /// as Sentry issues) has each skill's `when` name the exception type and message as reported, which is what an
    /// agent asks about. Unset, the scope compiles with the one prompt every scope does. A kind changes the prompt,
    /// so the compiler's id: the scope's history compiles again. `""` unsets it, back to the org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_kind: Option<String>,
    /// Whether the engine learns advice from the scope's rewarded runs and sessions on its own, and judges those
    /// nobody rewarded (`crate::advice_loop`): model calls over every recorder's traces. On unless set false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice: Option<bool>,
    /// Whether advice serves a situation's note with the procedures linked to it (`crate::advice`), as it did
    /// before notes were served alone. Off unless set true: on ORCA-bench the note alone scored 55% where the
    /// note with its procedures scored 42%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_procedures: Option<bool>,
    /// The people a note learned from runs must have helped (each by their latest judged session) before it is
    /// served outside the trial arm (`crate::sessions`), over the server's `AMPUP_MEMORY_ADVICE_EVIDENCE`. 0 serves
    /// every note: where agents ask outside sessions (a review bot's `POST /advice`), no session ever judges one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_evidence: Option<usize>,
    /// The people whose runs a team note learned from runs must come from, and each of its items (by the runs it
    /// cites), over the server's `AMPUP_MEMORY_ADVICE_LEARN_PEOPLE` (3): so no team note holds one person's private
    /// traces, or one person's preference as the team's. Never below 2 (`crate::advice`); a person's own notes are
    /// learned from that person alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_learn_people: Option<usize>,
    /// The rewarded runs or sessions each person's own advice waits for in the scope (`crate::advice_loop`), where
    /// that is fewer than the server's bars: a person's map is learned once they have this many new ones (instead
    /// of `AMPUP_MEMORY_ADVICE_MIN_NEW`, 10), a situation of theirs gets a note at this many runs (instead of 2 that
    /// went wrong or 4 that went right), and their note is written again after this many new runs (instead of
    /// `AMPUP_MEMORY_ADVICE_NOTE_MIN_NEW`, 5, or 2 wrong); and it is learned by the server's session sweep once such
    /// a run is rewarded, instead of at the advice timer's next round. At 1, only a run someone gave its reward
    /// counts, not one the judge alone read. A bar already lower stays. The team's notes are untouched. 1 to 10; 0
    /// unsets it, back to the org's (or the server's `AMPUP_MEMORY_ADVICE_PERSONAL_RUNS`); `off` turns it off here,
    /// over the org's value (for a team) and the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_personal_runs: Option<PersonalRuns>,
    /// How advice routes over the skills compiled from the scope's documents (`crate::advice`): `fallback` (the
    /// default) weighs the kinds of task learned from runs first, every one of them, and the compiled skills only
    /// when none fits; `together` weighs both on one list, as advice did before; `off` never serves a compiled
    /// skill. On a production deployment (three days, 1,579 people's lookups replayed) one list of 131 kinds of task and 621
    /// skills left 4 kinds of task in the 20 lines weighed, on average: weighed alone, 225 notes were served
    /// where 118 notes and 32 skills had been.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_skills: Option<String>,
    /// The accounts whose runs and sessions teach the scope's team notes nothing (`crate::advice`): a login people
    /// share, an eval harness's or a bot's, whose sessions mix tests with work and whose one id would count as one
    /// person for [`Self::advice_learn_people`] whoever was behind it. They still teach their own notes, and are
    /// still served the team's. User ids (emails) as their sign-in gives them; an empty list unsets it, back to
    /// the org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_learn_exclude: Option<Vec<String>>,
    /// Whether advice asked about an error (a Sentry short ID or issue link, an exception and its message, or stack
    /// frames) also leads to up to 3 similar past errors and the PRs that fixed them (`crate::error_leads`), from
    /// the `sentry:` and `github:` episodes the caller may read here and in the scopes this one reads: matched by
    /// exception, message, frames and the files the fixes changed, and by their words. No model call. Off unless set
    /// true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_leads: Option<bool>,
    /// Whether the engine reads every episode of the scope no goal extracts for facts, goal-blind
    /// (`crate::extract`): what the team remembers (its Claude Code sessions, notes) becomes facts with no schema naming it
    /// set up, which `search_memory` then finds with the episode cited. A model call per episode. On unless set
    /// false; `AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1` turns it on everywhere, whatever the settings say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<bool>,
    /// The model the scope's episodes are extracted with, on the server's extraction backend, instead of the
    /// server's (`AMPUP_MEMORY_EXTRACT_MODEL`). The operator's to set: a model spends the platform's budget.
    /// Empty: back to the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract_model: Option<String>,
    /// The model skill libraries are compiled with, on the compile backend (`AMPUP_MEMORY_COMPILE_MODEL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_model: Option<String>,
    /// The model each skill's AIP graph is compiled with, on the compile backend (`AMPUP_MEMORY_AIP_MODEL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aip_model: Option<String>,
    /// Whether the server's training log (`AMPUP_MEMORY_TRAIN_LOG`) keeps the scope's model calls whole, prompts and
    /// replies, to train the org's own models from. The text of the org's memory, so the operator's to set, and off
    /// unless set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_log: Option<bool>,
    /// Whether the plugin's hooks upload the transcripts of sessions that called Claude Code's advisor or memstore's get_advice, tool calls
    /// and results included (redacted, each result capped), with a record of each call, to train an advice model
    /// on the conversation at each call (`crate::advice_traces`). Each is an episode of its author's own, which
    /// forgetting, retention and offboarding remove as any other; only the org's admins export them. More of a
    /// session than memstore keeps otherwise, so the operator's to set, and off unless set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_traces: Option<bool>,
    /// Whether fact search in the scope ranks as it did before the current ranking (`search::Rank::classic`):
    /// keyword (BM25 over a fact's subject, predicate and value alike) and meaning fused by reciprocal rank,
    /// each scaled by recency as of the search's date, with no groups, no near-duplicate folding and no time
    /// window. For a reader whose evidence was measured on that ranking (the review bot's EX11 memory). Off
    /// unless set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classic_ranking: Option<bool>,
    /// Whether the scope's reads mark what its schema says has gone stale (`crate::freshness`): a value not
    /// stated again within its predicate's `freshness_days` reads "as of <date>, not confirmed since", and a plan
    /// date that has passed while its entity is still pending reads overdue, in `search_memory`, `/search`,
    /// `query_memory`'s entity cards and facts, and the brief. Nothing is dropped or ranked differently. Unset,
    /// the server's `AMPUP_MEMORY_FRESHNESS` decides (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<bool>,
    /// Whether the scope keeps facts that disagree apart (`crate::conflicts`): write-time reconcile may record a
    /// new fact as in conflict with a known one instead of ending it, and a value another fact disagrees with reads
    /// `conflict: "<other value>" (<source>, <date>)` beside it, in `search_memory`, `/search`, `query_memory`'s
    /// entity cards and facts, and the brief. Unset, the server's `AMPUP_MEMORY_CONFLICTS` decides (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<bool>,
    /// Whether the scope's learned advice is curated as it is learned (`crate::advice`'s curation): a note whose
    /// served runs do worse than the rest of its situation's is withheld (kept, served to no one), a note changes
    /// by small edits instead of being rewritten whole (an update dropping more than a quarter of it is refused),
    /// and situations that are the same task are merged. Unset, the server's `AMPUP_MEMORY_ADVICE_CURATION` decides
    /// (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_curation: Option<bool>,
    /// Whether advice is routed for sessions a program runs (a harness named `:sdk`: `claude -p`, an SDK run, a
    /// review bot, an eval's generator). Off unless set true: advice is learned from people's sessions only, so a
    /// program's own kind of task never has a note, and routing its lookups only costs model calls and seconds (on
    /// a production deployment, over three days, 1,561 of them were served 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice_automated: Option<bool>,
    /// Whether each person's own notes in the scope are folded into one deduplicated, capped profile (the newer
    /// note winning where two disagree), which `start_session` hands them with their newest few notes instead of
    /// only their newest 20 (`memstore::profile`). Unset, the server's `AMPUP_MEMORY_PROFILE` decides (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<bool>,
    /// Whether the plugin's hooks keep each coding agent session's whole transcript (prompts, answers, every tool
    /// call with its input and result; never thinking) as its author's own episode, readable by them alone, so a
    /// later pass can check what was noted against what actually ran (`memstore::transcripts`). Never extracted,
    /// searched or read for the team; forgetting, retention and offboarding remove it as any of their episodes.
    /// Tool output can hold anything the session read, customers' data too, so off unless set true. Unset, the
    /// server's `AMPUP_MEMORY_TRANSCRIPTS` decides (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcripts: Option<bool>,
    /// How the MCP `initialize` instructions and the descriptions of `start_session`, `get_advice`, `end_session`
    /// and `remember_memory` read: `default`, the session's call order (start_session first, get_advice when a
    /// task starts, remember_memory in the turn a preference is stated, end_session last); `purpose`, what memory
    /// is for and when it helps, with no fixed procedure, the judgment left to the agent. The tools, their
    /// arguments and permissions are the same either way. Unset (`""`), the server's `AMPUP_MEMORY_INSTRUCTIONS`
    /// decides (`default` unless set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Whether each person's own notes in the scope are dreamed over (`memstore::notes_dream`): checked against
    /// their own sessions' records, a note the person changed in a later session marked replaced, one stated
    /// confidently where the record shows doubt marked unsure, two that can't both be true marked a conflict, and
    /// near-duplicates folded, all as records beside the notes that `start_session` reads. Unset, the server's
    /// `AMPUP_MEMORY_NOTES_DREAM_ON` decides (off by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_dream: Option<bool>,
    /// Whether the notes a session is handed (a person's own and what their team shares) are picked by a decisions
    /// model judging which bear on the task (`memstore::keep::bearing_on`), on the route the server's extraction
    /// takes. Set false, a person's notes never go to that model: the ones that share a word with the task are
    /// handed instead. On unless set false (where the server has a decisions model on that route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_decide: Option<bool>,
    /// Other scopes of the same org whose memory this scope's readers also search (`team:<org>/<team>`, or
    /// `org:<org>` for a team): the read routes they search through (`/search`, `/episode`, `search_memory`,
    /// `get_episode`) read this scope and these, each hit naming its scope. Writes, extraction, the schema and
    /// advice stay in the scope itself, and so do its other reads. The setting is the scope's own: a team never
    /// takes its org's, and a scope read this way doesn't pass on its own (`Directory::reads`). It decides who
    /// reads what, so it is the operator's to set. An empty list unsets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<Vec<String>>,
    /// Whether `serve`'s nightly maintenance window deletes from storage what the scope's sources deleted
    /// (`POST /retract`) and what aged past its retention policy, once hidden for longer than the server's
    /// `AMPUP_MEMORY_PURGE_GRACE` (7 days): the episodes, their bodies, the facts citing them and what hangs off those,
    /// left out of the same rewrite that compacts each table, as `memstore org purge` does on request. Reads hide
    /// all of it already; this takes it out of the files. What it deletes can't be brought back by loosening the
    /// policy (and the store's own soft delete, where it has one, keeps it only as long as that lasts). Off unless
    /// set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge: Option<bool>,
    /// Whether the org's admins may read each person's activity (`/admin {op: "activity"}`): the questions
    /// they asked, their sessions' answers, and the facts, episodes and advice each used. An org setting
    /// (a team's is ignored); on unless set false, for an org that keeps people's questions to themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<bool>,
    /// Whether each session that writes gets a branch of its own (`crate::sessions`): what it remembers lands
    /// there, its reads see main as of then plus its own writes, and once it ends a clean branch merges into main
    /// on its own while one that conflicts waits for the key's owner. An org setting; off unless set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_branches: Option<bool>,
    /// Whether a change that waits for its owners (`crate::governance`) is decided by a decisions model instead:
    /// it approves or rejects each proposal on its evidence, recorded as `memstore:auto-review` with its model
    /// and confidence for the owners to audit; an owner may still approve what it rejected. An org setting; on
    /// unless set false (where a decisions model is configured: without one, every proposal waits for its owners).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_review: Option<bool>,
    /// Whether text written into the scope (ingest, `remember_memory`, a fact's closing reason) has its personal
    /// identifiers replaced before it is stored (`crate::pii`): payment card numbers, US social security numbers,
    /// IBANs and international phone numbers become `[card]`, `[ssn]`, `[iban]` and `[phone]`. Only text written
    /// after it is set is changed; what is stored stays as it was. Its team's setting wins over its org's; off
    /// unless set true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pii: Option<bool>,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        self.compile.is_none()
            && self.compile_sources.is_none()
            && self.compile_batch.is_none()
            && self.compile_kind.is_none()
            && self.advice.is_none()
            && self.advice_procedures.is_none()
            && self.advice_evidence.is_none()
            && self.advice_learn_people.is_none()
            && self.advice_personal_runs.is_none()
            && self.advice_skills.is_none()
            && self.advice_learn_exclude.is_none()
            && self.error_leads.is_none()
            && self.extract.is_none()
            && self.train_log.is_none()
            && self.advice_traces.is_none()
            && self.classic_ranking.is_none()
            && self.freshness.is_none()
            && self.conflicts.is_none()
            && self.advice_curation.is_none()
            && self.advice_automated.is_none()
            && self.profile.is_none()
            && self.transcripts.is_none()
            && self.instructions.is_none()
            && self.notes_dream.is_none()
            && self.notes_decide.is_none()
            && self.reads.is_none()
            && self.purge.is_none()
            && self.activity.is_none()
            && self.session_branches.is_none()
            && self.auto_review.is_none()
            && self.pii.is_none()
            && !self.sets_models()
    }
    /// Whether it names a model (the operator's to set).
    pub fn sets_models(&self) -> bool {
        self.extract_model.is_some() || self.compile_model.is_some() || self.aip_model.is_some()
    }
    /// What `other` sets, over this.
    fn merge(&mut self, other: Settings) {
        if other.compile.is_some() {
            self.compile = other.compile;
        }
        if other.compile_sources.is_some() {
            self.compile_sources = other.compile_sources;
        }
        if other.compile_batch.is_some() {
            self.compile_batch = other.compile_batch;
        }
        if other.compile_kind.is_some() {
            self.compile_kind = other.compile_kind;
        }
        if other.advice.is_some() {
            self.advice = other.advice;
        }
        if other.activity.is_some() {
            self.activity = other.activity;
        }
        if other.session_branches.is_some() {
            self.session_branches = other.session_branches;
        }
        if other.auto_review.is_some() {
            self.auto_review = other.auto_review;
        }
        if other.pii.is_some() {
            self.pii = other.pii;
        }
        if other.advice_procedures.is_some() {
            self.advice_procedures = other.advice_procedures;
        }
        if other.advice_evidence.is_some() {
            self.advice_evidence = other.advice_evidence;
        }
        if other.advice_learn_people.is_some() {
            self.advice_learn_people = other.advice_learn_people;
        }
        if other.advice_personal_runs.is_some() {
            self.advice_personal_runs = other.advice_personal_runs;
        }
        if other.advice_skills.is_some() {
            self.advice_skills = other.advice_skills.clone();
        }
        if other.advice_learn_exclude.is_some() {
            self.advice_learn_exclude = other.advice_learn_exclude.clone();
        }
        if other.error_leads.is_some() {
            self.error_leads = other.error_leads;
        }
        if other.extract.is_some() {
            self.extract = other.extract;
        }
        if other.train_log.is_some() {
            self.train_log = other.train_log;
        }
        if other.advice_traces.is_some() {
            self.advice_traces = other.advice_traces;
        }
        if other.classic_ranking.is_some() {
            self.classic_ranking = other.classic_ranking;
        }
        if other.freshness.is_some() {
            self.freshness = other.freshness;
        }
        if other.conflicts.is_some() {
            self.conflicts = other.conflicts;
        }
        if other.advice_curation.is_some() {
            self.advice_curation = other.advice_curation;
        }
        if other.advice_automated.is_some() {
            self.advice_automated = other.advice_automated;
        }
        if other.profile.is_some() {
            self.profile = other.profile;
        }
        if other.transcripts.is_some() {
            self.transcripts = other.transcripts;
        }
        if other.notes_dream.is_some() {
            self.notes_dream = other.notes_dream;
        }
        if other.notes_decide.is_some() {
            self.notes_decide = other.notes_decide;
        }
        if other.instructions.is_some() {
            self.instructions = other.instructions;
        }
        if other.reads.is_some() {
            self.reads = other.reads;
        }
        if other.purge.is_some() {
            self.purge = other.purge;
        }
        for (mine, theirs) in [
            (&mut self.extract_model, other.extract_model),
            (&mut self.compile_model, other.compile_model),
            (&mut self.aip_model, other.aip_model),
        ] {
            if theirs.is_some() {
                *mine = theirs;
            }
        }
    }
}

/// A model id as the backends name them (`anthropic/claude-haiku-5.5`, `qwen/qwen3.7-plus`, `gpt-5-mini`), or
/// empty for the server's own.
pub fn valid_model(m: &str) -> bool {
    m.len() <= 128
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._:-@".contains(&b))
}

/// What an org told `memstore org init` it wants from memory.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// What the org does, in its own words.
    #[serde(default)]
    pub description: String,
    /// `sales`, `engineering` or `general`: which built-in schema (if any) its memory is typed with.
    #[serde(default)]
    pub kind: String,
    /// What memory should pay most attention to.
    #[serde(default)]
    pub focus: Vec<String>,
    /// What memory must never keep.
    #[serde(default)]
    pub avoid: Vec<String>,
    /// Where its knowledge comes from (slack, github, email, calls, crm, docs).
    #[serde(default)]
    pub sources: Vec<String>,
}

impl Profile {
    /// The profile as instructions for an agent reading this org's memory.
    pub fn instructions(&self, org: &str) -> String {
        let mut out = format!("This is the memory of {org}");
        if !self.description.is_empty() {
            out.push_str(&format!(" ({})", self.description.trim_end_matches('.')));
        }
        out.push('.');
        if !self.focus.is_empty() {
            out.push_str(&format!(
                " It matters most here: {}.",
                self.focus.join("; ")
            ));
        }
        if !self.avoid.is_empty() {
            out.push_str(&format!(
                " Never record or repeat: {}.",
                self.avoid.join("; ")
            ));
        }
        out
    }
}

/// An org as a snapshot keeps it: every field the log folds to.
#[derive(Serialize, Deserialize)]
struct SnapOrg {
    members: BTreeMap<String, Role>,
    teams: BTreeMap<String, BTreeSet<String>>,
    profile: Option<Profile>,
    provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth0_org: Option<String>,
    domains: BTreeMap<String, Option<String>>,
    groups: BTreeMap<String, BTreeSet<String>>,
    keys: BTreeMap<String, SnapKey>,
    removed: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    declined: BTreeMap<String, i64>,
    created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<crate::workspace::Spec>,
    #[serde(default, skip_serializing_if = "Settings::is_empty")]
    settings: Settings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    team_settings: BTreeMap<String, Settings>,
}

/// A service key as a snapshot keeps it: with its hash, never when it was last used (not the log's).
#[derive(Serialize, Deserialize)]
struct SnapKey {
    team: Option<String>,
    role: KeyRole,
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sources: Vec<String>,
    created_by: String,
    created_at: i64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    imported_user: Option<String>,
}

/// The directory folded from its log, and how much of the log that is.
#[derive(Clone, Debug)]
pub struct Folded {
    pub dir: Directory,
    /// How many changes are folded in.
    pub events: u64,
    /// The time of the last of them (µs since the epoch; 0: none). Every later change is after it.
    pub last_at: i64,
    /// A hash chained over every change folded in, in order: equal digests, equal logs.
    pub digest: String,
    /// The changes the newest snapshot this fold knows of covers (0: none).
    pub snapshot_events: u64,
    /// The first change this engine can't fold (its version is newer than [`EVENT_VERSION`], or its op
    /// isn't one of [`OPS`]): the fold stops before it, and every change after it waits too, since
    /// changes apply in order. Its version and time. None: every change was folded.
    pub blocked: Option<(i64, i64)>,
}

impl Folded {
    /// Before the first change.
    pub fn empty() -> Self {
        Folded {
            dir: Directory::empty(),
            events: 0,
            last_at: 0,
            digest: String::new(),
            snapshot_events: 0,
            blocked: None,
        }
    }

    /// Fold in `events`: all after what is folded in already, in order. Fails closed on a change this
    /// engine can't read (a newer version, or an op it doesn't know): it and everything after it are left
    /// out (never applied in part, never skipped over), and [`Folded::blocked`] says so. The fold is then
    /// the directory as of the change before: the last state this engine knows to be right.
    pub fn apply(&mut self, events: &[Event]) {
        self.blocked = None;
        let known = events
            .iter()
            .position(|e| !readable(e))
            .unwrap_or(events.len());
        if let Some(e) = events.get(known) {
            self.blocked = Some((e.version, e.at));
        }
        let events = &events[..known];
        self.dir.apply_events(events);
        for e in events {
            self.digest = chain(&self.digest, e);
            self.events += 1;
            self.last_at = self.last_at.max(e.at);
        }
    }
}

/// The digest of a log: `prev`'s changes, then `e`.
fn chain(prev: &str, e: &Event) -> String {
    let mut row = json!([e.at, e.actor, e.op, e.org, e.team, e.user, e.role, e.detail]);
    // Version 1 rows hash as they did before versions, so existing snapshots' digests still match.
    if e.version != 1 {
        row.as_array_mut().unwrap().push(json!(e.version));
    }
    let row = row.to_string();
    crate::auth::hex(&<sha2::Sha256 as sha2::Digest>::digest(
        format!("{prev}\n{row}").as_bytes(),
    ))
}

/// What a snapshot's state means changes when the fold does: a snapshot of another format is never
/// loaded (the log is replayed instead). Bump it with any change to what [`Directory::apply_events`] does.
pub const SNAPSHOT_FORMAT: i64 = 11;

/// A folded snapshot of the directory log (`memory_system.directory_snapshot`): the state after the log's
/// first `events` changes, the last of them at `at`.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub at: i64,
    pub events: u64,
    /// The log's digest after those changes ([`Folded::digest`]).
    pub digest: String,
    pub format: i64,
    /// [`Directory::snapshot_json`].
    pub state: String,
    /// µs since the epoch.
    pub taken_at: i64,
    pub actor: String,
    /// What vouches for it: a seal under the server's `AMPUP_MEMORY_SECRET_KEY` over its format, extent,
    /// digest and state ([`Snapshot::sealed`]). None: written by a server without the key, or before
    /// snapshots were sealed. A snapshot this server can't vouch for is checked against the log.
    pub seal: Option<String>,
}

impl Snapshot {
    pub fn of(f: &Folded, actor: &str, now: i64) -> Self {
        Snapshot {
            at: f.last_at,
            events: f.events,
            digest: f.digest.clone(),
            format: SNAPSHOT_FORMAT,
            state: f.dir.snapshot_json(),
            taken_at: now,
            actor: actor.into(),
            seal: None,
        }
    }

    /// What the seal covers: everything loading takes from the snapshot.
    fn sealed_text(&self) -> String {
        let state = crate::auth::hex(&<sha2::Sha256 as sha2::Digest>::digest(
            self.state.as_bytes(),
        ));
        format!(
            "memstore directory snapshot\n{}\n{}\n{}\n{}\n{state}",
            self.format, self.at, self.events, self.digest
        )
    }

    /// This snapshot, sealed under `key` (none: left unsealed).
    pub fn sealed_with(mut self, key: Option<&crate::auth::secret::SecretKey>) -> Self {
        self.seal = key.map(|k| k.seal(&self.sealed_text(), ""));
        self
    }

    /// Whether `key` sealed this snapshot as it is: a server holding the key wrote it from its own fold,
    /// so loading may start from it without reading the log before it.
    pub fn sealed(&self, key: &crate::auth::secret::SecretKey) -> bool {
        self.seal.as_deref().is_some_and(|s| {
            key.unseal(&self.sealed_text(), s)
                .is_ok_and(|p| p.is_empty())
        })
    }

    /// The fold this snapshot stands for.
    pub fn folded(&self) -> Result<Folded> {
        anyhow::ensure!(
            self.format == SNAPSHOT_FORMAT,
            "snapshot format {} (this engine reads {SNAPSHOT_FORMAT})",
            self.format
        );
        Ok(Folded {
            dir: Directory::from_snapshot_json(&self.state)?,
            events: self.events,
            last_at: self.at,
            digest: self.digest.clone(),
            snapshot_events: self.events,
            blocked: None,
        })
    }
}

/// Whether the newest snapshot agrees with replaying the whole log (`memstore identity check`,
/// `/admin {"op": "check_directory"}`).
#[derive(Clone, Debug, Serialize)]
pub struct SnapshotCheck {
    /// Changes in the log.
    pub events: u64,
    /// The newest snapshot: the changes it covers and the time of the last; none: no snapshot yet.
    pub snapshot: Option<(u64, i64)>,
    /// What is wrong; empty: the snapshot is the log's first changes folded, and loading from it gives
    /// what replaying the whole log gives.
    pub problems: Vec<String>,
}

/// Check `snap` against the whole log, `events` (in order).
pub fn check_snapshot(events: &[Event], snap: Option<&Snapshot>) -> SnapshotCheck {
    let mut out = SnapshotCheck {
        events: events.len() as u64,
        snapshot: snap.map(|s| (s.events, s.at)),
        problems: vec![],
    };
    if let Some(e) = events.iter().find(|e| !readable(e)) {
        out.problems.push(format!(
            "the log has {} at {}: this engine serves the directory as of the change before, and signs no one in",
            blocked_why(e.version),
            e.at
        ));
    }
    if let Some(snap) = snap {
        out.problems.extend(snapshot_problems(events, snap));
    }
    out
}

/// What is wrong with `snap` as the start of the log `events` (the whole log, in order); empty: it is the
/// log's first changes folded, and loading from it gives what replaying the whole log gives.
pub fn snapshot_problems(events: &[Event], snap: &Snapshot) -> Vec<String> {
    let mut out = vec![];
    let from_snap = match snap.folded() {
        Ok(f) => f,
        Err(e) => {
            out.push(format!("the snapshot can't be read: {e:#}"));
            return out;
        }
    };
    let covered = events.iter().take_while(|e| e.at <= snap.at).count();
    if covered as u64 != snap.events || events[covered..].iter().any(|e| e.at <= snap.at) {
        out.push(format!(
            "the snapshot folds {} changes up to {}, but the log has {covered} there (loading ignores it and replays the log)",
            snap.events, snap.at
        ));
        return out;
    }
    let mut upto = Folded::empty();
    upto.apply(&events[..covered]);
    if upto.digest != snap.digest {
        out.push("the snapshot's digest is not the log's: it was folded from other changes".into());
    }
    if upto.dir.snapshot_json() != snap.state {
        out.push("the snapshot's state is not what the log's changes fold to".into());
    }
    let mut full = upto;
    full.apply(&events[covered..]);
    let mut loaded = from_snap;
    loaded.apply(&events[covered..]);
    if loaded.dir.snapshot_json() != full.dir.snapshot_json() {
        out.push("loading from the snapshot gives another directory than replaying the log".into());
    }
    out
}

/// The directory as of its last change.
#[derive(Clone, Debug, Serialize)]
pub struct Directory {
    pub orgs: BTreeMap<String, Org>,
    /// Orgs deleted whose memory isn't known to be purged yet (`org.delete` with no `org.purged` after
    /// it): nobody can create one of these names again until `memstore org delete` has finished it.
    #[serde(skip)]
    pub purging: BTreeSet<String>,
    /// Orgs whose memory was purged and whose name hasn't been taken again since: a server still
    /// serving the old org (its directory a few seconds behind) may have written to it after the purge,
    /// so creating the name again purges whatever is there first.
    #[serde(skip)]
    pub purged: BTreeSet<String>,
    /// The last `user.disable` or `user.enable` about each user (by [`user_id`]): true, disabled. Every
    /// server refuses a disabled user, whichever server disabled them.
    #[serde(skip)]
    pub disabled: BTreeMap<String, bool>,
    /// Each user's closure edit (`closure.set`), by [`crate::auth::acl::norm_id`].
    #[serde(skip)]
    pub closure_edits: BTreeMap<String, ClosureEdit>,
    /// The pilot team csv last pushed (`team.csv`; none: never pushed, or cleared since). Where a server has
    /// its own csv too (`AMPUP_MEMORY_TEAM_CSV`), only people both list hold team groups (this csv's)
    /// ([`crate::auth::acl::team_of`]).
    #[serde(skip)]
    pub team_csv: Option<String>,
}

/// User ids are compared as the IdP's emails are: case-insensitively, folding ASCII letters only (as
/// [`crate::auth::acl::norm_id`] does). Unicode case rules would make other characters into ASCII ones (the
/// Kelvin sign into `k`), so an address an IdP verified could sign its holder in as someone else. An id with
/// other letters is compared as it is: one stored before (folded by Unicode rules) still matches the same
/// spelling, and no longer one that only folds into it.
pub fn user_id(u: &str) -> String {
    u.trim().to_ascii_lowercase()
}

/// Whether forgetting `user` takes what names `stored` as its person: the same id ([`user_id`]), or one written
/// before ids folded ASCII letters only, which Unicode case rules lower-cased (forgetting them took it then,
/// so it still does).
pub fn forgets(stored: &str, user: &str) -> bool {
    user_id(stored) == user_id(user) || stored.trim().to_lowercase() == user.trim().to_lowercase()
}

impl Directory {
    /// The state after `events`. The default org always exists: it is the catalog's own memory.
    /// Like [`Folded::apply`], it stops before a change of a newer version than this engine reads.
    pub fn fold(events: &[Event]) -> Self {
        let mut f = Folded::empty();
        f.apply(events);
        f.dir
    }

    /// The directory before its first change.
    pub fn empty() -> Self {
        Directory {
            orgs: BTreeMap::from([(DEFAULT_ORG.into(), Org::default())]),
            purging: BTreeSet::new(),
            purged: BTreeSet::new(),
            disabled: BTreeMap::new(),
            closure_edits: BTreeMap::new(),
            team_csv: None,
        }
    }

    /// Apply `events`, which come after every change already folded in, in order.
    pub fn apply_events(&mut self, events: &[Event]) {
        let orgs = &mut self.orgs;
        let purging = &mut self.purging;
        let purged = &mut self.purged;
        for e in events {
            let user = e.user.as_deref().map(user_id);
            let team = e.team.as_deref();
            match (e.op.as_str(), &user) {
                ("user.disable" | "user.enable", Some(_)) => {
                    self.disabled.insert(
                        crate::auth::acl::disabled_id(e.user.as_deref().unwrap_or_default()),
                        e.op == "user.disable",
                    );
                    continue;
                }
                ("closure.set", Some(_)) => {
                    // Keyed as closures compare ids: an email case-insensitively, any other id exactly.
                    let u = crate::auth::acl::norm_id(e.user.as_deref().unwrap_or_default());
                    let edit: ClosureEdit = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok())
                        .unwrap_or_default();
                    if edit.add.is_empty() && edit.remove.is_empty() {
                        self.closure_edits.remove(&u);
                    } else {
                        self.closure_edits.insert(u, edit);
                    }
                    continue;
                }
                ("team.csv", _) => {
                    self.team_csv = e.detail.clone();
                    continue;
                }
                _ => {}
            }
            if e.op == "org.create" {
                // A name whose old memory isn't purged yet is never taken again, whoever planned it (an
                // engine from before purging doesn't know to refuse it): the new org would read the old one.
                if purging.contains(&e.org) {
                    continue;
                }
                orgs.entry(e.org.clone()).or_insert_with(|| Org {
                    created_at: e.at,
                    ..Default::default()
                });
                purged.remove(&e.org);
                continue;
            }
            if e.op == "org.purged" {
                purging.remove(&e.org);
                purged.insert(e.org.clone());
                continue;
            }
            if e.op == "org.delete" {
                purging.insert(e.org.clone());
                orgs.remove(&e.org);
                continue;
            }
            let Some(org) = orgs.get_mut(&e.org) else {
                continue;
            };
            match (e.op.as_str(), team, user) {
                ("team.create", Some(t), _) => {
                    org.teams.entry(t.into()).or_default();
                }
                ("team.delete", Some(t), _) => {
                    org.teams.remove(t);
                    org.team_settings.remove(t);
                    // A team made again later is a new team: its old keys don't come back with it.
                    org.keys.retain(|_, k| k.team.as_deref() != Some(t));
                }
                ("member.add", _, Some(u)) => {
                    let role = e.role.as_deref().and_then(|r| Role::parse(r).ok());
                    org.removed.remove(&u);
                    org.pending.remove(&u);
                    org.declined.remove(&u);
                    org.members.insert(u, role.unwrap_or_default());
                }
                ("member.request", _, Some(u)) => {
                    if !org.members.contains_key(&u) {
                        org.pending.entry(u).or_insert(e.at);
                    }
                }
                ("member.decline", _, Some(u)) => {
                    org.pending.remove(&u);
                    org.declined.insert(u, e.at);
                }
                ("member.remove", _, Some(u)) => {
                    org.members.remove(&u);
                    org.pending.remove(&u);
                    org.removed.insert(u.clone());
                    for m in org.teams.values_mut() {
                        m.remove(&u);
                    }
                }
                ("team.add", Some(t), Some(u)) if org.members.contains_key(&u) => {
                    if let Some(m) = org.teams.get_mut(t) {
                        m.insert(u);
                    }
                }
                ("team.remove", Some(t), Some(u)) => {
                    if let Some(m) = org.teams.get_mut(t) {
                        m.remove(&u);
                    }
                }
                ("workspace.set", _, _) => {
                    org.workspace = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok());
                }
                ("org.profile", _, _) => {
                    org.profile = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok());
                }
                ("org.settings", t, _) => {
                    if let Some(set) = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok())
                    {
                        match t {
                            Some(t) => org.team_settings.entry(t.into()).or_default().merge(set),
                            None => org.settings.merge(set),
                        }
                    }
                }
                ("auth.provider", _, _) => {
                    org.provider = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str(d).ok());
                }
                ("org.auth0", _, _) => {
                    org.auth0_org = e.detail.clone();
                }
                ("domain.add", _, _) => {
                    if let Some(d) = &e.detail {
                        org.domains.insert(d.clone(), e.team.clone());
                    }
                }
                ("domain.remove", _, _) => {
                    if let Some(d) = &e.detail {
                        org.domains.remove(d);
                    }
                }
                ("group.map", Some(t), _) => {
                    if let Some(g) = &e.detail {
                        org.groups.entry(g.clone()).or_default().insert(t.into());
                    }
                }
                ("group.unmap", Some(t), _) => {
                    if let Some(g) = &e.detail
                        && let Some(teams) = org.groups.get_mut(g)
                    {
                        teams.remove(t);
                        if teams.is_empty() {
                            org.groups.remove(g);
                        }
                    }
                }
                ("key.create", _, _) => {
                    if let Some(k) = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str::<KeyDetail>(d).ok())
                    {
                        let imported_user = k.imported_user();
                        org.keys.insert(
                            k.name,
                            ServiceKey {
                                team: k.team,
                                role: k.role,
                                expires_at: k.expires_at,
                                sources: k.sources,
                                created_by: k.created_by,
                                created_at: e.at,
                                sha256: k.sha256,
                                last_used_at: None,
                                imported_user,
                            },
                        );
                    }
                }
                ("key.sources", _, _) => {
                    if let Some(d) = e
                        .detail
                        .as_deref()
                        .and_then(|d| serde_json::from_str::<KeySources>(d).ok())
                        && let Some(k) = org.keys.get_mut(&d.name)
                    {
                        k.sources = d.sources;
                    }
                }
                ("key.revoke", _, _) => {
                    if let Some(name) = &e.detail {
                        org.keys.remove(name);
                    }
                }
                // A known op missing what it acts on (no user, no team), or `member.forget` (a record only).
                // An op this engine doesn't know never gets here: the fold stops before it.
                _ => {}
            }
        }
    }

    /// The directory as of its last change: the newest snapshot this server knows plus the changes
    /// after it (see [`Store::directory`]), at most `AMPUP_MEMORY_MAX_STALENESS_MS` behind other servers.
    pub async fn load(store: &Store) -> Result<Self> {
        Ok(store.directory().await?.dir.clone())
    }

    /// The directory read from the catalog now, never a fold from before this call.
    pub async fn fresh(store: &Store) -> Result<Self> {
        Ok(store
            .directory_loaded_after(Some(std::time::Instant::now()))
            .await?
            .dir
            .clone())
    }

    /// The whole state, as a snapshot keeps it: everything the log folds to, hashes and all (unlike the
    /// views, which leave hashes out). Deterministic: equal directories give equal text.
    pub fn snapshot_json(&self) -> String {
        let orgs: BTreeMap<&String, SnapOrg> = self
            .orgs
            .iter()
            .map(|(name, o)| {
                let keys = o
                    .keys
                    .iter()
                    .map(|(n, k)| {
                        let key = SnapKey {
                            team: k.team.clone(),
                            role: k.role,
                            expires_at: k.expires_at,
                            sources: k.sources.clone(),
                            created_by: k.created_by.clone(),
                            created_at: k.created_at,
                            sha256: k.sha256.clone(),
                            imported_user: k.imported_user.clone(),
                        };
                        (n.clone(), key)
                    })
                    .collect();
                let org = SnapOrg {
                    members: o.members.clone(),
                    teams: o.teams.clone(),
                    profile: o.profile.clone(),
                    provider: o.provider.clone(),
                    auth0_org: o.auth0_org.clone(),
                    domains: o.domains.clone(),
                    groups: o.groups.clone(),
                    keys,
                    removed: o.removed.clone(),
                    pending: o.pending.clone(),
                    declined: o.declined.clone(),
                    created_at: o.created_at,
                    workspace: o.workspace.clone(),
                    settings: o.settings.clone(),
                    team_settings: o.team_settings.clone(),
                };
                (name, org)
            })
            .collect();
        json!({"orgs": orgs, "purging": self.purging,
               "purged": self.purged, "disabled": self.disabled,
               "closure_edits": self.closure_edits, "team_csv": self.team_csv})
        .to_string()
    }

    /// A directory from [`Self::snapshot_json`].
    pub fn from_snapshot_json(text: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Snap {
            orgs: BTreeMap<String, SnapOrg>,
            #[serde(default)]
            purging: BTreeSet<String>,
            #[serde(default)]
            purged: BTreeSet<String>,
            #[serde(default)]
            disabled: BTreeMap<String, bool>,
            #[serde(default)]
            closure_edits: BTreeMap<String, ClosureEdit>,
            #[serde(default)]
            team_csv: Option<String>,
        }
        let snap: Snap = serde_json::from_str(text)?;
        let orgs = snap
            .orgs
            .into_iter()
            .map(|(name, o)| {
                let keys = o
                    .keys
                    .into_iter()
                    .map(|(n, k)| {
                        let key = ServiceKey {
                            team: k.team,
                            role: k.role,
                            expires_at: k.expires_at,
                            sources: k.sources,
                            created_by: k.created_by,
                            created_at: k.created_at,
                            sha256: k.sha256,
                            last_used_at: None,
                            imported_user: k.imported_user,
                        };
                        (n, key)
                    })
                    .collect();
                let org = Org {
                    members: o.members,
                    teams: o.teams,
                    profile: o.profile,
                    provider: o.provider,
                    auth0_org: o.auth0_org,
                    domains: o.domains,
                    groups: o.groups,
                    keys,
                    removed: o.removed,
                    pending: o.pending,
                    declined: o.declined,
                    created_at: o.created_at,
                    workspace: o.workspace,
                    settings: o.settings,
                    team_settings: o.team_settings,
                };
                (name, org)
            })
            .collect();
        Ok(Directory {
            orgs,
            purging: snap.purging,
            purged: snap.purged,
            disabled: snap.disabled,
            closure_edits: snap.closure_edits,
            team_csv: snap.team_csv,
        })
    }

    /// Fill in when each key was last used, from `usage` (µs since the epoch, by sha256).
    pub fn with_usage(mut self, usage: &std::collections::HashMap<String, i64>) -> Self {
        for k in self.orgs.values_mut().flat_map(|o| o.keys.values_mut()) {
            k.last_used_at = usage.get(&k.sha256).copied();
        }
        self
    }

    fn org(&self, org: &str) -> Result<&Org> {
        self.orgs
            .get(org)
            .ok_or_else(|| anyhow::anyhow!("no org {org}"))
    }

    /// What `user`, signed in as `via` says, holds in each org: their own membership, plus what the org's
    /// domains and (through its own provider) groups give them. Orgs their provider doesn't vouch for, and
    /// orgs that require their own provider when they signed in through another, give them nothing.
    pub fn access(&self, user: &str, via: &SignIn) -> BTreeMap<String, Access> {
        let user = user_id(user);
        let mut out = BTreeMap::new();
        if let Some(k) = &via.key {
            // A key holds its own scope only, and only while the directory still has it (a server's
            // cache of keys may be a few seconds behind; this read is not).
            let Some(org) = self.orgs.get(&k.org) else {
                return out;
            };
            let Some(held) = org.keys.get(&k.name).filter(|s| s.sha256 == k.sha256) else {
                return out;
            };
            // An imported file key reads no more than its user may: once they are out of the org (or its
            // team), so is the key, whichever planner removed them.
            if let Some(u) = &held.imported_user
                && !self.holds_member(u, &k.org, held.team.as_deref())
            {
                return out;
            }
            let teams = match &k.team {
                Some(t) if org.teams.contains_key(t) => BTreeSet::from([t.clone()]),
                Some(_) => return out,
                None => BTreeSet::new(),
            };
            let access = Access {
                role: Role::Member,
                teams,
            };
            out.insert(k.org.clone(), access);
            return out;
        }
        for (name, org) in &self.orgs {
            if !self.vouched(name, via) {
                continue;
            }
            let removed = org.removed.contains(&user);
            // A domain admits a person whose provider said their email is verified (through another
            // org's provider, they never got this far).
            // Not through Auth0: an Organization's connection may admit anyone, so there the directory's
            // own members are the members.
            let by_domain = domain_of(&user)
                .and_then(|d| org.domains.get(d))
                .filter(|_| !removed && via.person && via.verified_email && !via.auth0);
            let by_group: BTreeSet<String> = match &via.orgs {
                Some(_) if !removed => via
                    .groups
                    .iter()
                    .filter_map(|g| org.groups.get(g))
                    .flatten()
                    .filter(|t| org.teams.contains_key(*t))
                    .cloned()
                    .collect(),
                _ => BTreeSet::new(),
            };
            let role = match org.members.get(&user) {
                Some(r) => *r,
                None if by_domain.is_some() || !by_group.is_empty() => Role::Member,
                None => continue,
            };
            let mut teams: BTreeSet<String> = org
                .teams
                .iter()
                .filter(|(_, m)| m.contains(&user))
                .map(|(t, _)| t.clone())
                .collect();
            if let Some(Some(t)) = by_domain
                && org.teams.contains_key(t)
            {
                teams.insert(t.clone());
            }
            teams.extend(by_group);
            out.insert(name.clone(), Access { role, teams });
        }
        out
    }

    /// Whether a sign-in counts in `org`: an org's own provider vouches for that org only, and an org
    /// that requires its provider takes no other sign-in, a person's or a service's (its own keys are
    /// held to it before this). A sign-in through the platform's
    /// Auth0 tenant counts only in the one org that recorded its Auth0 Organization, as this fold has it
    /// (the server's 5 s view of the bindings, `via.orgs`, can only narrow that), and never in an org
    /// that requires its own provider.
    fn vouched(&self, org: &str, via: &SignIn) -> bool {
        if via.auth0 {
            return via.orgs.as_ref().is_some_and(|b| b.contains(org))
                && self.auth0_bound(via.auth0_org.as_deref()) == Some(org)
                && self
                    .orgs
                    .get(org)
                    .is_some_and(|o| o.provider.as_ref().is_none_or(|p| !p.required));
        }
        // Bound to orgs (their own provider): those orgs alone; a keys-file key is bound to none of them. Not
        // bound: any org that doesn't require its own provider. That is every other way in, a person's or not:
        // the server's provider, a gateway (IAP, a Google ID token, Snowflake), an app's JWT. An
        // org's own keys never get here ([`Self::access`] holds a key to its scope first): the org's admins
        // make them, so they are how a service reaches an org that requires its provider.
        match &via.orgs {
            Some(bound) => bound.contains(org),
            None => !self
                .orgs
                .get(org)
                .and_then(|o| o.provider.as_ref())
                .is_some_and(|p| p.required),
        }
    }

    /// Whether the engine may compile `scope`'s skill library (`crate::compile`): its team's `compile` setting,
    /// else its org's; on where neither sets it. A scope the directory doesn't hold (a `serve --local` server's
    /// own, a user's) has no admin to set it, so it may too. What it compiles is [`Self::compile_sources`].
    pub fn compiles(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.compile, true)
    }

    /// The sources `scope`'s skill library is compiled from (source prefixes, or `*`): its team's
    /// `compile_sources`, else its org's; none where neither sets a list, or in a scope the directory doesn't
    /// hold.
    pub fn compile_sources(&self, scope: &crate::Scope) -> Option<Vec<String>> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let set = |s: &Settings| s.compile_sources.clone().filter(|v| !v.is_empty());
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(set)
            .or_else(|| set(&org.settings))
    }

    /// The sources per compile round `scope` sets (`Settings::compile_batch`): its team's, else its org's; none
    /// where neither sets one (0 is unset), or in a scope the directory doesn't hold.
    pub fn compile_batch(&self, scope: &crate::Scope) -> Option<usize> {
        self.team_or_org(scope, |s| s.compile_batch.filter(|&n| n > 0))
    }

    /// What `scope`'s compile sources are (`Settings::compile_kind`, such as `errors`): its team's, else its org's;
    /// none where neither sets one (`""` is unset), or in a scope the directory doesn't hold.
    pub fn compile_kind(&self, scope: &crate::Scope) -> Option<String> {
        self.team_or_org(scope, |s| s.compile_kind.clone().filter(|k| !k.is_empty()))
    }

    /// A setting of `scope`'s team where it sets one, else of its org; none in a scope the directory doesn't hold.
    fn team_or_org<T>(
        &self,
        scope: &crate::Scope,
        pick: impl Fn(&Settings) -> Option<T>,
    ) -> Option<T> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(&pick)
            .or_else(|| pick(&org.settings))
    }

    /// Whether the engine extracts `scope`'s episodes no goal reads, goal-blind (`crate::extract`): its team's
    /// `extract` setting, else its org's; on where neither sets it (what an org's people say to the memory is
    /// found by `search_memory`, with no one setting it up; `extract: false` turns it off), and off in a scope the
    /// directory doesn't hold.
    pub fn extracts_unclaimed(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.extract, self.holds(scope))
    }

    /// Whether the directory holds `scope`'s org.
    fn holds(&self, scope: &crate::Scope) -> bool {
        scope.org_name().is_some_and(|o| self.orgs.contains_key(o))
    }

    /// Whether the engine extracts `scope`'s starter documents (`crate::extract::STARTER`) where it extracts
    /// nothing else the schema doesn't read: unless its admins turned extraction off in so many words
    /// (`set_settings {extract: false}`).
    pub fn extracts_starter(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.extract, true)
    }

    /// Whether the nightly maintenance window deletes from storage what `scope`'s sources deleted and what aged
    /// past its retention (`Settings::purge`): its team's setting, else its org's; off where neither sets it, and
    /// in a scope the directory doesn't hold.
    pub fn purges(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.purge, false)
    }

    /// Whether the engine learns advice in `scope` on its own and judges its unrewarded runs
    /// (`crate::advice_loop`): its team's `advice` setting, else its org's; on where neither sets it (the
    /// sessions its people end teach advice, which is served beyond their own only once others ratify it;
    /// `advice: false` turns it off), and off in a scope the directory doesn't hold.
    pub fn learns_advice(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.advice, self.holds(scope))
    }

    /// Whether a session in `org` that writes gets a branch of its own (its `session_branches` setting): off unless
    /// the org set it true.
    pub fn session_branches(&self, org: &str) -> bool {
        self.orgs
            .get(org)
            .and_then(|o| o.settings.session_branches)
            .unwrap_or(false)
    }

    /// Whether `org`'s owner reviews are decided by a decisions model (its `auto_review` setting): on unless the org
    /// set it false.
    pub fn auto_review(&self, org: &str) -> bool {
        self.orgs
            .get(org)
            .and_then(|o| o.settings.auto_review)
            .unwrap_or(true)
    }

    /// Whether `org`'s admins may read each person's activity (its `activity` setting): on unless the org set it
    /// false.
    pub fn shows_activity(&self, org: &str) -> bool {
        self.orgs
            .get(org)
            .and_then(|o| o.settings.activity)
            .unwrap_or(true)
    }

    /// Whether advice in `scope` serves a situation's note with its linked procedures (`crate::advice`): its
    /// team's `advice_procedures` setting, else its org's; off (the note alone) where neither sets it, and in a
    /// scope the directory doesn't hold.
    pub fn advises_procedures(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.advice_procedures, false)
    }

    /// Whether advice in `scope` leads an error to similar past ones and their fixes (`Settings::error_leads`): its
    /// team's setting, else its org's; off where neither sets it, and in a scope the directory doesn't hold.
    pub fn leads_errors(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.error_leads, false)
    }

    /// The people a learned note must have helped before `scope` serves it outside the trial arm
    /// (`crate::sessions::min_evidence`): its team's `advice_evidence` setting, else its org's; none (the server's
    /// `AMPUP_MEMORY_ADVICE_EVIDENCE`) where neither sets it, or in a scope the directory doesn't hold.
    pub fn advice_evidence(&self, scope: &crate::Scope) -> Option<usize> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(|s| s.advice_evidence)
            .or(org.settings.advice_evidence)
    }

    /// How advice in `scope` routes over its compiled skills (`Settings::advice_skills`): its team's setting, else
    /// its org's; `fallback` where neither sets one (`""` is unset), and in a scope the directory doesn't hold.
    pub fn advice_skills(&self, scope: &crate::Scope) -> String {
        self.team_or_org(scope, |s| s.advice_skills.clone().filter(|k| !k.is_empty()))
            .unwrap_or_else(|| "fallback".into())
    }

    /// The people a team note learned from runs, and each of its items, must come from in `scope`
    /// (`crate::advice`): its team's `advice_learn_people` setting, else its org's; none (the server's) where
    /// neither sets it, or in a scope the directory doesn't hold.
    pub fn advice_learn_people(&self, scope: &crate::Scope) -> Option<usize> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(|s| s.advice_learn_people)
            .or(org.settings.advice_learn_people)
    }

    /// The rewarded runs each person's own advice waits for in `scope` (`Settings::advice_personal_runs`): its
    /// team's setting, else its org's (0 is unset), else `default` (the server's `AMPUP_MEMORY_ADVICE_PERSONAL_RUNS`),
    /// which a scope the directory doesn't hold also gets. None: the server's own bars, as where the team's setting,
    /// or else the org's, is `off`.
    pub fn advice_personal_runs(
        &self,
        scope: &crate::Scope,
        default: Option<usize>,
    ) -> Option<usize> {
        let set = |s: &Settings| {
            s.advice_personal_runs
                .filter(|r| *r != PersonalRuns::Runs(0))
        };
        match self.team_or_org(scope, set) {
            Some(PersonalRuns::Runs(n)) => Some(n),
            Some(PersonalRuns::Off) => None,
            None => default,
        }
    }

    /// The accounts whose runs teach `scope`'s team notes nothing (`Settings::advice_learn_exclude`), as ACL ids:
    /// its team's list, else its org's; none where neither sets one (an empty list is unset), or in a scope the
    /// directory doesn't hold.
    pub fn advice_learn_exclude(&self, scope: &crate::Scope) -> Vec<String> {
        let Some(org) = scope.org_name().and_then(|o| self.orgs.get(o)) else {
            return vec![];
        };
        let set = |s: &Settings| s.advice_learn_exclude.clone().filter(|v| !v.is_empty());
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(set)
            .or_else(|| set(&org.settings))
            .unwrap_or_default()
            .iter()
            .map(|u| crate::auth::acl::norm_id(u.trim()))
            .collect()
    }

    /// The model `scope` runs a job on (`pick`: [`Settings::extract_model`], `compile_model` or `aip_model`):
    /// its team's, else its org's; none (the server's own) where neither names one, or in a scope the
    /// directory doesn't hold.
    pub fn model(
        &self,
        scope: &crate::Scope,
        pick: impl Fn(&Settings) -> Option<&String>,
    ) -> Option<String> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let named = |s: &Settings| pick(s).filter(|m| !m.is_empty()).cloned();
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(named)
            .or_else(|| named(&org.settings))
    }

    /// Whether the training log keeps `scope`'s model calls (`Settings::train_log`): its team's setting, else its
    /// org's; off where neither sets it, and in a scope the directory doesn't hold.
    pub fn trains(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.train_log, false)
    }

    /// Whether the hooks upload `scope`'s advisor and get_advice transcripts (`Settings::advice_traces`): its team's
    /// setting, else its org's; None where neither sets it, and in a scope the directory doesn't hold.
    pub fn keeps_advice_traces(&self, scope: &crate::Scope) -> Option<bool> {
        let org = scope.org_name().and_then(|o| self.orgs.get(o))?;
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(|s| s.advice_traces)
            .or(org.settings.advice_traces)
    }

    /// Whether text written into `scope` has personal identifiers replaced first (`Settings::pii`): its team's
    /// setting, else its org's; off where neither sets it, and in a scope the directory doesn't hold.
    pub fn redacts_pii(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.pii, false)
    }

    /// Whether fact search in `scope` ranks the classic way (`Settings::classic_ranking`): its team's setting, else
    /// its org's; off where neither sets it, and in a scope the directory doesn't hold.
    pub fn ranks_classic(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.classic_ranking, false)
    }

    /// Whether reads in `scope` mark stale facts and passed plans (`Settings::freshness`): its team's setting,
    /// else its org's, else the server's `AMPUP_MEMORY_FRESHNESS` (off unless set), which a scope the directory
    /// doesn't hold also gets.
    pub fn marks_freshness(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.freshness, crate::freshness::from_env())
    }

    /// Whether `scope` keeps facts that disagree apart and its reads mark them (`Settings::conflicts`): its team's
    /// setting, else its org's, else the server's `AMPUP_MEMORY_CONFLICTS` (off unless set), which a scope the
    /// directory doesn't hold also gets.
    pub fn marks_conflicts(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.conflicts, crate::conflicts::from_env())
    }

    /// Whether advice in `scope` is curated as it is learned (`Settings::advice_curation`): its team's setting,
    /// else its org's, else `default` (the server's `AMPUP_MEMORY_ADVICE_CURATION`), which a scope the directory
    /// doesn't hold also gets.
    pub fn curates_advice(&self, scope: &crate::Scope, default: bool) -> bool {
        self.setting(scope, |s| s.advice_curation, default)
    }

    /// Whether advice in `scope` is routed for sessions a program runs (`Settings::advice_automated`): its team's
    /// setting, else its org's; off where neither sets it, and in a scope the directory doesn't hold.
    pub fn advises_automated(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.advice_automated, false)
    }

    /// Whether `scope` folds each person's notes into a profile and hands it at a session's start
    /// (`Settings::profile`): its team's setting, else its org's, else the server's `AMPUP_MEMORY_PROFILE` (off unless
    /// set, [`profiles_from_env`]), which a scope the directory doesn't hold also gets.
    pub fn folds_profiles(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.profile, profiles_from_env())
    }

    /// Whether the hooks keep `scope`'s sessions' whole transcripts (`Settings::transcripts`): its team's setting,
    /// else its org's, else the server's `AMPUP_MEMORY_TRANSCRIPTS` (off unless set), which a scope the directory
    /// doesn't hold also gets.
    pub fn keeps_transcripts(&self, scope: &crate::Scope) -> bool {
        let env = std::env::var("AMPUP_MEMORY_TRANSCRIPTS").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        });
        self.setting(scope, |s| s.transcripts, env)
    }

    /// Whether `scope`'s agents are told what memory is for rather than when to call each tool
    /// (`Settings::instructions` `purpose`): its team's setting, else its org's (`""` is unset), else the server's
    /// `AMPUP_MEMORY_INSTRUCTIONS` ([`purpose_from_env`]), which a scope the directory doesn't hold also gets.
    pub fn purpose_instructions(&self, scope: &crate::Scope) -> bool {
        self.team_or_org(scope, |s| s.instructions.clone().filter(|k| !k.is_empty()))
            .map_or_else(purpose_from_env, |k| k == "purpose")
    }

    /// Whether each person's notes in `scope` are dreamed over (`Settings::notes_dream`): its team's setting, else
    /// its org's, else `default` (the server's `AMPUP_MEMORY_NOTES_DREAM_ON`), which a scope the directory doesn't
    /// hold also gets.
    pub fn dreams_notes(&self, scope: &crate::Scope, default: bool) -> bool {
        self.setting(scope, |s| s.notes_dream, default)
    }

    /// Whether the notes a session in `scope` is handed may be judged by a decisions model
    /// (`Settings::notes_decide`): its team's setting, else its org's; on where neither sets it, and in a scope the
    /// directory doesn't hold.
    pub fn decides_notes(&self, scope: &crate::Scope) -> bool {
        self.setting(scope, |s| s.notes_decide, true)
    }

    /// The other scopes `scope`'s readers also search (`Settings::reads`), in the order the setting names them:
    /// only the scope's own setting (a team's never comes from its org), only scopes of its own org, never itself,
    /// and only teams the org still has. None in a scope the directory doesn't hold.
    pub fn reads(&self, scope: &crate::Scope) -> Vec<crate::Scope> {
        let Some((name, org)) = scope.org_name().and_then(|o| self.orgs.get_key_value(o)) else {
            return vec![];
        };
        let own = match scope {
            crate::Scope::Team(id) => {
                let team = id.split_once('/').map_or(id.as_str(), |(_, t)| t);
                org.team_settings.get(team).and_then(|s| s.reads.as_ref())
            }
            _ => org.settings.reads.as_ref(),
        };
        let mut out: Vec<crate::Scope> = vec![];
        for s in own.into_iter().flatten() {
            let Ok(s) = crate::Scope::parse(s) else {
                continue;
            };
            let held = match &s {
                crate::Scope::Team(id) => id
                    .split_once('/')
                    .is_some_and(|(o, t)| o == name && org.teams.contains_key(t)),
                crate::Scope::NamedOrg(o) => o == name,
                _ => false,
            };
            if held && &s != scope && !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// A setting in `scope`: its team's, else its org's, else `default`, which a scope the directory doesn't hold
    /// also gets.
    fn setting(
        &self,
        scope: &crate::Scope,
        pick: impl Fn(&Settings) -> Option<bool>,
        default: bool,
    ) -> bool {
        let Some(org) = scope.org_name().and_then(|o| self.orgs.get(o)) else {
            return default;
        };
        let team = match scope {
            crate::Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
            _ => None,
        };
        team.and_then(|t| org.team_settings.get(t))
            .and_then(&pick)
            .or(pick(&org.settings))
            .unwrap_or(default)
    }

    /// The org that recorded Auth0 Organization `id`. Two that both hold it (written past `plan`, by
    /// hand) name neither, as a key hash two keys share does.
    pub fn auth0_bound(&self, id: Option<&str>) -> Option<&str> {
        let id = id?;
        let mut found = self
            .orgs
            .iter()
            .filter(|(_, o)| o.auth0_org.as_deref() == Some(id));
        match (found.next(), found.next()) {
            (Some((name, _)), None) => Some(name.as_str()),
            _ => None,
        }
    }

    /// Where a person signed in through the platform's Auth0 tenant for no Organization belongs: each org
    /// whose own members include them, with the Auth0 Organization it is bound to. Only for an email the
    /// tenant vouches for itself (its own connections: `verified_email`), never an Organization's
    /// enterprise connection, which asserts whatever email it likes. It grants nothing: such a sign-in
    /// still reaches no org ([`Directory::vouched`]). The control plane makes the person a member of those
    /// Organizations, so their next sign-in lands in one; a domain admits nobody through Auth0, so only the
    /// directory's own members are named, and never in an org that requires its own provider.
    pub fn auth0_orgs_of(&self, user: &str, via: &SignIn) -> Vec<(String, String)> {
        if !(via.auth0 && via.person && via.verified_email)
            || via.auth0_org.is_some()
            || via.operator
            || via.key.is_some()
        {
            return Vec::new();
        }
        let user = user_id(user);
        self.orgs
            .iter()
            .filter(|(_, o)| o.members.contains_key(&user) && !o.removed.contains(&user))
            .filter(|(_, o)| o.provider.as_ref().is_none_or(|p| !p.required))
            .filter_map(|(name, o)| {
                let id = o.auth0_org.as_deref()?;
                (self.auth0_bound(Some(id)) == Some(name.as_str()))
                    .then(|| (name.clone(), id.to_string()))
            })
            .collect()
    }

    /// The orgs `user` asked to join and no admin has answered yet.
    pub fn pending_of(&self, user: &str) -> Vec<String> {
        let user = user_id(user);
        self.orgs
            .iter()
            .filter(|(_, o)| o.pending.contains_key(&user))
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Whether `user` is a member the directory itself holds in `org`, and in `team` when one is given: one an
    /// invite, a sync or `add_member` made, never one only an email domain or a provider's group admits (that
    /// takes a sign-in proving the email or the group). Whom a recorder key may act for (`x-memstore-as`): it
    /// names people, and proves nothing about them.
    pub fn holds_member(&self, user: &str, org: &str, team: Option<&str>) -> bool {
        let user = user_id(user);
        let Some(o) = self.orgs.get(org) else {
            return false;
        };
        o.members.contains_key(&user)
            && !o.removed.contains(&user)
            && team.is_none_or(|t| o.teams.get(t).is_some_and(|m| m.contains(&user)))
    }

    pub fn is_admin(&self, user: &str, via: &SignIn, org: &str) -> bool {
        self.access(user, via).get(org).map(|a| a.role) == Some(Role::Admin)
    }

    /// The orgs `user` belongs to, with their role and teams there.
    pub fn memberships(&self, user: &str, via: &SignIn) -> Value {
        json!(self.access(user, via))
    }

    /// The memory `user` gets for a selection: the team's scope when `team` is given, else the org's.
    /// With no org named, the one org they belong to (several: they must choose). Refused unless they
    /// are a member of that org and, for a team, of that team: org admins included, since a team is
    /// isolated from the rest of its org. A service key that selects nothing gets its own scope; a
    /// team's key never gets its org's.
    pub fn resolve(
        &self,
        user: &str,
        via: &SignIn,
        org: Option<&str>,
        team: Option<&str>,
    ) -> Result<Scope> {
        if let Some(k) = via.directory_key() {
            anyhow::bail!(
                "{} is a directory key: it manages org {}'s members and teams through /admin, and reads no memory",
                k.user(),
                k.org
            );
        }
        // A keys-file key (bound to no org) reads its server's own scope: the directory gives it nothing.
        if !via.person
            && !via.auth0
            && via.key.is_none()
            && via.orgs.as_ref().is_some_and(|o| o.is_empty())
        {
            anyhow::bail!(
                "{user} signed in with a keys-file key, which reads this server's own scope only: an org's memory takes one of the org's keys (ampup-memory apikey create)"
            );
        }
        let (org, team) = match &via.key {
            Some(k) if org.is_none() && team.is_none() => (Some(k.org.as_str()), k.team.as_deref()),
            _ => (org, team),
        };
        let user = user_id(user);
        let access = self.access(&user, via);
        let org = match org {
            Some(o) => o.to_string(),
            None => match access.keys().collect::<Vec<_>>().as_slice() {
                [one] => (*one).clone(),
                [] => anyhow::bail!("{user} belongs to no org"),
                many => anyhow::bail!(
                    "{user} belongs to several orgs ({}): select one",
                    many.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        };
        let Some(a) = access.get(&org) else {
            if via.auth0 && self.orgs.contains_key(&org) && !self.vouched(&org, via) {
                anyhow::bail!(
                    "this Auth0 sign-in is for {}, not org {org}'s: sign in through org {org}'s Auth0 organization",
                    via.auth0_org
                        .as_deref()
                        .map_or("no organization".to_string(), |o| format!(
                            "organization {o}"
                        ))
                );
            }
            if self.orgs.contains_key(&org) && !self.vouched(&org, via) && via.key.is_none() {
                anyhow::bail!(
                    "org {org} takes sign-ins through its own identity provider only: ampup-memory auth login (with org {org} selected); a service uses one of the org's keys (ampup-memory apikey create)"
                );
            }
            anyhow::bail!("{user} is not a member of org {org}");
        };
        match team {
            None => {
                if let Some(t) = via.key.as_ref().and_then(|k| k.team.as_ref()) {
                    anyhow::bail!("{user} reads team {org}/{t} only");
                }
                Scope::org(&org)
            }
            Some(t) => {
                anyhow::ensure!(
                    a.teams.contains(t),
                    "{user} is not a member of team {org}/{t}"
                );
                Scope::team(&org, t)
            }
        }
    }
}

/// Who asks for a change. `superuser`: reaches the catalog directly, or holds an admin API key.
#[derive(Clone, Debug, Default)]
pub struct Actor {
    pub user: String,
    pub superuser: bool,
    /// How they signed in (an org admin signed in through another org's provider is no admin here).
    pub via: SignIn,
    /// The catalog's owner, reaching it directly.
    pub owner: bool,
    /// sha256 (hex, lower-case) of every file key the actor's process holds: the server's keys file, or the
    /// catalog owner's (`AMPUP_MEMORY_API_KEYS`, with --direct). None of them may become a directory key.
    pub file_keys: BTreeSet<String>,
}

/// One member of an org as an IdP export lists them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SyncMember {
    pub user: String,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub teams: Vec<String>,
}

/// A change someone asks for.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    CreateOrg {
        org: String,
        /// Its first admin.
        admin: Option<String>,
        /// The Auth0 Organization (`org_…`) whose sign-ins count in it.
        #[serde(default)]
        auth0_org: Option<String>,
    },
    /// An org that is a workspace (`memstore workspace create`): an isolated, disposable memory.
    CreateWorkspace {
        org: String,
        #[serde(default)]
        spec: crate::workspace::Spec,
    },
    /// Delete an org and its memory for good (`memstore org delete`): `confirm` must repeat its name.
    DeleteOrg {
        org: String,
        #[serde(default)]
        confirm: Option<String>,
    },
    CreateTeam {
        org: String,
        team: String,
    },
    DeleteTeam {
        org: String,
        team: String,
    },
    AddMember {
        org: String,
        user: String,
        #[serde(default)]
        role: Role,
    },
    RemoveMember {
        org: String,
        user: String,
    },
    /// Add users to a team (and to its org as members, if they aren't yet).
    AddToTeam {
        org: String,
        team: String,
        users: Vec<String>,
    },
    RemoveFromTeam {
        org: String,
        team: String,
        users: Vec<String>,
    },
    /// Invite people: each becomes a member (an admin, if their role says so) and joins the teams listed
    /// with them. Unlike `Sync`, nobody is removed and teams must already exist (a typo isn't a new team).
    Invite {
        org: String,
        members: Vec<SyncMember>,
    },
    /// Ask to join the org, as the person making the change (`memstore join`, when the directory doesn't hold
    /// them there): the org's admins are told, and an invite (or `add_member`) lets the person in. Nobody asks
    /// for someone else, and a request grants nothing until an admin answers it.
    RequestJoin {
        org: String,
    },
    /// Turn away someone who asked to join: an admin's answer (an invite is the other).
    Decline {
        org: String,
        user: String,
    },
    /// Set what the org wants from memory (`memstore org init`).
    SetProfile {
        org: String,
        profile: Profile,
    },
    /// Set how the org's memory runs, or one team's (what it names; the rest stays as it was).
    SetSettings {
        org: String,
        #[serde(default)]
        team: Option<String>,
        settings: Settings,
    },
    /// Make the org's members, roles and teams what an IdP export says. Teams it names are created;
    /// members it doesn't list are removed, unless `keep_absent`.
    Sync {
        org: String,
        members: Vec<SyncMember>,
        #[serde(default)]
        keep_absent: bool,
    },
    /// Connect the org's own identity provider (replacing the one it had).
    SetProvider {
        org: String,
        provider: Provider,
    },
    /// Go back to the server's identity provider.
    RemoveProvider {
        org: String,
    },
    /// Bind the org to an Auth0 Organization (`org_…`), or unbind it (null): sign-ins through the
    /// platform's Auth0 tenant for that Organization count in this org. The operator's alone. The field
    /// must be there: a change that leaves it out is not taken for an unbind.
    SetAuth0Org {
        org: String,
        #[serde(deserialize_with = "present")]
        auth0_org: Option<String>,
    },
    /// Verified emails at `domain` join the org (and `team`) without an invite.
    AddDomain {
        org: String,
        domain: String,
        team: Option<String>,
    },
    RemoveDomain {
        org: String,
        domain: String,
    },
    /// People the org's provider puts in `group` are members of `team`, for as long as it does.
    MapGroup {
        org: String,
        group: String,
        team: String,
    },
    UnmapGroup {
        org: String,
        group: String,
        team: String,
    },
    /// A service account's key (`memstore apikey create`). The CLI makes the key and sends it once;
    /// it is hashed where it arrives, and only its sha256 is kept. A client never names a hash: it could
    /// name one it doesn't hold the key for (a keys file's), and so capture that key's requests.
    CreateKey {
        org: String,
        name: String,
        team: Option<String>,
        #[serde(default)]
        role: KeyRole,
        /// Seconds since the epoch.
        expires_at: Option<i64>,
        key: NewKey,
        /// An ingest key's source prefixes: it writes and retracts only sources starting with one of
        /// them (`email:`, `slack:acme:`). None: any source it may write.
        #[serde(default)]
        sources: Vec<String>,
    },
    /// Set an ingest key's source prefixes (an empty list: any source again). The list must be there: a
    /// change that leaves it out (or misspells it) is refused, never taken for "any source".
    SetKeySources {
        org: String,
        name: String,
        sources: Vec<String>,
    },
    RevokeKey {
        org: String,
        name: String,
    },
    /// Move a keys file's rows into the directory as the org's (or `team`'s) keys, each named after its
    /// row's id, with `role`. Only rows that map to no more than they hold are taken; one that doesn't is
    /// refused, and with it the whole import.
    ImportKeys {
        org: String,
        team: Option<String>,
        #[serde(default = "writer")]
        role: KeyRole,
        /// The rows, as the keys file holds them: through a server, the rows of its own file with these ids
        /// (it replaces them with its own); with --direct, the catalog owner's.
        #[serde(default)]
        keys: Vec<FileKey>,
    },
}

/// `, writing only sources under a, b` for a key held to sources, else nothing.
fn sources_note(sources: &[String]) -> String {
    match sources.is_empty() {
        true => String::new(),
        false => format!(", writing only sources under {}", sources.join(", ")),
    }
}

fn writer() -> KeyRole {
    KeyRole::Writer
}

/// A field that must be given, though it may be null.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(d)
}

/// An Auth0 Organization id as Auth0 makes them: `org_` and letters and digits.
pub fn valid_auth0_org(id: &str) -> bool {
    id.strip_prefix("org_").is_some_and(|r| {
        !r.is_empty() && r.len() <= 64 && r.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

impl Change {
    /// Whether an operator (`SignIn::operator`) may make this change in `dir`, as an admin key would:
    /// create an org, bind (or rebind) it to Auth0, set its admins and members, choose its models, and make or
    /// revoke its `ingest` keys (the control plane's feed into a new org; an ingest key only writes). Nothing else an
    /// admin key may do (delete an org, its teams, other keys, providers, domains, profile, imports) is
    /// theirs directly, but an admin they make, themselves included, holds everything an org admin holds
    /// there: an ingest key reaches no further than that.
    pub fn operators(&self, dir: &Directory) -> bool {
        match self {
            Change::CreateOrg { .. }
            | Change::SetAuth0Org { .. }
            | Change::AddMember { .. }
            | Change::RemoveMember { .. }
            | Change::Invite { .. }
            | Change::Decline { .. } => true,
            // An org's models, its training log and which scopes its readers also search, and nothing else of
            // its settings.
            Change::SetSettings { settings, .. } => {
                (settings.sets_models()
                    || settings.train_log.is_some()
                    || settings.advice_traces.is_some()
                    || settings.reads.is_some())
                    && settings.compile.is_none()
                    && settings.advice.is_none()
                    && settings.advice_procedures.is_none()
                    && settings.advice_evidence.is_none()
                    && settings.advice_learn_people.is_none()
                    && settings.advice_personal_runs.is_none()
                    && settings.advice_skills.is_none()
                    && settings.advice_learn_exclude.is_none()
                    && settings.extract.is_none()
                    && settings.classic_ranking.is_none()
                    && settings.freshness.is_none()
                    && settings.conflicts.is_none()
                    && settings.advice_curation.is_none()
                    && settings.advice_automated.is_none()
                    && settings.purge.is_none()
                    && settings.profile.is_none()
                    && settings.transcripts.is_none()
                    && settings.instructions.is_none()
                    && settings.notes_dream.is_none()
                    && settings.notes_decide.is_none()
                    && settings.activity.is_none()
                    && settings.session_branches.is_none()
                    && settings.auto_review.is_none()
                    && settings.pii.is_none()
            }
            Change::CreateKey { role, .. } => *role == KeyRole::Ingest,
            Change::RevokeKey { org, name } | Change::SetKeySources { org, name, .. } => dir
                .orgs
                .get(org)
                .and_then(|o| o.keys.get(name))
                .is_some_and(|k| k.role == KeyRole::Ingest),
            _ => false,
        }
    }

    pub fn org(&self) -> &str {
        match self {
            Change::CreateOrg { org, .. }
            | Change::CreateWorkspace { org, .. }
            | Change::DeleteOrg { org, .. }
            | Change::CreateTeam { org, .. }
            | Change::DeleteTeam { org, .. }
            | Change::AddMember { org, .. }
            | Change::RemoveMember { org, .. }
            | Change::AddToTeam { org, .. }
            | Change::RemoveFromTeam { org, .. }
            | Change::Invite { org, .. }
            | Change::RequestJoin { org }
            | Change::Decline { org, .. }
            | Change::SetProfile { org, .. }
            | Change::SetSettings { org, .. }
            | Change::Sync { org, .. }
            | Change::SetProvider { org, .. }
            | Change::RemoveProvider { org }
            | Change::SetAuth0Org { org, .. }
            | Change::AddDomain { org, .. }
            | Change::RemoveDomain { org, .. }
            | Change::MapGroup { org, .. }
            | Change::UnmapGroup { org, .. }
            | Change::CreateKey { org, .. }
            | Change::RevokeKey { org, .. }
            | Change::SetKeySources { org, .. }
            | Change::ImportKeys { org, .. } => org,
        }
    }
}

/// A new key, as the client made it: hashed where it arrives, never stored, logged or shown.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NewKey(pub String);

impl std::fmt::Debug for NewKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NewKey(..)")
    }
}

impl NewKey {
    /// The sha256 the directory keeps, hex.
    pub fn sha256(&self) -> String {
        crate::auth::hex(&<sha2::Sha256 as sha2::Digest>::digest(self.0.as_bytes()))
    }
}

/// A change refused to its caller: they may not make it, or the directory as it stands won't take it (an org
/// that exists already, a team that doesn't). Its message is shown to them; a server answers it 403.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

macro_rules! refuse {
    ($($t:tt)*) => { return Err(Refused(format!($($t)*)).into()) };
}

/// A request no one could make as it is: a value out of range, a name that can't be one, a field missing or of
/// the wrong type. Its message is shown to the caller, who fixes the request, not their rights; a server answers
/// it 400. Checked after who may ask, so a caller who may not still hears only that ([`Refused`]).
#[derive(Debug)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

macro_rules! invalid {
    ($($t:tt)*) => { return Err(Invalid(format!($($t)*)).into()) };
}

/// A key's source prefixes as given, checked: an ingest key's only (they bound `/ingest` and `/retract`,
/// which no other key reaches), each non-empty and outside the server's own families, sorted, once each.
fn key_sources(role: KeyRole, sources: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = sources.iter().map(|s| s.trim().to_string()).collect();
    if out.is_empty() {
        return Ok(out);
    }
    if role != KeyRole::Ingest {
        invalid!(
            "source prefixes hold an ingest key to what it writes: a {} key writes no sources",
            role.as_str()
        );
    }
    if out.iter().any(|s| s.is_empty()) {
        invalid!("a source prefix is not empty (it would match every source)");
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Ready `change` for the directory, where the server (or the catalog's owner) holds `key`: a provider's
/// literal client secret is sealed with it, a sealed one must open with it; refused without a key.
pub fn seal_secrets(
    change: &mut Change,
    key: Option<&crate::auth::secret::SecretKey>,
) -> Result<()> {
    if let Change::SetProvider { provider, .. } = change
        && let Some(why) = client_type_problem(provider)
    {
        // Before sealing: a secret refused here is neither sealed nor looked up.
        return Err(Refused(why.into()).into());
    }
    if let Change::SetProvider { org, provider } = change
        && let Some(s) = &provider.client_secret
    {
        provider.client_secret = Some(crate::auth::secret::protect(org, s, key).map_err(Refused)?);
    }
    Ok(())
}

/// Whether a directory key `k` (signed in as `me`) may make `change`: in its own org only, and while the
/// directory still holds it, it adds members (never as admins, never demoting one), removes members (never
/// an admin), and creates, deletes and fills teams. Everything else is an admin's.
fn directory_key_may(
    dir: &Directory,
    me: &str,
    k: &crate::auth::OrgKey,
    change: &Change,
) -> Result<()> {
    let org_name = change.org();
    if org_name != k.org {
        refuse!(
            "{me} is a directory key of org {}: it may not change org {org_name}",
            k.org
        );
    }
    let live = dir.access(
        me,
        &SignIn {
            key: Some(k.clone()),
            ..Default::default()
        },
    );
    if k.team.is_some() || !live.contains_key(org_name) {
        refuse!("{me} is no longer a directory key of org {org_name}");
    }
    let org = dir.org(org_name)?;
    let admin = |u: &str| org.members.get(&user_id(u)) == Some(&Role::Admin);
    match change {
        Change::AddMember { user, role, .. } => {
            if *role != Role::Member {
                refuse!(
                    "{me} is a directory key: it adds members, never admins (an org admin makes admins)"
                );
            }
            if admin(user) {
                refuse!(
                    "{} is an admin of {org_name}: a directory key doesn't change an admin's role",
                    user_id(user)
                );
            }
        }
        Change::RemoveMember { user, .. } => {
            if admin(user) {
                refuse!(
                    "{} is an admin of {org_name}: a directory key doesn't remove admins (an org admin does)",
                    user_id(user)
                );
            }
        }
        Change::DeleteTeam { team, .. } => {
            // A team an admin set up (its keys, settings, the domains and IdP groups that fill it) goes with
            // its admin only: deleting it would take that config with it. Members alone don't hold it back.
            let mut held = vec![];
            let keys: Vec<&str> = org
                .keys
                .iter()
                .filter(|(_, k)| k.team.as_deref() == Some(team.as_str()))
                .map(|(n, _)| n.as_str())
                .collect();
            if !keys.is_empty() {
                held.push(format!("service keys ({})", keys.join(", ")));
            }
            if org.team_settings.get(team).is_some_and(|s| !s.is_empty()) {
                held.push("team settings".to_string());
            }
            let groups: Vec<&str> = org
                .groups
                .iter()
                .filter(|(_, t)| t.contains(team))
                .map(|(g, _)| g.as_str())
                .collect();
            if !groups.is_empty() {
                held.push(format!("IdP group mappings ({})", groups.join(", ")));
            }
            let domains: Vec<&str> = org
                .domains
                .iter()
                .filter(|(_, t)| t.as_deref() == Some(team.as_str()))
                .map(|(d, _)| d.as_str())
                .collect();
            if !domains.is_empty() {
                held.push(format!(
                    "domains that join people to it ({})",
                    domains.join(", ")
                ));
            }
            if !held.is_empty() {
                refuse!(
                    "team {org_name}/{team} holds {}, which an org admin set up: a directory key doesn't delete it (an org admin must)",
                    held.join(", ")
                );
            }
        }
        Change::AddToTeam { .. } | Change::RemoveFromTeam { .. } | Change::CreateTeam { .. } => {}
        _ => refuse!(
            "{me} is a directory key: it adds and removes members and manages teams only (add_member, remove_member, add_to_team, remove_from_team, create_team, delete_team); this change needs an org admin"
        ),
    }
    Ok(())
}

/// The events that make `change`, checked against `dir` and what `actor` may do. Nothing is written.
pub fn plan(dir: &Directory, actor: &Actor, change: &Change) -> Result<Vec<Event>> {
    let mut out = plan_change(dir, actor, change)?;
    out.extend(import_revokes(dir, &actor.user, &out));
    Ok(out)
}

/// The `key.revoke`s that go with `events`: a file key imported in someone's name reads no more than they do,
/// so taking them out of an org (`member.remove`) revokes every key imported in their name there, and out of a
/// team (`team.remove`), those of that team. In the same commit as the removal, so no server ever sees them
/// gone and the key still there.
pub fn import_revokes(dir: &Directory, me: &str, events: &[Event]) -> Vec<Event> {
    let mut gone: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut out = vec![];
    for e in events {
        let Some(org) = dir.orgs.get(&e.org) else {
            continue;
        };
        let Some(user) = e.user.as_deref().map(user_id) else {
            continue;
        };
        let team = match e.op.as_str() {
            "member.remove" => None,
            "team.remove" => match e.team.as_deref() {
                Some(t) => Some(t),
                None => continue,
            },
            _ => continue,
        };
        for (name, k) in &org.keys {
            let theirs = k.imported_user.as_deref() == Some(user.as_str())
                && team.is_none_or(|t| k.team.as_deref() == Some(t));
            let revoked = events.iter().any(|r| {
                r.op == "key.revoke" && r.org == e.org && r.detail.as_deref() == Some(name)
            });
            if theirs && !revoked && gone.insert((e.org.as_str(), name.as_str())) {
                out.push(Event::new(me, "key.revoke", &e.org).detail(name));
            }
        }
    }
    out
}

fn plan_change(dir: &Directory, actor: &Actor, change: &Change) -> Result<Vec<Event>> {
    let org_name = change.org();
    if check_name("org", org_name).is_err() {
        invalid!(
            "invalid org name {org_name:?}: use lower-case letters, digits and single underscores"
        );
    }
    let me = actor.user.as_str();
    // An operator makes the changes that are theirs as an admin key would, and no other.
    let superuser = actor.superuser || (actor.via.operator && change.operators(dir));
    // Creating and deleting orgs is the catalog owner's; the rest is the org's admins'.
    // A file key is the operator's to import, never an org admin's: the rows are the keys file's own, as the
    // server holding it reads them (`directory_change` takes them from its file by id), or the catalog owner's.
    if matches!(change, Change::ImportKeys { .. }) && !actor.superuser {
        refuse!(
            "{me} may not import file keys: only the operator (an admin key, or the catalog's owner with --direct) may"
        );
    }
    // Binding an org to Auth0 decides who signs in to it: never its own admins' to choose.
    let owner_only = matches!(
        change,
        Change::CreateOrg { .. }
            | Change::CreateWorkspace { .. }
            | Change::DeleteOrg { .. }
            | Change::SetAuth0Org { .. }
    );
    let imports = matches!(change, Change::ImportKeys { .. });
    // Asking to join is the asker's own: checked below, where the org is known.
    let asks = matches!(change, Change::RequestJoin { .. });
    if let Some(k) = actor.via.directory_key() {
        // A directory key (a SCIM bridge's) manages its own org's people, never an admin, and nothing else.
        directory_key_may(dir, me, k, change)?;
    } else if !superuser
        && !imports
        && !asks
        && (owner_only || !dir.is_admin(me, &actor.via, org_name))
    {
        refuse!(
            "{me} may not change org {org_name}{}",
            if owner_only {
                " (creating and deleting orgs, and binding them to Auth0, needs the catalog owner or an admin key)"
            } else {
                " (only its admins may)"
            }
        );
    }
    let ev = |op: &str| Event::new(me, op, org_name);
    if let Change::CreateWorkspace { spec, .. } = change {
        if dir.orgs.contains_key(org_name) {
            refuse!("org {org_name} already exists");
        }
        if dir.purging.contains(org_name) {
            refuse!(
                "{org_name} was deleted and its memory isn't purged yet: finish that first (ampup-memory workspace delete {org_name} --confirm {org_name})"
            );
        }
        return Ok(vec![
            ev("org.create"),
            ev("workspace.set").detail(&serde_json::to_string(spec)?),
        ]);
    }
    // One Auth0 Organization is one org's: its sign-ins must name one org.
    let auth0_free = |id: &str| -> Result<()> {
        if !valid_auth0_org(id) {
            invalid!("{id:?} is not an Auth0 organization id (org_ and letters and digits)");
        }
        if let Some((other, _)) = dir
            .orgs
            .iter()
            .find(|(n, o)| *n != org_name && o.auth0_org.as_deref() == Some(id))
        {
            refuse!("Auth0 organization {id} is already org {other}'s");
        }
        Ok(())
    };
    if let Change::CreateOrg {
        admin, auth0_org, ..
    } = change
    {
        if dir.orgs.contains_key(org_name) {
            refuse!("org {org_name} already exists");
        }
        // Its old memory may still be in the catalog: a new org of that name must never read it.
        if dir.purging.contains(org_name) {
            refuse!(
                "org {org_name} was deleted and its memory isn't purged yet: finish that first (ampup-memory org delete {org_name} --confirm {org_name})"
            );
        }
        let mut out = vec![ev("org.create")];
        if let Some(id) = auth0_org {
            auth0_free(id)?;
            out.push(ev("org.auth0").detail(id));
        }
        if let Some(a) = admin {
            out.push(ev("member.add").user(&user_id(a)).role(Role::Admin));
        }
        return Ok(out);
    }
    if let Change::DeleteOrg { confirm, .. } = change {
        if org_name == DEFAULT_ORG {
            refuse!("the default org can't be deleted");
        }
        if confirm.as_deref() != Some(org_name) {
            invalid!(
                "deleting org {org_name} deletes its memory for good: repeat its name to confirm (--confirm {org_name})"
            );
        }
        // Deleted already, its memory not yet purged (a delete that stopped halfway): nothing more to
        // write here; `apply` finishes the purge.
        if !dir.orgs.contains_key(org_name) && dir.purging.contains(org_name) {
            return Ok(vec![]);
        }
    }
    // Anyone signed in may ask to join. An org that isn't there and one nobody joins (a workspace) get one
    // answer: that a name is taken is told (as `/.well-known/oauth-protected-resource?org=` tells it), what
    // kind of org holds it is not.
    if asks && !dir.org(org_name).is_ok_and(|o| o.workspace.is_none()) {
        refuse!("can't ask to join {org_name}: no such org, or one nobody joins");
    }
    let Ok(org) = dir.org(org_name) else {
        refuse!("no org {org_name}");
    };
    let team_ok = |t: &str| -> Result<()> {
        if check_name("team", t).is_err() {
            invalid!(
                "invalid team name {t:?}: use lower-case letters, digits and single underscores"
            );
        }
        Ok(())
    };
    let has_team = |t: &str| -> Result<()> {
        if !org.teams.contains_key(t) {
            refuse!("no team {org_name}/{t}");
        }
        Ok(())
    };
    // An org admin can't take away their own admin role: someone must be left to manage the org.
    let keeps_me = |gone: bool| -> Result<()> {
        if gone && !superuser {
            refuse!("{me} can't remove their own admin role from {org_name}");
        }
        Ok(())
    };
    Ok(match change {
        Change::CreateOrg { .. } | Change::CreateWorkspace { .. } => unreachable!(),
        Change::DeleteOrg { .. } => {
            let mut e = ev("org.delete");
            e.version = PURGE_VERSION;
            vec![e]
        }
        Change::CreateTeam { team, .. } => {
            team_ok(team)?;
            if org.teams.contains_key(team) {
                refuse!("team {org_name}/{team} already exists");
            }
            vec![ev("team.create").team(team)]
        }
        Change::DeleteTeam { team, .. } => {
            has_team(team)?;
            vec![ev("team.delete").team(team)]
        }
        Change::AddMember { user, role, .. } => {
            let u = user_id(user);
            keeps_me(u == user_id(me) && *role != Role::Admin)?;
            vec![ev("member.add").user(&u).role(*role)]
        }
        Change::RemoveMember { user, .. } => {
            let u = user_id(user);
            // Someone a domain or a group brings in is removed too: neither brings them back.
            let joins = domain_of(&u).is_some_and(|d| org.domains.contains_key(d))
                || !org.groups.is_empty();
            if !org.members.contains_key(&u) && !joins {
                refuse!("{u} is not a member of {org_name}");
            }
            keeps_me(u == user_id(me))?;
            vec![ev("member.remove").user(&u)]
        }
        Change::AddToTeam { team, users, .. } => {
            has_team(team)?;
            let mut out = vec![];
            for u in users.iter().map(|u| user_id(u)) {
                if !org.members.contains_key(&u) {
                    out.push(ev("member.add").user(&u).role(Role::Member));
                }
                out.push(ev("team.add").team(team).user(&u));
            }
            out
        }
        Change::RemoveFromTeam { team, users, .. } => {
            has_team(team)?;
            users
                .iter()
                .map(|u| ev("team.remove").team(team).user(&user_id(u)))
                .collect()
        }
        Change::RequestJoin { .. } => {
            // A person an identity provider knows, by a verified email: what an invite then names. A key or
            // an app's token asks for nobody, and the operator joins as an admin would add them.
            if !actor.via.person || actor.via.key.is_some() {
                refuse!(
                    "{me} is not a person signed in through an identity provider: only a person asks to join an org"
                );
            }
            if !actor.via.verified_email {
                refuse!(
                    "{me}'s email isn't verified by their identity provider: verify it, then ask again"
                );
            }
            // Through the server's own provider or the platform's Auth0 tenant only: an org's own provider is
            // its admins', and asserts whatever email it likes (as `AddDomain` and `scope=mine` hold too).
            if !(actor.via.orgs.is_none() || actor.via.auth0) {
                refuse!(
                    "{me} signed in through an org's own identity provider, which vouches for them in that org only: ask to join through the server's sign-in"
                );
            }
            let u = user_id(me);
            // A plain address: it is quoted to admins as a command to run, and shown on the approval page.
            if !plain_email(&u) {
                invalid!("{u:?} is not a plain email address (letters, digits, . _ + - @)");
            }
            if org.members.contains_key(&u) {
                refuse!("{u} is a member of {org_name} already");
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            if org
                .declined
                .get(&u)
                .is_some_and(|at| now - at < ASK_AGAIN_AFTER)
            {
                refuse!(
                    "an admin of {org_name} turned {u}'s request down today: ask again tomorrow"
                );
            }
            if !org.pending.contains_key(&u) && org.pending.len() >= MAX_PENDING {
                refuse!(
                    "{MAX_PENDING} people are waiting to join {org_name} already: ask again once its admins have answered them"
                );
            }
            // Asked already: nothing more to write (the admins were told).
            if org.pending.contains_key(&u) {
                vec![]
            } else {
                vec![ev("member.request").user(&u)]
            }
        }
        Change::Decline { user, .. } => {
            let u = user_id(user);
            if !org.pending.contains_key(&u) {
                refuse!("{u} hasn't asked to join {org_name}");
            }
            vec![ev("member.decline").user(&u)]
        }
        Change::Invite { members, .. } => {
            let mut out = vec![];
            for m in members {
                let u = user_id(&m.user);
                if !u.contains('@') || u.starts_with('@') || u.ends_with('@') {
                    invalid!("{u:?} is not an email address");
                }
                for t in &m.teams {
                    has_team(t)?;
                }
                // An invite never demotes: an admin invited again as a member stays an admin.
                match org.members.get(&u) {
                    None => out.push(ev("member.add").user(&u).role(m.role)),
                    Some(Role::Member) if m.role == Role::Admin => {
                        out.push(ev("member.add").user(&u).role(Role::Admin))
                    }
                    _ => {}
                }
                for t in &m.teams {
                    if !org.teams.get(t).is_some_and(|h| h.contains(&u)) {
                        out.push(ev("team.add").team(t).user(&u));
                    }
                }
            }
            out
        }
        Change::SetProfile { profile, .. } => {
            let mut e = ev("org.profile");
            e.detail = Some(serde_json::to_string(profile)?);
            vec![e]
        }
        Change::SetSettings { team, settings, .. } => {
            if team.is_some() && settings.activity.is_some() {
                invalid!("activity is the org's setting, not a team's: set it without a team");
            }
            if team.is_some() && settings.session_branches.is_some() {
                invalid!(
                    "session_branches is the org's setting, not a team's: set it without a team"
                );
            }
            if team.is_some() && settings.auto_review.is_some() {
                invalid!("auto_review is the org's setting, not a team's: set it without a team");
            }
            if settings.is_empty() {
                invalid!(
                    "settings names nothing to set (settings: {{\"compile\": true|false, \"compile_sources\": [\"<source prefix>\" | \"*\"], \"compile_batch\": <sources>, \"compile_kind\": \"errors\", \"advice\": true|false, \"advice_procedures\": true|false, \"advice_evidence\": <people>, \"advice_learn_people\": <people>, \"advice_personal_runs\": <runs>|\"off\", \"advice_learn_exclude\": [\"<user id>\"], \"advice_skills\": \"fallback\"|\"together\"|\"off\", \"error_leads\": true|false, \"extract\": true|false, \"train_log\": true|false, \"advice_traces\": true|false, \"classic_ranking\": true|false, \"purge\": true|false, \"activity\": true|false, \"session_branches\": true|false, \"auto_review\": true|false, \"pii\": true|false, \"freshness\": true|false, \"conflicts\": true|false, \"advice_curation\": true|false, \"advice_automated\": true|false, \"profile\": true|false, \"instructions\": \"default\"|\"purpose\", \"transcripts\": true|false, \"notes_dream\": true|false, \"notes_decide\": true|false, \"reads\": [\"team:<org>/<team>\"], \"extract_model\" |\"compile_model\" | \"aip_model\": \"<model>\"}})"
                );
            }
            if let Some(sources) = &settings.compile_sources {
                if sources.len() > 50 {
                    invalid!("compile_sources names at most 50 source prefixes");
                }
                for p in sources {
                    if p.trim().is_empty() || p.trim() != p || p.len() > 200 {
                        invalid!(
                            "invalid compile source {p:?}: a source prefix (kb:, github:acme/) or * for every source"
                        );
                    }
                }
            }
            if let Some(users) = &settings.advice_learn_exclude {
                if users.len() > 50 {
                    invalid!("advice_learn_exclude names at most 50 accounts");
                }
                for u in users {
                    if u.trim().is_empty() || u.trim() != u || u.len() > 254 {
                        invalid!(
                            "invalid account {u:?} in advice_learn_exclude: a user id as its sign-in gives it (dev@example.com)"
                        );
                    }
                }
            }
            if settings
                .compile_batch
                .is_some_and(|n| n > MAX_COMPILE_BATCH)
            {
                invalid!(
                    "compile_batch is at most {MAX_COMPILE_BATCH} sources a round (0 unsets it)"
                );
            }
            if let Some(PersonalRuns::Runs(n)) = settings.advice_personal_runs
                && n > MAX_PERSONAL_RUNS
            {
                invalid!(
                    "advice_personal_runs is at most {MAX_PERSONAL_RUNS} rewarded runs (0 unsets it, \"off\" turns it off): it only lowers the bars a person's own advice waits for"
                );
            }
            if let Some(k) = &settings.advice_skills
                && !ADVICE_SKILLS.contains(&k.as_str())
            {
                invalid!(
                    "invalid advice_skills {k:?}: \"fallback\" (compiled skills only where no kind of task learned from runs fits), \"together\" (both on one list), \"off\" (never a compiled skill), or \"\" to unset it"
                );
            }
            if let Some(k) = &settings.instructions
                && !INSTRUCTIONS.contains(&k.as_str())
            {
                invalid!(
                    "invalid instructions {k:?}: \"default\" (the session's call order), \"purpose\" (what memory is for, no fixed procedure), or \"\" to unset it"
                );
            }
            if let Some(k) = &settings.compile_kind
                && !COMPILE_KINDS.contains(&k.as_str())
            {
                invalid!(
                    "invalid compile_kind {k:?}: \"errors\" (the sources are error reports), or \"\" to unset it"
                );
            }
            // A model spends the platform's model budget: the operator's to choose, never an org admin's.
            if settings.sets_models() && !superuser {
                refuse!(
                    "{me} may not set org {org_name}'s models: they spend the platform's model budget, so only the operator (an admin key or an operator sign-in) may"
                );
            }
            // The log keeps the text of the org's memory whole, to train on: the operator's to turn on.
            if settings.train_log.is_some() && !superuser {
                refuse!(
                    "{me} may not set org {org_name}'s training log: it keeps the org's model calls whole, so only the operator (an admin key or an operator sign-in) may"
                );
            }
            // The uploads keep sessions' tool calls and results, more than memstore keeps otherwise: the operator's.
            if settings.advice_traces.is_some() && !superuser {
                refuse!(
                    "{me} may not set org {org_name}'s advice traces: they keep sessions' tool calls and results, so only the operator (an admin key or an operator sign-in) may"
                );
            }
            // Which other scopes a scope's readers search decides who reads what: the operator's to set, and only
            // within the org (one org, so one cell: `cell::of_org`).
            if let Some(reads) = &settings.reads {
                if !superuser {
                    refuse!(
                        "{me} may not set which scopes org {org_name}'s readers also search: it decides who reads what, so only the operator (an admin key or an operator sign-in) may"
                    );
                }
                if reads.len() > MAX_READS {
                    invalid!("reads names at most {MAX_READS} scopes");
                }
                let own = match team {
                    Some(t) => crate::Scope::team(org_name, t),
                    None => crate::Scope::org(org_name),
                }?;
                for r in reads {
                    let scope = match crate::Scope::parse(r.trim()) {
                        Ok(s @ (crate::Scope::Team(_) | crate::Scope::NamedOrg(_))) => s,
                        _ => invalid!(
                            "reads names scopes of org {org_name}: team:{org_name}/<team> (or org:{org_name} for a team), not {r:?}"
                        ),
                    };
                    if scope.org_name() != Some(org_name) {
                        invalid!(
                            "reads names scopes of org {org_name} only, not {r:?}: another org's memory is never read here"
                        );
                    }
                    if scope == own {
                        invalid!("{r} is the scope itself: reads names other scopes");
                    }
                    if let crate::Scope::Team(id) = &scope {
                        has_team(id.split_once('/').map_or(id.as_str(), |(_, t)| t))?;
                    }
                }
            }
            for m in [
                &settings.extract_model,
                &settings.compile_model,
                &settings.aip_model,
            ]
            .into_iter()
            .flatten()
            {
                if !valid_model(m) {
                    invalid!(
                        "invalid model {m:?}: a backend's model id, such as anthropic/claude-haiku-5.5"
                    );
                }
            }
            let mut e = ev("org.settings").detail(&serde_json::to_string(settings)?);
            if let Some(t) = team {
                has_team(t)?;
                e = e.team(t);
            }
            vec![e]
        }
        Change::SetProvider { provider: p, .. } => {
            if !PROVIDER_KINDS.contains(&p.kind.as_str()) {
                invalid!(
                    "provider is one of {}, got {:?}",
                    PROVIDER_KINDS.join(", "),
                    p.kind
                );
            }
            let issuer = p.issuer.trim_end_matches('/');
            if !secure_issuer(issuer) {
                invalid!("the issuer must be an https:// URL, got {issuer:?}");
            }
            if p.client_ids.is_empty() || p.client_ids.iter().any(|c| c.trim().is_empty()) {
                invalid!("the provider needs its client id (--client-id)");
            }
            if p.groups_claim.trim().is_empty() {
                invalid!("the groups claim can't be empty");
            }
            if let Some(why) = client_type_problem(p) {
                invalid!("{why}");
            }
            // Anyone who can read the bucket reads the directory: a secret goes in sealed with the
            // server's key, or as the name of the server's environment variable that holds it.
            if let Some(s) = &p.client_secret
                && crate::auth::secret::stored(s) == crate::auth::secret::Stored::Plain
            {
                invalid!(
                    "the client secret goes into the directory sealed or as env:AMPUP_MEMORY_IDP_SECRET_<NAME>, never in plain text"
                );
            }
            // A client is one org's: sharing it would let one org's sign-ins count in another.
            for (other, o) in &dir.orgs {
                if let Some(q) = o.provider.as_ref().filter(|_| other != org_name)
                    && q.issuer.trim_end_matches('/') == issuer
                    && let Some(c) = p.client_ids.iter().find(|c| q.client_ids.contains(c))
                {
                    refuse!("client {c} of {issuer} already signs people in to org {other}");
                }
            }
            let mut p = p.clone();
            p.issuer = issuer.to_string();
            vec![ev("auth.provider").detail(&serde_json::to_string(&p)?)]
        }
        Change::SetAuth0Org { auth0_org, .. } => match auth0_org {
            Some(id) if org.auth0_org.as_deref() == Some(id.as_str()) => vec![],
            Some(id) => {
                auth0_free(id)?;
                vec![ev("org.auth0").detail(id)]
            }
            None if org.auth0_org.is_none() => vec![],
            None => vec![ev("org.auth0")],
        },
        Change::RemoveProvider { .. } => {
            if org.provider.is_none() {
                refuse!("org {org_name} has no identity provider of its own");
            }
            vec![ev("auth.provider")]
        }
        Change::AddDomain { domain, team, .. } => {
            let d = domain.trim().trim_start_matches('@').to_ascii_lowercase();
            let valid = d.contains('.')
                && !d.starts_with('.')
                && !d.ends_with('.')
                && d.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
            if !valid {
                invalid!("{domain:?} is not a domain name");
            }
            if PUBLIC_MAIL.contains(&d.as_str()) {
                invalid!("{d} is a public mail service: anyone can have an address there");
            }
            // An admin can claim their own domain only, proven by the server's provider (an org's own
            // provider is run by its admins, so it proves nothing here); the catalog owner or an admin key
            // can claim any.
            let proven = actor.via.person && actor.via.verified_email && actor.via.orgs.is_none();
            if !superuser && (!proven || domain_of(&user_id(me)) != Some(d.as_str())) {
                refuse!(
                    "{me} can only add their own email's domain, signed in through the server's identity provider (an admin key can add any)"
                );
            }
            if let Some((other, _)) = dir
                .orgs
                .iter()
                .find(|(n, o)| *n != org_name && o.domains.contains_key(&d))
            {
                refuse!("{d} already joins people to org {other}");
            }
            if let Some(t) = team {
                has_team(t)?;
            }
            let mut e = ev("domain.add").detail(&d);
            e.team = team.clone();
            vec![e]
        }
        Change::RemoveDomain { domain, .. } => {
            let d = domain.trim().trim_start_matches('@').to_ascii_lowercase();
            if !org.domains.contains_key(&d) {
                refuse!("{d} doesn't join people to {org_name}");
            }
            vec![ev("domain.remove").detail(&d)]
        }
        Change::MapGroup { group, team, .. } => {
            let Some(p) = &org.provider else {
                refuse!(
                    "groups come from the org's own identity provider: connect one first (ampup-memory auth setup)"
                );
            };
            if p.kind == "google" {
                refuse!(
                    "Google sign-in doesn't say which groups people are in; use teams or domains"
                );
            }
            if group.trim().is_empty() {
                invalid!("which group?");
            }
            has_team(team)?;
            vec![ev("group.map").team(team).detail(group.trim())]
        }
        Change::UnmapGroup { group, team, .. } => {
            if !org
                .groups
                .get(group.trim())
                .is_some_and(|t| t.contains(team))
            {
                refuse!("group {group} isn't mapped to {org_name}/{team}");
            }
            vec![ev("group.unmap").team(team).detail(group.trim())]
        }
        Change::CreateKey {
            name,
            team,
            role,
            expires_at,
            key,
            sources,
            ..
        } => {
            let sources = key_sources(*role, sources)?;
            if check_name("key", name).is_err() {
                invalid!(
                    "invalid key name {name:?}: use lower-case letters, digits and single underscores"
                );
            }
            if org.keys.contains_key(name) {
                refuse!("org {org_name} already has a key {name} (revoke it first)");
            }
            if *role == KeyRole::Directory && team.is_some() {
                invalid!(
                    "a directory key manages the whole org's members and teams: leave out the team"
                );
            }
            if let Some(t) = team {
                has_team(t)?;
            }
            let prefix = crate::auth::KEY_PREFIX;
            if !key.0.starts_with(prefix) || key.0.len() < prefix.len() + 32 {
                invalid!(
                    "a key is {prefix} and 32 or more characters after it, as `ampup-memory apikey create` makes it"
                );
            }
            let sha256 = key.sha256();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            if expires_at.is_some_and(|t| t <= now) {
                invalid!("the key would already have expired");
            }
            // One key, one service account: a bearer must name exactly one.
            if dir
                .orgs
                .values()
                .flat_map(|o| o.keys.values())
                .any(|k| k.sha256 == sha256)
            {
                refuse!("that key is already in use");
            }
            // A file key (an admin key above all) is the keys file's: as a directory key too, one bearer
            // would be two callers. Every server refuses a bearer that is both (it names neither), and
            // this refuses making one wherever the keys file is known.
            if actor.file_keys.contains(&sha256) {
                refuse!("that key is already in use");
            }
            let detail = KeyDetail {
                name: name.clone(),
                sha256,
                team: team.clone(),
                role: *role,
                expires_at: *expires_at,
                sources,
                created_by: user_id(me),
                imported_from: None,
                imported_user: None,
            };
            let mut e = ev("key.create").detail(&serde_json::to_string(&detail)?);
            e.team = team.clone();
            e.role = Some(role.as_str().into());
            if !detail.sources.is_empty() {
                e.version = SOURCES_VERSION;
            }
            vec![e]
        }
        Change::RevokeKey { name, .. } => {
            if !org.keys.contains_key(name) {
                refuse!("org {org_name} has no key {name}");
            }
            vec![ev("key.revoke").detail(name)]
        }
        Change::SetKeySources { name, sources, .. } => {
            let Some(k) = org.keys.get(name) else {
                refuse!("org {org_name} has no key {name}");
            };
            let sources = key_sources(k.role, sources)?;
            if sources == k.sources {
                vec![]
            } else {
                let detail = KeySources {
                    name: name.clone(),
                    sources,
                };
                let mut e = ev("key.sources").detail(&serde_json::to_string(&detail)?);
                e.version = SOURCES_VERSION;
                vec![e]
            }
        }
        Change::ImportKeys {
            team, role, keys, ..
        } => {
            if matches!(role, KeyRole::Directory | KeyRole::Recorder) {
                invalid!(
                    "a file key is imported as a reader, writer or ingest key, never a {} key (make one with `ampup-memory apikey create --role {}`)",
                    role.as_str(),
                    role.as_str()
                );
            }
            if let Some(t) = team {
                has_team(t)?;
            }
            if keys.is_empty() {
                invalid!("no keys to import");
            }
            let scope = match team {
                Some(t) => format!("{org_name}/{t}"),
                None => org_name.to_string(),
            };
            let mut out = vec![];
            let mut names = BTreeSet::new();
            for k in keys {
                let id = &k.id;
                let sha256 = k.sha256.to_ascii_lowercase();
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    invalid!("file key {id}: its sha256 is not 64 hex characters");
                }
                // A directory key can't administer anything: an admin key would lose what it holds.
                if k.admin {
                    refuse!(
                        "file key {id} is an admin key: directory keys can't administer, so it stays in the keys file (leave it out with --only)"
                    );
                }
                // A directory key holds no groups: what it read through them it would read no more.
                if !k.groups.is_empty() {
                    refuse!(
                        "file key {id} holds groups ({}): directory keys hold none, so it would lose what they let it read; give it a team instead, or leave it out with --only",
                        k.groups.join(", ")
                    );
                }
                // It may read only what its user could select through the server's sign-in: no more.
                let user = user_id(&k.user);
                let held = dir.access(&user, &SignIn::default());
                let reaches = held
                    .get(org_name)
                    .is_some_and(|a| team.as_ref().is_none_or(|t| a.teams.contains(t)));
                if !reaches {
                    if org.provider.as_ref().is_some_and(|p| p.required) {
                        refuse!(
                            "file key {id}: org {org_name} takes sign-ins through its own identity provider only, so its user doesn't reach it this way, and an import would read more than they do; an admin of {org_name} makes the service a key instead (ampup-memory apikey create)"
                        );
                    }
                    refuse!(
                        "file key {id} acts as {user}, who is not a member of {scope}: as a key of {scope} it would read more than they do"
                    );
                }
                let Some(name) = key_name_of(id) else {
                    invalid!(
                        "file key {id}: its id makes no key name (lower-case letters, digits, single underscores)"
                    );
                };
                if !names.insert(name.clone()) {
                    invalid!("two file keys would both be named {name}");
                }
                // Imported already (the same key under the same name): nothing to do.
                if org.keys.get(&name).is_some_and(|s| s.sha256 == sha256) {
                    continue;
                }
                if org.keys.contains_key(&name) {
                    refuse!("org {org_name} already has a key {name} (revoke it first)");
                }
                // One key, one service account: a bearer names one key.
                if dir
                    .orgs
                    .values()
                    .flat_map(|o| o.keys.values())
                    .any(|s| s.sha256 == sha256)
                {
                    refuse!("file key {id} is already in the directory under another name");
                }
                let detail = KeyDetail {
                    name,
                    sha256,
                    team: team.clone(),
                    role: *role,
                    expires_at: None,
                    sources: vec![],
                    created_by: user_id(me),
                    imported_from: None,
                    imported_user: Some(user.clone()),
                };
                let mut e = ev("key.create").detail(&serde_json::to_string(&detail)?);
                e.team = team.clone();
                e.role = Some(role.as_str().into());
                out.push(e);
            }
            out
        }
        Change::Sync {
            members,
            keep_absent,
            ..
        } => {
            let mut out = vec![];
            let mut listed: BTreeMap<String, &SyncMember> = BTreeMap::new();
            for m in members {
                let u = user_id(&m.user);
                if u.is_empty() {
                    invalid!("a member without a user");
                }
                if listed.insert(u.clone(), m).is_some() {
                    invalid!("{u} is listed twice");
                }
            }
            let wanted_teams: BTreeSet<&str> = members
                .iter()
                .flat_map(|m| m.teams.iter().map(String::as_str))
                .collect();
            for t in &wanted_teams {
                team_ok(t)?;
                if !org.teams.contains_key(*t) {
                    out.push(ev("team.create").team(t));
                }
            }
            let me_id = user_id(me);
            for (u, m) in &listed {
                if org.members.get(u) != Some(&m.role) {
                    keeps_me(*u == me_id && m.role != Role::Admin && org.members.contains_key(u))?;
                    out.push(ev("member.add").user(u).role(m.role));
                }
                let want: BTreeSet<&str> = m.teams.iter().map(String::as_str).collect();
                for (t, held) in &org.teams {
                    if held.contains(u) && !want.contains(t.as_str()) {
                        out.push(ev("team.remove").team(t).user(u));
                    }
                }
                for t in &want {
                    if !org.teams.get(*t).is_some_and(|h| h.contains(u)) {
                        out.push(ev("team.add").team(t).user(u));
                    }
                }
            }
            if !keep_absent {
                for u in org.members.keys().filter(|u| !listed.contains_key(*u)) {
                    keeps_me(*u == me_id)?;
                    out.push(ev("member.remove").user(u));
                }
            }
            out
        }
    })
}

/// An issuer people may be sent to and keys may be read from: https, or http on this machine (a provider
/// running locally).
pub fn secure_issuer(issuer: &str) -> bool {
    match reqwest::Url::parse(issuer) {
        Ok(u) => {
            u.scheme() == "https"
                || (u.scheme() == "http"
                    && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        }
        Err(_) => false,
    }
}

/// Make `change` as `actor`: check it, write its events, and create the memory of any org or team it
/// creates. Returns what changed (empty: nothing to do).
pub async fn apply(store: &Store, actor: &Actor, change: &Change) -> Result<Vec<Event>> {
    // A change that makes, purges or deletes an org's memory is that org's cell's: refused elsewhere before
    // anything is written, never left halfway (the directory changed, the memory not).
    if let Change::CreateOrg { org, .. }
    | Change::CreateWorkspace { org, .. }
    | Change::DeleteOrg { org, .. } = change
    {
        crate::cell::check_org(org)?;
    }
    // A purged org's name taken again: whatever a lagging server wrote to the old org after its purge
    // goes first, so the new org starts with nothing of the old one's. Only the operator creates orgs.
    if let Change::CreateOrg { org, .. } | Change::CreateWorkspace { org, .. } = change
        && (actor.superuser || (actor.via.operator && matches!(change, Change::CreateOrg { .. })))
        && Directory::load(store).await?.purged.contains(org)
    {
        crate::offboard::purge_memory(store, org).await?;
    }
    // Deleting an org rewrites the audit and key usage tables after the directory change: a catalog that
    // can't is refused before anything changes (once the change is known to be allowed), not halfway.
    if let Change::DeleteOrg { .. } = change {
        plan(&Directory::load(store).await?, actor, change)?;
        store.check_rewrites().await?;
    }
    // Planned against the directory as it is when the change commits: if another writer commits first,
    // the change is re-read and re-planned (and refused, if it no longer holds), never committed stale.
    let mut events = store
        .append_directory_with(|dir| plan(dir, actor, change))
        .await?;
    // Deleting an org deletes its memory: the directory change above has already cut off every key,
    // provider and member, so what follows only removes what nobody can read any more. Run again, it
    // picks up where a delete that stopped halfway left off.
    if let Change::DeleteOrg { org, .. } = change {
        crate::offboard::purge_org(store, &actor.user, org).await?;
        let me = actor.user.clone();
        events.extend(
            store
                .append_directory_with(|dir| {
                    let pending = dir.purging.contains(org) && !dir.orgs.contains_key(org);
                    Ok(match pending {
                        true => vec![Event::new(&me, "org.purged", org)],
                        false => vec![],
                    })
                })
                .await?,
        );
    }
    for e in &events {
        match (e.op.as_str(), &e.team) {
            ("org.create", _) => store.ensure_scope(&Scope::org(&e.org)?).await?,
            ("team.create", Some(t)) => store.ensure_scope(&Scope::team(&e.org, t)?).await?,
            _ => {}
        }
    }
    Ok(events)
}

/// An IdP export: JSON (`{"members": [...]}` or a list of `{"user", "role", "teams"}`), or CSV lines
/// `user,role,team;team` (a header line starting with `user` is skipped).
pub fn parse_members(text: &str) -> Result<Vec<SyncMember>> {
    let t = text.trim_start();
    if t.starts_with('{') {
        #[derive(Deserialize)]
        struct File {
            members: Vec<SyncMember>,
        }
        return Ok(serde_json::from_str::<File>(t)?.members);
    }
    if t.starts_with('[') {
        return Ok(serde_json::from_str(t)?);
    }
    let mut out = vec![];
    for (i, line) in t.lines().enumerate() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || (i == 0 && (line.starts_with("user") || line.starts_with("email")))
        {
            continue;
        }
        let mut cols = line.split(',').map(str::trim);
        let user = cols.next().unwrap_or_default().to_string();
        let role = match cols.next().filter(|r| !r.is_empty()) {
            Some(r) => Role::parse(r)?,
            None => Role::Member,
        };
        let teams = cols
            .next()
            .unwrap_or_default()
            .split([';', ' '])
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        out.push(SyncMember { user, role, teams });
    }
    Ok(out)
}

/// Whether the server's `AMPUP_MEMORY_PROFILE` (`1`, `true`, `on` or `yes`) turns profiles on where no team or org
/// setting says (`Settings::profile`).
pub fn profiles_from_env() -> bool {
    std::env::var("AMPUP_MEMORY_PROFILE").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// Whether the server's `AMPUP_MEMORY_INSTRUCTIONS=purpose` tells agents what memory is for rather than when to call
/// each tool, where no team or org setting says (`Settings::instructions`).
pub fn purpose_from_env() -> bool {
    std::env::var("AMPUP_MEMORY_INSTRUCTIONS")
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("purpose"))
}
