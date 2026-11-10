//! Adaptive memory store engine: facts live in Iceberg tables in the customer's catalog,
//! and one ledger table is the commit point (spec v1.1, "Storage and commit model").
//! A row is visible only once the ledger row for its commit has landed, and a fact is
//! visible to a principal only if they can see every episode it was derived from.

/// A log line, naming the org it is about when one is known (`log_org`): `org=<org> ` first. Defined before
/// the modules, so it is every module's `eprintln!`.
macro_rules! eprintln {
    () => { ::std::eprintln!() };
    ($($t:tt)+) => {
        match $crate::log_org::current() {
            Some(org) => ::std::eprintln!("org={org} {}", ::std::format_args!($($t)+)),
            None => ::std::eprintln!($($t)+),
        }
    };
}

pub mod audit;
pub mod auth;
pub mod cell;
pub mod conflicts;
pub mod episode_types;
pub mod freshness;
pub mod governance;
pub mod log_org;
pub mod metrics;
pub mod offboard;
pub mod people;
pub mod pins;
pub mod platform;
pub mod query;
pub mod retention;
pub mod search;
pub mod usage;
pub mod workspace;

/// The tool every `remember_memory` episode's source starts with.
pub const REMEMBER_SOURCE: &str = "mcp:remember_memory";
/// Whether `source` is a `remember_memory` note by `user` (`mcp:remember_memory:<user>:…`), their id compared
/// as user ids are ([`people::directory::user_id`]: ASCII letters folded). A note written before ids were
/// compared that way names its author as Unicode case rules lower-cased them, and is theirs too (what
/// forgetting them erased then, it still erases).
pub fn remembered_by(source: &str, user: &str) -> bool {
    let ascii = format!("{REMEMBER_SOURCE}:{}:", people::directory::user_id(user));
    let legacy = format!("{REMEMBER_SOURCE}:{}:", user.trim().to_lowercase());
    source.to_ascii_lowercase().starts_with(&ascii) || source.to_lowercase().starts_with(&legacy)
}

/// The source family of one person's own learned advice, `advice:mine:<user id>`: forgetting the person deletes
/// it, as it does what they wrote ([`REMEMBER_SOURCE`]).
pub const PERSONAL_ADVICE_SOURCE: &str = "advice:mine:";
/// What follows a person in their profile's source (`mcp:remember_memory:<them>:memstore:profile`, the server's
/// fold of their notes, `memstore::profile`): no note may take it. A profile version names the notes it cites by
/// id, so a purge of a note takes every version naming it ([`is_profile_source`]).
pub const PROFILE_SOURCE_TAIL: &str = "memstore:profile";

/// Whether `source` is a person's profile's ([`PROFILE_SOURCE_TAIL`]).
pub fn is_profile_source(source: &str) -> bool {
    source
        .strip_prefix(REMEMBER_SOURCE)
        .and_then(|s| s.strip_suffix(PROFILE_SOURCE_TAIL))
        .is_some_and(|who| who.len() > 2 && who.starts_with(':') && who.ends_with(':'))
}
/// The documents a dream records its duplicate links under, `dream/same/<kept fact>/<duplicate fact>`: one
/// episode each (a `dream:` source), readable by the audience the dream ran for. A reader who may read one is
/// served one of the two facts where search would show both ([`Vis::duplicates`]).
pub const DREAM_SAME: &str = "dream/same/";
/// The tag on every fact a dream writes.
pub const DREAM_TAG: &str = "dream";

/// A name a dream gave a thing (`_name`, `_alias`): it finds the thing by that name (`match`, a passage search's
/// name variants) and says nothing of it, so search never serves it as a hit or a group's member.
pub fn dream_name(f: &Fact) -> bool {
    (f.predicate == query::schema::NAME || f.predicate == query::schema::ALIAS)
        && f.tags.iter().any(|t| t == DREAM_TAG)
}

use people::directory;
use query::schema;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arrow_array::{
    Array, ArrayRef, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
    cast::AsArray, types::TimestampMicrosecondType,
};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use iceberg::arrow::schema_to_arrow_schema;
use iceberg::scan::FileScanTask;
use iceberg::spec::{
    DataFileFormat, FormatVersion, NestedField, PrimitiveType, Schema, TableProperties, Type,
};
use iceberg::table::Table;
use iceberg::transaction::{AddColumn, ApplyTransactionAction, Transaction};
use iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use iceberg::writer::file_writer::ParquetWriterBuilder;
use iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, DefaultLocationGenerator,
};
use iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use iceberg::writer::{IcebergWriter, IcebergWriterBuilder};
use iceberg::{Catalog, NamespaceIdent, TableCreation, TableIdent};
use sha2::{Digest, Sha256};

pub const SYSTEM_NS: &str = "memory_system";
const LEDGER: &str = "ledger";
pub const FACTS: &str = "facts";
pub const EPISODES: &str = "episodes";
/// Quotes attached to facts after they were written (backfill); facts keep their own `quote`
/// when extraction produced one. Append-only, committed through the ledger like any write.
const QUOTES: &str = "fact_quotes";
/// The scope's schema, versioned through the ledger: the latest committed row on a branch wins,
/// so a branch can try a different schema without touching main.
const SCHEMAS: &str = "schemas";
/// Explicit ends: a fact stopped being true at a time, with no successor to supersede it (a
/// champion left, a role was removed). Append-only, committed through the ledger like quotes; each
/// row cites the episode that shows the end, and applies only to readers who can see it.
const CLOSURES: &str = "fact_closures";
/// Which episodes an extractor has read: one row per (episode, extractor), committed with the facts it
/// produced, so an episode is extracted once per extractor and a crash can't leave facts without their row.
pub const EXTRACTIONS: &str = "extractions";
/// Deletes at the source (a Slack message deleted, a channel or a record removed), committed through the
/// ledger. Each row names an episode id or one exact source; a delete writes one row per episode it
/// matched, and one for the exact id or source it named, which also covers that record arriving after
/// its delete. A retracted episode is gone for every reader, on every branch and at every time, with the
/// facts that cite it. The rows stay until a purge rewrites the tables. A row marked `superseded` is not a
/// delete: `/ingest`'s `replaces_source` retired that version because a newer one replaced it, so it is hidden
/// only until it is written again (see [`retracted`]).
const RETRACTIONS: &str = "episode_retractions";
/// What the summariser derived per subject (GM4, [`aggregate`]): item statuses, element scores, flags.
/// Append-only and committed through the ledger; each run's rows replace the subject's previous run.
pub const AGGREGATES: &str = "aggregates";
pub const AGG_REQUESTS: &str = "aggregate_requests";
pub const QA_LOG: &str = "qa_log";
pub const FEEDBACK: &str = "feedback";
/// The scope's retention policy ([`retention::Retention`]), versioned through the ledger: main's newest
/// committed row holds on every branch.
const RETENTION: &str = "retention";
/// Orgs, teams and who belongs to them (`directory`): one append-only table of changes in the system
/// namespace, folded into the current state on read.
const DIRECTORY: &str = "directory";
/// Who owns each connector installation (`people::installations`): registrations and revocations,
/// append-only in the system namespace, folded into the current state on read.
const INSTALLATIONS: &str = "installations";
/// Folded snapshots of the directory (`directory_snapshot`): loading reads the newest plus the changes
/// after it, not the whole log.
const DIRECTORY_SNAPSHOT: &str = "directory_snapshot";
/// When service keys were last used (`memory_system.key_usage`): one row per key per flush, written by
/// servers at most once per key per hour, kept apart from the directory so its log stays small.
const KEY_USAGE: &str = "key_usage";
/// The usage book (`crate::usage`): each hour's counters as a server last wrote them, a row per hour per
/// write (every few minutes while it changes); the newest row of an hour holds.
const USAGE_HOURS: &str = "usage_hours";

/// A scope is a physical namespace, so the catalog's own grants enforce it in every engine.
///
/// One catalog can hold several orgs. The deployment's own org is `default`: its scopes keep the
/// namespaces they always had (`memory_org`, `memory_team_<id>`). Another org's are
/// `memory_org_<org>` and `memory_org_<org>__team_<team>`; such a team's id is `<org>/<team>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// The default org.
    Org,
    /// A team: `<id>` in the default org, `<org>/<team>` in another.
    Team(String),
    User(String),
    /// An org other than the default one.
    NamedOrg(String),
}

/// The org a catalog's existing scopes belong to.
pub const DEFAULT_ORG: &str = "default";

impl Scope {
    pub fn namespace(&self) -> String {
        match self {
            Scope::Org => "memory_org".into(),
            Scope::NamedOrg(org) => format!("memory_org_{org}"),
            Scope::Team(id) => match id.split_once('/') {
                Some((org, team)) => format!("memory_org_{org}__team_{team}"),
                None => format!("memory_team_{id}"),
            },
            Scope::User(id) => format!("memory_user_{id}"),
        }
    }

    /// `org`, `org:<org>`, `team:<id>`, `team:<org>/<team>` or `user:<id>`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.split_once(':') {
            None if s == "org" => Scope::Org,
            Some(("org", org)) => Scope::org(org)?,
            Some(("team", id)) => match id.split_once('/') {
                Some((org, team)) => Scope::team(org, team)?,
                None => Scope::Team(id.into()),
            },
            Some(("user", id)) => Scope::User(id.into()),
            _ => anyhow::bail!(
                "scope must be org, org:<org>, team:<id>, team:<org>/<team> or user:<id>, got {s}"
            ),
        })
    }

    /// An org's own scope.
    pub fn org(org: &str) -> Result<Self> {
        check_name("org", org)?;
        Ok(match org {
            DEFAULT_ORG => Scope::Org,
            _ => Scope::NamedOrg(org.into()),
        })
    }

    /// The org a scope belongs to (None for a user's own scope).
    pub fn org_name(&self) -> Option<&str> {
        match self {
            Scope::Org => Some(DEFAULT_ORG),
            Scope::NamedOrg(o) => Some(o),
            Scope::Team(id) => Some(id.split_once('/').map_or(DEFAULT_ORG, |(o, _)| o)),
            Scope::User(_) => None,
        }
    }

    /// A team's scope in an org.
    pub fn team(org: &str, team: &str) -> Result<Self> {
        check_name("org", org)?;
        check_name("team", team)?;
        Ok(Scope::Team(match org {
            DEFAULT_ORG => team.into(),
            _ => format!("{org}/{team}"),
        }))
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Org => f.write_str("org"),
            Scope::NamedOrg(org) => write!(f, "org:{org}"),
            Scope::Team(id) => write!(f, "team:{id}"),
            Scope::User(id) => write!(f, "user:{id}"),
        }
    }
}

pub use memstore_client::check_name;

/// Who is reading. Groups come from the customer's IdP (SCIM); the engine defines none. They are the
/// caller's closure: every team, role, account and channel they hold, already flattened.
#[derive(Clone, Debug)]
pub struct Principal {
    user: String,
    groups: Vec<String>,
    /// The ACL entries this closure can match (`auth::tokens_of`), sorted.
    tokens: Arc<Vec<String>>,
    /// The episodes this closure may read, for the last visibility snapshot it was asked about.
    seen: Arc<Mutex<Option<SeenAt>>>,
}

/// A visibility snapshot id and the episodes readable at it.
type SeenAt = (u64, Arc<roaring::RoaringBitmap>);

impl Principal {
    pub fn new(user: &str, groups: &[&str]) -> Self {
        Self::from_parts(user.into(), groups.iter().map(|g| g.to_string()).collect())
    }

    pub fn from_parts(user: String, groups: Vec<String>) -> Self {
        Principal {
            tokens: Arc::new(auth::tokens_of(&user, &groups)),
            user,
            groups,
            seen: Default::default(),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn groups(&self) -> &[String] {
        &self.groups
    }

    /// The ACL entries this closure can match: all that decides what it may see.
    pub fn tokens(&self) -> &Arc<Vec<String>> {
        &self.tokens
    }

    /// Adds groups to the closure.
    pub fn add_groups(&mut self, more: impl IntoIterator<Item = String>) {
        self.groups.extend(more);
        *self = Self::from_parts(
            std::mem::take(&mut self.user),
            std::mem::take(&mut self.groups),
        );
    }

    /// ACL entries are `*`, `user:<id>`, `group:<id>` or `role:<name>` (a Snowflake role, held when the
    /// principal's groups include `role:<name>`). Names compare exactly: Snowflake's quoted identifiers are
    /// case-sensitive, so case folding would let `ALICE` read what was meant for `"alice"`. Writers store
    /// unquoted names upper-case, as Snowflake does.
    pub fn can_see(&self, acl: &[String]) -> bool {
        acl.iter().any(|a| match a.split_once(':') {
            _ if a == "*" => true,
            Some(("user", u)) => auth::norm_id(u) == auth::norm_id(&self.user),
            // held roles share `groups` as `role:<NAME>`; a group entry never matches one
            Some(("group", g)) => self
                .groups
                .iter()
                .any(|mine| auth::norm_id(mine) == auth::norm_id(g) && !mine.starts_with("role:")),
            Some(("role", r)) => self
                .groups
                .iter()
                .any(|mine| mine.strip_prefix("role:") == Some(r)),
            _ => false,
        })
    }
}

/// That `extractor` (a model and prompt, see `extract::Extractor::id`) read `episode_id` and found `facts`
/// facts in it, committed with those facts by [`Store::commit_extracted`]. `facts` is [`FAILED_TRY`] for a
/// try that failed on the episode itself (the model refused it, its body can't be read): after
/// [`EXTRACT_TRIES`] of those the episode is given up on rather than paid for on every run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extraction {
    pub episode_id: String,
    pub extractor: String,
    pub facts: i64,
    /// The input and output tokens the read cost, where it was counted (once per read: one of its rows).
    pub tokens: Option<(i64, i64)>,
}

/// `fact_id` stopped being true at `valid_to`, as committed episode `episode_id` shows: a closure
/// committed with other writes by [`Store::commit_extracted`] (see [`Store::close_facts`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub fact_id: String,
    pub valid_to: i64,
    pub episode_id: String,
}

/// Refuses a write to a `held-` branch: what extraction holds for owners, proposed as extraction's own, so only
/// extraction writes there (`governance::Store::stage_extracted`, `restage_held`), never a caller.
fn not_held(branch: &str) -> Result<()> {
    anyhow::ensure!(
        !branch.starts_with(governance::HELD_PREFIX),
        "branch names starting {:?} are memstore's own: nothing is written there but by extraction",
        governance::HELD_PREFIX
    );
    Ok(())
}

/// The ACL of a forgotten episode ([`Store::forget`]): an entry no one holds (`.invalid` is a reserved
/// domain), so no one reads it.
pub const FORGOTTEN: &str = "user:forgotten@memstore.invalid";

/// [`Extraction::facts`] of a failed try.
pub const FAILED_TRY: i64 = -1;
/// Failed tries after which an extractor gives up on an episode.
pub const EXTRACT_TRIES: usize = 3;

/// A raw source record (chat, call, CRM row...). Its body is stored once, content-addressed,
/// in the customer's bucket (D-2); its metadata row lives in the scope's `episodes` table.
///
/// See also [`Extraction`], the record that an extractor has read one.
#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    /// sha256 of the body, hex ([`Episode::sourced`]: of the source and the body).
    pub episode_id: String,
    pub source: String,
    pub occurred_at: i64,
    /// Who may see this episode at its source; facts inherit it (D-6).
    pub acl: Vec<String>,
    /// Ids of the entities the episode is about (`deal:42`): what passage search filters on.
    pub about: Vec<String>,
    /// Whether passage search indexes the body. Writers turn it off for records that aren't
    /// prose (a CRM row's JSON); the body is still stored and its facts still cite it.
    pub index_text: bool,
    pub body: Vec<u8>,
    /// When the engine records this row (microseconds since the epoch); `None` is now. Rows rank by it
    /// (the newest ACL wins), so a change feed sets it past the row it replaces.
    pub recorded_at: Option<i64>,
    /// The key of the document this episode is one version of (a wiki page, a policy, compiled
    /// notes), or `None` for a record that has no versions. Of a document's versions a reader sees
    /// one at a time: the newest they may read that occurred by the time asked (see
    /// [`Store::document`]); passage search hides the others.
    pub document: Option<String>,
}

impl Episode {
    pub fn new(source: &str, body: &[u8], acl: &[&str]) -> Self {
        let episode_id = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Episode {
            episode_id,
            source: source.into(),
            occurred_at: now_micros(),
            acl: acl.iter().map(|a| a.to_string()).collect(),
            about: vec![],
            index_text: true,
            body: body.to_vec(),
            recorded_at: None,
            document: None,
        }
    }

    /// An episode whose id covers its source as well as its body. Records from different sources
    /// with the same text (two "ok" replies) are then different episodes, each keeping its own ACL;
    /// with body-only ids the newer ACL would win for both. The same source and body still give the
    /// same id, so a retried write is the same episode.
    pub fn sourced(source: &str, body: &[u8], acl: &[&str]) -> Self {
        let mut e = Self::new(source, body, acl);
        let mut h = Sha256::new();
        h.update((source.len() as u64).to_be_bytes());
        h.update(source.as_bytes());
        h.update(body);
        e.episode_id = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        e
    }

    /// The same episode, about these entities.
    pub fn with_about(mut self, ids: &[&str]) -> Self {
        self.about = ids.iter().map(|a| a.to_string()).collect();
        self
    }

    /// The same episode, as a version of the document `key`.
    pub fn version_of(mut self, key: &str) -> Self {
        self.document = Some(key.into());
        self
    }

    /// The same episode, left out of passage search (its body isn't prose).
    pub fn without_text_index(mut self) -> Self {
        self.index_text = false;
        self
    }
}

/// Which committed episodes a source's ACL change applies to.
#[derive(Clone, Debug, PartialEq)]
pub enum AclTarget {
    /// One episode, by id.
    Episode(String),
    /// Every episode from this source (`slack:C09:1712.345`).
    Source(String),
    /// Every episode whose source starts with this (`slack:C09:`, a whole channel).
    SourcePrefix(String),
    /// Every episode from this source but those kept: the versions a record's newer body replaced. Unlike
    /// [`AclTarget::Source`], it holds only for what exists now, so the record's next version stays.
    SourceExcept { source: String, keep: Vec<String> },
}

impl AclTarget {
    fn matches(&self, episode_id: &str, source: &str) -> bool {
        match self {
            AclTarget::Episode(id) => id == episode_id,
            AclTarget::Source(s) => s == source,
            AclTarget::SourcePrefix(p) => source.starts_with(p.as_str()),
            AclTarget::SourceExcept { source: s, keep } => {
                s == source && !keep.iter().any(|k| k == episode_id)
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fact {
    pub fact_id: String,
    pub entity: String,
    pub predicate: String,
    pub value: String,
    /// Valid time (D-3), microseconds since epoch; `valid_to = None` means still true.
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub source_episodes: Vec<String>,
    /// The verbatim span of a source episode that supports the fact, when extraction kept one.
    pub quote: Option<String>,
    /// Typed columns, filled from the scope's schema on write (see `schema::Schema::normalize`);
    /// all None in a scope without a schema.
    pub entity_type: Option<String>,
    pub value_type: Option<String>,
    pub value_num: Option<f64>,
    /// A date or timestamp value, microseconds since the epoch.
    pub value_ts: Option<i64>,
    /// A ref value: the id of the entity it names.
    pub value_ref: Option<String>,
    /// A number's unit (`%`, `USD`, `reps`), from the schema's predicate on a typed write.
    pub unit: Option<String>,
    /// Where in its episode the fact comes from: the key moment it cites, as JSON of
    /// [`episode_types::Cite`] (a call's turn and offset, a message, a paragraph, a field).
    pub cite: Option<String>,
    /// How its valid time is known: `stated` (the source gives the date), `as_of_source` (true as
    /// of when the source happened) or `inferred` (resolved from a relative date such as "next week").
    pub time_basis: Option<String>,
    /// Labels a writer gives the fact (`billing`, `incident`), lower case: what search filters on (`tags`).
    pub tags: Vec<String>,
}

/// Most tags one fact carries.
pub const MAX_TAGS: usize = 10;

/// A tag as stored: lower-case letters, digits, `_` and `-`, at most 32.
pub fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 32
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Tags as the `tags` column holds them: `,a,b,` (each wrapped in commas, so a tag matches whole), or none.
pub fn tags_column(tags: &[String]) -> Option<String> {
    (!tags.is_empty()).then(|| format!(",{},", tags.join(",")))
}

/// A fact's citation as JSON, when it has one.
pub fn cite_json(f: &Fact) -> Option<serde_json::Value> {
    f.cite.as_deref().and_then(|c| serde_json::from_str(c).ok())
}

/// The values `Fact::time_basis` may take.
pub const TIME_BASES: [&str; 3] = ["stated", "as_of_source", "inferred"];

impl Fact {
    pub fn new(entity: &str, predicate: &str, value: &str) -> Self {
        Fact {
            fact_id: uuid::Uuid::new_v4().to_string(),
            entity: entity.into(),
            predicate: predicate.into(),
            value: value.into(),
            valid_from: now_micros(),
            valid_to: None,
            source_episodes: vec![],
            quote: None,
            ..Default::default()
        }
    }

    pub fn from_episodes(mut self, episodes: &[&Episode]) -> Self {
        self.source_episodes = episodes.iter().map(|e| e.episode_id.clone()).collect();
        self
    }
}

pub const MAIN: &str = "main";

struct LedgerRow {
    commit_id: String,
    branch: String,
    parents: Vec<String>,
    recorded_at: i64,
}

/// Commit graph of one scope. Every commit names its parents, so what a branch sees is
/// everything reachable from its heads, as in git.
pub struct Ledger {
    rows: Vec<LedgerRow>,
}

impl Ledger {
    /// Commits on `branch` that no other commit on `branch` names as a parent.
    /// Concurrent writers can leave several heads; readers see all of them (no lost commits).
    // ponytail: forks stay forked until the next commit names every head as a parent; a
    // strictly linear ledger needs a conditional append (M7 check=concurrent-commits).
    pub fn heads(&self, branch: &str) -> Vec<String> {
        let on_branch = || self.rows.iter().filter(|r| r.branch == branch);
        let named: HashSet<&str> = on_branch()
            .flat_map(|r| r.parents.iter().map(String::as_str))
            .collect();
        on_branch()
            .filter(|r| !named.contains(r.commit_id.as_str()))
            .map(|r| r.commit_id.clone())
            .collect()
    }

    fn reachable(&self, from: &[String]) -> HashSet<String> {
        let parents: HashMap<&str, &[String]> = self
            .rows
            .iter()
            .map(|r| (r.commit_id.as_str(), r.parents.as_slice()))
            .collect();
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = from.to_vec();
        while let Some(c) = stack.pop() {
            // Merges make diamonds; visiting a commit twice would make this exponential.
            if !seen.insert(c.clone()) {
                continue;
            }
            if let Some(ps) = parents.get(c.as_str()) {
                stack.extend(ps.iter().filter(|p| !seen.contains(*p)).cloned());
            }
        }
        seen
    }

    /// Every branch with a commit.
    fn branches(&self) -> BTreeSet<&str> {
        self.rows.iter().map(|r| r.branch.as_str()).collect()
    }

    fn visible(&self, branch: &str) -> HashSet<String> {
        self.reachable(&self.heads(branch))
    }

    /// Each commit's ledger time: past its parents', so a commit is always later than what it was made on.
    fn times(&self) -> HashMap<&str, i64> {
        self.rows
            .iter()
            .map(|r| (r.commit_id.as_str(), r.recorded_at))
            .collect()
    }

    /// The parents of a branch's creation row: main's heads when it was created.
    fn base(&self, branch: &str) -> Option<Vec<String>> {
        let ids: HashSet<&str> = self
            .rows
            .iter()
            .filter(|r| r.branch == branch)
            .map(|r| r.commit_id.as_str())
            .collect();
        self.rows
            .iter()
            .find(|r| r.branch == branch && !r.parents.iter().any(|p| ids.contains(p.as_str())))
            .map(|r| r.parents.clone())
    }
}

/// A table as last read: its metadata and decoded rows, kept per data file so a new
/// snapshot only costs reading the files it added.
#[derive(Clone)]
pub struct Snapshot {
    table: Table,
    pub rows: Arc<Vec<RecordBatch>>,
}

#[derive(Clone)]
struct CacheEntry {
    snap: Snapshot,
    /// None once our own write made the entry stale.
    fetched: Option<Instant>,
    snapshot_id: Option<i64>,
    schema_id: i32,
    /// Bumped by each own-write invalidation, so a refresh that started before the write
    /// cannot mark its older rows fresh.
    generation: u64,
    loaded: Instant,
    files: HashMap<String, Arc<Vec<RecordBatch>>>,
    planned: Planned,
}

/// What a cached snapshot's files were planned from, beyond their rows: the manifests its manifest list names,
/// and one of its scan tasks, whose table-wide fields (schema, projection, name mapping) a task for a file
/// appended later copies. With them the next refresh reads only the manifests appended since, not every
/// manifest the table has (one per commit, so a full plan grows with every write).
#[derive(Clone, Default)]
struct Planned {
    manifests: Arc<HashSet<String>>,
    pub(crate) template: Option<FileScanTask>,
}

#[derive(Default)]
struct Cache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    /// Per table (`<ns>.<name>`): bumped by each own-write invalidation, never removed, so a load that began
    /// before a write can't be cached as fresh after it, even when the table's entry was dropped meanwhile.
    generations: Mutex<HashMap<String, u64>>,
    /// Per namespace: when it was last read, on [`Cache::clock`] (the cache budget evicts the least recent).
    used: Mutex<HashMap<String, u64>>,
    clock: std::sync::atomic::AtomicU64,
    refreshing: Mutex<HashSet<String>>,
    /// Per table: held by the reader doing a blocking refresh, so readers that arrive meanwhile wait for
    /// its rows instead of each asking the catalog.
    loading: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Cache {
    fn loading(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.loading
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone()
    }
}

/// The capacity of the Arrow buffers under `batches` not already in `seen` (by address), so buffers shared
/// between batches (a slice, a snapshot over its files) count once. Adds what it counts to `seen`.
pub fn distinct_bytes<'a>(
    batches: impl Iterator<Item = &'a RecordBatch>,
    seen: &mut HashSet<usize>,
) -> usize {
    fn walk(d: &arrow_data::ArrayData, seen: &mut HashSet<usize>) -> usize {
        let mut n = 0;
        let nulls = d.nulls().map(|b| b.buffer());
        for b in d.buffers().iter().chain(nulls) {
            if seen.insert(b.data_ptr().as_ptr() as usize) {
                n += b.capacity();
            }
        }
        n + d.child_data().iter().map(|c| walk(c, seen)).sum::<usize>()
    }
    batches
        .flat_map(|b| b.columns().iter())
        .map(|c| walk(&c.to_data(), seen))
        .sum()
}

/// The most decoded table rows the cache holds, in bytes (`MEMSTORE_CACHE_BYTES`, default 3 GiB or a quarter
/// of the container's memory if that is less ([`metrics::sized`]); 0 for no limit). Past it, whole scopes are
/// dropped, least recently read first, and loaded again when next read.
pub fn cache_budget() -> usize {
    std::env::var("MEMSTORE_CACHE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| metrics::sized(3 << 30, 0.25))
}

/// A duration in milliseconds from the environment, or `default`.
fn env_millis(name: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

impl CacheEntry {
    /// Age of the metadata, ignoring own-write invalidation (the table is still a valid base).
    fn snap_age(&self) -> Duration {
        self.loaded.elapsed()
    }
}

/// What one branch may see: its committed set and the ACL of each committed episode.
pub struct Vis {
    pub committed: HashSet<String>,
    pub acls: HashMap<String, Vec<String>>,
    /// The same ACLs as per-entry bitmaps over episode ordinals: who may read what, in one bit test.
    index: auth::AclIndex,
    /// Committed backfilled quotes by fact id (the latest one when a fact was backfilled again).
    quotes: HashMap<String, String>,
    /// The branch's schema: the latest committed one, if any.
    pub schema: Option<Arc<schema::Schema>>,
    /// Every schema committed on the branch, oldest first (the last is [`Vis::schema`]): what extraction from
    /// the schema (its `extract` section) offered at each version.
    pub schemas: Vec<Arc<schema::Schema>>,
    /// Committed closures by fact id, newest recorded first: (when it ended, the episode that
    /// shows it).
    closures: HashMap<String, Vec<(i64, String)>>,
    /// The committed merge records ([`MERGE_SOURCE`]): a version one closes lost a merge's conflict.
    merges: HashSet<String>,
    /// The facts each fact was recorded in conflict with ([`conflicts::PREFIX`]), both ways, whoever reads: a read
    /// serves one only to a reader who may read both facts ([`conflicts::marks`]).
    conflicts: HashMap<String, Vec<String>>,
    /// Which committed episodes are versions of a document.
    documents: Documents,
    /// Each committed episode's type (`sales_call`, `slack_thread`, …), as its source names it.
    kinds: HashMap<String, &'static str>,
    /// Committed retracted episodes: deleted, superseded and not written since, or aged out. They are left out
    /// of `acls` and `index`, so no reader sees them.
    retracted: HashSet<String>,
    /// The retracted episodes only a newer version superseded, each with its source: written again, each is
    /// current again ([`retracted`]).
    superseded: HashMap<String, String>,
    /// Superseded episodes written again since, current again, each with the ledger time it came back.
    revived: HashMap<String, i64>,
    /// Facts retracted by id: no reader sees them. (A withdrawn extractor's commits are left out of
    /// `committed` instead, with everything they wrote.)
    pub retracted_facts: HashSet<String>,
    /// What committed deletes name (`id:` or `source:` and the name).
    retraction_targets: HashSet<String>,
    /// The scope's retention policy. Episodes past it are in `retracted`.
    pub retention: retention::Retention,
    /// When the next kept episode ages out (microseconds): this state is stale from then on.
    fresh_until: Option<i64>,
    /// The heads this state sees from (sorted): the branch's when it was built, or those it was pinned to.
    heads: Vec<String>,
    /// Who made each committed commit (the ledger's actor): `extract:<model>` for the engine's extraction,
    /// the caller for an agent's or a script's write, an import's name.
    pub actors: HashMap<String, String>,
    /// Changes whenever this branch's memory loses something a reader could have read (an episode or fact
    /// retracted, forgotten or aged out, an ACL narrowed, a commit withdrawn, the retention or schema
    /// changed), and only then: new writes keep it. What is kept past new writes (a metric's answer) is
    /// kept under it, so it never outlives what it read.
    pub removals: u64,
    /// For each source with a superseded version ([`Vis::superseded`]), the ACLs of its versions readers see
    /// now: whether the version that replaced one is readable by everyone who could read it.
    source_acls: HashMap<String, Vec<Vec<String>>>,
    /// Each superseded episode's newest ACL here: one forgotten or narrowed since the state before is a loss,
    /// though a newer version replaced it.
    replaced_acls: HashMap<String, Vec<String>>,
    /// The superseded episodes aged out: what was kept may hold their text, which retention takes away.
    replaced_aged: HashSet<String>,
}

/// The next fresh [`Vis::removals`]: a state with no earlier one to compare with gets one never used before.
fn fresh_removals() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Documents kept in versions: each version's document, and each document's versions, newest
/// first, as (occurred_at, episode id).
#[derive(Default)]
struct Documents {
    of: HashMap<String, String>,
    versions: HashMap<String, Vec<(i64, String)>>,
}

impl Documents {
    fn build(batches: &[RecordBatch], committed: &HashSet<String>) -> Self {
        let mut of: HashMap<String, (String, i64)> = HashMap::new();
        for b in batches {
            let docs = optional_strings(b, "document");
            if docs.iter().all(Option::is_none) {
                continue;
            }
            let (commit, id) = (strings(b, "commit_id"), strings(b, "episode_id"));
            let occurred = timestamps(b, "occurred_at");
            for i in 0..b.num_rows() {
                if let Some(d) = &docs[i]
                    && committed.contains(&commit[i])
                {
                    of.insert(id[i].clone(), (d.clone(), occurred[i].unwrap_or_default()));
                }
            }
        }
        let mut versions: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        for (id, (d, at)) in &of {
            versions
                .entry(d.clone())
                .or_default()
                .push((*at, id.clone()));
        }
        for v in versions.values_mut() {
            v.sort_by(|a, b| b.cmp(a));
        }
        Documents {
            of: of.into_iter().map(|(id, (d, _))| (id, d)).collect(),
            versions,
        }
    }
}

impl Vis {
    /// Whether this state lost anything a reader could read in `prev`, the state it follows: an episode,
    /// a fact or a commit `prev` held and this one doesn't, an episode's ACL changed (narrowed, or
    /// forgotten), or another retention or schema. Only additions since: false.
    ///
    /// A version a newer one of its source replaced (`replaces_source`) is no loss when every version of that
    /// source readers see now may be read by everyone who could read the old one: what was kept says what the
    /// older version said until someone asks anew. A newer version with a narrower ACL is a loss. Says which
    /// check found the loss.
    ///
    /// What was kept may hold a replaced version's text, so it goes when that version is deleted, forgotten,
    /// narrowed or aged out later.
    fn lost_since(&self, prev: &Vis) -> Option<&'static str> {
        let replaced = |e: &str, old: Option<&Vec<String>>| {
            let (Some(source), Some(old)) = (self.superseded.get(e), old) else {
                return false;
            };
            if self.replaced_acls.get(e) != Some(old) || self.replaced_aged.contains(e) {
                return false;
            }
            self.source_acls.get(source).is_some_and(|now| {
                !now.is_empty() && now.iter().all(|acl| old.iter().all(|a| acl.contains(a)))
            })
        };
        if self
            .retracted
            .iter()
            .any(|e| !prev.retracted.contains(e) && !replaced(e, prev.acls.get(e)))
        {
            return Some("an episode retracted, forgotten or aged out");
        }
        if prev
            .superseded
            .keys()
            .any(|e| !self.superseded.contains_key(e) && self.retracted.contains(e))
        {
            return Some("a replaced version deleted");
        }
        if prev.replaced_acls.iter().any(|(e, acl)| {
            self.superseded.contains_key(e) && self.replaced_acls.get(e) != Some(acl)
        }) {
            return Some("a replaced version forgotten or narrowed");
        }
        if self
            .replaced_aged
            .iter()
            .any(|e| !prev.replaced_aged.contains(e))
        {
            return Some("a replaced version aged out");
        }
        if self
            .retracted_facts
            .iter()
            .any(|f| !prev.retracted_facts.contains(f))
        {
            return Some("a fact retracted");
        }
        if prev.committed.iter().any(|c| !self.committed.contains(c)) {
            return Some("a commit withdrawn");
        }
        if prev.acls.iter().any(|(e, acl)| match self.acls.get(e) {
            Some(now) => now != acl,
            None => !replaced(e, Some(acl)),
        }) {
            return Some("an ACL changed");
        }
        if self.retention != prev.retention {
            return Some("the retention changed");
        }
        if self.schemas.len() != prev.schemas.len() {
            return Some("the schema changed");
        }
        None
    }

    /// Whether an episode is gone for everyone: retracted, erased, or forgotten by its author (re-ACLed to
    /// [`FORGOTTEN`]). What rests on it (a logged answer, a run, feedback) goes with it.
    pub fn is_gone(&self, episode: &str) -> bool {
        self.is_retracted(episode)
            || self
                .acls
                .get(episode)
                .is_none_or(|acl| acl.iter().all(|a| a == FORGOTTEN))
    }

    /// Who made a committed commit.
    pub fn actor(&self, commit: &str) -> Option<&str> {
        self.actors.get(commit).map(String::as_str)
    }

    /// Whether a committed retraction removed this episode: a delete, or a newer version that superseded it
    /// (until it is written again).
    pub fn is_retracted(&self, episode: &str) -> bool {
        self.retracted.contains(episode)
    }

    /// The episodes only a newer version superseded, each with its source: hidden, and current again once
    /// written again.
    pub fn superseded(&self) -> &HashMap<String, String> {
        &self.superseded
    }

    /// The version of `document` this reader sees at `at` (microseconds since the epoch; `None` is
    /// now): the newest they may read that occurred by then, as (occurred_at, episode id).
    pub fn version_at(
        &self,
        document: &str,
        who: &Principal,
        at: Option<i64>,
    ) -> Option<(i64, &str)> {
        self.documents
            .versions
            .get(document)?
            .iter()
            .find(|(t, id)| at.is_none_or(|at| *t <= at) && self.can_see_episode(id, who))
            .map(|(t, id)| (*t, id.as_str()))
    }

    /// Whether search shows this episode at `at`: always, unless it is a version of a document and
    /// another version is the one [`Vis::version_at`] gives then.
    pub fn is_current(&self, episode_id: &str, who: &Principal, at: Option<i64>) -> bool {
        match self.documents.of.get(episode_id) {
            None => true,
            Some(d) => self
                .version_at(d, who, at)
                .is_some_and(|(_, id)| id == episode_id),
        }
    }

    /// The duplicate links this reader may read ([`DREAM_SAME`]): each linked fact with the one fact its group of
    /// duplicates goes by (facts linked to each other directly or through others are one group). Search serves
    /// one fact of a group and keeps the others with it, as it does facts with the same value.
    pub fn duplicates(&self, who: &Principal) -> HashMap<String, String> {
        let mut parent: HashMap<String, String> = HashMap::new();
        fn root<'a>(parent: &'a HashMap<String, String>, mut f: &'a str) -> String {
            while let Some(p) = parent.get(f).filter(|p| *p != f) {
                f = p;
            }
            f.to_string()
        }
        for key in self.documents.versions.keys() {
            let Some((a, b)) = key
                .strip_prefix(DREAM_SAME)
                .and_then(|rest| rest.split_once('/'))
            else {
                continue;
            };
            if a == b || self.version_at(key, who, None).is_none() {
                continue;
            }
            for f in [a, b] {
                parent.entry(f.to_string()).or_insert_with(|| f.to_string());
            }
            let (ra, rb) = (root(&parent, a), root(&parent, b));
            // The smaller id leads, so the same links give the same groups however they are read.
            let (lead, other) = if ra <= rb { (ra, rb) } else { (rb, ra) };
            parent.insert(other, lead);
        }
        parent
            .keys()
            .map(|f| (f.clone(), root(&parent, f)))
            .collect()
    }

    /// The facts `fact_id` was recorded in conflict with ([`conflicts::PREFIX`]), whoever reads: what a reader is
    /// shown of them is [`conflicts::marks`]'s to decide.
    pub fn conflicts_of(&self, fact_id: &str) -> &[String] {
        self.conflicts.get(fact_id).map_or(&[], Vec::as_slice)
    }

    /// The documents under `prefix` this reader sees a version of at `at`, sorted.
    fn documents_under(&self, prefix: &str, who: &Principal, at: Option<i64>) -> Vec<String> {
        let mut keys: Vec<String> = self
            .documents
            .versions
            .keys()
            .filter(|k| k.starts_with(prefix) && self.version_at(k, who, at).is_some())
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// The episodes this reader may see. Memoized on the reader for this snapshot, and shared
    /// across readers with the same closure.
    fn seen(&self, who: &Principal) -> Arc<roaring::RoaringBitmap> {
        let mut memo = who.seen.lock().unwrap();
        if let Some((id, b)) = memo.as_ref()
            && *id == self.index.id()
        {
            return b.clone();
        }
        let b = self.index.visible(&who.tokens);
        *memo = Some((self.index.id(), b.clone()));
        b
    }

    /// Every episode the reader may see, when there are at most `max`: a narrow reader's whole visible set,
    /// which search hands to the index as a prefilter instead of ranking everyone's and filtering after.
    /// `None` for a wide reader, who is filtered after ranking.
    pub fn narrow_episodes(&self, who: &Principal, max: usize) -> Option<Vec<String>> {
        self.index.ids_if_narrow(&self.seen(who), max)
    }

    /// Whether the reader may see a committed episode.
    /// An episode's type (`sales_call`, `slack_thread`, …), if this state holds it.
    pub fn episode_type(&self, episode: &str) -> Option<&'static str> {
        self.kinds.get(episode).copied()
    }

    /// The episodes of type `kind` that `who` may read.
    pub fn episodes_of_type(&self, kind: &str, who: &Principal) -> BTreeSet<String> {
        self.kinds
            .iter()
            .filter(|(e, t)| **t == kind && self.can_see_episode(e, who))
            .map(|(e, _)| e.clone())
            .collect()
    }

    /// How many episodes of each type `who` may read.
    pub fn episode_type_counts(&self, who: &Principal) -> BTreeMap<&'static str, usize> {
        let mut n = BTreeMap::new();
        for (e, t) in &self.kinds {
            if self.can_see_episode(e, who) {
                *n.entry(*t).or_default() += 1;
            }
        }
        n
    }

    pub fn can_see_episode(&self, episode: &str, who: &Principal) -> bool {
        self.index
            .ordinal(episode)
            .is_some_and(|o| self.seen(who).contains(o))
    }

    /// The fact as this reader sees it: its quote filled in from `fact_quotes` when it was written
    /// without one, and its `valid_to` as [`Vis::valid_to`] gives it.
    /// The fact with its backfilled quote, whoever reads.
    pub fn with_quote(&self, mut f: Fact) -> Fact {
        if f.quote.is_none() {
            f.quote = self.quotes.get(&f.fact_id).cloned();
        }
        f
    }

    pub fn annotate(&self, mut f: Fact, who: &Principal) -> Fact {
        if f.quote.is_none() {
            f.quote = self.quotes.get(&f.fact_id).cloned();
        }
        f.valid_to = self.valid_to(&f, who);
        f
    }

    /// When the fact stops being true for this reader: its stored `valid_to`, or an earlier end
    /// from the newest committed closure whose episode the reader can see (a later closure
    /// corrects an earlier one). A closure the reader can't see is ignored for them, so it can
    /// neither end the fact nor override a closure they can see.
    pub fn valid_to(&self, f: &Fact, who: &Principal) -> Option<i64> {
        let closed = self
            .closures
            .get(&f.fact_id)
            .into_iter()
            .flatten()
            .find(|(_, episode)| self.can_see_episode(episode, who))
            .map(|(end, _)| *end);
        match (f.valid_to, closed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether a merge set this version aside, whoever reads: a closure from a merge record ends it (the record is
    /// readable by everyone who reads the version's sources). It never held on main, for anyone.
    pub fn closed_by_merge(&self, fact_id: &str) -> bool {
        self.closures
            .get(fact_id)
            .into_iter()
            .flatten()
            .any(|(_, episode)| self.merges.contains(episode))
    }

    /// Whether, by `t`, a merge set this version aside for the other side's value of its key: a closure the
    /// reader can see comes from a merge record (any of them, so a later ordinary end doesn't undo it). Such a
    /// version never held on main once merged, so it supersedes nothing there (an ordinary end still leaves the
    /// older version superseded).
    pub fn lost_merge(&self, f: &Fact, who: &Principal, t: i64) -> bool {
        self.closures
            .get(&f.fact_id)
            .into_iter()
            .flatten()
            .any(|(end, episode)| {
                *end <= t && self.merges.contains(episode) && self.can_see_episode(episode, who)
            })
    }

    /// Committed on this branch, valid at `valid_at`, cites at least one episode, and the
    /// principal can see every episode it cites (D-6, fail closed).
    pub fn allows(&self, commit: &str, f: &Fact, who: &Principal, valid_at: Option<i64>) -> bool {
        !self.retracted_facts.contains(&f.fact_id)
            && self.allows_parts(
                commit,
                Some(f.valid_from),
                self.valid_to(f, who),
                &f.source_episodes,
                who,
                valid_at,
            )
    }

    fn allows_parts(
        &self,
        commit: &str,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        sources: &[String],
        who: &Principal,
        valid_at: Option<i64>,
    ) -> bool {
        if !self.committed.contains(commit) {
            return false;
        }
        if let Some(t) = valid_at
            && (valid_from.unwrap_or(i64::MIN) > t || valid_to.is_some_and(|end| end <= t))
        {
            return false;
        }
        let seen = self.seen(who);
        !sources.is_empty()
            && sources
                .iter()
                .all(|id| self.index.ordinal(id).is_some_and(|o| seen.contains(o)))
    }
}

type Plan = (Instant, Table, Vec<FileScanTask>);
type DecodedFiles = HashMap<String, Arc<Vec<RecordBatch>>>;
type CachedVis = ([Arc<Vec<RecordBatch>>; 7], Arc<Vis>);
/// Held while one visibility state is built (see `Store::visibility_at`).
type VisBuild = Arc<tokio::sync::Mutex<()>>;

/// Takes a visibility build's gate out of the map when dropped, if it is still that gate.
struct GateGone<'a> {
    builds: &'a Mutex<HashMap<(String, String), VisBuild>>,
    key: (String, String),
    gate: VisBuild,
}

impl Drop for GateGone<'_> {
    fn drop(&mut self) {
        let mut builds = self.builds.lock().unwrap();
        if builds
            .get(&self.key)
            .is_some_and(|g| Arc::ptr_eq(g, &self.gate))
        {
            builds.remove(&self.key);
        }
    }
}

/// A read session pinned to one ledger state (spec D-9): every read through it sees exactly
/// the commits (and episode ACLs) that were visible on its branch when it was pinned, however
/// many writers commit meanwhile. Rows removed since (an erasure) do disappear: the session
/// pins which commits count, not deleted data.
#[derive(Clone)]
pub struct Session {
    scope: Scope,
    branch: String,
    vis: Arc<Vis>,
    /// The time a session pinned to a snapshot reads as of (when it was taken): "now" in its queries and
    /// the recency its searches rank by, so the same reads give the same answers later.
    at: Option<i64>,
    /// A pinned session's index versions: its searches rank with the indexes as its snapshot recorded
    /// them, never with a later update's.
    index: Option<Arc<search::IndexPin>>,
}

/// What the first page of a paged answer read, carried by its cursor so every later page reads the same:
/// the branch's heads then (nothing committed after shows), the time it ranked as of, and the index
/// versions it searched.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PagePin {
    #[serde(rename = "h")]
    pub heads: Vec<String>,
    #[serde(rename = "t")]
    pub at: i64,
    /// The index versions it searched (none: a cursor from before index versions were carried).
    #[serde(rename = "i", default, skip_serializing_if = "Option::is_none")]
    pub index: Option<PageIndex>,
}

/// The dataset versions of each index part a page searched; empty for an index that wasn't there.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PageIndex {
    #[serde(rename = "f", default)]
    pub facts: BTreeMap<String, u64>,
    #[serde(rename = "p", default)]
    pub passages: BTreeMap<String, u64>,
}

impl PagePin {
    /// The pin as cursor text (URL-safe base64 of its JSON; it holds no '.').
    pub fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).unwrap_or_default())
    }

    /// A pin from cursor text; none when it isn't one.
    pub fn decode(text: &str) -> Option<PagePin> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

impl Session {
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The index versions a pinned session searches with; none for a session on the current indexes.
    pub fn index_pin(&self) -> Option<&search::IndexPin> {
        self.index.as_deref()
    }

    /// What a page read in this session, as of `at`, for its cursor ([`Session::resume`]): its heads, and the
    /// index versions it searches with now (none for a session pinned to a snapshot, whose are fixed).
    pub async fn page_pin(&self, store: &Store, at: i64) -> Result<PagePin> {
        let index = match self.index {
            Some(_) => None,
            None => Some(PageIndex {
                facts: search::current_versions(store, &self.scope).await?,
                passages: search::passages::current_versions(store, &self.scope).await?,
            }),
        };
        Ok(PagePin {
            heads: self.vis.heads.clone(),
            at,
            index,
        })
    }

    /// This session as a cursor's first page read it: pinned to its heads, time and index versions, so a
    /// later page neither repeats nor skips what landed meanwhile, nor ranks with a later index update. A
    /// session already pinned to a snapshot stays as it is.
    pub async fn resume(&self, store: &Store, pin: &PagePin) -> Result<Session> {
        if self.at.is_some() {
            return Ok(self.clone());
        }
        let index = pin.index.as_ref().map(|i| {
            let pinned = |versions: &BTreeMap<String, u64>, config: search::IndexConfig| {
                (!versions.is_empty()).then(|| search::Coverage {
                    at: 0,
                    heads: BTreeMap::new(),
                    versions: versions.clone(),
                    config,
                })
            };
            search::IndexPin {
                facts: pinned(&i.facts, search::config()),
                passages: pinned(&i.passages, search::passages::config(store)),
            }
        });
        if pin.heads == self.vis.heads {
            return Ok(Session {
                at: Some(pin.at),
                index: index.map(Arc::new),
                ..self.clone()
            });
        }
        let name = format!(
            "page-{}",
            &auth::hex(&Sha256::digest(pin.heads.join(",").as_bytes()))[..16]
        );
        store
            .pin_at(&self.scope, &self.branch, &name, &pin.heads, pin.at, index)
            .await
    }

    /// Whether this session reads main as it is now (no branch, no snapshot or page pin): where the engine's
    /// own bookkeeping may be written from a read.
    pub fn on_main_now(&self) -> bool {
        self.branch == MAIN && self.at.is_none()
    }

    /// The time this session reads as of: its snapshot's, else now (µs since the epoch).
    pub fn now(&self) -> i64 {
        self.at.unwrap_or_else(now_micros)
    }

    /// The schema of the session's branch, if the scope has one.
    pub fn schema(&self) -> Option<Arc<schema::Schema>> {
        self.vis.schema.clone()
    }

    pub fn vis(&self) -> &Vis {
        &self.vis
    }

    /// The visibility state itself: the same one for every session pinned to the same ledger state.
    pub fn vis_arc(&self) -> &Arc<Vis> {
        &self.vis
    }

    pub async fn read(
        &self,
        store: &Store,
        who: &Principal,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let facts = store.rows(&self.scope.namespace(), FACTS).await?;
        Ok(facts
            .rows
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .filter(|(commit, f)| self.vis.allows(commit, f, who, valid_at))
            .map(|(_, f)| self.vis.annotate(f, who))
            .collect())
    }

    /// The facts matching `filter` that the principal may see, as [`Session::read`] gives them: read
    /// through the search index (its scalar indexes serve the filter) and the unindexed tail, so a read of
    /// a few entities or facts never decodes every fact in the scope.
    pub async fn read_where(
        &self,
        store: &Store,
        who: &Principal,
        filter: &search::Filter,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        // Superseded rows are read too unless only what is valid now is asked for.
        let history = valid_at != Some(i64::MAX);
        Ok(search::scan(store, &self.scope, filter, history)
            .await?
            .into_iter()
            .filter(|(commit, f)| self.vis.allows(commit, f, who, valid_at))
            .map(|(_, f)| self.vis.annotate(f, who))
            .collect())
    }

    /// [`Session::read_where`] for the facts whose `col` is one of `values`, a chunk of values per scan.
    pub async fn read_in(
        &self,
        store: &Store,
        who: &Principal,
        col: search::Col,
        values: &[String],
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        const CHUNK: usize = 500;
        let mut out = vec![];
        for chunk in values.chunks(CHUNK) {
            let filter = search::Filter::In(
                col,
                chunk.iter().map(|v| search::Lit::Str(v.clone())).collect(),
            );
            out.extend(self.read_where(store, who, &filter, valid_at).await?);
        }
        Ok(out)
    }

    pub async fn lookup(
        &self,
        store: &Store,
        who: &Principal,
        entity: &str,
        predicate: &str,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        self.key(store, who, search::Key::Entity(entity, predicate), valid_at)
            .await
    }

    pub async fn lookup_value(
        &self,
        store: &Store,
        who: &Principal,
        predicate: &str,
        value: &str,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        self.key(store, who, search::Key::Value(predicate, value), valid_at)
            .await
    }

    /// Top-`k` facts matching `query` the reader may see, valid at `valid_at` (None = any time),
    /// ordered by `rank` (see [`search::Rank`]).
    pub async fn search(
        &self,
        store: &Store,
        who: &Principal,
        query: &str,
        k: usize,
        valid_at: Option<i64>,
        rank: search::Rank,
    ) -> Result<Vec<search::Hit>> {
        search::search_with(
            store,
            &self.scope,
            &self.vis,
            self.index.as_deref(),
            who,
            query,
            k,
            valid_at,
            None,
            rank,
        )
        .await
    }

    /// [`Session::search`] restricted to facts matching `filter`, applied before ranking.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_filtered(
        &self,
        store: &Store,
        who: &Principal,
        query: &str,
        k: usize,
        valid_at: Option<i64>,
        filter: &search::Filter,
        rank: search::Rank,
    ) -> Result<Vec<search::Hit>> {
        search::search_with(
            store,
            &self.scope,
            &self.vis,
            self.index.as_deref(),
            who,
            query,
            k,
            valid_at,
            Some(filter),
            rank,
        )
        .await
    }

    /// Whether the session sees episode `episode_id` committed and the principal may read it.
    pub fn can_read_episode(&self, episode_id: &str, who: &Principal) -> bool {
        self.vis.can_see_episode(episode_id, who)
    }

    /// An episode's body, if the session sees it committed and the principal may read it.
    pub async fn episode_body(
        &self,
        store: &Store,
        who: &Principal,
        episode_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        if !self.vis.can_see_episode(episode_id, who) {
            return Ok(None);
        }
        let table = &store.rows(&self.scope.namespace(), EPISODES).await?.table;
        let input = table.file_io().new_input(body_path(table, episode_id))?;
        match input.read().await {
            Ok(body) => Ok(Some(body.to_vec())),
            // Deleted since this session's tables were read (a purge, or offboarding, in another process):
            // the episode is gone.
            Err(_) if !input.exists().await? => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// These episodes as the principal may read them: their source, when they happened and their bodies, read
    /// concurrently. What search shows a result's source as. One this session doesn't see committed, that the
    /// principal may not read, or whose body is gone is left out. The rest of each row (ACL, `about`) is empty.
    pub async fn episodes(
        &self,
        store: &Store,
        who: &Principal,
        ids: &[String],
    ) -> Result<HashMap<String, Episode>> {
        let mut found = self.episode_sources(store, who, ids).await?;
        if found.is_empty() {
            return Ok(HashMap::new());
        }
        let bodies = store
            .episode_bodies(&self.scope, found.keys().cloned().collect())
            .await?;
        Ok(bodies
            .into_iter()
            .filter_map(|(id, body)| {
                let (source, occurred_at) = found.remove(&id)?;
                let mut ep = Episode::new(&source, &body, &[]);
                ep.episode_id = id.clone();
                ep.occurred_at = occurred_at;
                Some((id, ep))
            })
            .collect())
    }

    /// These episodes' sources and when they happened, as (source, occurred_at) by id, for those this session sees
    /// committed and the principal may read. No body is read.
    pub async fn episode_sources(
        &self,
        store: &Store,
        who: &Principal,
        ids: &[String],
    ) -> Result<HashMap<String, (String, i64)>> {
        let wanted: HashSet<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| self.vis.can_see_episode(id, who))
            .collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let mut found: HashMap<String, (String, i64)> = HashMap::new();
        for b in store
            .rows(&self.scope.namespace(), EPISODES)
            .await?
            .rows
            .iter()
        {
            let id = b
                .column_by_name("episode_id")
                .expect("episode_id")
                .as_string::<i32>();
            let rows: Vec<usize> = (0..id.len())
                .filter(|&i| wanted.contains(id.value(i)))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let (commit, source) = (strings(b, "commit_id"), strings(b, "source"));
            let at = timestamps(b, "occurred_at");
            for i in rows {
                if self.vis.committed.contains(&commit[i]) {
                    found.insert(
                        id.value(i).to_string(),
                        (source[i].clone(), at[i].unwrap_or_default()),
                    );
                }
            }
        }
        Ok(found)
    }

    /// The episodes whose source starts with one of `prefixes` that the principal may read in this session, as
    /// (episode id, source, occurred_at), in the table's order: no body is read. A version a newer one of its
    /// source replaced (`replaces_source`) is left out, as everywhere it is hidden.
    pub async fn sourced(
        &self,
        store: &Store,
        who: &Principal,
        prefixes: &[&str],
    ) -> Result<Vec<(String, String, i64)>> {
        let mut out = vec![];
        let mut seen = HashSet::new();
        for b in store
            .rows(&self.scope.namespace(), EPISODES)
            .await?
            .rows
            .iter()
        {
            let source = b
                .column_by_name("source")
                .expect("source")
                .as_string::<i32>();
            let rows: Vec<usize> = (0..source.len())
                .filter(|&i| prefixes.iter().any(|p| source.value(i).starts_with(p)))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let (id, commit) = (strings(b, "episode_id"), strings(b, "commit_id"));
            let at = timestamps(b, "occurred_at");
            for i in rows {
                if self.vis.committed.contains(&commit[i])
                    && self.vis.can_see_episode(&id[i], who)
                    && seen.insert(id[i].clone())
                {
                    out.push((
                        id[i].clone(),
                        source.value(i).to_string(),
                        at[i].unwrap_or_default(),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// The version of document `key` that `who` sees in this session at `at` (microseconds since the
    /// epoch; `None` is now), as (episode id, occurred_at, body); see [`Store::document`].
    pub async fn document(
        &self,
        store: &Store,
        who: &Principal,
        key: &str,
        at: Option<i64>,
    ) -> Result<Option<(String, i64, Vec<u8>)>> {
        let Some((t, id)) = self.vis.version_at(key, who, at) else {
            return Ok(None);
        };
        let id = id.to_string();
        Ok(self
            .episode_body(store, who, &id)
            .await?
            .map(|b| (id, t, b)))
    }

    /// The keys of the documents under `prefix` that `who` sees a version of at `at`, sorted.
    pub fn documents(&self, who: &Principal, prefix: &str, at: Option<i64>) -> Vec<String> {
        self.vis.documents_under(prefix, who, at)
    }

    /// Which version of document `key` [`Session::document`] gives `who` at `at`: its number, counting from 1 the
    /// versions they may read up to it, and when it occurred. Read without any body.
    pub fn version_number(
        &self,
        who: &Principal,
        key: &str,
        at: Option<i64>,
    ) -> Option<(usize, i64)> {
        let mut seen = self
            .vis
            .documents
            .versions
            .get(key)?
            .iter()
            .filter(|(t, id)| at.is_none_or(|at| *t <= at) && self.vis.can_see_episode(id, who));
        let (t, _) = seen.next()?;
        Some((seen.count() + 1, *t))
    }

    /// The episode of each document's newest version under `prefix`, whoever may read it, for the first `limit`
    /// keys in order: for the engine's own work.
    pub fn newest_versions_under(&self, prefix: &str, limit: usize) -> HashSet<String> {
        let mut keys: Vec<&String> = self
            .vis
            .documents
            .versions
            .keys()
            .filter(|k| k.starts_with(prefix))
            .collect();
        keys.sort();
        keys.into_iter()
            .take(limit)
            .filter_map(|k| {
                self.vis.documents.versions[k]
                    .first()
                    .map(|(_, id)| id.clone())
            })
            .collect()
    }

    /// Every version of document `key` that `who` may read in this session, newest first, as (episode id,
    /// occurred_at, body): its history, where [`Session::document`] gives one version.
    pub async fn versions(
        &self,
        store: &Store,
        who: &Principal,
        key: &str,
    ) -> Result<Vec<(String, i64, Vec<u8>)>> {
        let mut out = vec![];
        for (t, id) in self.vis.documents.versions.get(key).into_iter().flatten() {
            if let Some(b) = self.episode_body(store, who, id).await? {
                out.push((id.clone(), *t, b));
            }
        }
        Ok(out)
    }

    /// The keys of the documents under `prefix` with a version that isn't gone, whoever may read them, in order: for
    /// the engine's own reads ([`Session::engine_document`]).
    pub fn engine_documents(&self, prefix: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .vis
            .documents
            .versions
            .iter()
            .filter(|(k, vs)| {
                k.starts_with(prefix) && vs.iter().any(|(_, id)| !self.vis.is_gone(id))
            })
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// The newest version of document `key` that isn't gone, whoever may read it: for the engine's own
    /// documents (`advice/map`), which it reads whatever audience it last wrote them for.
    pub async fn engine_document(&self, store: &Store, key: &str) -> Result<Option<Vec<u8>>> {
        let newest = self
            .vis
            .documents
            .versions
            .get(key)
            .into_iter()
            .flatten()
            .find(|(_, id)| !self.vis.is_gone(id));
        let Some((_, id)) = newest else {
            return Ok(None);
        };
        Ok(store
            .episode_bodies(&self.scope, vec![id.clone()])
            .await?
            .remove(id))
    }

    async fn key(
        &self,
        store: &Store,
        who: &Principal,
        key: search::Key<'_>,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let rows = search::key_lookup(store, &self.scope, key).await?;
        Ok(rows
            .into_iter()
            .filter(|(commit, f)| self.vis.allows(commit, f, who, valid_at))
            .map(|(_, f)| self.vis.annotate(f, who))
            .collect())
    }
}

/// What a branch of a scope already holds ([`Store::committed_rows`]): reloading a corpus skips it.
#[derive(Default, Debug)]
pub struct Committed {
    /// Each committed episode's newest ACL, sorted.
    pub episodes: HashMap<String, Vec<String>>,
    pub fact_ids: HashSet<String>,
    /// Each committed fact's entity, predicate, value and sorted evidence.
    pub fact_keys: HashSet<(String, String, String, Vec<String>)>,
    /// (fact id, end, episode id) of each committed closure.
    pub closures: HashSet<(String, i64, String)>,
}

impl Committed {
    pub fn fact_key(f: &Fact) -> (String, String, String, Vec<String>) {
        let mut sources = f.source_episodes.clone();
        sources.sort();
        (
            f.entity.clone(),
            f.predicate.clone(),
            f.value.clone(),
            sources,
        )
    }

    /// Whether this fact is committed already: by id, or the same claim from the same evidence.
    pub fn has_fact(&self, f: &Fact) -> bool {
        self.fact_ids.contains(&f.fact_id) || self.fact_keys.contains(&Self::fact_key(f))
    }

    /// Whether this episode is committed already, with this ACL (a changed ACL is a change to load).
    pub fn has_episode(&self, e: &Episode) -> bool {
        let mut acl = e.acl.clone();
        acl.sort();
        self.episodes.get(&e.episode_id) == Some(&acl)
    }
}

/// A commit waiting for the group-commit leader.
struct PendingCommit {
    scope: Scope,
    branch: String,
    actor: String,
    episodes: Vec<Episode>,
    facts: Vec<Fact>,
    extractions: Vec<Extraction>,
    closures: Vec<Closure>,
    done: tokio::sync::oneshot::Sender<std::result::Result<String, String>>,
    /// Its room under the deployment cap, counted once this commit lands and given back if it fails.
    capped: Option<workspace::CapReservation>,
    /// Its room under its workspace's caps, held until it settles.
    _workspace: Option<workspace::WsReservation>,
    /// The committing store's fence ([`Store::fenced`]), checked again just before the batch is written.
    fence: Option<Fence>,
}

#[derive(Default)]
struct Group {
    queue: Vec<PendingCommit>,
    leading: bool,
}

/// The group-commit leader's hold on the queue. The leader runs in a task of its own, so a caller that
/// goes away (an HTTP client that disconnects) cancels neither its commit nor the ones queued with it.
/// Disarmed when the leader stops with the queue empty; a leader that stops otherwise (a panic) drops
/// whatever is still queued, so those writers hear it at once rather than waiting for the next commit
/// to lead, and any lock a queued commit holds (the deployment cap's) is released.
struct LeaderGuard {
    group: Arc<Mutex<Group>>,
    armed: bool,
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let orphans = match self.group.lock() {
            Ok(mut g) => {
                g.leading = false;
                std::mem::take(&mut g.queue)
            }
            Err(_) => vec![],
        };
        drop(orphans);
    }
}

#[derive(Clone)]
pub struct Store {
    /// The catalog, in a slot every store sharing this one's caches also holds: a refreshed catalog (fresh
    /// credentials) reaches work that started on an older store, such as a long extraction run.
    catalog: Arc<std::sync::RwLock<Arc<dyn Catalog>>>,
    cache: Arc<Cache>,
    max_staleness: Duration,
    /// Never serve cached rows or plans older than this without blocking on a refresh, even if
    /// background refreshes keep failing (`MEMSTORE_HARD_STALENESS_MS`, default 60000).
    hard_staleness: Duration,
    /// Per scope: facts table metadata and its planned data files, for at most the staleness
    /// window, so key lookups don't reload the catalog each time.
    plans: Arc<Mutex<HashMap<String, Plan>>>,
    /// Per scope: bumped by each own write to facts, so a plan loaded before the write can't
    /// be cached after it (read-your-writes for lookups).
    plan_generation: Arc<Mutex<HashMap<String, u64>>>,
    /// Per scope: decoded facts data files not covered by the search index, by file key.
    tail: Arc<Mutex<HashMap<String, DecodedFiles>>>,
    vis: Arc<Mutex<HashMap<(String, String), CachedVis>>>,
    /// Per visibility key: held while that state is built, so readers that miss it together wait for one
    /// build and take what it built rather than each building it again.
    vis_builds: Arc<Mutex<HashMap<(String, String), VisBuild>>>,
    /// Per side table (`<ns>.<name>`: schemas, closures): when it was last found missing (a scope
    /// created before the table existed, not yet evolved by `ensure_scope`), so reads don't ask
    /// the catalog every time.
    no_side_table: Arc<Mutex<HashMap<String, Instant>>>,
    clock_offset_us: i64,
    index_base: String,
    /// How passages are chunked, as (bytes, overlap): `MEMSTORE_CHUNK_BYTES` and `MEMSTORE_CHUNK_OVERLAP`
    /// (default 1500 and 150). Another chunking is another passage index ([`search::passages`]).
    chunking: (usize, usize),
    /// Shared by this store's clones: one group commit per store, whichever clone writes.
    group: Arc<Mutex<Group>>,
    /// The directory as last folded, and its writes. Shared by the stores a server makes in turn
    /// ([`Store::sharing_directory`]), so a new store neither re-reads the log nor races its own server.
    dir: Arc<DirState>,
    /// Whether this store may still write ([`Store::fenced`]): checked before each commit and each
    /// write to a side table. None: always.
    fence: Option<Fence>,
}

/// [`Store::system_storage`].
#[derive(Clone)]
pub struct SystemStorage {
    /// The system tables' directory, e.g. `gs://bucket/warehouse/memory_system`.
    pub system: String,
    /// The ledger table's own directory, under it.
    pub ledger: String,
    /// The ledger's file IO properties: storage credentials and settings (`gcs.oauth2.token`,
    /// `s3.access-key-id`, `adls.sas-token.<account>`, ...). Secrets: never logged.
    pub props: HashMap<String, String>,
}

/// Whether a store may still write: a server's background work writes only while it holds the writer lease.
pub type Fence = Arc<dyn Fn() -> bool + Send + Sync>;

/// A fenced store's write refused: the fence ([`Store::fenced`]) says it may no longer write.
#[derive(Debug)]
pub struct Fenced;

impl std::fmt::Display for Fenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not written: this server no longer holds the writer lease")
    }
}

impl std::error::Error for Fenced {}

/// A server's directory fold: when it was loaded, the directory table snapshot it was folded from (none:
/// not known), and the fold.
type HeldFold = (Instant, Option<i64>, Arc<directory::Folded>);

/// The directory as one server holds it.
#[derive(Default)]
struct DirState {
    /// The fold, when it was loaded, and the directory table snapshot it was folded from (none: not
    /// known). Held across a load, so concurrent loads are one load.
    folded: tokio::sync::Mutex<Option<HeldFold>>,
    /// One directory write at a time from this server; other servers are raced with a
    /// compare-and-set on the table's snapshot ([`Store::append_directory_with`]).
    writes: tokio::sync::Mutex<()>,
    /// Changes covered by the newest snapshot this server knows of.
    snapshot_events: std::sync::atomic::AtomicU64,
    /// Load latency, commit retries, and whether the log holds a change this engine can't read.
    metrics: metrics::DirMetrics,
    /// The key snapshots are sealed with and checked against (`MEMSTORE_SECRET_KEY`, or the server's own:
    /// [`Store::with_directory_key`]). Taken from the environment on first use if no one set it.
    key: std::sync::OnceLock<Option<auth::secret::SecretKey>>,
}

/// A directory change is re-read, re-planned and tried again at most this many times when other
/// servers keep committing first.
const DIRECTORY_ATTEMPTS: u32 = 50;

/// After this many changes past the newest snapshot, the server that writes the next change also writes
/// a snapshot (`MEMSTORE_DIRECTORY_SNAPSHOT_EVERY`; 0: never on its own).
fn snapshot_every() -> u64 {
    std::env::var("MEMSTORE_DIRECTORY_SNAPSHOT_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
}

impl Store {
    /// Reads may be up to `MEMSTORE_MAX_STALENESS_MS` (default 1000) behind other writers, and
    /// block on the catalog rather than serve rows older than `MEMSTORE_HARD_STALENESS_MS`
    /// (default 60000); this store's own writes are always visible to its next read.
    pub fn new(catalog: Arc<dyn Catalog>) -> Self {
        // Only this cell's memory, whatever the call path (`cell`).
        let catalog = cell::CellCatalog::wrap(catalog);
        Store {
            catalog: Arc::new(std::sync::RwLock::new(catalog)),
            cache: Arc::default(),
            max_staleness: env_millis("MEMSTORE_MAX_STALENESS_MS", 1000),
            hard_staleness: env_millis("MEMSTORE_HARD_STALENESS_MS", 60_000),
            plans: Arc::default(),
            plan_generation: Arc::default(),
            tail: Arc::default(),
            vis: Arc::default(),
            vis_builds: Arc::default(),
            no_side_table: Arc::default(),
            clock_offset_us: 0,
            index_base: search::default_index_base(),
            chunking: search::passages::default_chunking(),
            group: Arc::default(),
            dir: Arc::default(),
            fence: None,
        }
    }

    /// This store, writing only while `fence` says it may: each commit ([`Store::commit`] and the like) and
    /// each side-table write ([`Store::write_target`]) checks it first, and a commit queued for a group commit
    /// checks it again just before its batch is written. A write it refuses fails with [`Fenced`]. For a
    /// server's background work, which only the writer lease's holder does.
    pub fn fenced(mut self, fence: Fence) -> Self {
        self.fence = Some(fence);
        self
    }

    /// [`Fenced`] if this store may no longer write.
    pub fn check_fence(&self) -> Result<()> {
        match &self.fence {
            Some(f) if !f() => Err(Fenced.into()),
            _ => Ok(()),
        }
    }

    /// Wait until no group commit is in flight on this store's group (the caller bounds the wait): a server
    /// stopping its background writing lets what is already being written land before it hands over.
    pub async fn commits_settled(&self) {
        while self.group.lock().unwrap().leading {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// This store, holding the directory as `other` does (its fold, and its turn to write): for a store
    /// that replaces `other` on the same catalog.
    pub fn sharing_directory(mut self, other: &Store) -> Self {
        self.dir = other.dir.clone();
        self
    }

    /// Seal directory snapshots with `key`, and load only those it sealed without reading the log before
    /// them (none: every snapshot is checked against the log). A store's first use of its directory
    /// fixes this: unless a server set it before, it is `MEMSTORE_SECRET_KEY`.
    pub fn with_directory_key(self, key: Option<auth::secret::SecretKey>) -> Self {
        let _ = self.dir.key.set(key);
        self
    }

    fn directory_key(&self) -> Option<&auth::secret::SecretKey> {
        self.dir
            .key
            .get_or_init(|| auth::secret::SecretKey::from_env().ok().flatten())
            .as_ref()
    }

    /// The current catalog: the newest one a store sharing these caches came in with.
    pub fn catalog(&self) -> Arc<dyn Catalog> {
        self.catalog.read().unwrap().clone()
    }

    /// This store, reading through `other`'s caches (table rows, fact plans, decoded files,
    /// visibility): for a store that replaces `other` on a fresh catalog connection, so the switch
    /// doesn't send every read back to the catalog. Cached tables are reloaded through this store's
    /// catalog as they pass the staleness window, as always. `other`, and work still running on it
    /// (an extraction run, an index build), goes on through this store's catalog from now on: the old
    /// one's credentials expire.
    pub fn sharing_caches(mut self, other: &Store) -> Self {
        // This store's catalog goes into the slot `other` (and every store before it) reads from.
        let fresh = self.catalog();
        self.catalog = other.catalog.clone();
        *self.catalog.write().unwrap() = fresh;
        self.cache = other.cache.clone();
        self.plans = other.plans.clone();
        self.plan_generation = other.plan_generation.clone();
        self.tail = other.tail.clone();
        self.vis = other.vis.clone();
        self.vis_builds = other.vis_builds.clone();
        self.no_side_table = other.no_side_table.clone();
        self
    }

    /// Reload every cached table and fact plan through this store's catalog, one at a time, while
    /// reads keep getting the cached copies: after [`Store::sharing_caches`], so what is cached
    /// carries this catalog's storage credentials however long the staleness limits are.
    pub async fn rewarm(&self) {
        let keys: Vec<String> = self.cache.entries.lock().unwrap().keys().cloned().collect();
        for key in keys {
            let Some((ns, name)) = key.rsplit_once('.') else {
                continue;
            };
            if !self.cache.refreshing.lock().unwrap().insert(key.clone()) {
                continue;
            }
            if let Err(e) = refresh(&*self.catalog(), &self.cache, ns, name).await {
                eprintln!("memstore: reloading {key} failed (serving the cached copy): {e:#}");
            }
            self.cache.refreshing.lock().unwrap().remove(&key);
        }
        let planned: Vec<String> = self.plans.lock().unwrap().keys().cloned().collect();
        for ns in planned {
            let started = self.plan_generation.lock().unwrap().get(&ns).copied();
            match load_plan(&*self.catalog(), &ns).await {
                // Not over a plan our own write made stale meanwhile.
                Ok((t, tasks))
                    if self.plan_generation.lock().unwrap().get(&ns).copied() == started =>
                {
                    self.plans
                        .lock()
                        .unwrap()
                        .insert(ns, (Instant::now(), t, tasks));
                }
                Ok(_) => {}
                Err(e) => eprintln!("memstore: reloading the plan of {ns} failed: {e:#}"),
            }
        }
    }

    /// Current rows of a table. Served from memory; once older than the staleness window a
    /// background refresh is started and the cached rows are still returned. A read blocks on
    /// the catalog only on a cold cache, after this store's own write (read-your-writes), or
    /// when the cache is older than the hard staleness limit. Readers blocked on one table share
    /// one load.
    pub async fn rows(&self, ns: &str, name: &str) -> Result<Snapshot> {
        let key = format!("{ns}.{name}");
        self.touch(ns);
        if let Some(snap) = self.cached_rows(ns, name, &key) {
            return Ok(snap);
        }
        let loading = self.cache.loading(&key);
        let _turn = loading.lock().await;
        // The reader ahead may have just loaded it.
        if let Some(snap) = self.cached_rows(ns, name, &key) {
            return Ok(snap);
        }
        let snap = refresh(&*self.catalog(), &self.cache, ns, name).await?;
        self.trim(ns);
        Ok(snap)
    }

    /// Mark `ns` as just read, for the cache budget.
    fn touch(&self, ns: &str) {
        if ns == SYSTEM_NS {
            return;
        }
        let at = self
            .cache
            .clock
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut used = self.cache.used.lock().unwrap();
        match used.get_mut(ns) {
            Some(t) => *t = at,
            None => {
                used.insert(ns.to_string(), at);
            }
        }
    }

    /// What this store holds in memory, for the server's memory log: per namespace, the distinct Arrow buffer
    /// bytes of its cached tables (decoded files and snapshot rows together, each shared buffer counted once,
    /// so a snapshot that copies its files shows), the decoded fact files held for the index (`tail`), and the
    /// counts of visibility states and fact plans. The largest namespaces first.
    pub fn memory_report(&self) -> String {
        let mut seen = HashSet::new();
        let mut by: HashMap<String, (usize, usize)> = HashMap::new();
        for (key, e) in self.cache.entries.lock().unwrap().iter() {
            let ns = key.rsplit_once('.').map_or(key.as_str(), |(n, _)| n);
            let files = e.files.values().flat_map(|b| b.iter());
            by.entry(ns.to_string()).or_default().0 +=
                distinct_bytes(files.chain(e.snap.rows.iter()), &mut seen);
        }
        for (ns, files) in self.tail.lock().unwrap().iter() {
            by.entry(ns.clone()).or_default().1 +=
                distinct_bytes(files.values().flat_map(|b| b.iter()), &mut seen);
        }
        let (tables, tail): (usize, usize) =
            by.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let mut top: Vec<_> = by.into_iter().collect();
        top.sort_by_key(|(_, (t, x))| std::cmp::Reverse(t + x));
        let top: Vec<String> = top
            .iter()
            .take(4)
            .map(|(ns, (t, x))| format!("{ns} {}+{} MB", t >> 20, x >> 20))
            .collect();
        format!(
            "tables {} MB, index tail {} MB, vis states {}, plans {} (largest: {})",
            tables >> 20,
            tail >> 20,
            self.vis.lock().unwrap().len(),
            self.plans.lock().unwrap().len(),
            top.join(", ")
        )
    }

    /// The bytes of decoded table rows each namespace holds in the cache (the system tables aside).
    fn cached_bytes(&self) -> HashMap<String, usize> {
        let entries = self.cache.entries.lock().unwrap();
        let mut by: HashMap<String, usize> = HashMap::new();
        for (key, e) in entries.iter() {
            let Some((ns, _)) = key.rsplit_once('.') else {
                continue;
            };
            if ns == SYSTEM_NS {
                continue;
            }
            // `snap.rows` shares these batches' buffers: count the files only.
            let bytes: usize = e
                .files
                .values()
                .flat_map(|b| b.iter())
                .map(|b| b.get_array_memory_size())
                .sum();
            *by.entry(ns.to_string()).or_default() += bytes;
        }
        by
    }

    /// Keep the cache within its budget ([`cache_budget`]): drop whole namespaces, least recently read first,
    /// never `keep` (the one being read), so a namespace bigger than the budget is served and logged, not
    /// thrown out and loaded again on every read.
    fn trim(&self, keep: &str) {
        let budget = cache_budget();
        let by = self.cached_bytes();
        let mut total: usize = by.values().sum();
        crate::metrics::CACHE_BYTES.set(total as i64);
        if budget == 0 || total <= budget {
            return;
        }
        let used = self.cache.used.lock().unwrap().clone();
        let mut victims: Vec<(&String, &usize)> = by.iter().filter(|(ns, _)| *ns != keep).collect();
        victims.sort_by_key(|(ns, _)| used.get(*ns).copied().unwrap_or(0));
        for (ns, bytes) in victims {
            if total <= budget {
                break;
            }
            self.evict(ns);
            total -= bytes;
            eprintln!(
                "memstore: cache over its budget ({} MB): dropped {ns} ({} MB) until it is read again",
                budget >> 20,
                bytes >> 20
            );
        }
        if total > budget {
            eprintln!(
                "memstore: cache over its budget ({} MB) with only {keep} left in it ({} MB)",
                budget >> 20,
                total >> 20
            );
        }
        crate::metrics::CACHE_BYTES.set(total as i64);
    }

    /// Forget everything cached for `ns`: its tables' rows, its visibility states (and the views on them), its
    /// fact plans and decoded files. The next read loads it again.
    fn evict(&self, ns: &str) {
        let prefix = format!("{ns}.");
        self.cache
            .entries
            .lock()
            .unwrap()
            .retain(|k, _| !k.starts_with(&prefix));
        self.vis.lock().unwrap().retain(|(n, _), _| n != ns);
        self.plans.lock().unwrap().remove(ns);
        // Bumped, never removed: a plan load that began before this must not be cached after it.
        *self
            .plan_generation
            .lock()
            .unwrap()
            .entry(ns.to_string())
            .or_default() += 1;
        self.tail.lock().unwrap().remove(ns);
        self.no_side_table
            .lock()
            .unwrap()
            .retain(|k, _| !k.starts_with(&prefix));
        self.cache.used.lock().unwrap().remove(ns);
    }

    /// A table's cached rows, if fresh enough to serve without blocking; past the staleness window
    /// this also starts a background refresh.
    fn cached_rows(&self, ns: &str, name: &str, key: &str) -> Option<Snapshot> {
        let p = self.cache.entries.lock().unwrap().get(key).cloned()?;
        let at = p.fetched?;
        if at.elapsed() >= self.hard_staleness {
            return None;
        }
        if at.elapsed() >= self.max_staleness
            && self
                .cache
                .refreshing
                .lock()
                .unwrap()
                .insert(key.to_string())
        {
            let (catalog, cache) = (self.catalog(), self.cache.clone());
            let (ns, name) = (ns.to_string(), name.to_string());
            tokio::spawn(async move {
                // One load of a table at a time: a reader that finds the cache past the hard limit meanwhile
                // waits for this one rather than reading the whole table again beside it.
                let loading = cache.loading(&format!("{ns}.{name}"));
                let _turn = loading.lock().await;
                if let Err(e) = refresh(&*catalog, &cache, &ns, &name).await {
                    eprintln!("memstore: background refresh of {ns}.{name} failed: {e:#}");
                }
                cache
                    .refreshing
                    .lock()
                    .unwrap()
                    .remove(&format!("{ns}.{name}"));
            });
        }
        Some(p.snap)
    }

    pub fn max_staleness(&self) -> Duration {
        self.max_staleness
    }

    /// How old a cached read may get before a reader waits for a fresh one (`MEMSTORE_HARD_STALENESS_MS`).
    pub fn hard_staleness(&self) -> Duration {
        self.hard_staleness
    }

    pub fn invalidate(&self, ns: &str, name: &str) {
        if name == FACTS {
            *self
                .plan_generation
                .lock()
                .unwrap()
                .entry(ns.into())
                .or_default() += 1;
            self.plans.lock().unwrap().remove(ns);
        }
        let key = format!("{ns}.{name}");
        // Bumped whether or not the table is cached: a load under way (of a table not cached, or dropped by the
        // budget since) compares against it.
        let generation = {
            let mut g = self.cache.generations.lock().unwrap();
            let n = g.entry(key.clone()).or_default();
            *n += 1;
            *n
        };
        if let Some(e) = self.cache.entries.lock().unwrap().get_mut(&key) {
            e.fetched = None;
            e.generation = generation;
        }
    }

    /// A side table just created here: no longer taken for missing ([`Store::side_rows`]).
    pub(crate) fn forget_missing(&self, ns: &str, name: &str) {
        self.no_side_table
            .lock()
            .unwrap()
            .remove(&format!("{ns}.{name}"));
    }

    /// Create the system ledger if missing: every cell's, whatever scopes it holds.
    pub async fn ensure_system(&self) -> Result<()> {
        self.ensure_table(SYSTEM_NS, LEDGER, ledger_schema()?)
            .await?;
        self.ensure_ledger_cas().await
    }

    /// Create the system ledger and the scope's tables if missing.
    ///
    /// The side tables are checked all at once, each retried on a throttled or failed catalog call: a server's
    /// first request for a scope waited on a dozen of them in turn (7 s on BigLake), and every pod restarting at
    /// once spent the catalog's per-minute read quota on them. Facts stays last.
    pub async fn ensure_scope(&self, scope: &Scope) -> Result<()> {
        let ns = scope.namespace();
        with_retries(|| self.ensure_system()).await?;
        let tables = [
            (EPISODES, episodes_schema()?),
            (QUOTES, quotes_schema()?),
            (SCHEMAS, schemas_schema()?),
            (CLOSURES, closures_schema()?),
            (EXTRACTIONS, extractions_schema()?),
            (RETRACTIONS, retractions_schema()?),
            (RETENTION, retention_schema()?),
            (AGGREGATES, aggregates_schema()?),
            (AGG_REQUESTS, agg_requests_schema()?),
            (QA_LOG, qa_log_schema()?),
            (FEEDBACK, feedback_schema()?),
            (governance::TABLE, governance::table_schema()?),
        ];
        // Each boxed: a dozen table checks held inline overflowed a debug build's stack.
        futures::future::try_join_all(tables.iter().map(|(name, schema)| {
            let ns = &ns;
            Box::pin(with_retries(move || {
                Box::pin(self.ensure_table(ns, name, schema.clone()))
            }))
        }))
        .await?;
        {
            let mut absent = self.no_side_table.lock().unwrap();
            absent.remove(&format!("{ns}.{SCHEMAS}"));
            absent.remove(&format!("{ns}.{CLOSURES}"));
            absent.remove(&format!("{ns}.{EXTRACTIONS}"));
            absent.remove(&format!("{ns}.{RETRACTIONS}"));
            absent.remove(&format!("{ns}.{RETENTION}"));
            absent.remove(&format!("{ns}.{AGGREGATES}"));
            absent.remove(&format!("{ns}.{AGG_REQUESTS}"));
            absent.remove(&format!("{ns}.{QA_LOG}"));
            absent.remove(&format!("{ns}.{FEEDBACK}"));
            absent.remove(&format!("{ns}.{}", governance::TABLE));
        }
        let facts = facts_schema()?;
        with_retries(|| Box::pin(self.ensure_table(&ns, FACTS, facts.clone()))).await
    }

    /// Commit `schema` as the scope's schema on `branch` (main, or a branch to try it on). Facts
    /// already written keep the typed columns they were written with; later writes are checked
    /// against the new schema. So on main a change that would read those facts differently
    /// (`Schema::breaking_changes`) needs a higher version and `allow_breaking`; a branch takes
    /// any change, which is where a breaking one is tried before merging. Returns the commit id.
    pub async fn set_schema(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        schema: &schema::Schema,
        allow_breaking: bool,
    ) -> Result<String> {
        not_held(branch)?;
        schema.check()?;
        self.ensure_scope(scope).await?;
        self.check_schema(scope, branch, schema, allow_breaking)
            .await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, SCHEMAS).await?;
        let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", s(&id)),
                ("name", s(&schema.name)),
                ("version", s(&schema.version.to_string())),
                ("body", s(&serde_json::to_string(schema)?)),
                ("recorded_at", ts([Some(now_micros())])),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, SCHEMAS);
        let branch_name = branch.to_string();
        self.append_ledger(&id, scope, branch, actor, &move |l: &Ledger| {
            Ok(l.heads(&branch_name))
        })
        .await?;
        // Rebuild the cached visibility now, so this store's next write is checked against it.
        self.visibility(scope, branch).await?;
        Ok(id)
    }

    /// Every check [`Store::set_schema`] makes before it commits, committing nothing (a dry run): the
    /// schema is valid, the branch exists, the version doesn't go backwards, and on main a breaking change
    /// has a higher version and `allow_breaking`. Returns the scope's schema on `branch` now, if it has one.
    pub async fn check_schema(
        &self,
        scope: &Scope,
        branch: &str,
        schema: &schema::Schema,
        allow_breaking: bool,
    ) -> Result<Option<Arc<schema::Schema>>> {
        schema.check()?;
        let ns = scope.namespace();
        // A scope nobody has written to has no tables yet: no schema, no branch. Nothing is created here.
        let exists = self.has_episodes(scope).await?;
        if branch != MAIN {
            anyhow::ensure!(
                exists && self.ledger(scope).await?.base(branch).is_some(),
                "no branch {branch:?} in {ns}"
            );
        }
        let old = match exists {
            true => self.schema(scope, branch).await?,
            false => None,
        };
        if let Some(old) = &old {
            anyhow::ensure!(
                schema.version >= old.version,
                "{ns} on {branch} is at schema {} v{}; v{} would go backwards",
                old.name,
                old.version,
                schema.version
            );
            // Branches are where breaking changes are tried (and backfilled); main takes one only
            // with a higher version and an explicit go-ahead.
            let breaks = schema.breaking_changes(old);
            if branch == MAIN && !breaks.is_empty() {
                anyhow::ensure!(
                    allow_breaking && schema.version > old.version,
                    "breaking change to {ns} schema {} v{}: {}. Try it on a branch, or raise \
                     the version and allow breaking changes",
                    old.name,
                    old.version,
                    breaks.join("; ")
                );
            }
        }
        Ok(old)
    }

    /// End facts that stopped being true with no successor to supersede them (a champion left, a
    /// role was removed). Each closure says when the fact ended and cites the episode that shows
    /// it; the episodes and closure rows are committed together on `branch`. A closure applies
    /// only to readers who can see its episode: for anyone else the fact stays as it was. The
    /// fact keeps its row; reads use the earlier of its stored `valid_to` and the closure's.
    /// Returns the commit id.
    pub async fn close_facts(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        closures: &[(&str, i64, &Episode)],
    ) -> Result<String> {
        not_held(branch)?;
        let id = self.write_closures(scope, branch, closures).await?;
        let branch_name = branch.to_string();
        self.append_ledger(&id, scope, branch, actor, &move |l: &Ledger| {
            anyhow::ensure!(
                branch_name == MAIN || l.base(&branch_name).is_some(),
                "no branch {branch_name:?}"
            );
            Ok(l.heads(&branch_name))
        })
        .await?;
        self.visibility(scope, branch).await?;
        Ok(id)
    }

    /// Step 1 of [`Store::close_facts`] on main: the episodes and closure rows under a commit id
    /// with no ledger row, so none of it is visible yet.
    pub async fn write_pending_closures(
        &self,
        scope: &Scope,
        closures: &[(&str, i64, &Episode)],
    ) -> Result<String> {
        self.write_closures(scope, MAIN, closures).await
    }

    async fn write_closures(
        &self,
        scope: &Scope,
        branch: &str,
        closures: &[(&str, i64, &Episode)],
    ) -> Result<String> {
        anyhow::ensure!(!closures.is_empty(), "no closures to write");
        let mut episodes: Vec<Episode> = vec![];
        for (_, _, e) in closures {
            if !episodes.iter().any(|x| x.episode_id == e.episode_id) {
                episodes.push((*e).clone());
            }
        }
        validate_write(&episodes, &[])?;
        episode_types::check_stored(&episodes)?;
        self.ensure_scope(scope).await?;
        self.check_about(scope, branch, &episodes).await?;
        let ns = scope.namespace();
        let ends: Vec<(&str, i64)> = closures.iter().map(|(f, t, _)| (*f, *t)).collect();
        self.check_closures(scope, branch, &ends, &[]).await?;
        let commit_id = uuid::Uuid::new_v4().to_string();
        let eps: Vec<(&str, &Episode)> = episodes.iter().map(|e| (commit_id.as_str(), e)).collect();
        self.write_episodes(&ns, &eps).await?;
        let rows: Vec<Closure> = closures
            .iter()
            .map(|(f, t, e)| Closure {
                fact_id: f.to_string(),
                valid_to: *t,
                episode_id: e.episode_id.clone(),
            })
            .collect();
        let rows: Vec<(&str, &Closure)> = rows.iter().map(|c| (commit_id.as_str(), c)).collect();
        self.write_closure_rows(&ns, &rows).await?;
        Ok(commit_id)
    }

    /// Each closure must end a fact the branch has, after it began, and cite an episode the branch has
    /// or one committed with it.
    async fn check_closures(
        &self,
        scope: &Scope,
        branch: &str,
        ends: &[(&str, i64)],
        cited: &[(&str, &[Episode])],
    ) -> Result<()> {
        if ends.is_empty() {
            return Ok(());
        }
        let ns = scope.namespace();
        self.invalidate(&ns, FACTS);
        let (vis, facts) = tokio::try_join!(self.visibility(scope, branch), self.rows(&ns, FACTS))?;
        for (ep, with) in cited {
            anyhow::ensure!(
                vis.acls.contains_key(*ep) || with.iter().any(|e| e.episode_id == *ep),
                "closure cites episode {ep}, which {ns} doesn't have"
            );
        }
        let wanted: HashMap<&str, i64> = ends.iter().copied().collect();
        let mut began: HashMap<String, i64> = HashMap::new();
        for b in facts.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                if wanted.contains_key(f.fact_id.as_str()) && vis.committed.contains(&commit) {
                    began.insert(f.fact_id, f.valid_from);
                }
            }
        }
        for (fact, end) in &wanted {
            let from = began
                .get(*fact)
                .with_context(|| format!("no committed fact {fact} in {ns} to close"))?;
            anyhow::ensure!(
                end > from,
                "closure of {fact} must end after the fact began ({end} <= {from})"
            );
        }
        Ok(())
    }

    async fn write_closure_rows(&self, ns: &str, rows: &[(&str, &Closure)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let table = self.write_target(ns, CLOSURES).await?;
        let strs = |v: Vec<&str>| -> ArrayRef { Arc::new(StringArray::from(v)) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", strs(rows.iter().map(|r| r.0).collect())),
                (
                    "fact_id",
                    strs(rows.iter().map(|r| r.1.fact_id.as_str()).collect()),
                ),
                ("valid_to", ts(rows.iter().map(|r| Some(r.1.valid_to)))),
                (
                    "episode_id",
                    strs(rows.iter().map(|r| r.1.episode_id.as_str()).collect()),
                ),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now_micros()), rows.len())),
                ),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(ns, CLOSURES);
        Ok(())
    }

    /// The scope's schema on `branch`, if it has one.
    pub async fn schema(&self, scope: &Scope, branch: &str) -> Result<Option<Arc<schema::Schema>>> {
        Ok(self.visibility(scope, branch).await?.schema.clone())
    }

    /// Every schema committed on `branch`, oldest first: the last is [`Store::schema`].
    pub async fn schema_history(
        &self,
        scope: &Scope,
        branch: &str,
    ) -> Result<Vec<Arc<schema::Schema>>> {
        Ok(self.visibility(scope, branch).await?.schemas.clone())
    }

    /// The schema to check a write against: the cached visibility's when there is one (schemas
    /// change rarely, and `set_schema` refreshes this store's cache), else a fresh read.
    async fn write_schema(
        &self,
        scope: &Scope,
        branch: &str,
    ) -> Result<Option<Arc<schema::Schema>>> {
        let cached = self
            .vis
            .lock()
            .unwrap()
            .get(&(scope.namespace(), branch.to_string()))
            .map(|(_, v)| v.schema.clone());
        match cached {
            Some(s) => Ok(s),
            None => self.schema(scope, branch).await,
        }
    }

    /// In a typed scope, every id an episode is about must name one of the schema's types. One that doesn't
    /// refuses the write as [`AboutRefused`], naming the episode by its place in `episodes`.
    async fn check_about(&self, scope: &Scope, branch: &str, episodes: &[Episode]) -> Result<()> {
        if episodes.iter().all(|e| e.about.is_empty()) {
            return Ok(());
        }
        if let Some(schema) = self.write_schema(scope, branch).await? {
            for (i, e) in episodes.iter().enumerate() {
                for id in &e.about {
                    if schema.entity_type(id).is_none() {
                        return Err(AboutRefused(format!(
                            "episodes[{i}]: episode {} is about {id:?}, which names no type of schema {}",
                            e.episode_id, schema.name
                        ))
                        .into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Check facts against the branch's schema and fill their typed columns; unchanged when the
    /// scope has no schema. `adds`, when given, is what an extraction offered beyond the scope's schema (an
    /// episode type's catalog): laid over the branch's schema as it is now, additively, so the
    /// stored schema keeps the last word: an addition can't change or retype what it declares.
    async fn typed(
        &self,
        scope: &Scope,
        branch: &str,
        facts: &[Fact],
        adds: Option<&schema::Schema>,
    ) -> Result<Vec<Fact>> {
        let mut out = facts.to_vec();
        let stored = self.write_schema(scope, branch).await?;
        let laid;
        let schema = match adds {
            None => stored.as_deref(),
            Some(a) => {
                laid = schema::overlay(stored.as_deref(), a)
                    .context("what the extraction offered, on the scope's schema")?;
                Some(&laid)
            }
        };
        if let Some(schema) = schema {
            for f in &mut out {
                schema.normalize(f)?;
            }
        }
        Ok(out)
    }

    /// The ledger append is a compare-and-set on the ledger table's snapshot. iceberg-rust's
    /// own commit retry would re-apply a stale append (and its stale parents) onto a newer
    /// snapshot, forking the ledger, so it is turned off for this table; `append_ledger`
    /// retries itself with freshly read heads.
    async fn ensure_ledger_cas(&self) -> Result<()> {
        let table = self.table(SYSTEM_NS, LEDGER).await?;
        let key = TableProperties::PROPERTY_COMMIT_NUM_RETRIES;
        if table.metadata().properties().get(key).map(String::as_str) != Some("0") {
            let tx = Transaction::new(&table);
            tx.update_table_properties()
                .set(key.into(), "0".into())
                .apply(tx)?
                .commit(&*self.catalog())
                .await
                .context("turn off automatic commit retry on the ledger")?;
            self.invalidate(SYSTEM_NS, LEDGER);
        }
        Ok(())
    }

    async fn ensure_table(&self, ns: &str, name: &str, schema: Schema) -> Result<()> {
        let ns_ident = NamespaceIdent::new(ns.into());
        // A table that exists with every column (all but a scope's first request ever, or a new engine's): one
        // catalog call. Anything else goes the long way below, which creates, waits out a rewrite, and evolves.
        let ident = TableIdent::new(ns_ident.clone(), name.into());
        match Box::pin(self.catalog().load_table(&ident)).await {
            Ok(t) => {
                let current = t.metadata().current_schema();
                if schema
                    .as_struct()
                    .fields()
                    .iter()
                    .all(|f| current.field_by_name(&f.name).is_some())
                {
                    return Ok(());
                }
            }
            Err(e) => {
                let e = anyhow::Error::from(e);
                if is_transient(&e) {
                    return Err(e).with_context(|| format!("load {ns}.{name}"));
                }
            }
        }
        // Two processes starting at once both see "missing" and both create: losing that race is
        // fine as long as the object exists afterwards.
        if !self.catalog().namespace_exists(&ns_ident).await?
            && let Err(e) = self
                .catalog()
                .create_namespace(&ns_ident, HashMap::new())
                .await
            && !self.catalog().namespace_exists(&ns_ident).await?
        {
            return Err(e).with_context(|| format!("create namespace {ns}"));
        }
        // A table renamed aside by a rewrite (`offboard`) is missing for a moment: wait for the rewritten
        // one rather than make an empty table in its place.
        let waited = Instant::now();
        while !self.catalog().table_exists(&ident).await?
            && is_aside(&*self.catalog(), &ns_ident, name).await
        {
            anyhow::ensure!(
                waited.elapsed() < REWRITE_WAIT,
                "{ns}.{name} has been renamed aside by a rewrite for {}s",
                REWRITE_WAIT.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !self.catalog().table_exists(&ident).await? {
            let creation = TableCreation::builder()
                .name(name.into())
                .schema(schema.clone())
                // Snowflake Horizon accepts external writes on v2 tables only.
                .format_version(FormatVersion::V2)
                .build();
            match self.catalog().create_table(&ns_ident, creation).await {
                Ok(_) => return Ok(()),
                // created concurrently: fall through to the schema check on what is there
                Err(_) if self.catalog().table_exists(&ident).await? => {}
                Err(e) => return Err(e).with_context(|| format!("create table {ns}.{name}")),
            }
        }
        // Additive evolution: add any optional column the engine knows and the table lacks. Two
        // processes on a newer engine both see the column missing and both add it: the loser's commit
        // fails, and it reads the table again, where the column now is.
        let mut evolved = false;
        for attempt in 0.. {
            let table = self.catalog().load_table(&ident).await?;
            let current = table.metadata().current_schema().clone();
            let missing: Vec<_> = schema
                .as_struct()
                .fields()
                .iter()
                .filter(|f| current.field_by_name(&f.name).is_none())
                .collect();
            if missing.is_empty() {
                break;
            }
            evolved = true;
            let tx = Transaction::new(&table);
            let mut action = tx.update_schema();
            for f in missing {
                anyhow::ensure!(
                    !f.required,
                    "cannot add required column {ns}.{name}.{}",
                    f.name
                );
                action = action.add_column(AddColumn::optional(&f.name, (*f.field_type).clone()));
            }
            match action.apply(tx)?.commit(&*self.catalog()).await {
                Ok(_) => break,
                Err(_) if attempt < 3 => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("evolve schema of {ns}.{name}"));
                }
            }
        }
        if evolved {
            self.invalidate(ns, name);
        }
        Ok(())
    }

    /// Write episodes and facts on `main`, then append the ledger row. Returns the commit id.
    pub async fn commit(
        &self,
        scope: &Scope,
        actor: &str,
        episodes: &[Episode],
        facts: &[Fact],
    ) -> Result<String> {
        self.commit_on(scope, MAIN, actor, episodes, facts).await
    }

    /// Same as [`Store::commit`], on a named branch. Every episode must be of an episode type
    /// ([`episode_types::check`]), but for the engine's own documents: nothing untyped is committed.
    pub async fn commit_on(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        episodes: &[Episode],
        facts: &[Fact],
    ) -> Result<String> {
        episode_types::check_stored(episodes)?;
        not_held(branch)?;
        self.commit_full(scope, branch, actor, episodes, facts, &[], &[], None)
            .await
    }

    /// Commit the facts an extractor read from committed episodes, with one [`Extraction`] row per episode
    /// it read (also those it found nothing in), on main. Both land in one commit or neither does.
    /// Commit what a compile wrote (skill, AIP and index documents) with the record of the episodes it read
    /// (`extractions` rows under the compiler's id), on `branch`, in one commit.
    pub async fn commit_compiled(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        documents: &[Episode],
        read: &[Extraction],
    ) -> Result<String> {
        anyhow::ensure!(!read.is_empty(), "nothing compiled");
        not_held(branch)?;
        self.commit_full(scope, branch, actor, documents, &[], read, &[], None)
            .await
    }

    /// Commit what the engine derived from what the scope holds, on main, in one commit: its own records
    /// (`episode_types::ENGINE_SOURCES` documents), facts that cite them or what they rest on, closures citing
    /// them, and the record of what it read (`extractions` rows under its id). A dream's pass, say.
    pub async fn commit_derived(
        &self,
        scope: &Scope,
        actor: &str,
        records: &[Episode],
        facts: &[Fact],
        read: &[Extraction],
        closures: &[Closure],
    ) -> Result<String> {
        anyhow::ensure!(
            records
                .iter()
                .all(|e| episode_types::engine_source(&e.source)),
            "only the engine's own documents are committed as what it derived"
        );
        anyhow::ensure!(
            !(records.is_empty() && facts.is_empty() && read.is_empty() && closures.is_empty()),
            "nothing derived"
        );
        self.commit_full(scope, MAIN, actor, records, facts, read, closures, None)
            .await
    }

    /// `adds`: what the extraction offered beyond the scope's schema (an episode type's catalog).
    /// The commit checks the facts against the scope's schema as it is now with that laid over it
    /// additively, as the read checked them, so a fact the read kept on a catalog predicate isn't refused here;
    /// the scope's schema still has the last word. None: the scope's schema alone.
    pub async fn commit_extracted(
        &self,
        scope: &Scope,
        actor: &str,
        facts: &[Fact],
        extractions: &[Extraction],
        closures: &[Closure],
        adds: Option<&schema::Schema>,
    ) -> Result<String> {
        self.commit_extracted_on(scope, MAIN, actor, facts, extractions, closures, adds)
            .await
    }

    /// [`Store::commit_extracted`] with the engine's own records of what reconcile found among the facts
    /// ([`conflicts`]), in the same commit. Where what extraction read waits for its owners, the records follow it
    /// in a commit of their own: a record names facts, and one whose facts aren't on main says nothing to anyone.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_reconciled(
        &self,
        scope: &Scope,
        actor: &str,
        facts: &[Fact],
        extractions: &[Extraction],
        closures: &[Closure],
        records: &[Episode],
        adds: Option<&schema::Schema>,
    ) -> Result<String> {
        if records.is_empty() {
            return self
                .commit_extracted(scope, actor, facts, extractions, closures, adds)
                .await;
        }
        anyhow::ensure!(!extractions.is_empty(), "nothing extracted");
        anyhow::ensure!(
            records
                .iter()
                .all(|e| episode_types::engine_source(&e.source)),
            "only the engine's own documents are committed as what reconcile found"
        );
        match self
            .commit_full(
                scope,
                MAIN,
                actor,
                records,
                facts,
                extractions,
                closures,
                adds,
            )
            .await
        {
            Err(e) if governance::OwnerApprovalRequired::caused(&e) => {
                let id = Box::pin(self.stage_extracted(
                    scope,
                    actor,
                    facts,
                    extractions,
                    closures,
                    adds,
                ))
                .await?;
                self.commit_full(scope, MAIN, actor, records, &[], &[], &[], None)
                    .await?;
                Ok(id)
            }
            r => r,
        }
    }

    /// [`Store::commit_extracted`] on `branch`: a schema set on a workspace branch has its facts there.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_extracted_on(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        facts: &[Fact],
        extractions: &[Extraction],
        closures: &[Closure],
        adds: Option<&schema::Schema>,
    ) -> Result<String> {
        anyhow::ensure!(!extractions.is_empty(), "nothing extracted");
        match self
            .commit_full(
                scope,
                branch,
                actor,
                &[],
                facts,
                extractions,
                closures,
                adds,
            )
            .await
        {
            // What it read about owned entities waits for their owners; the rest lands.
            Err(e) if branch == MAIN && governance::OwnerApprovalRequired::caused(&e) => {
                Box::pin(self.stage_extracted(scope, actor, facts, extractions, closures, adds))
                    .await
            }
            r => r,
        }
    }

    /// The episodes `extractor` has already read on main, or given up on.
    pub async fn extracted(&self, scope: &Scope, extractor: &str) -> Result<HashSet<String>> {
        self.extracted_on(scope, MAIN, extractor).await
    }

    /// [`Store::extracted`] as `branch` sees it: the reads its commits record.
    pub async fn extracted_on(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
    ) -> Result<HashSet<String>> {
        self.reads(scope, branch, extractor, false).await
    }

    /// The episodes `extractor` has read on `branch`, or given up on. With `since_revival`, an episode a newer
    /// version superseded and that was written again since counts only by what was read of it after it came
    /// back: a reader whose output the newer version overwrote (the compiler's library) reads it again.
    async fn reads(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
        since_revival: bool,
    ) -> Result<HashSet<String>> {
        let ns = scope.namespace();
        let (vis, rows) = tokio::try_join!(
            self.visibility(scope, branch),
            self.side_rows(&ns, EXTRACTIONS)
        )?;
        let ledger = match since_revival && !vis.revived.is_empty() {
            true => Some(Ledger::from_batches(
                &self.rows(SYSTEM_NS, LEDGER).await?.rows,
                &ns,
            )),
            false => None,
        };
        let times = ledger.as_ref().map(Ledger::times);
        let mut done = HashSet::new();
        let mut failed: HashMap<String, usize> = HashMap::new();
        for b in rows.iter() {
            let (commit, ep, by, facts) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "extractor"),
                longs(b, "facts"),
            );
            for i in 0..b.num_rows() {
                if by[i] != extractor || !vis.committed.contains(&commit[i]) {
                    continue;
                }
                if let Some(times) = &times
                    && let Some(back) = vis.revived.get(&ep[i])
                    && times.get(commit[i].as_str()).is_none_or(|t| t < back)
                {
                    continue;
                }
                if facts[i] == Some(FAILED_TRY) {
                    *failed.entry(ep[i].clone()).or_default() += 1;
                } else {
                    done.insert(ep[i].clone());
                }
            }
        }
        done.extend(
            failed
                .into_iter()
                .filter(|(_, n)| *n >= EXTRACT_TRIES)
                .map(|(ep, _)| ep),
        );
        Ok(done)
    }

    /// Committed episodes on main that `extractor` hasn't read, oldest first, at most `limit`, with their
    /// bodies, and apart the ids of those whose body couldn't be read. For the engine's own extraction,
    /// which reads every episode whatever its ACL; the facts it writes cite them, so readers still see only
    /// what the episodes allow.
    pub async fn to_extract(
        &self,
        scope: &Scope,
        extractor: &str,
        limit: usize,
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        self.to_extract_on(scope, MAIN, extractor, limit).await
    }

    /// [`Store::to_extract`] on `branch`. Each episode carries its document key, if it is a version of one.
    pub async fn to_extract_on(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
        limit: usize,
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        self.to_extract_where_on(scope, branch, extractor, limit, &|_| true)
            .await
    }

    /// [`Store::to_extract_on`] for a reader whose output a record's newer version overwrites (the compiler's
    /// library, which the newest version it read updated): a version a newer one superseded and that was written
    /// again since reads as unread, as a new episode does, so what it says is read back in. Facts need no such
    /// read: the ones an episode supports come back with it.
    pub async fn to_reread_on(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
        limit: usize,
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        let done = self.reads(scope, branch, extractor, true).await?;
        self.unread(scope, branch, &done, limit, &|_| true, false)
            .await
    }

    /// [`Store::to_extract`], of the episodes `keep` takes (judged on their source, time and subjects,
    /// before any body is read): each extraction path picks its own episodes before the limit applies, so
    /// one path never starves on episodes another owns.
    pub async fn to_extract_where(
        &self,
        scope: &Scope,
        extractor: &str,
        limit: usize,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        self.to_extract_where_on(scope, MAIN, extractor, limit, keep)
            .await
    }

    /// [`Store::to_extract_where`] on `branch`.
    pub async fn to_extract_where_on(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
        limit: usize,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        let done = self.extracted_on(scope, branch, extractor).await?;
        self.unread(scope, branch, &done, limit, keep, false).await
    }

    /// The newest `limit` episodes on main that `keep` takes, with their bodies, whether or not anything has
    /// read them.
    pub async fn newest_where(
        &self,
        scope: &Scope,
        limit: usize,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
    ) -> Result<Vec<Episode>> {
        // Nothing counts as read.
        Ok(self
            .unread(scope, MAIN, &HashSet::new(), limit, keep, true)
            .await?
            .0)
    }

    /// [`Store::to_extract_where`], newest first: a backfill reads recent history before old.
    pub async fn to_backfill(
        &self,
        scope: &Scope,
        extractor: &str,
        limit: usize,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        let done = self.extracted_on(scope, MAIN, extractor).await?;
        self.unread(scope, MAIN, &done, limit, keep, true).await
    }

    async fn unread(
        &self,
        scope: &Scope,
        branch: &str,
        done: &HashSet<String>,
        limit: usize,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
        newest_first: bool,
    ) -> Result<(Vec<Episode>, Vec<String>)> {
        let (heads, snap) = self.episode_heads(scope, branch).await?;
        let mut todo: Vec<Episode> = heads
            .into_iter()
            .filter(|e| !done.contains(&e.episode_id) && keep(e))
            .collect();
        todo.sort_by_key(|e| e.occurred_at);
        if newest_first {
            todo.reverse();
        }
        todo.truncate(limit);
        let table = snap.table.clone();
        let reads = todo.into_iter().map(|e| {
            let (io, path) = (table.file_io().clone(), body_path(&table, &e.episode_id));
            async move {
                let body =
                    with_retries(|| async { Ok(io.new_input(path.clone())?.read().await?) }).await;
                match body {
                    Ok(b) => Ok(Episode {
                        body: b.to_vec(),
                        ..e
                    }),
                    Err(err) => {
                        eprintln!("episode {}: body unreadable: {err:#}", e.episode_id);
                        Err(e.episode_id)
                    }
                }
            }
        });
        // One unreadable body mustn't hold up the rest: it comes back apart, as a failed try.
        let (mut read, mut unreadable) = (vec![], vec![]);
        let mut results = futures::stream::iter(reads).buffer_unordered(32);
        while let Some(r) = results.next().await {
            match r {
                Ok(e) => read.push(e),
                Err(id) => unreadable.push(id),
            }
        }
        // Bodies come back in whatever order they were read: episodes of the same time keep the id order they were
        // picked in, so a run over the same episodes reads them in the same order (and asks a model the same).
        read.sort_by(|a: &Episode, b: &Episode| {
            (a.occurred_at, &a.episode_id).cmp(&(b.occurred_at, &b.episode_id))
        });
        if newest_first {
            read.reverse();
        }
        unreadable.sort();
        Ok((read, unreadable))
    }

    /// Every committed, unretracted episode of the scope on `branch`, once each, without its body; and the
    /// episodes table it was read from (for the bodies).
    async fn episode_heads(&self, scope: &Scope, branch: &str) -> Result<(Vec<Episode>, Snapshot)> {
        let ns = scope.namespace();
        let vis = self.visibility(scope, branch).await?;
        let snap = self.rows(&ns, EPISODES).await?;
        let mut todo: BTreeMap<String, Episode> = BTreeMap::new();
        for b in snap.rows.iter() {
            let (commit, id, source) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            let at = timestamps(b, "occurred_at");
            let about = optional_strings(b, "about");
            let docs = optional_strings(b, "document");
            for i in 0..b.num_rows() {
                if !vis.committed.contains(&commit[i]) || vis.is_retracted(&id[i]) {
                    continue;
                }
                let acl = vis.acls.get(&id[i]).cloned().unwrap_or_default();
                todo.entry(id[i].clone()).or_insert_with(|| Episode {
                    episode_id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: at[i].unwrap_or_default(),
                    acl,
                    about: about[i].as_deref().map(split_list).unwrap_or_default(),
                    index_text: true,
                    body: vec![],
                    recorded_at: None,
                    document: docs[i].clone(),
                });
            }
        }
        Ok((todo.into_values().collect(), snap))
    }

    /// How far `extractor` has read the episodes `keep` takes on `branch`: how many it has read, and how many
    /// are still to read. Counted from the episodes' heads alone, no body is read.
    pub async fn extraction_progress_on(
        &self,
        scope: &Scope,
        branch: &str,
        extractor: &str,
        keep: &(dyn Fn(&Episode) -> bool + Send + Sync),
    ) -> Result<(usize, usize)> {
        let done = self.extracted_on(scope, branch, extractor).await?;
        let (heads, _) = self.episode_heads(scope, branch).await?;
        Ok(heads
            .iter()
            .filter(|e| keep(e))
            .fold((0, 0), |(read, left), e| {
                if done.contains(&e.episode_id) {
                    (read + 1, left)
                } else {
                    (read, left + 1)
                }
            }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_full(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        episodes: &[Episode],
        facts: &[Fact],
        extractions: &[Extraction],
        closures: &[Closure],
        adds: Option<&schema::Schema>,
    ) -> Result<String> {
        self.check_fence()?;
        validate_write(episodes, facts)?;
        let capped = workspace::reserve(self, episodes.len()).await?;
        let _workspace = workspace::check_write(self, scope, episodes.len(), facts.len()).await?;
        let ends: Vec<(&str, i64)> = closures
            .iter()
            .map(|c| (c.fact_id.as_str(), c.valid_to))
            .collect();
        let cited: Vec<(&str, &[Episode])> = closures
            .iter()
            .map(|c| (c.episode_id.as_str(), episodes))
            .collect();
        self.check_closures(scope, branch, &ends, &cited).await?;
        let facts = &self.typed(scope, branch, facts, adds).await?;
        self.check_about(scope, branch, episodes).await?;
        // Main always exists; only a named branch needs a ledger read to check.
        if branch != MAIN {
            let ledger = self.ledger(scope).await?;
            anyhow::ensure!(
                ledger.base(branch).is_some(),
                "no branch {branch:?} in {}",
                scope.namespace()
            );
        }
        // Group commit: concurrent commits on this store share one data append per table and
        // one ledger append. The first caller in leads, committing whatever is queued (its own
        // request included) until the queue is empty; everyone gets their own commit id.
        let (done, wait) = tokio::sync::oneshot::channel();
        let lead = {
            let mut g = self.group.lock().unwrap();
            g.queue.push(PendingCommit {
                scope: scope.clone(),
                branch: branch.to_string(),
                actor: actor.to_string(),
                episodes: episodes.to_vec(),
                facts: facts.to_vec(),
                extractions: extractions.to_vec(),
                closures: closures.to_vec(),
                done,
                capped,
                _workspace,
                fence: self.fence.clone(),
            });
            !std::mem::replace(&mut g.leading, true)
        };
        if lead {
            // In a task of its own: the commits it takes on must land even if this caller goes away. Unfenced:
            // it writes other callers' commits too, each checked against its own fence.
            let mut store = self.clone();
            store.fence = None;
            tokio::spawn(async move { store.lead_commits().await });
        }
        wait.await
            .map_err(|_| anyhow::anyhow!("the group-commit leader stopped"))?
            // The leader hands each commit its outcome as text (one failure can be several commits'); an owner's
            // refusal is typed again here, so a caller can tell it apart (`commit_extracted_on`).
            .map_err(
                |e| match governance::OwnerApprovalRequired::is_message(&e) {
                    true => governance::OwnerApprovalRequired.into(),
                    false => anyhow::anyhow!(e),
                },
            )
    }

    /// Commit whatever is queued, batch after batch, until the queue is empty.
    async fn lead_commits(&self) {
        let mut guard = LeaderGuard {
            group: self.group.clone(),
            armed: true,
        };
        loop {
            let batch = {
                let mut g = self.group.lock().unwrap();
                if g.queue.is_empty() {
                    g.leading = false;
                    // Stopped, not failed: a newer leader may already own the queue.
                    guard.armed = false;
                    break;
                }
                std::mem::take(&mut g.queue)
            };
            // A commit whose store may no longer write (its server lost the writer lease while it waited
            // its turn) is refused now, not written.
            let (batch, refused): (Vec<_>, Vec<_>) = batch
                .into_iter()
                .partition(|p| p.fence.as_ref().is_none_or(|f| f()));
            for p in refused {
                let _ = p.done.send(Err(Fenced.to_string()));
            }
            if batch.is_empty() {
                continue;
            }
            let ids: Vec<String> = batch
                .iter()
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect();
            let result = self
                .commit_batch(&batch, &ids)
                .await
                .map_err(|e| format!("{e:#}"));
            // An owner's refusal is one commit's, not its batch's: the others (another org's write, say) are
            // committed again one at a time, and only what the owner holds is refused. Each under a new id: the
            // refused attempt's rows are in the tables under the old ones, which no ledger row names.
            let mut ids = ids;
            let results: Vec<std::result::Result<(), String>> = match &result {
                Err(e) if batch.len() > 1 && governance::OwnerApprovalRequired::is_message(e) => {
                    let mut each = Vec::with_capacity(batch.len());
                    for (p, id) in batch.iter().zip(ids.iter_mut()) {
                        *id = uuid::Uuid::new_v4().to_string();
                        each.push(
                            self.commit_batch(std::slice::from_ref(p), std::slice::from_ref(id))
                                .await
                                .map_err(|e| format!("{e:#}")),
                        );
                    }
                    each
                }
                r => vec![r.clone(); batch.len()],
            };
            // A commit refused for its own content (it needs owner approval: extraction then holds it for the
            // owners) is the writer's refusal, not the store failing to commit: it isn't counted as a failure.
            let failed = results
                .iter()
                .filter(|r| {
                    r.as_ref()
                        .is_err_and(|e| !governance::OwnerApprovalRequired::is_message(e))
                })
                .count();
            if failed > 0 {
                crate::metrics::STORE_COMMIT_FAILURES.inc_by(failed as u64);
            }
            for ((mut p, id), result) in batch.into_iter().zip(ids).zip(results) {
                if let (Ok(()), Some(r)) = (&result, p.capped.take()) {
                    r.landed();
                }
                // Given back before the caller hears, so its next write isn't counted against the cap twice
                // (once in the catalog, once still held).
                drop(p._workspace.take());
                let _ = p.done.send(result.map(|_| id));
            }
        }
    }

    /// One batch of commits: rows grouped per scope into one append per table, then one ledger
    /// append whose rows are chained per (scope, branch).
    async fn commit_batch(&self, batch: &[PendingCommit], ids: &[String]) -> Result<()> {
        type Rows<'a> = (
            Vec<(&'a str, &'a Episode)>,
            Vec<(&'a str, &'a Fact)>,
            Vec<(&'a str, &'a Extraction)>,
            Vec<(&'a str, &'a Closure)>,
        );
        let mut by_ns: BTreeMap<String, Rows> = BTreeMap::new();
        for (p, id) in batch.iter().zip(ids) {
            let e = by_ns.entry(p.scope.namespace()).or_default();
            e.0.extend(p.episodes.iter().map(|x| (id.as_str(), x)));
            e.1.extend(p.facts.iter().map(|x| (id.as_str(), x)));
            e.2.extend(p.extractions.iter().map(|x| (id.as_str(), x)));
            e.3.extend(p.closures.iter().map(|x| (id.as_str(), x)));
        }
        futures::future::try_join_all(by_ns.iter().map(|(ns, (eps, fs, xs, cs))| async move {
            tokio::try_join!(
                self.write_episodes(ns, eps),
                self.write_facts(ns, fs),
                self.write_extractions(ns, xs),
                self.write_closure_rows(ns, cs)
            )
        }))
        .await?;
        let entries: Vec<(&str, String, &str, &str, &Scope)> = batch
            .iter()
            .zip(ids)
            .map(|(p, id)| {
                (
                    id.as_str(),
                    p.scope.namespace(),
                    p.branch.as_str(),
                    p.actor.as_str(),
                    &p.scope,
                )
            })
            .collect();
        self.append_ledger_rows(&entries).await
    }

    /// Append several commits' ledger rows in one compare-and-set. Within the batch, each
    /// (scope, branch)'s rows form a chain: the first names the current heads, each next one
    /// the previous row.
    async fn append_ledger_rows(
        &self,
        entries: &[(&str, String, &str, &str, &Scope)],
    ) -> Result<()> {
        let started = Instant::now();
        for attempt in 0u32.. {
            let snap = self.ledger_now().await?;
            let mut ledgers: HashMap<&str, Ledger> = HashMap::new();
            // One owner check per scope for all of the batch's commits to main: when none of them touches what an
            // owner holds (the usual case), that one check clears them all, not one check each inside this
            // compare-and-set. Otherwise each is checked on its own below, so only the commit an owner holds is
            // refused and an approved proposal still publishes.
            let mut cleared: HashSet<&str> = HashSet::new();
            let mut to_main: BTreeMap<&str, (&Scope, Vec<String>)> = BTreeMap::new();
            for (id, ns, branch, _, scope) in entries {
                if *branch == MAIN {
                    to_main
                        .entry(ns.as_str())
                        .or_insert_with(|| (scope, vec![]))
                        .1
                        .push(id.to_string());
                }
            }
            for (ns, (scope, ids)) in &to_main {
                if ids.len() < 2 {
                    continue;
                }
                let ledger = ledgers
                    .entry(ns)
                    .or_insert_with(|| Ledger::from_batches(&snap.rows, ns));
                if self.publications_untouched(scope, ids, ledger).await? {
                    cleared.insert(ns);
                }
            }
            let mut chains: HashMap<(&str, &str), (String, i64)> = HashMap::new();
            let (mut ids, mut branches, mut actors, mut scopes, mut times, mut parents) =
                (vec![], vec![], vec![], vec![], vec![], vec![]);
            for (id, ns, branch, actor, scope) in entries {
                let ledger = ledgers
                    .entry(ns.as_str())
                    .or_insert_with(|| Ledger::from_batches(&snap.rows, ns));
                let (ps, latest) = match chains.get(&(ns.as_str(), *branch)) {
                    Some((prev, t)) => (vec![prev.clone()], Some(*t)),
                    None => {
                        let heads = ledger.heads(branch);
                        let latest = heads
                            .iter()
                            .filter_map(|h| ledger.rows.iter().find(|r| &r.commit_id == h))
                            .map(|r| r.recorded_at)
                            .max();
                        (heads, latest)
                    }
                };
                let recorded = latest.map_or(self.now(), |t| self.now().max(t + 1));
                if *branch == MAIN && !cleared.contains(ns.as_str()) {
                    self.authorize_publication(scope, id, &ps, ledger).await?;
                }
                // Later rows in this atomic batch are checked against its earlier rows too.
                ledger.rows.push(LedgerRow {
                    commit_id: id.to_string(),
                    branch: branch.to_string(),
                    parents: ps.clone(),
                    recorded_at: recorded,
                });
                chains.insert((ns.as_str(), *branch), (id.to_string(), recorded));
                ids.push(id.to_string());
                branches.push(branch.to_string());
                actors.push(actor.to_string());
                scopes.push(ns.clone());
                times.push(Some(recorded));
                parents.push(ps.join(","));
            }
            let s = |v: Vec<String>| -> ArrayRef { Arc::new(StringArray::from(v)) };
            let columns = vec![
                ("commit_id", s(ids)),
                ("branch", s(branches)),
                ("actor", s(actors)),
                ("scope", s(scopes)),
                ("recorded_at", ts(times)),
                ("parents", s(parents)),
            ];
            match append(&*self.catalog(), &snap.table, columns, OnConflict::Fail).await {
                Ok(()) => {
                    self.invalidate(SYSTEM_NS, LEDGER);
                    note_ledger_wait(started);
                    return Ok(());
                }
                // A lost race, or the ledger renamed aside by a rewrite under this commit.
                Err(e)
                    if attempt < 200 && (is_commit_conflict(&e) || self.ledger_aside().await) =>
                {
                    let ms = 5
                        + (uuid::Uuid::new_v4().as_u128() % 50) as u64
                            * (attempt.min(8) as u64 + 1);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// A branch is a ledger row whose parents are main's heads; it sees main as of then.
    pub async fn create_branch(&self, scope: &Scope, branch: &str, actor: &str) -> Result<()> {
        // The engine's own: what extraction holds for owners is proposed by the engine, which reads none of it,
        // so no one else may make a branch it would take for its own.
        anyhow::ensure!(
            !branch.starts_with(governance::HELD_PREFIX),
            "branch names starting {:?} are memstore's own",
            governance::HELD_PREFIX
        );
        self.make_branch(scope, branch, actor).await
    }

    /// [`Store::create_branch`] for any name, the engine's reserved ones included.
    pub(crate) async fn make_branch(&self, scope: &Scope, branch: &str, actor: &str) -> Result<()> {
        // '@' separates a branch from a snapshot in a pinned read ([`Store::pin_at`]).
        anyhow::ensure!(
            !branch.is_empty()
                && branch.len() <= 64
                && branch
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b)),
            "invalid branch name {branch:?}: use letters, digits, '-', '_', '.' and '/' (at most 64)"
        );
        let id = uuid::Uuid::new_v4().to_string();
        self.append_ledger(&id, scope, branch, actor, &|l: &Ledger| {
            anyhow::ensure!(
                branch != MAIN && l.base(branch).is_none(),
                "branch {branch:?} already exists in {}",
                scope.namespace()
            );
            Ok(l.heads(MAIN))
        })
        .await
    }

    /// Merge a branch into main: a three-way comparison per (entity, predicate) against the
    /// branch's base. If both sides changed the same key to different values the merge fails
    /// and lists the conflicts; nothing is resolved by last write. Returns the merge commit id.
    pub async fn merge(&self, scope: &Scope, branch: &str, actor: &str) -> Result<String> {
        self.merge_resolving(scope, branch, actor, &BTreeMap::new())
            .await
    }

    /// [`Store::merge`], with the side kept for each conflicting key (`<entity>.<predicate>`, as
    /// [`BranchDiff::conflicts`] names it): every conflict needs one, and nothing else may have one. The other
    /// side's facts for the key are closed as of the merge (they keep their rows: what was true until then),
    /// cited by a record of the decision (`merge:<branch>`, ids only) committed with the merge itself.
    pub async fn merge_resolving(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        keep: &BTreeMap<String, Side>,
    ) -> Result<String> {
        // The conflict check holds only for the heads it saw; if either side moves before the
        // merge commit lands, check again.
        for _ in 0..20 {
            match self.merge_once(scope, branch, actor, keep).await {
                Err(e) if e.is::<HeadsMoved>() => continue,
                r => return r,
            }
        }
        anyhow::bail!("merge of {branch:?} kept racing concurrent commits; giving up")
    }

    /// What main and `branch` each changed since the branch's base, and where they disagree: the keys
    /// (`<entity>.<predicate>`) both changed to different values. A merge is refused while any remain.
    pub async fn branch_diff(&self, scope: &Scope, branch: &str) -> Result<BranchDiff> {
        let d = self.since_base(scope, branch, false).await?;
        Ok(BranchDiff {
            main_changed: d.main_changes.len(),
            branch_changed: d.branch_changes.len(),
            branch_commits: d.branch_new.len(),
            conflicts: d.conflicts(),
        })
    }

    /// Each conflict of `branch` ([`Store::branch_diff`]) with the values main and the branch wrote for it since the
    /// base, as `who` reads them on each side: a value from a fact `who` may not read is left out and counted.
    pub async fn conflict_values(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
    ) -> Result<Vec<ConflictValues>> {
        let d = self.since_base(scope, branch, false).await?;
        let conflicts = d.conflicts();
        if conflicts.is_empty() {
            return Ok(vec![]);
        }
        let ns = scope.namespace();
        let (main, theirs, facts) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.visibility(scope, branch),
            self.rows(&ns, FACTS)
        )?;
        let mut out: BTreeMap<String, ConflictValues> = conflicts
            .iter()
            .map(|k| {
                (
                    k.clone(),
                    ConflictValues {
                        key: k.clone(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let mut seen: HashSet<(bool, String)> = HashSet::new();
        for b in facts.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                let key = format!("{}.{}", f.entity, f.predicate);
                let Some(c) = out.get_mut(&key) else { continue };
                for (on_main, since, vis) in
                    [(true, &d.main_new, &main), (false, &d.branch_new, &theirs)]
                {
                    if !since.contains(&commit) || !seen.insert((on_main, f.fact_id.clone())) {
                        continue;
                    }
                    let (values, hidden) = match on_main {
                        true => (&mut c.main, &mut c.main_hidden),
                        false => (&mut c.branch, &mut c.branch_hidden),
                    };
                    if vis.allows(&commit, &f, who, None) {
                        if !values.contains(&f.value) {
                            values.push(f.value.clone());
                        }
                    } else {
                        *hidden += 1;
                    }
                }
            }
        }
        Ok(out.into_values().collect())
    }

    /// The episodes `branch` committed that main doesn't have yet: its own, since its base or its last merge.
    pub async fn branch_episodes(&self, scope: &Scope, branch: &str) -> Result<HashSet<String>> {
        let d = self.since_base(scope, branch, false).await?;
        let ns = scope.namespace();
        let mut out = HashSet::new();
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id) = (strings(b, "commit_id"), strings(b, "episode_id"));
            for i in 0..b.num_rows() {
                if d.branch_new.contains(&commit[i]) {
                    out.insert(id[i].clone());
                }
            }
        }
        Ok(out)
    }

    /// The commits main and `branch` each made since the branch's base, and the (entity, predicate) values each
    /// wrote in them. A merge reads both tables `fresh` (dropping their cached rows, shared by every reader);
    /// a look at the difference reads what is cached.
    async fn since_base(&self, scope: &Scope, branch: &str, fresh: bool) -> Result<SinceBase> {
        let ns = scope.namespace();
        if fresh {
            self.invalidate(SYSTEM_NS, LEDGER);
            self.invalidate(&ns, FACTS);
        }
        let (ledger, facts) =
            tokio::try_join!(self.rows(SYSTEM_NS, LEDGER), self.rows(&ns, FACTS))?;
        let ledger = Ledger::from_batches(&ledger.rows, &ns);
        let base = ledger
            .base(branch)
            .with_context(|| format!("no branch {branch:?} in {}", scope.namespace()))?;
        // What both sides already share: the branch's base, and whatever of the branch an earlier merge brought
        // into main. A merged branch has nothing left to merge; worked on again, only its new commits count.
        let (on_main, on_branch) = (
            ledger.reachable(&ledger.heads(MAIN)),
            ledger.reachable(&ledger.heads(branch)),
        );
        let mut base_set = ledger.reachable(&base);
        base_set.extend(on_main.intersection(&on_branch).cloned());
        let since = |b: &str| -> HashSet<String> {
            let all = if b == MAIN { &on_main } else { &on_branch };
            all.difference(&base_set).cloned().collect()
        };
        let (main_new, branch_new) = (since(MAIN), since(branch));
        let (mut main_changes, mut branch_changes) = (Changes::new(), Changes::new());
        for b in facts.rows.iter() {
            let (commit, ent, pred, val) = (
                strings(b, "commit_id"),
                strings(b, "entity"),
                strings(b, "predicate"),
                strings(b, "value"),
            );
            for i in 0..b.num_rows() {
                let key = (ent[i].clone(), pred[i].clone());
                if main_new.contains(&commit[i]) {
                    main_changes
                        .entry(key.clone())
                        .or_default()
                        .insert(val[i].clone());
                }
                if branch_new.contains(&commit[i]) {
                    branch_changes
                        .entry(key)
                        .or_default()
                        .insert(val[i].clone());
                }
            }
        }
        Ok(SinceBase {
            ledger,
            main_new,
            branch_new,
            main_changes,
            branch_changes,
        })
    }

    async fn merge_once(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        keep: &BTreeMap<String, Side>,
    ) -> Result<String> {
        let ns = scope.namespace();
        let d = self.since_base(scope, branch, true).await?;
        let conflicts = d.conflicts();
        if let Some(k) = keep.keys().find(|k| d.ambiguous().contains(*k)) {
            anyhow::bail!(
                "{k} names more than one (entity, predicate) changed since the branch was made: \
                 resolve it on the branch instead"
            );
        }
        // Each kept side by the exact (entity, predicate) in conflict it names.
        let resolve: HashMap<(String, String), Side> = d
            .conflict_pairs()
            .into_iter()
            .filter_map(|k| {
                let side = keep.get(&format!("{}.{}", k.0, k.1))?;
                Some((k, *side))
            })
            .collect();
        let (ledger, main_new, branch_new) = (d.ledger, d.main_new, d.branch_new);
        let open: Vec<&String> = conflicts
            .iter()
            .filter(|k| !keep.contains_key(*k))
            .collect();
        anyhow::ensure!(
            open.is_empty(),
            "merge of {branch:?} has {} conflict(s): {}",
            open.len(),
            open.iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
        if let Some(k) = keep.keys().find(|k| !conflicts.contains(k)) {
            anyhow::bail!("{k} is not a conflict of {branch:?}: nothing to resolve");
        }

        // A schema committed on the branch becomes main's by this merge, so it meets main's rules
        // here: never backwards, and a breaking change only with a higher version.
        self.invalidate(&ns, SCHEMAS);
        let schemas = self.side_rows(&ns, SCHEMAS).await?;
        let changed = |since: &HashSet<String>| {
            schemas
                .iter()
                .any(|b| strings(b, "commit_id").iter().any(|c| since.contains(c)))
        };
        if changed(&branch_new) {
            let (ours, theirs) =
                tokio::try_join!(self.schema(scope, MAIN), self.schema(scope, branch))?;
            anyhow::ensure!(
                !changed(&main_new) || ours == theirs,
                "merge of {branch:?}: main and the branch both changed the schema"
            );
            if let (Some(ours), Some(theirs)) = (ours, theirs) {
                let breaks = theirs.breaking_changes(&ours);
                anyhow::ensure!(
                    theirs.version >= ours.version
                        && (breaks.is_empty() || theirs.version > ours.version),
                    "merge of {branch:?} would change main's schema {} v{} to {} v{}{}; raise \
                     the branch schema's version",
                    ours.name,
                    ours.version,
                    theirs.name,
                    theirs.version,
                    if breaks.is_empty() {
                        String::new()
                    } else {
                        format!(", breaking: {}", breaks.join("; "))
                    }
                );
            }
        }

        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let (main_heads, branch_heads) = (sorted(ledger.heads(MAIN)), sorted(ledger.heads(branch)));
        let id = uuid::Uuid::new_v4().to_string();
        if !keep.is_empty() {
            self.write_resolutions(scope, branch, actor, &id, &resolve, &main_new, &branch_new)
                .await?;
        }
        self.append_ledger(&id, scope, MAIN, actor, &|l: &Ledger| {
            if sorted(l.heads(MAIN)) != main_heads || sorted(l.heads(branch)) != branch_heads {
                return Err(HeadsMoved.into());
            }
            Ok(main_heads.iter().chain(&branch_heads).cloned().collect())
        })
        .await?;
        Ok(id)
    }

    /// A merge's resolved conflicts, under its commit id `id` (visible once its ledger row lands): the closures of
    /// the losing side's facts for each key, as of now, and the record they cite. The record holds ids only and is
    /// readable by everyone who reads any of the closed facts' sources, so a closure reaches each of their readers.
    #[allow(clippy::too_many_arguments)]
    async fn write_resolutions(
        &self,
        scope: &Scope,
        branch: &str,
        actor: &str,
        id: &str,
        keep: &HashMap<(String, String), Side>,
        main_new: &HashSet<String>,
        branch_new: &HashSet<String>,
    ) -> Result<()> {
        let ns = scope.namespace();
        let (main, theirs, facts) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.visibility(scope, branch),
            self.rows(&ns, FACTS)
        )?;
        let now = now_micros();
        let mut ends: BTreeMap<String, i64> = BTreeMap::new();
        let mut acl: BTreeSet<String> = BTreeSet::new();
        let mut decided: BTreeMap<String, (Side, Vec<String>)> = BTreeMap::new();
        for b in facts.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                let key = (f.entity.clone(), f.predicate.clone());
                let Some(side) = keep.get(&key) else { continue };
                // The side not kept loses its writes since the base.
                let lost = match side {
                    Side::Main => branch_new.contains(&commit),
                    Side::Branch => main_new.contains(&commit),
                };
                if !lost || ends.contains_key(&f.fact_id) {
                    continue;
                }
                for e in &f.source_episodes {
                    for v in [&main, &theirs] {
                        acl.extend(v.acls.get(e).into_iter().flatten().cloned());
                    }
                }
                ends.insert(f.fact_id.clone(), now.max(f.valid_from + 1));
                decided
                    .entry(format!("{}.{}", key.0, key.1))
                    .or_insert((*side, vec![]))
                    .1
                    .push(f.fact_id);
            }
        }
        if ends.is_empty() {
            return Ok(());
        }
        acl.retain(|a| a != FORGOTTEN);
        anyhow::ensure!(
            !acl.is_empty(),
            "the facts this merge would close have no sources anyone reads"
        );
        let body = serde_json::json!({
            "merged": branch,
            "by": actor,
            "kept": decided.values().map(|(s, ids)| serde_json::json!({"side": s.name(), "closed": ids}))
                .collect::<Vec<_>>(),
        });
        let acl: Vec<&str> = acl.iter().map(String::as_str).collect();
        let mut record = Episode::sourced(
            &format!("{MERGE_SOURCE}{branch}"),
            body.to_string().as_bytes(),
            &acl,
        )
        .without_text_index();
        record.occurred_at = now;
        self.write_episodes(&ns, &[(id, &record)]).await?;
        let rows: Vec<Closure> = ends
            .iter()
            .map(|(f, t)| Closure {
                fact_id: f.clone(),
                valid_to: *t,
                episode_id: record.episode_id.clone(),
            })
            .collect();
        let rows: Vec<(&str, &Closure)> = rows.iter().map(|c| (id, c)).collect();
        self.write_closure_rows(&ns, &rows).await
    }

    /// Step 1 of a commit: bodies, then rows tagged with a pending commit id.
    /// Nothing here is visible until step 2 lands.
    pub async fn write_pending(
        &self,
        scope: &Scope,
        episodes: &[Episode],
        facts: &[Fact],
    ) -> Result<String> {
        validate_write(episodes, facts)?;
        episode_types::check_stored(episodes)?;
        let facts = &self.typed(scope, MAIN, facts, None).await?;
        self.check_about(scope, MAIN, episodes).await?;
        let commit_id = uuid::Uuid::new_v4().to_string();
        let ns = scope.namespace();
        let eps: Vec<(&str, &Episode)> = episodes.iter().map(|e| (commit_id.as_str(), e)).collect();
        let fs: Vec<(&str, &Fact)> = facts.iter().map(|f| (commit_id.as_str(), f)).collect();
        // Episodes and facts are independent until the ledger row, so write them concurrently.
        tokio::try_join!(self.write_episodes(&ns, &eps), self.write_facts(&ns, &fs))?;
        Ok(commit_id)
    }

    async fn write_episodes(&self, ns: &str, rows: &[(&str, &Episode)]) -> Result<()> {
        let episodes: Vec<&Episode> = rows.iter().map(|(_, e)| *e).collect();
        if !episodes.is_empty() {
            let table = self.write_target(ns, EPISODES).await?;
            // Same body, same path: rewriting it is idempotent. Bounded concurrency: a large
            // batch must not open thousands of connections to the object store at once.
            // Owned inputs per future (not a closure over borrows) keep the commit future
            // `Send` for every lifetime, so it can be spawned.
            let writes: Vec<_> = episodes
                .iter()
                .map(|e| {
                    let (io, path) = (table.file_io().clone(), body_path(&table, &e.episode_id));
                    let (id, body) = (e.episode_id.clone(), Bytes::from(e.body.clone()));
                    async move {
                        with_retries(|| async {
                            io.new_output(path.clone())?
                                .write(body.clone())
                                .await
                                .with_context(|| format!("write body of episode {id}"))
                        })
                        .await
                    }
                })
                .collect();
            futures::stream::iter(writes)
                .buffer_unordered(64)
                .try_collect::<Vec<_>>()
                .await?;
            let col = |f: fn(&Episode) -> String| -> ArrayRef {
                Arc::new(StringArray::from_iter_values(episodes.iter().map(|e| f(e))))
            };
            let columns: Vec<(&str, ArrayRef)> = vec![
                (
                    "commit_id",
                    Arc::new(StringArray::from_iter_values(rows.iter().map(|(c, _)| *c))),
                ),
                ("episode_id", col(|e| e.episode_id.clone())),
                ("source", col(|e| e.source.clone())),
                (
                    "occurred_at",
                    ts(episodes.iter().map(|e| Some(e.occurred_at))),
                ),
                (
                    "recorded_at",
                    ts(episodes
                        .iter()
                        .map(|e| Some(e.recorded_at.unwrap_or_else(now_micros)))),
                ),
                ("acl", col(|e| e.acl.join(","))),
                ("about", col(|e| e.about.join(","))),
                (
                    "index_text",
                    Arc::new(arrow_array::BooleanArray::from(
                        episodes.iter().map(|e| e.index_text).collect::<Vec<_>>(),
                    )),
                ),
                (
                    "document",
                    Arc::new(StringArray::from(
                        episodes
                            .iter()
                            .map(|e| e.document.as_deref())
                            .collect::<Vec<_>>(),
                    )),
                ),
            ];
            append(&*self.catalog(), &table, columns, OnConflict::Recommit).await?;
            self.invalidate(ns, EPISODES);
        }
        Ok(())
    }

    async fn write_extractions(&self, ns: &str, rows: &[(&str, &Extraction)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let table = self.write_target(ns, EXTRACTIONS).await?;
        let s = |v: Vec<&str>| -> ArrayRef { Arc::new(StringArray::from(v)) };
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("commit_id", s(rows.iter().map(|r| r.0).collect())),
            (
                "episode_id",
                s(rows.iter().map(|r| r.1.episode_id.as_str()).collect()),
            ),
            (
                "extractor",
                s(rows.iter().map(|r| r.1.extractor.as_str()).collect()),
            ),
            (
                "facts",
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.1.facts))),
            ),
            (
                "recorded_at",
                ts(std::iter::repeat_n(Some(now_micros()), rows.len())),
            ),
            (
                "input_tokens",
                Arc::new(Int64Array::from_iter(
                    rows.iter().map(|r| r.1.tokens.map(|t| t.0)),
                )),
            ),
            (
                "output_tokens",
                Arc::new(Int64Array::from_iter(
                    rows.iter().map(|r| r.1.tokens.map(|t| t.1)),
                )),
            ),
        ];
        append(&*self.catalog(), &table, columns, OnConflict::Recommit).await?;
        self.invalidate(ns, EXTRACTIONS);
        Ok(())
    }

    async fn write_facts(&self, ns: &str, rows: &[(&str, &Fact)]) -> Result<()> {
        let facts: Vec<&Fact> = rows.iter().map(|(_, f)| *f).collect();
        if !facts.is_empty() {
            let table = self.write_target(ns, FACTS).await?;
            let n = facts.len();
            let col = |g: fn(&Fact) -> String| -> ArrayRef {
                Arc::new(StringArray::from_iter_values(facts.iter().map(|f| g(f))))
            };
            let opt = |g: fn(&Fact) -> Option<String>| -> ArrayRef {
                Arc::new(StringArray::from(
                    facts.iter().map(|f| g(f)).collect::<Vec<_>>(),
                ))
            };
            let columns: Vec<(&str, ArrayRef)> = vec![
                (
                    "commit_id",
                    Arc::new(StringArray::from_iter_values(rows.iter().map(|(c, _)| *c))),
                ),
                ("fact_id", col(|f| f.fact_id.clone())),
                ("entity", col(|f| f.entity.clone())),
                ("predicate", col(|f| f.predicate.clone())),
                ("value", col(|f| f.value.clone())),
                ("valid_from", ts(facts.iter().map(|f| Some(f.valid_from)))),
                ("valid_to", ts(facts.iter().map(|f| f.valid_to))),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now_micros()), n)),
                ),
                ("source_episodes", col(|f| f.source_episodes.join(","))),
                ("quote", opt(|f| f.quote.clone())),
                ("entity_type", opt(|f| f.entity_type.clone())),
                ("value_type", opt(|f| f.value_type.clone())),
                (
                    "value_num",
                    Arc::new(arrow_array::Float64Array::from(
                        facts.iter().map(|f| f.value_num).collect::<Vec<_>>(),
                    )),
                ),
                ("value_ts", ts(facts.iter().map(|f| f.value_ts))),
                ("value_ref", opt(|f| f.value_ref.clone())),
                ("unit", opt(|f| f.unit.clone())),
                ("cite", opt(|f| f.cite.clone())),
                ("time_basis", opt(|f| f.time_basis.clone())),
                ("tags", opt(|f| tags_column(&f.tags))),
            ];
            append(&*self.catalog(), &table, columns, OnConflict::Recommit).await?;
            self.invalidate(ns, FACTS);
        }
        Ok(())
    }

    /// Table metadata to append to: the cached copy when recent, else a fresh load. A stale
    /// base is fine, since the commit re-applies onto the catalog's current metadata.
    pub async fn write_target(&self, ns: &str, name: &str) -> Result<Table> {
        self.check_fence()?;
        let cached = self
            .cache
            .entries
            .lock()
            .unwrap()
            .get(&format!("{ns}.{name}"))
            .cloned();
        match cached {
            Some(e) if e.snap_age() < self.hard_staleness => Ok(e.snap.table),
            _ => self.table(ns, name).await,
        }
    }

    /// Step 2 of a commit: the atomic step that makes the commit visible (NFR-5). A compare-
    /// and-set on the ledger snapshot: parents are computed by `parents_of` from the ledger as
    /// of the snapshot being appended to, and a concurrent append makes this one fail and retry
    /// with fresh heads. So each branch's ledger stays linear under concurrent writers.
    /// `recorded_at` is the writer's clock, but never earlier than a parent's, so recorded
    /// times stay monotonic along the ledger even with skewed clocks.
    pub async fn append_ledger(
        &self,
        commit_id: &str,
        scope: &Scope,
        branch: &str,
        actor: &str,
        parents_of: &(dyn Fn(&Ledger) -> Result<Vec<String>> + Send + Sync),
    ) -> Result<()> {
        let ns = scope.namespace();
        let started = Instant::now();
        for attempt in 0u32.. {
            let snap = self.ledger_now().await?;
            let ledger = Ledger::from_batches(&snap.rows, &ns);
            let parents = parents_of(&ledger)?;
            if branch == MAIN {
                self.authorize_publication(scope, commit_id, &parents, &ledger)
                    .await?;
            }
            let latest_parent = parents
                .iter()
                .filter_map(|p| ledger.rows.iter().find(|r| &r.commit_id == p))
                .map(|r| r.recorded_at)
                .max();
            let recorded = latest_parent.map_or(self.now(), |t| self.now().max(t + 1));
            let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
            let columns = vec![
                ("commit_id", s(commit_id)),
                ("branch", s(branch)),
                ("actor", s(actor)),
                ("scope", s(&ns)),
                ("recorded_at", ts([Some(recorded)])),
                ("parents", s(&parents.join(","))),
            ];
            match append(&*self.catalog(), &snap.table, columns, OnConflict::Fail).await {
                Ok(()) => {
                    self.invalidate(SYSTEM_NS, LEDGER);
                    note_ledger_wait(started);
                    return Ok(());
                }
                // A lost race, or the ledger renamed aside by a rewrite under this commit.
                Err(e)
                    if attempt < 200 && (is_commit_conflict(&e) || self.ledger_aside().await) =>
                {
                    // Jittered backoff so racing writers spread out.
                    let ms = 5
                        + (uuid::Uuid::new_v4().as_u128() % 50) as u64
                            * (attempt.min(8) as u64 + 1);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// The ledger as it is now, read from the catalog. While an org's rows are removed from it (an org
    /// delete rewrites it) the table is missing for a moment: wait for it rather than fail the commit.
    async fn ledger_now(&self) -> Result<Snapshot> {
        // After any load of the ledger under way (a reader's, or a refresh's): that one read the whole table
        // when cold, and this one then reads only what was appended since. A cold ledger takes minutes to read.
        // `refresh` waits out a rewrite of the ledger.
        let loading = self.cache.loading(&format!("{SYSTEM_NS}.{LEDGER}"));
        let _turn = loading.lock().await;
        refresh(&*self.catalog(), &self.cache, SYSTEM_NS, LEDGER).await
    }

    /// Whether the catalog has a ledger (a fresh one has none until its first scope), or one being rewritten.
    async fn has_ledger(&self) -> Result<bool> {
        let ident = TableIdent::new(NamespaceIdent::new(SYSTEM_NS.into()), LEDGER.into());
        Ok(self.has_table(&ident).await? || self.ledger_aside().await)
    }

    /// Whether the ledger is renamed aside right now: a rewrite ([`Store::rewrite_table`]) is under way.
    async fn ledger_aside(&self) -> bool {
        let ns = NamespaceIdent::new(SYSTEM_NS.into());
        let ident = TableIdent::new(ns.clone(), LEDGER.into());
        if self.has_table(&ident).await.unwrap_or(true) {
            return false;
        }
        is_aside(&*self.catalog(), &ns, LEDGER).await
    }

    /// Microseconds since the epoch, shifted by this store's test clock offset.
    fn now(&self) -> i64 {
        now_micros() + self.clock_offset_us
    }

    /// Where this store keeps its derived search/key indexes.
    pub fn index_base(&self) -> &str {
        &self.index_base
    }

    /// Keep this store's indexes somewhere else (a local path, `gs://…` or `s3://…`).
    pub fn with_index_base(mut self, base: impl Into<String>) -> Self {
        self.index_base = base.into();
        self
    }

    /// How this store chunks passages: (bytes, overlap).
    pub fn chunking(&self) -> (usize, usize) {
        self.chunking
    }

    /// Chunk passages differently: a passage index of its own, beside the one of any other chunking.
    pub fn with_chunking(mut self, bytes: usize, overlap: usize) -> Self {
        self.chunking = (bytes, overlap);
        self
    }

    /// Shift this store's clock (for clock-skew checks).
    pub fn with_clock_offset(mut self, offset: Duration, ahead: bool) -> Self {
        let us = offset.as_micros() as i64;
        self.clock_offset_us = if ahead { us } else { -us };
        self
    }

    /// Facts the principal may see in a scope, optionally as of a valid time.
    /// A fact is visible only if it cites at least one source episode and the principal can
    /// see every one of them (D-6); a fact citing an uncommitted episode is hidden (FR-23).
    pub async fn read(
        &self,
        scope: &Scope,
        who: &Principal,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        self.read_on(scope, MAIN, who, valid_at).await
    }

    /// Same as [`Store::read`], on a named branch.
    pub async fn read_on(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        self.pin(scope, branch)
            .await?
            .read(self, who, valid_at)
            .await
    }

    /// Fact-key lookup: the facts for one (entity, predicate) the principal may see, optionally
    /// as of a valid time. Served by the search index's scalar key index (plus the unindexed
    /// tail), so it never holds the scope's facts in memory.
    pub async fn lookup(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
        entity: &str,
        predicate: &str,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let session = self.pin(scope, branch).await?;
        session.lookup(self, who, entity, predicate, valid_at).await
    }

    /// Facts with a given (predicate, value) the principal may see: the reverse key, used for
    /// aggregation ("who is in team X") without a scan.
    pub async fn lookup_value(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
        predicate: &str,
        value: &str,
        valid_at: Option<i64>,
    ) -> Result<Vec<Fact>> {
        let session = self.pin(scope, branch).await?;
        session
            .lookup_value(self, who, predicate, value, valid_at)
            .await
    }

    /// Table maintenance: expire the Iceberg history of the system tables (the ledger, the directory and its
    /// snapshots, installations, key usage, the audit log) and the scope's tables past the newest `keep`
    /// snapshots and the grace of [`History`], and delete the manifest lists and metadata files only that history
    /// named. Every commit does this too once a table has `keep` snapshots to expire (`append`); this catches a
    /// table no one has written to since. Pinned sessions pin ledger commits, not Iceberg snapshots, so they are
    /// unaffected. Returns (table, snapshots before, after) per table that exists.
    pub async fn maintain(
        &self,
        scope: &Scope,
        keep: usize,
    ) -> Result<Vec<(String, usize, usize)>> {
        self.maintain_over(scope, keep, keep, true).await
    }

    /// [`Store::maintain`], touching only a table that holds more than `over` snapshots: `serve`'s timer
    /// waits for twice `keep`, so a quiet table isn't committed to on every round. The system tables only
    /// with `system`: they are the catalog's, not the scope's, so a round over many scopes does them once.
    pub async fn maintain_over(
        &self,
        scope: &Scope,
        keep: usize,
        over: usize,
        system: bool,
    ) -> Result<Vec<(String, usize, usize)>> {
        let ns = scope.namespace();
        let mut out = match system {
            true => self.maintain_system(keep, over).await?,
            false => vec![],
        };
        let tables = [
            EPISODES,
            FACTS,
            QUOTES,
            SCHEMAS,
            CLOSURES,
            EXTRACTIONS,
            RETRACTIONS,
            RETENTION,
            AGGREGATES,
            AGG_REQUESTS,
            QA_LOG,
            FEEDBACK,
            governance::TABLE,
        ]
        .map(|name| (ns.as_str(), name));
        out.extend(self.maintain_tables(&tables, keep, over).await?);
        Ok(out)
    }

    /// [`Store::maintain_over`] of the system tables alone (the ledger, the directory and its snapshots,
    /// installations, key usage, the audit log): they are the catalog's, not a scope's.
    async fn maintain_system(
        &self,
        keep: usize,
        over: usize,
    ) -> Result<Vec<(String, usize, usize)>> {
        let tables = [
            (SYSTEM_NS, LEDGER),
            (SYSTEM_NS, DIRECTORY),
            (SYSTEM_NS, DIRECTORY_SNAPSHOT),
            (SYSTEM_NS, INSTALLATIONS),
            (SYSTEM_NS, KEY_USAGE),
            (SYSTEM_NS, crate::audit::AUDIT),
        ];
        self.maintain_tables(&tables, keep, over).await
    }

    async fn maintain_tables(
        &self,
        tables: &[(&str, &str)],
        keep: usize,
        over: usize,
    ) -> Result<Vec<(String, usize, usize)>> {
        let mut out = vec![];
        for &(tns, name) in tables {
            let ident = TableIdent::new(NamespaceIdent::new(tns.into()), name.into());
            if !self.has_table(&ident).await? {
                continue;
            }
            let table = self.table(tns, name).await?;
            let n = table.metadata().snapshots().count();
            let (before, after) = if n > over.max(keep) {
                let fresh = self.catalog().load_table(&ident).await?;
                trim_to(&*self.catalog(), &fresh, keep).await?
            } else {
                trim_to(&*self.catalog(), &table, n.max(keep)).await?
            };
            self.invalidate(tns, name);
            out.push((format!("{tns}.{name}"), before, after));
        }
        Ok(out)
    }

    /// The scope's ledger rows, read fresh: (commit id, branch, parents, recorded_at).
    pub async fn ledger_rows(
        &self,
        scope: &Scope,
    ) -> Result<Vec<(String, String, Vec<String>, i64)>> {
        let snap = self.ledger_now().await?;
        Ok(Ledger::from_batches(&snap.rows, &scope.namespace())
            .rows
            .into_iter()
            .map(|r| (r.commit_id, r.branch, r.parents, r.recorded_at))
            .collect())
    }

    /// Pin a read session to the branch's current ledger state (D-9).
    pub async fn pin(&self, scope: &Scope, branch: &str) -> Result<Session> {
        Ok(Session {
            scope: scope.clone(),
            branch: branch.to_string(),
            vis: self.visibility(scope, branch).await?,
            at: None,
            index: None,
        })
    }

    /// Pin a read session to `heads`, the ledger heads a snapshot (`pin`, taken at `taken_at`) recorded for
    /// the scope's `branch`: it sees exactly the commits reachable from them, whatever is committed later,
    /// and reads as of `taken_at`.
    /// `index` is the index versions it searches with (none: the current indexes).
    #[allow(clippy::too_many_arguments)]
    pub async fn pin_at(
        &self,
        scope: &Scope,
        branch: &str,
        pin: &str,
        heads: &[String],
        taken_at: i64,
        index: Option<search::IndexPin>,
    ) -> Result<Session> {
        Ok(Session {
            scope: scope.clone(),
            branch: branch.to_string(),
            vis: self
                .visibility_at(scope, branch, Some((pin, heads)))
                .await?,
            at: Some(taken_at),
            index: index.map(Arc::new),
        })
    }

    /// What `branch` of `scope` already holds, for a load to skip: each committed episode's ACL, and the
    /// committed facts and closures.
    pub async fn committed_rows(&self, scope: &Scope, branch: &str) -> Result<Committed> {
        let ns = scope.namespace();
        let (vis, facts, closures) = tokio::try_join!(
            self.visibility(scope, branch),
            self.rows(&ns, FACTS),
            self.side_rows(&ns, CLOSURES)
        )?;
        let mut out = Committed {
            episodes: vis
                .acls
                .iter()
                .map(|(id, acl)| {
                    let mut acl = acl.clone();
                    acl.sort();
                    (id.clone(), acl)
                })
                .collect(),
            ..Default::default()
        };
        for b in facts.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                if vis.committed.contains(&commit) {
                    out.fact_ids.insert(f.fact_id.clone());
                    out.fact_keys.insert(Committed::fact_key(&f));
                }
            }
        }
        for b in closures.iter() {
            let (commit, fid, ep) = (
                strings(b, "commit_id"),
                strings(b, "fact_id"),
                strings(b, "episode_id"),
            );
            let end = timestamps(b, "valid_to");
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i])
                    && let Some(t) = end[i]
                {
                    out.closures.insert((fid[i].clone(), t, ep[i].clone()));
                }
            }
        }
        Ok(out)
    }

    /// The commits reachable from `heads` in the scope's ledger, read now.
    pub async fn reachable(&self, scope: &Scope, heads: &[String]) -> Result<HashSet<String>> {
        if heads.is_empty() || !self.has_ledger().await? {
            return Ok(HashSet::new());
        }
        let snap = self.ledger_now().await?;
        Ok(Ledger::from_batches(&snap.rows, &scope.namespace()).reachable(heads))
    }

    /// Every branch of the scope with its heads, read from the catalog now (main has none before its first
    /// commit).
    pub async fn branch_heads(&self, scope: &Scope) -> Result<BTreeMap<String, Vec<String>>> {
        if !self.has_ledger().await? {
            return Ok(BTreeMap::new());
        }
        let snap = self.ledger_now().await?;
        let ledger = Ledger::from_batches(&snap.rows, &scope.namespace());
        let branches: BTreeSet<&str> = ledger.rows.iter().map(|r| r.branch.as_str()).collect();
        Ok(branches
            .into_iter()
            .map(|b| {
                let mut heads = ledger.heads(b);
                heads.sort();
                (b.to_string(), heads)
            })
            .collect())
    }

    /// The committed set and episode ACLs for one branch, rebuilt only when the ledger's or the
    /// episodes table's cached rows change.
    /// Every Snowflake role named in a committed episode's ACL (`role:<name>`), as written: the
    /// candidates to check a caller against.
    /// Every change ever made to the org and team directory (`memory_system.directory`), in the
    /// order it was made, read from the catalog now (the whole log: [`Store::directory`] is what
    /// serving reads). Empty before the first change.
    pub async fn directory_events(&self) -> Result<Vec<directory::Event>> {
        match self.directory_table().await? {
            Some(t) => directory_rows(&t, None).await,
            None => Ok(vec![]),
        }
    }

    /// The directory's changes made at `at` (µs since the epoch) or after, in order, read from the
    /// catalog now: the log's files before then are skipped.
    pub async fn directory_events_since(&self, at: i64) -> Result<Vec<directory::Event>> {
        match self.directory_table().await? {
            Some(t) => directory_rows(&t, Some(at.saturating_sub(1))).await,
            None => Ok(vec![]),
        }
    }

    /// Whether a table exists: not when its namespace doesn't (a fresh catalog has none of the
    /// system tables, nor their namespace, and some catalogs answer that with an error).
    pub async fn has_table(&self, ident: &TableIdent) -> Result<bool> {
        with_retries(|| async {
            Ok(self.catalog().namespace_exists(ident.namespace()).await?
                && self.catalog().table_exists(ident).await?)
        })
        .await
    }

    /// Whether `scope` holds memory: its episodes table exists (an org nobody has written to has none).
    pub async fn has_episodes(&self, scope: &Scope) -> Result<bool> {
        self.has_table(&TableIdent::new(
            NamespaceIdent::new(scope.namespace()),
            EPISODES.into(),
        ))
        .await
    }

    /// Whether `memory_system.<name>` exists (see [`Store::has_table`]).
    async fn system_table_exists(&self, name: &str) -> Result<bool> {
        self.has_table(&TableIdent::new(
            NamespaceIdent::new(SYSTEM_NS.into()),
            name.into(),
        ))
        .await
    }

    /// The directory table as it is now; none before the first change.
    async fn directory_table(&self) -> Result<Option<Table>> {
        let ident = TableIdent::new(NamespaceIdent::new(SYSTEM_NS.into()), DIRECTORY.into());
        match with_retries(|| async { Ok(self.catalog().load_table(&ident).await?) }).await {
            Ok(t) => Ok(Some(t)),
            Err(_) if !self.has_table(&ident).await? => Ok(None),
            Err(e) => Err(e).context("load table memory_system.directory"),
        }
    }

    /// The directory, folded: at most `MEMSTORE_MAX_STALENESS_MS` behind other servers, and never behind
    /// this server's own changes. A server folds the log once: after that it reads, and folds, only
    /// the changes after the last it folded (on its first load, those after the newest snapshot).
    pub async fn directory(&self) -> Result<Arc<directory::Folded>> {
        self.directory_loaded_after(Instant::now().checked_sub(self.max_staleness))
            .await
    }

    /// The directory as loaded after `since` (none: any load will do), reading the catalog if this
    /// server's fold is older. Concurrent callers share one read.
    pub async fn directory_loaded_after(
        &self,
        since: Option<Instant>,
    ) -> Result<Arc<directory::Folded>> {
        let mut held = self.dir.folded.lock().await;
        if let Some((at, _, f)) = &*held
            && since.is_none_or(|s| *at >= s)
        {
            return Ok(f.clone());
        }
        let t0 = Instant::now();
        let (table, f) = self.fold_directory(held.as_ref()).await?;
        self.dir
            .metrics
            .refresh_seconds
            .observe(t0.elapsed().as_secs_f64());
        let sid = table
            .as_ref()
            .and_then(|t| t.metadata().current_snapshot())
            .map(|s| s.snapshot_id());
        // Dated by when the read began, not when it ended: a caller asking for a load after some instant
        // never gets a fold that read the catalog before it, though it finished after.
        *held = Some((t0, sid, f.clone()));
        Ok(f)
    }

    /// Where the catalog keeps the system tables (`memory_system`): the ledger's location without its last
    /// segment, e.g. `gs://bucket/warehouse/memory_system`. Small objects the servers on this catalog share
    /// (the background writer's lease) live under it.
    pub async fn system_location(&self) -> Result<String> {
        Ok(self.system_storage().await?.system)
    }

    /// Where the system tables are kept, and how the catalog reaches them: the ledger's own location and
    /// its file IO's properties (the storage credentials the catalog vended for it, or was configured with),
    /// as the ledger's latest load has them.
    pub async fn system_storage(&self) -> Result<SystemStorage> {
        let t = self.table(SYSTEM_NS, LEDGER).await?;
        let ledger = t.metadata().location().trim_end_matches('/').to_string();
        Ok(SystemStorage {
            system: ledger
                .rsplit_once('/')
                .map_or(&*ledger, |(p, _)| p)
                .to_string(),
            props: t.file_io().config().props().clone(),
            ledger,
        })
    }

    /// The directory's metrics (load latency, commit retries, the version gate).
    pub fn directory_metrics(&self) -> &metrics::DirMetrics {
        &self.dir.metrics
    }

    /// The directory table now, and the directory folded from it: `held` (this server's last fold)
    /// plus the changes after it, or on a first load the newest snapshot plus the changes after it.
    /// What it starts from must account for exactly the log's changes up to its last one (the table's
    /// row count says how many there are); if not (a snapshot that doesn't match the log, a change a
    /// writer from before compare-and-set committed out of order), the whole log is replayed.
    async fn fold_directory(
        &self,
        held: Option<&HeldFold>,
    ) -> Result<(Option<Table>, Arc<directory::Folded>)> {
        let (table, f) = self.fold_directory_from(held).await?;
        self.dir.metrics.loaded(f.blocked);
        Ok((table, f))
    }

    async fn fold_directory_from(
        &self,
        held: Option<&HeldFold>,
    ) -> Result<(Option<Table>, Arc<directory::Folded>)> {
        let Some(table) = self.directory_table().await? else {
            return Ok((None, Arc::new(directory::Folded::empty())));
        };
        let sid = table.metadata().current_snapshot().map(|s| s.snapshot_id());
        if let Some((_, Some(was), f)) = held
            && Some(*was) == sid
        {
            return Ok((Some(table), f.clone()));
        }
        let base = match held {
            Some((_, _, f)) => Some(f.clone()),
            None => match self.directory_snapshot().await {
                // Sealed by a server holding this server's key, from its own fold: loading starts there.
                Ok(Some(s)) if self.directory_key().is_some_and(|k| s.sealed(k)) => {
                    match s.folded() {
                        Ok(f) => {
                            self.dir
                                .snapshot_events
                                .fetch_max(s.events, std::sync::atomic::Ordering::Relaxed);
                            Some(Arc::new(f))
                        }
                        Err(e) => {
                            eprintln!("directory snapshot unusable, replaying the log: {e:#}");
                            None
                        }
                    }
                }
                // Nothing vouches for it (anyone who can write the catalog can write a snapshot): the log
                // is replayed, and the snapshot only checked against it.
                Ok(Some(s)) => {
                    let events = directory_rows(&table, None).await?;
                    let problems = directory::snapshot_problems(&events, &s);
                    if problems.is_empty() {
                        self.dir
                            .snapshot_events
                            .fetch_max(s.events, std::sync::atomic::Ordering::Relaxed);
                        if s.seal.is_some() {
                            eprintln!(
                                "directory: the newest snapshot's seal doesn't open with this server's {} (another key on another replica?); it agrees with the log",
                                auth::secret::KEY_ENV
                            );
                        }
                    } else {
                        self.dir.metrics.snapshot_refused.inc();
                        eprintln!(
                            "ALERT directory: the newest snapshot (taken {} by {}, {} changes) is not what the log folds to, so it is not loaded and the log is replayed: {}. Someone who can write the catalog may have written it",
                            s.taken_at,
                            s.actor,
                            s.events,
                            problems.join("; ")
                        );
                    }
                    let mut f = directory::Folded::empty();
                    f.apply(&events);
                    return Ok((Some(table), Arc::new(f)));
                }
                Ok(None) => None,
                Err(e) => {
                    eprintln!("directory snapshot unreadable, replaying the log: {e:#}");
                    None
                }
            },
        };
        let total = live_rows(&table).await?;
        if let Some(base) = base {
            let tail = directory_rows(&table, Some(base.last_at)).await?;
            if total == Some(base.events + tail.len() as u64) {
                if tail.is_empty() && base.blocked.is_none() {
                    return Ok((Some(table), base));
                }
                let mut f = (*base).clone();
                f.apply(&tail);
                return Ok((Some(table), Arc::new(f)));
            }
            eprintln!(
                "directory: {} changes up to {} folded, {} after it, but the log has {}: replaying the log",
                base.events,
                base.last_at,
                tail.len(),
                total.map_or("an unknown number".into(), |n| n.to_string())
            );
        }
        let mut f = directory::Folded::empty();
        f.apply(&directory_rows(&table, None).await?);
        Ok((Some(table), Arc::new(f)))
    }

    /// Make a directory change: `plan` it against the directory as it is now and commit its changes,
    /// unless another writer committed first, in which case the directory is read again and the
    /// change re-planned (so a change that no longer holds is refused by `plan`, not committed). Up to
    /// [`DIRECTORY_ATTEMPTS`] times. Changes are stamped after the last one planned against, so they
    /// apply in the order they were committed. Returns what was committed (empty: nothing to do).
    pub async fn append_directory_with<F>(&self, plan: F) -> Result<Vec<directory::Event>>
    where
        F: Fn(&directory::Directory) -> Result<Vec<directory::Event>>,
    {
        let _turn = self.dir.writes.lock().await;
        self.ensure_table(SYSTEM_NS, DIRECTORY, directory_schema()?)
            .await?;
        self.ensure_directory_cas().await?;
        // The last attempt's changes and what they were planned against, until it is known whether
        // they landed (a commit whose answer was lost may have).
        let mut tried: Option<(i64, Vec<directory::Event>)> = None;
        for attempt in 0..DIRECTORY_ATTEMPTS {
            let read_at = Instant::now();
            let (table, base) = {
                let mut held = self.dir.folded.lock().await;
                let (table, f) = self.fold_directory(held.as_ref()).await?;
                let table = table.context("memory_system.directory is gone")?;
                let sid = table.metadata().current_snapshot().map(|s| s.snapshot_id());
                *held = Some((read_at, sid, f.clone()));
                (table, f)
            };
            if let Some((after, events)) = tried.take() {
                let since = directory_rows(&table, Some(after)).await?;
                if events.iter().all(|e| since.contains(e)) {
                    return self.directory_written(read_at, base, events).await;
                }
            }
            // Fail closed: a change planned against a directory that stops short of the log could
            // undo or contradict what the newer change did.
            if let Some((version, _)) = base.blocked {
                anyhow::bail!(
                    "the directory holds {}: upgrade this engine before changing the directory",
                    directory::blocked_why(version)
                );
            }
            let mut events = plan(&base.dir)?;
            if events.is_empty() {
                return Ok(events);
            }
            let start = self.now().max(base.last_at + 1);
            for (i, e) in events.iter_mut().enumerate() {
                e.at = start + i as i64;
            }
            match append(
                &*self.catalog(),
                &table,
                directory_columns(&events),
                OnConflict::Fail,
            )
            .await
            {
                Ok(()) => {
                    let mut after = (*base).clone();
                    after.apply(&events);
                    return self
                        .directory_written(read_at, Arc::new(after), events)
                        .await;
                }
                Err(e) if is_commit_conflict(&e) => {
                    self.dir.metrics.commit_retries.inc();
                    tried = Some((base.last_at, events));
                    let ms = 5
                        + (uuid::Uuid::new_v4().as_u128() % 40) as u64
                            * (attempt.min(8) as u64 + 1);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
        anyhow::bail!(
            "the directory kept changing under this change ({DIRECTORY_ATTEMPTS} tries): try again"
        )
    }

    /// `events` landed and `after` is the directory with them: keep it (this server reads its own
    /// changes at once), and write a snapshot if one is due. `read_at`: when the read `after` was planned
    /// against began (the compare-and-set let nothing else commit between that read and these events), which
    /// dates the fold kept: another server's change committed since isn't in it.
    async fn directory_written(
        &self,
        read_at: Instant,
        after: Arc<directory::Folded>,
        events: Vec<directory::Event>,
    ) -> Result<Vec<directory::Event>> {
        self.invalidate(SYSTEM_NS, DIRECTORY);
        {
            let mut held = self.dir.folded.lock().await;
            if held
                .as_ref()
                .is_none_or(|(_, _, f)| f.events <= after.events)
            {
                *held = Some((read_at, None, after.clone()));
            }
        }
        let every = snapshot_every();
        let known = self
            .dir
            .snapshot_events
            .load(std::sync::atomic::Ordering::Relaxed);
        if every > 0 && after.events >= known + every {
            let snap = directory::Snapshot::of(&after, "memstore", self.now())
                .sealed_with(self.directory_key());
            match self.append_directory_snapshot(&snap).await {
                Ok(()) => {
                    self.dir
                        .snapshot_events
                        .fetch_max(snap.events, std::sync::atomic::Ordering::Relaxed);
                }
                // The change is made; the snapshot is only a shortcut, tried again on the next change.
                Err(e) => eprintln!("directory snapshot not written: {e:#}"),
            }
        }
        Ok(events)
    }

    /// Every installation registration and revocation (`memory_system.installations`), in the order
    /// they were recorded. Empty before the first. Read fresh, not from the cache: connectors check a
    /// delivery against it moments after the installation is registered, maybe on another server.
    pub async fn installation_events(&self) -> Result<Vec<people::installations::Event>> {
        if !self.system_table_exists(INSTALLATIONS).await? {
            return Ok(vec![]);
        }
        let ident = TableIdent::new(NamespaceIdent::new(SYSTEM_NS.into()), INSTALLATIONS.into());
        let table = self.catalog().load_table(&ident).await?;
        let mut events = vec![];
        for b in scan_rows(&table, None, None).await? {
            events.extend(installation_batch(&b));
        }
        events.sort_by_key(|e| e.at);
        Ok(events)
    }

    /// Record the installation events `plan` makes from the installations as they stand, as one
    /// commit that fails if anyone else wrote since they were read; then read again and re-plan, up
    /// to [`DIRECTORY_ATTEMPTS`] times. Plans are idempotent (registering what is registered makes
    /// nothing), so a commit whose answer was lost re-plans to nothing. A [`people::installations::Refused`]
    /// from `plan` comes back as the error, for the caller to downcast. Returns what was written.
    pub async fn append_installations_with<F>(
        &self,
        plan: F,
    ) -> Result<Vec<people::installations::Event>>
    where
        F: Fn(
            &std::collections::BTreeMap<String, people::installations::Installation>,
        ) -> Result<Vec<people::installations::Event>, people::installations::Refused>,
    {
        self.ensure_table(SYSTEM_NS, INSTALLATIONS, installations_schema()?)
            .await?;
        for attempt in 0..DIRECTORY_ATTEMPTS {
            let table = self
                .catalog()
                .load_table(&TableIdent::new(
                    NamespaceIdent::new(SYSTEM_NS.into()),
                    INSTALLATIONS.into(),
                ))
                .await?;
            let mut current = vec![];
            for b in scan_rows(&table, None, None).await? {
                current.extend(installation_batch(&b));
            }
            current.sort_by_key(|e| e.at);
            let mut events = plan(&people::installations::fold(&current))?;
            if events.is_empty() {
                return Ok(events);
            }
            let start = self.now().max(current.last().map_or(0, |e| e.at) + 1);
            for (i, e) in events.iter_mut().enumerate() {
                e.at = start + i as i64;
            }
            let col = |f: &dyn Fn(&people::installations::Event) -> Option<&str>| -> ArrayRef {
                Arc::new(StringArray::from(events.iter().map(f).collect::<Vec<_>>()))
            };
            let columns = vec![
                ("at", ts(events.iter().map(|e| Some(e.at)))),
                ("actor", col(&|e| Some(e.actor.as_str()))),
                ("op", col(&|e| Some(e.op.as_str()))),
                (
                    "installation_id",
                    col(&|e| Some(e.installation_id.as_str())),
                ),
                ("provider", col(&|e| e.provider.as_deref())),
                ("group_ref", col(&|e| e.group_ref.as_deref())),
                ("on_behalf_of", col(&|e| e.on_behalf_of.as_deref())),
            ];
            match append(&*self.catalog(), &table, columns, OnConflict::Fail).await {
                Ok(()) => {
                    self.invalidate(SYSTEM_NS, INSTALLATIONS);
                    return Ok(events);
                }
                Err(e) if is_commit_conflict(&e) => {
                    let ms = 5
                        + (uuid::Uuid::new_v4().as_u128() % 40) as u64
                            * (attempt.min(8) as u64 + 1);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
        anyhow::bail!(
            "the installations kept changing under this change ({DIRECTORY_ATTEMPTS} tries): try again"
        )
    }

    /// Append directory changes as they are, re-stamped after the last change there: for changes made
    /// by an engine that plans them elsewhere. A directory change is [`directory::apply`].
    pub async fn append_directory(&self, events: &[directory::Event]) -> Result<()> {
        self.append_directory_with(|_| Ok(events.to_vec())).await?;
        Ok(())
    }

    /// The directory table's appends are a compare-and-set on its snapshot (see
    /// [`Store::ensure_ledger_cas`]): iceberg-rust's own retry would commit a change planned against
    /// a directory that has since changed.
    async fn ensure_directory_cas(&self) -> Result<()> {
        let table = self.table(SYSTEM_NS, DIRECTORY).await?;
        let key = TableProperties::PROPERTY_COMMIT_NUM_RETRIES;
        if table.metadata().properties().get(key).map(String::as_str) != Some("0") {
            let tx = Transaction::new(&table);
            let r = tx
                .update_table_properties()
                .set(key.into(), "0".into())
                .apply(tx)?
                .commit(&*self.catalog())
                .await;
            // Another server setting it at the same time is as good.
            if let Err(e) = r {
                let now = self.table(SYSTEM_NS, DIRECTORY).await?;
                if now.metadata().properties().get(key).map(String::as_str) != Some("0") {
                    return Err(e).context("turn off automatic commit retry on the directory");
                }
            }
        }
        Ok(())
    }

    /// Write a snapshot of the directory as it is now (`memory_system.directory_snapshot`): loading
    /// then folds only the changes after it. Returns it.
    pub async fn snapshot_directory(&self, actor: &str) -> Result<directory::Snapshot> {
        let f = self.directory_loaded_after(Some(Instant::now())).await?;
        if let Some((version, _)) = f.blocked {
            anyhow::bail!(
                "the directory holds {}: snapshot it with an engine that reads it",
                directory::blocked_why(version)
            );
        }
        let snap = directory::Snapshot::of(&f, actor, self.now()).sealed_with(self.directory_key());
        self.append_directory_snapshot(&snap).await?;
        self.dir
            .snapshot_events
            .fetch_max(snap.events, std::sync::atomic::Ordering::Relaxed);
        Ok(snap)
    }

    /// Append `snap` to the snapshot table as it is. A snapshot is loaded only if it accounts for
    /// exactly the log's changes up to its last (else the log is replayed), and
    /// [`Store::check_directory`] says whether it agrees with the log; [`Store::snapshot_directory`]
    /// is how one is made.
    pub async fn append_directory_snapshot(&self, snap: &directory::Snapshot) -> Result<()> {
        self.ensure_table(SYSTEM_NS, DIRECTORY_SNAPSHOT, directory_snapshot_schema()?)
            .await?;
        let table = self.table(SYSTEM_NS, DIRECTORY_SNAPSHOT).await?;
        let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        let n = |v: i64| -> ArrayRef { Arc::new(Int64Array::from(vec![v])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("at", ts([Some(snap.at)])),
                ("events", n(snap.events as i64)),
                ("digest", s(&snap.digest)),
                ("format", n(snap.format)),
                ("state", s(&snap.state)),
                ("taken_at", ts([Some(snap.taken_at)])),
                ("actor", s(&snap.actor)),
                (
                    "seal",
                    Arc::new(StringArray::from(vec![snap.seal.as_deref()])) as ArrayRef,
                ),
            ],
            OnConflict::Recommit,
        )
        .await
    }

    /// The newest directory snapshot written, of the format this engine reads, if any. Only the rows'
    /// small columns are read to find it; then that row.
    pub async fn directory_snapshot(&self) -> Result<Option<directory::Snapshot>> {
        let ident = TableIdent::new(
            NamespaceIdent::new(SYSTEM_NS.into()),
            DIRECTORY_SNAPSHOT.into(),
        );
        if !self.has_table(&ident).await? {
            return Ok(None);
        }
        let table = self.table(SYSTEM_NS, DIRECTORY_SNAPSHOT).await?;
        if table.metadata().current_snapshot().is_none() {
            return Ok(None);
        }
        // The newest written; never one from the future (a clock that far ahead, or a forged row).
        let latest_ok = self.now() + 60_000_000;
        let mut newest: Option<(i64, i64, i64)> = None;
        let small = ["at", "events", "format", "taken_at"];
        for b in scan_rows(&table, Some(&small), None).await? {
            let (at, events, format, taken) = (
                timestamps(&b, "at"),
                longs(&b, "events"),
                longs(&b, "format"),
                timestamps(&b, "taken_at"),
            );
            for i in 0..b.num_rows() {
                let (Some(at), Some(n), Some(t)) = (at[i], events[i], taken[i]) else {
                    continue;
                };
                if format[i] == Some(directory::SNAPSHOT_FORMAT)
                    && at <= latest_ok
                    && t <= latest_ok
                    && newest.is_none_or(|w| (t, at, n) > w)
                {
                    newest = Some((t, at, n));
                }
            }
        }
        let Some((t, at, n)) = newest else {
            return Ok(None);
        };
        let filter = iceberg::expr::Reference::new("at")
            .equal_to(iceberg::spec::Datum::timestamptz_micros(at));
        for b in scan_rows(&table, None, Some(filter)).await? {
            let (ats, events, format) = (
                timestamps(&b, "at"),
                longs(&b, "events"),
                longs(&b, "format"),
            );
            let (digest, state, taken, actor, seal) = (
                strings(&b, "digest"),
                strings(&b, "state"),
                timestamps(&b, "taken_at"),
                strings(&b, "actor"),
                optional_strings(&b, "seal"),
            );
            for i in 0..b.num_rows() {
                if ats[i] == Some(at)
                    && events[i] == Some(n)
                    && taken[i] == Some(t)
                    && format[i] == Some(directory::SNAPSHOT_FORMAT)
                {
                    return Ok(Some(directory::Snapshot {
                        at,
                        events: n as u64,
                        digest: digest[i].clone(),
                        format: directory::SNAPSHOT_FORMAT,
                        state: state[i].clone(),
                        taken_at: t,
                        actor: actor[i].clone(),
                        seal: seal[i].clone(),
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Whether the newest snapshot agrees with replaying the whole log, read now.
    pub async fn check_directory(&self) -> Result<directory::SnapshotCheck> {
        let events = self.directory_events().await?;
        let snap = self.directory_snapshot().await?;
        Ok(directory::check_snapshot(&events, snap.as_ref()))
    }

    /// When each service key was last used, by its sha256: µs since the epoch, the latest of the
    /// usage rows servers flushed. Empty before the first.
    pub async fn key_usage(&self) -> Result<HashMap<String, i64>> {
        let batches = self.side_rows(SYSTEM_NS, KEY_USAGE).await?;
        let mut out: HashMap<String, i64> = HashMap::new();
        for b in batches.iter() {
            let (sha, at) = (strings(b, "sha256"), timestamps(b, "used_at"));
            for i in 0..b.num_rows() {
                let at = at[i].unwrap_or_default();
                let e = out.entry(sha[i].clone()).or_default();
                *e = (*e).max(at);
            }
        }
        Ok(out)
    }

    /// The usage book as last written (`crate::usage`): each hour's newest row, by the hour's start.
    pub async fn usage_hours(&self) -> Result<Vec<(chrono::DateTime<chrono::Utc>, usage::Bucket)>> {
        let batches = self.side_rows(SYSTEM_NS, USAGE_HOURS).await?;
        let mut newest: HashMap<i64, (i64, String)> = HashMap::new();
        for b in batches.iter() {
            let (hour, written, record) = (
                timestamps(b, "hour"),
                timestamps(b, "written_at"),
                strings(b, "record"),
            );
            for i in 0..b.num_rows() {
                let (Some(h), w) = (hour[i], written[i].unwrap_or_default()) else {
                    continue;
                };
                if newest.get(&h).is_none_or(|(at, _)| w >= *at) {
                    newest.insert(h, (w, record[i].clone()));
                }
            }
        }
        let mut out = vec![];
        for (h, (_, record)) in newest {
            let Some(hour) = chrono::DateTime::from_timestamp_micros(h) else {
                continue;
            };
            match serde_json::from_str(&record) {
                Ok(b) => out.push((hour, b)),
                Err(e) => eprintln!("usage: the {hour} row doesn't read ({e}); skipped"),
            }
        }
        out.sort_by_key(|(h, _)| *h);
        Ok(out)
    }

    /// Write hours of the usage book, one append for all.
    pub async fn append_usage_hours(
        &self,
        hours: &[(chrono::DateTime<chrono::Utc>, usage::Bucket)],
    ) -> Result<()> {
        if hours.is_empty() {
            return Ok(());
        }
        self.ensure_table(SYSTEM_NS, USAGE_HOURS, usage_hours_schema()?)
            .await?;
        self.no_side_table
            .lock()
            .unwrap()
            .remove(&format!("{SYSTEM_NS}.{USAGE_HOURS}"));
        let now = now_micros();
        let records = hours
            .iter()
            .map(|(_, b)| serde_json::to_string(b))
            .collect::<Result<Vec<_>, _>>()?;
        let table = self.table(SYSTEM_NS, USAGE_HOURS).await?;
        append(
            &*self.catalog(),
            &table,
            vec![
                (
                    "hour",
                    ts(hours.iter().map(|(h, _)| Some(h.timestamp_micros()))),
                ),
                ("written_at", ts(hours.iter().map(|_| Some(now)))),
                (
                    "record",
                    Arc::new(StringArray::from(
                        records.iter().map(|r| Some(r.as_str())).collect::<Vec<_>>(),
                    )) as ArrayRef,
                ),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(SYSTEM_NS, USAGE_HOURS);
        Ok(())
    }

    /// Record that keys were used: `(sha256, org, name, µs since the epoch)`, one append for all. The uses of a
    /// key the directory, read now, no longer has in that org (the org deleted, or created again since) are left
    /// out ([`audit::key_use_stays`]); any whose org's delete committed while they were written are taken out
    /// again, as the audit's are ([`Store::append_audit`]).
    pub async fn append_key_usage(&self, uses: &[KeyUse]) -> Result<()> {
        if uses.is_empty() {
            return Ok(());
        }
        let dir = directory::Directory::fresh(self).await?;
        let uses: Vec<KeyUse> = uses
            .iter()
            .filter(|u| audit::key_use_stays(&dir, &u.1, &u.0))
            .cloned()
            .collect();
        if uses.is_empty() {
            return Ok(());
        }
        self.ensure_table(SYSTEM_NS, KEY_USAGE, key_usage_schema()?)
            .await?;
        self.no_side_table
            .lock()
            .unwrap()
            .remove(&format!("{SYSTEM_NS}.{KEY_USAGE}"));
        let col = |f: &dyn Fn(&KeyUse) -> &str| -> ArrayRef {
            Arc::new(StringArray::from(
                uses.iter().map(|u| Some(f(u))).collect::<Vec<_>>(),
            ))
        };
        let table = self.table(SYSTEM_NS, KEY_USAGE).await?;
        append(
            &*self.catalog(),
            &table,
            vec![
                ("sha256", col(&|u| u.0.as_str())),
                ("org", col(&|u| u.1.as_str())),
                ("name", col(&|u| u.2.as_str())),
                ("used_at", ts(uses.iter().map(|u| Some(u.3)))),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(SYSTEM_NS, KEY_USAGE);
        let written: BTreeSet<(String, String)> =
            uses.iter().map(|u| (u.1.clone(), u.0.clone())).collect();
        self.unwrite_gone(KEY_USAGE, written).await;
        Ok(())
    }

    pub async fn acl_roles(&self, scope: &Scope) -> Result<Vec<String>> {
        let vis = self.visibility(scope, MAIN).await?;
        let roles: BTreeSet<String> = vis
            .acls
            .values()
            .flatten()
            .filter_map(|a| a.strip_prefix("role:"))
            .map(String::from)
            .collect();
        Ok(roles.into_iter().collect())
    }

    pub async fn visibility(&self, scope: &Scope, branch: &str) -> Result<Arc<Vis>> {
        self.visibility_at(scope, branch, None).await
    }

    /// What `branch` sees: now, or, given `pinned` (the pin's name and the heads it holds), as of those
    /// heads: every commit reachable from them and nothing committed since. The ledger is append-only, so
    /// the same heads always give the same commits.
    pub async fn visibility_at(
        &self,
        scope: &Scope,
        branch: &str,
        pinned: Option<(&str, &[String])>,
    ) -> Result<Arc<Vis>> {
        let ns = scope.namespace();
        let (ledger, episodes, quotes, schemas, closures, retractions, retention) = tokio::try_join!(
            self.rows(SYSTEM_NS, LEDGER),
            self.rows(&ns, EPISODES),
            self.rows(&ns, QUOTES),
            self.side_rows(&ns, SCHEMAS),
            self.side_rows(&ns, CLOSURES),
            self.side_rows(&ns, RETRACTIONS),
            self.side_rows(&ns, RETENTION)
        )?;
        let stamp = [
            ledger.rows.clone(),
            episodes.rows.clone(),
            quotes.rows.clone(),
            schemas.clone(),
            closures.clone(),
            retractions.clone(),
            retention.clone(),
        ];
        let key = match pinned {
            None => (ns.clone(), branch.to_string()),
            // No branch name holds '@'.
            Some((pin, _)) => (ns.clone(), format!("{branch}@{pin}")),
        };
        // Episodes age out with no write to mark it: past `fresh_until` the state is rebuilt.
        let cached = || {
            self.vis
                .lock()
                .unwrap()
                .get(&key)
                .filter(|(s, v)| {
                    s.iter().zip(&stamp).all(|(a, b)| Arc::ptr_eq(a, b))
                        && v.fresh_until.is_none_or(|t| now_micros() < t)
                })
                .map(|(_, v)| v.clone())
        };
        if let Some(v) = cached() {
            return Ok(v);
        }
        // One build of this state at a time: a reader that waited for another's build takes it, if it was
        // built from the same tables this reader read.
        let gate = self
            .vis_builds
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        // However this reader ends (built, failed, or dropped mid-build by a client that gave up), the gate
        // leaves the map: a reader waiting on it still holds it, and one arriving later finds the state cached
        // or builds it under a new gate. Keys include every followed cursor's pin, so none may stay behind.
        let _gone = GateGone {
            builds: &self.vis_builds,
            key: key.clone(),
            gate: gate.clone(),
        };
        let _building = gate.lock().await;
        if let Some(v) = cached() {
            return Ok(v);
        }
        // The state this one follows, to tell whether memory lost anything since (`Vis::removals`). A
        // pinned read's state follows no other.
        let prev = match pinned {
            None => self.vis.lock().unwrap().get(&key).map(|(_, v)| v.clone()),
            Some(_) => None,
        };
        let prev_removals = prev.as_ref().map(|p| p.removals);
        let scope_ns = ns.clone();
        let scope_ns_log = ns.clone();
        let branch = branch.to_string();
        let heads = pinned.map(|(_, h)| h.to_vec());
        let vis = tokio::task::spawn_blocking(move || {
            let mut actors: HashMap<String, String> = HashMap::new();
            for b in ledger.rows.iter() {
                let (commit, actor, sc) = (
                    strings(b, "commit_id"),
                    strings(b, "actor"),
                    strings(b, "scope"),
                );
                for i in 0..b.num_rows() {
                    if sc[i] == ns {
                        actors.insert(commit[i].clone(), actor[i].clone());
                    }
                }
            }
            let ledger = Ledger::from_batches(&ledger.rows, &ns);
            let pinned_read = heads.is_some();
            let mut committed = match &heads {
                None => ledger.visible(&branch),
                Some(h) => ledger.reachable(h),
            };
            let mut heads = heads.unwrap_or_else(|| ledger.heads(&branch));
            heads.sort();
            // Who may read what is main's current word, whatever is pinned: a delete at the source, an ACL
            // narrowed, a memory forgotten are made on main and hold on every branch and every pinned
            // read. A branch made before them, or a `/query` cursor minted (or replayed, or forged) from
            // older heads, must not read through them.
            let on_main = match branch == MAIN && !pinned_read {
                true => None,
                false => Some(ledger.visible(MAIN)),
            };
            let deleted =
                |c: &str| committed.contains(c) || on_main.as_ref().is_some_and(|m| m.contains(c));
            // A version superseded on main and written again since is current again here too, as a delete
            // on main holds here.
            let times = ledger.times();
            let Retracted {
                hidden: mut retracted,
                targets: retraction_targets,
                superseded,
                revived,
            } = retracted(&retractions, &episodes.rows, &deleted, &|c| {
                times.get(c).copied()
            });
            // Facts retracted by id, and extractors withdrawn: like a delete, main's word holds everywhere. A
            // withdrawal takes back every commit the extractor made before it, so a re-run after it stands.
            let (retracted_facts, withdrawals) = retracted_writes(&retractions, &deleted);
            // A withdrawal takes the extractor's commits that came before it: its ancestors in the ledger,
            // whatever the writers' clocks said.
            let mut withdrawn: HashSet<String> = HashSet::new();
            for (prefix, at_commit) in &withdrawals {
                for c in ledger.reachable(std::slice::from_ref(at_commit)) {
                    if c != *at_commit
                        && committed.contains(&c)
                        && actors.get(&c).is_some_and(|a| withdraws(prefix, a))
                    {
                        withdrawn.insert(c);
                    }
                }
            }
            // Retention is the scope's, set on main: it holds on every branch.
            let retention = newest_retention(&retention, &deleted)?;
            committed.retain(|c| !withdrawn.contains(c));
            let (aged, fresh_until) = expired(&episodes.rows, &retention, now_micros());
            let replaced_aged: HashSet<String> = aged
                .iter()
                .filter(|e| superseded.contains_key(*e))
                .cloned()
                .collect();
            retracted.extend(aged);
            // Each episode this view holds, under its newest ACL here or on main.
            let mut acls = match &on_main {
                None => Self::episode_acls(&episodes.rows, &committed),
                Some(main) => {
                    let held = Self::episode_acls(&episodes.rows, &committed);
                    let governing: HashSet<String> = committed.union(main).cloned().collect();
                    let mut now = Self::episode_acls(&episodes.rows, &governing);
                    now.retain(|e, _| held.contains_key(e));
                    now
                }
            };
            let replaced_acls: HashMap<String, Vec<String>> = superseded
                .keys()
                .filter_map(|e| Some((e.clone(), acls.get(e)?.clone())))
                .collect();
            acls.retain(|e, _| !retracted.contains(e));
            let documents = Documents::build(&episodes.rows, &committed);
            let mut kinds = HashMap::new();
            let mut merges = HashSet::new();
            let mut conflict_records = HashSet::new();
            let replaced_sources: HashSet<&String> = superseded.values().collect();
            let mut source_acls: HashMap<String, Vec<Vec<String>>> = HashMap::new();
            for b in episodes.rows.iter() {
                let (commit, id, source) = (
                    strings(b, "commit_id"),
                    strings(b, "episode_id"),
                    strings(b, "source"),
                );
                for i in 0..b.num_rows() {
                    if !acls.contains_key(&id[i]) || !committed.contains(&commit[i]) {
                        continue;
                    }
                    if let Some(t) = episode_types::name_of_source(&source[i]) {
                        kinds.insert(id[i].clone(), t);
                    }
                    if source[i].starts_with(MERGE_SOURCE) {
                        merges.insert(id[i].clone());
                    }
                    if source[i].starts_with(conflicts::SOURCE) {
                        conflict_records.insert(id[i].clone());
                    }
                    if replaced_sources.contains(&source[i]) {
                        source_acls
                            .entry(source[i].clone())
                            .or_default()
                            .push(acls[&id[i]].clone());
                    }
                }
            }
            let conflicts = conflicts::recorded(&documents.versions, &conflict_records);
            let mut latest: HashMap<String, (i64, String)> = HashMap::new();
            for b in quotes.rows.iter() {
                let (commit, fid, q) = (
                    strings(b, "commit_id"),
                    strings(b, "fact_id"),
                    strings(b, "quote"),
                );
                let at = timestamps(b, "recorded_at");
                for i in 0..b.num_rows() {
                    let t = at[i].unwrap_or_default();
                    if committed.contains(&commit[i])
                        && latest.get(&fid[i]).is_none_or(|(seen, _)| t >= *seen)
                    {
                        latest.insert(fid[i].clone(), (t, q[i].clone()));
                    }
                }
            }
            let quote_of = latest.into_iter().map(|(f, (_, q))| (f, q)).collect();
            // The branch's schemas, oldest first: its latest committed row is its schema.
            let mut rows: Vec<(i64, String, String)> = vec![];
            for b in schemas.iter() {
                let (commit, body) = (strings(b, "commit_id"), strings(b, "body"));
                let at = timestamps(b, "recorded_at");
                for i in 0..b.num_rows() {
                    if committed.contains(&commit[i]) {
                        rows.push((
                            at[i].unwrap_or_default(),
                            commit[i].clone(),
                            body[i].clone(),
                        ));
                    }
                }
            }
            rows.sort();
            let schemas = rows
                .into_iter()
                .map(|(_, _, body)| schema::Schema::parse(&body).map(Arc::new))
                .collect::<Result<Vec<_>>>()?;
            let schema = schemas.last().cloned();
            // Closures: every committed row per fact, newest recorded first. Which one applies
            // depends on the reader (see `Vis::valid_to`).
            let mut ends: HashMap<String, Vec<(i64, i64, String)>> = HashMap::new();
            for b in closures.iter() {
                let (commit, fid, episode) = (
                    strings(b, "commit_id"),
                    strings(b, "fact_id"),
                    strings(b, "episode_id"),
                );
                let (end, at) = (timestamps(b, "valid_to"), timestamps(b, "recorded_at"));
                for i in 0..b.num_rows() {
                    if let Some(end) = end[i]
                        && committed.contains(&commit[i])
                    {
                        let t = at[i].unwrap_or_default();
                        ends.entry(fid[i].clone())
                            .or_default()
                            .push((t, end, episode[i].clone()));
                    }
                }
            }
            let closures = ends
                .into_iter()
                .map(|(f, mut rows)| {
                    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
                    (f, rows.into_iter().map(|(_, end, ep)| (end, ep)).collect())
                })
                .collect();
            actors.retain(|c, _| committed.contains(c));
            let mut vis = Vis {
                committed,
                index: auth::AclIndex::build(&acls),
                acls,
                quotes: quote_of,
                schema,
                schemas,
                closures,
                merges,
                conflicts,
                documents,
                kinds,
                retracted,
                superseded,
                revived,
                retracted_facts,
                retraction_targets,
                retention,
                fresh_until,
                heads,
                actors,
                removals: 0,
                source_acls,
                replaced_acls,
                replaced_aged,
            };
            vis.removals = match &prev {
                Some(p) => match vis.lost_since(p) {
                    None => p.removals,
                    Some(why) => {
                        eprintln!(
                            "memstore: {scope_ns_log} lost something ({why}): its kept metric answers go"
                        );
                        fresh_removals()
                    }
                },
                None => fresh_removals(),
            };
            Ok::<_, anyhow::Error>(Arc::new(vis))
        })
        .await??;
        // Main's memory lost something: what was kept past new writes from before goes.
        if key.1 == MAIN && prev_removals != Some(vis.removals) {
            query::drop_lasting(&scope_ns, vis.removals);
        }
        let mut cached = self.vis.lock().unwrap();
        // Pages pin their first page's heads ([`Session::resume`]): keep a few such states per scope, not one
        // per cursor ever followed.
        if key.1.contains("@page-") {
            let paged = cached
                .keys()
                .filter(|(n, b)| *n == key.0 && b.contains("@page-"))
                .count();
            if paged >= 32 {
                cached.retain(|(n, b), _| *n != key.0 || !b.contains("@page-"));
            }
        }
        cached.insert(key, (stamp, vis.clone()));
        Ok(vis)
    }

    /// A side table's rows (schemas, closures); none when the scope predates the table and no one
    /// has evolved it with `ensure_scope` yet.
    pub async fn side_rows(&self, ns: &str, name: &str) -> Result<Arc<Vec<RecordBatch>>> {
        static NONE: std::sync::LazyLock<Arc<Vec<RecordBatch>>> =
            std::sync::LazyLock::new(Arc::default);
        let key = format!("{ns}.{name}");
        let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
        let known = self.cache.entries.lock().unwrap().contains_key(&key);
        if !known {
            let checked = self.no_side_table.lock().unwrap().get(&key).copied();
            if checked.is_some_and(|at| at.elapsed() < self.max_staleness) {
                return Ok(NONE.clone());
            }
            if !self.has_table(&ident).await? {
                self.no_side_table
                    .lock()
                    .unwrap()
                    .insert(key, Instant::now());
                return Ok(NONE.clone());
            }
        }
        Ok(self.rows(ns, name).await?.rows)
    }

    /// The facts table's plan for `ns`: its table and data files. Same policy as `rows`: the cached plan,
    /// refreshed in the background once older than the staleness window.
    async fn facts_plan(&self, ns: &str) -> Result<(Table, Vec<FileScanTask>)> {
        let ns = ns.to_string();
        // Same policy as `rows`: serve the cached plan and refresh it in the background once
        // older than the staleness window; block only when cold, after an own write (the entry
        // is removed), or past the hard staleness limit.
        let planned = self.plans.lock().unwrap().get(&ns).cloned();
        Ok(match planned {
            Some((at, table, tasks)) if at.elapsed() < self.hard_staleness => {
                if at.elapsed() >= self.max_staleness
                    && self
                        .cache
                        .refreshing
                        .lock()
                        .unwrap()
                        .insert(format!("plan:{ns}"))
                {
                    let (catalog, plans, generations, cache) = (
                        self.catalog(),
                        self.plans.clone(),
                        self.plan_generation.clone(),
                        self.cache.clone(),
                    );
                    let ns = ns.clone();
                    tokio::spawn(async move {
                        let started = generations.lock().unwrap().get(&ns).copied();
                        match load_plan(&*catalog, &ns).await {
                            Ok((t, tasks))
                                if generations.lock().unwrap().get(&ns).copied() == started =>
                            {
                                plans
                                    .lock()
                                    .unwrap()
                                    .insert(ns.clone(), (Instant::now(), t, tasks));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("memstore: background plan refresh of {ns} failed: {e:#}")
                            }
                        }
                        cache
                            .refreshing
                            .lock()
                            .unwrap()
                            .remove(&format!("plan:{ns}"));
                    });
                }
                (table, tasks)
            }
            _ => {
                let started = self.plan_generation.lock().unwrap().get(&ns).copied();
                let (table, tasks) = load_plan(&*self.catalog(), &ns).await?;
                // Our own write landed meanwhile: use this plan once, but don't cache it.
                if self.plan_generation.lock().unwrap().get(&ns).copied() == started {
                    self.plans
                        .lock()
                        .unwrap()
                        .insert(ns.clone(), (Instant::now(), table.clone(), tasks.clone()));
                }
                (table, tasks)
            }
        })
    }

    /// The facts table's data files for `scope` by file key, with the sequence number of the snapshot
    /// they're from (0 for none), as one plan holds them: what a later snapshot no longer holds was
    /// rewritten away.
    pub async fn fact_files_at(&self, scope: &Scope) -> Result<(i64, Vec<String>)> {
        let (table, tasks) = self.facts_plan(&scope.namespace()).await?;
        let seq = table
            .metadata()
            .current_snapshot()
            .map_or(0, |s| s.sequence_number());
        Ok((seq, tasks.iter().map(file_key).collect()))
    }

    /// Rows of each of the facts table's current data files, by file key, as its metadata counts them (before
    /// any rows a delete file drops); none if a file's count isn't recorded.
    pub async fn fact_file_rows(&self, scope: &Scope) -> Result<Option<HashMap<String, u64>>> {
        let (_, tasks) = self.facts_plan(&scope.namespace()).await?;
        Ok(tasks
            .iter()
            .map(|t| t.record_count.map(|n| (file_key(t), n)))
            .collect())
    }

    /// The facts table's current data files, by file key (path plus delete files): the set of
    /// all current keys, and the decoded rows of those not in `skip`. A file that gained row
    /// deletes gets a new key, so anything derived from its old rows reads as stale. Metadata
    /// is reused within the staleness window and decoded files are cached until they leave the
    /// current snapshot or get indexed.
    pub async fn fact_files(
        &self,
        scope: &Scope,
        skip: &HashSet<String>,
    ) -> Result<(HashSet<String>, Vec<(String, Arc<Vec<RecordBatch>>)>)> {
        let ns = scope.namespace();
        let (table, tasks) = self.facts_plan(&ns).await?;
        let current: HashSet<String> = tasks.iter().map(file_key).collect();
        let mut cached = self.tail.lock().unwrap().remove(&ns).unwrap_or_default();
        // The read cache may already hold some of these files decoded.
        if let Some(e) = self
            .cache
            .entries
            .lock()
            .unwrap()
            .get(&format!("{ns}.{FACTS}"))
        {
            for (k, b) in &e.files {
                cached.entry(k.clone()).or_insert_with(|| b.clone());
            }
        }
        let t_tail = Instant::now();
        let read_now = std::sync::atomic::AtomicUsize::new(0);
        let reads = tasks
            .into_iter()
            .map(|t| (file_key(&t), t))
            .filter(|(k, _)| !skip.contains(k))
            .map(|(k, task)| {
                let hit = cached.get(&k).cloned();
                let table = &table;
                let read_now = &read_now;
                async move {
                    if let Some(b) = hit {
                        return Ok::<_, anyhow::Error>((k, b));
                    }
                    read_now.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok((k, Arc::new(read_task(table, task).await?)))
                }
            });
        let tail = futures::future::try_join_all(reads).await?;
        // Unindexed files read from storage: how many, and how long, when it was slow.
        let read_now = read_now.into_inner();
        if t_tail.elapsed() > Duration::from_secs(1) {
            eprintln!(
                "memstore: slow tail of {ns}: {} ms reading {read_now} of {} unindexed files",
                t_tail.elapsed().as_millis(),
                tail.len()
            );
        }
        self.tail
            .lock()
            .unwrap()
            .insert(ns, tail.iter().cloned().collect());
        Ok((current, tail))
    }

    /// The episodes table's current data files, by file key (path plus delete files), decoded.
    pub async fn episode_files(
        &self,
        scope: &Scope,
    ) -> Result<HashMap<String, Arc<Vec<RecordBatch>>>> {
        let ns = scope.namespace();
        self.rows(&ns, EPISODES).await?;
        Ok(self
            .cache
            .entries
            .lock()
            .unwrap()
            .get(&format!("{ns}.{EPISODES}"))
            .map(|e| e.files.clone())
            .unwrap_or_default())
    }

    /// Bodies of these episodes, read concurrently; an unreadable body is left out.
    pub async fn episode_bodies(
        &self,
        scope: &Scope,
        ids: Vec<String>,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let table = self.rows(&scope.namespace(), EPISODES).await?.table;
        let reads = ids.into_iter().map(|id| {
            let (io, path) = (table.file_io().clone(), body_path(&table, &id));
            async move {
                let body =
                    with_retries(|| async { Ok(io.new_input(path.clone())?.read().await?) }).await;
                (id, body.ok().map(|b| b.to_vec()))
            }
        });
        Ok(futures::stream::iter(reads)
            .buffer_unordered(64)
            .filter_map(|(id, b)| async move { b.map(|b| (id, b)) })
            .collect()
            .await)
    }

    /// An episode's body, if it is committed on main and the principal may see it.
    pub async fn episode_body(
        &self,
        scope: &Scope,
        who: &Principal,
        episode_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        // The cached visibility state holds main's committed episode ACLs; rebuilding them here
        // per call cost ~20 ms on a 14k-episode scope, up to 8 times per search with snippets.
        self.pin(scope, MAIN)
            .await?
            .episode_body(self, who, episode_id)
            .await
    }

    /// When each of `ids` happened (its `occurred_at`), for those committed on main.
    pub async fn episode_times(
        &self,
        scope: &Scope,
        ids: &[String],
    ) -> Result<HashMap<String, i64>> {
        let ns = scope.namespace();
        let vis = self.visibility(scope, MAIN).await?;
        let mut out = HashMap::new();
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id) = (strings(b, "commit_id"), strings(b, "episode_id"));
            let at = timestamps(b, "occurred_at");
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i]) && ids.contains(&id[i]) {
                    out.insert(id[i].clone(), at[i].unwrap_or_default());
                }
            }
        }
        Ok(out)
    }

    /// The fact `fact_id` as `who` reads it on main, ended or not; None when there is none or they may not
    /// read it. A closure from them must name a fact they can see.
    pub async fn fact_seen(
        &self,
        scope: &Scope,
        who: &Principal,
        fact_id: &str,
    ) -> Result<Option<Fact>> {
        let ns = scope.namespace();
        self.invalidate(&ns, FACTS);
        let (vis, facts) = tokio::try_join!(self.visibility(scope, MAIN), self.rows(&ns, FACTS))?;
        Ok(facts
            .rows
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .find(|(c, f)| f.fact_id == fact_id && vis.allows(c, f, who, None))
            .map(|(_, f)| vis.annotate(f, who)))
    }

    /// The facts of `ids` that `who` may read on main, by id, in one pass over the facts as cached: a lookup
    /// never empties the cache other reads depend on, so a fact committed a moment ago may not be found yet.
    pub async fn facts_seen(
        &self,
        scope: &Scope,
        who: &Principal,
        ids: &BTreeSet<String>,
    ) -> Result<HashMap<String, Fact>> {
        let ns = scope.namespace();
        let (vis, facts) = tokio::try_join!(self.visibility(scope, MAIN), self.rows(&ns, FACTS))?;
        Ok(facts
            .rows
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .filter(|(c, f)| ids.contains(&f.fact_id) && vis.allows(c, f, who, None))
            .map(|(_, f)| (f.fact_id.clone(), vis.annotate(f, who)))
            .collect())
    }

    /// The facts on main `who` may read that cite an episode from `user`: one they wrote (a remembered note, their
    /// own learned advice: what forgetting them erases) or one of `also` (their conversations' transcripts),
    /// newest first, at most `limit`, and whether there were more. Studio's "by person" filter (`/admin {op:
    /// "user_facts"}`): one pass over the episodes and the facts as cached, only when asked, so nothing is kept
    /// per episode for it.
    pub async fn facts_from(
        &self,
        scope: &Scope,
        who: &Principal,
        user: &str,
        also: &HashSet<String>,
        limit: usize,
    ) -> Result<(Vec<Fact>, bool)> {
        let ns = scope.namespace();
        let (vis, episodes, facts) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.rows(&ns, EPISODES),
            self.rows(&ns, FACTS)
        )?;
        let mine = format!(
            "{PERSONAL_ADVICE_SOURCE}{}",
            auth::acl::norm_id(user.trim())
        );
        let mut from: HashSet<String> = also.clone();
        for b in episodes.rows.iter() {
            let (commit, id, source) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i])
                    && (remembered_by(&source[i], user) || source[i] == mine)
                {
                    from.insert(id[i].clone());
                }
            }
        }
        let mut out: Vec<Fact> = facts
            .rows
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .filter(|(c, f)| {
                f.source_episodes.iter().any(|e| from.contains(e)) && vis.allows(c, f, who, None)
            })
            .map(|(_, f)| vis.annotate(f, who))
            .collect();
        out.sort_by(|a, b| {
            b.valid_from
                .cmp(&a.valid_from)
                .then(a.fact_id.cmp(&b.fact_id))
        });
        out.dedup_by(|a, b| a.fact_id == b.fact_id);
        let more = out.len() > limit;
        out.truncate(limit);
        Ok((out, more))
    }

    /// Who wrote each committed fact of `scope` on main: the actor of the commit that added it (`extract:<model>`
    /// for the engine's extraction, the caller for `/facts`, an import's name for an import).
    pub async fn fact_authors(&self, scope: &Scope) -> Result<HashMap<String, String>> {
        let ns = scope.namespace();
        let (snap, vis, facts) = tokio::try_join!(
            self.ledger_now(),
            self.visibility(scope, MAIN),
            self.rows(&ns, FACTS)
        )?;
        let mut actor_of: HashMap<String, String> = HashMap::new();
        for b in snap.rows.iter() {
            let (commit, actor, sc) = (
                strings(b, "commit_id"),
                strings(b, "actor"),
                strings(b, "scope"),
            );
            for i in 0..b.num_rows() {
                if sc[i] == ns {
                    actor_of.insert(commit[i].clone(), actor[i].clone());
                }
            }
        }
        let mut out = HashMap::new();
        for b in facts.rows.iter() {
            let (commit, id) = (strings(b, "commit_id"), strings(b, "fact_id"));
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i])
                    && let Some(a) = actor_of.get(&commit[i])
                {
                    out.insert(id[i].clone(), a.clone());
                }
            }
        }
        Ok(out)
    }

    /// Episodes whose committed rows come from more than one source, each with its rows (source, ACL,
    /// recorded at), oldest first. Before remembered notes hashed their author into the id, the same text
    /// remembered by someone else (or already in memory from elsewhere) became a new row of the same
    /// episode, and the newest row's ACL wins: such an episode may now be readable more widely than its
    /// first writer chose. `memstore check collisions` lists them for an operator to re-ACL.
    pub async fn shared_episodes(
        &self,
        scope: &Scope,
    ) -> Result<Vec<(String, Vec<(String, Vec<String>, i64)>)>> {
        let ns = scope.namespace();
        self.invalidate(&ns, LEDGER);
        self.invalidate(&ns, EPISODES);
        let vis = self.visibility(scope, MAIN).await?;
        let mut rows: BTreeMap<String, Vec<(String, Vec<String>, i64)>> = BTreeMap::new();
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id, source, acl) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
                strings(b, "acl"),
            );
            let at = timestamps(b, "recorded_at");
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i]) {
                    rows.entry(id[i].clone()).or_default().push((
                        source[i].clone(),
                        split_list(&acl[i]),
                        at[i].unwrap_or_default(),
                    ));
                }
            }
        }
        Ok(rows
            .into_iter()
            .filter(|(_, r)| r.iter().any(|x| x.0 != r[0].0))
            .map(|(id, mut r)| {
                r.sort_by_key(|x| x.2);
                (id, r)
            })
            .collect())
    }

    /// Forget an episode its author wants gone: its ACL becomes [`FORGOTTEN`], an entry nobody holds, so
    /// no one reads it or the facts that cite it from the next read on (as a source's ACL change does).
    /// Nothing is deleted: the ledger keeps the change and an operator can restore the episode with
    /// [`Store::change_acl`]. Only an episode every committed row of which has a source `is_author` accepts
    /// (the source records who wrote it) is forgotten: one row from anyone else and it is not. Any other, or
    /// none, is `false`, so a caller learns nothing of others'.
    pub async fn forget(
        &self,
        scope: &Scope,
        actor: &str,
        episode_id: &str,
        is_author: impl Fn(&str) -> bool,
    ) -> Result<bool> {
        let ns = scope.namespace();
        self.invalidate(SYSTEM_NS, LEDGER);
        self.invalidate(&ns, EPISODES);
        let vis = self.visibility(scope, MAIN).await?;
        let (mut rows, mut theirs) = (0, true);
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id, src) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            for i in 0..b.num_rows() {
                if id[i] == episode_id && vis.committed.contains(&commit[i]) {
                    rows += 1;
                    theirs &= is_author(&src[i]);
                }
            }
        }
        if rows == 0 || !theirs {
            return Ok(false);
        }
        self.change_acl(
            scope,
            actor,
            &AclTarget::Episode(episode_id.to_string()),
            &[FORGOTTEN.to_string()],
        )
        .await?;
        Ok(true)
    }

    /// The version of document `key` that `who` sees on main at `at` (microseconds since the epoch;
    /// `None` is now): the newest version they may read that occurred by then, as (episode id,
    /// occurred_at, body). `None` when they may read no version from by then.
    pub async fn document(
        &self,
        scope: &Scope,
        who: &Principal,
        key: &str,
        at: Option<i64>,
    ) -> Result<Option<(String, i64, Vec<u8>)>> {
        self.pin(scope, MAIN)
            .await?
            .document(self, who, key, at)
            .await
    }

    /// The keys of the documents under `prefix` that `who` sees a version of on main at `at`, sorted.
    pub async fn documents(
        &self,
        scope: &Scope,
        who: &Principal,
        prefix: &str,
        at: Option<i64>,
    ) -> Result<Vec<String>> {
        Ok(self.pin(scope, MAIN).await?.documents(who, prefix, at))
    }

    /// Each committed, unretracted episode's newest row on main (its current source, time, subjects and
    /// ACL; no body), read fresh: a change sent right after another must see it, or it could skip an
    /// episode that needs it. An older row that matches a target must not stand in for a newer one
    /// that doesn't, so targets are matched against these.
    pub async fn current_episodes(&self, scope: &Scope) -> Result<HashMap<String, Episode>> {
        let ns = scope.namespace();
        self.invalidate(SYSTEM_NS, LEDGER);
        self.invalidate(&ns, EPISODES);
        self.invalidate(&ns, RETRACTIONS);
        let vis = self.visibility(scope, MAIN).await?;
        let mut newest: HashMap<String, (i64, Episode)> = HashMap::new();
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id, source, have) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
                strings(b, "acl"),
            );
            let (at, occurred) = (timestamps(b, "recorded_at"), timestamps(b, "occurred_at"));
            let (about, indexed, document) = (
                optional_strings(b, "about"),
                optional_bools(b, "index_text"),
                optional_strings(b, "document"),
            );
            for i in 0..b.num_rows() {
                let at = at[i].unwrap_or(i64::MIN);
                // A retracted episode is gone: no ACL change brings it back.
                if !vis.committed.contains(&commit[i]) || vis.is_retracted(&id[i]) {
                    continue;
                }
                let row = Episode {
                    episode_id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: occurred[i].unwrap_or_default(),
                    acl: split_list(&have[i]),
                    about: about[i].as_deref().map(split_list).unwrap_or_default(),
                    index_text: indexed[i].unwrap_or(true),
                    body: vec![],
                    recorded_at: Some(at),
                    document: document[i].clone(),
                };
                match newest.get_mut(&id[i]) {
                    Some((seen, _)) if *seen > at => {}
                    // rows recorded at the same instant (one batch) are one state: their ACLs union
                    Some((seen, have)) if *seen == at => {
                        for a in row.acl {
                            if !have.acl.contains(&a) {
                                have.acl.push(a);
                            }
                        }
                    }
                    _ => {
                        newest.insert(id[i].clone(), (at, row));
                    }
                }
            }
        }
        Ok(newest.into_iter().map(|(id, (_, e))| (id, e)).collect())
    }

    /// What of `held` (stored episodes under a new ACL, refused as letting others read what an entity's owners
    /// must approve first, `governance::private_episodes`) takes readers away, now: each one's current readers
    /// the new ACL drops lose it at once (under [`FORGOTTEN`] when none stay), and only the widening waits.
    pub async fn revoke_held(&self, scope: &Scope, actor: &str, held: &[Episode]) -> Result<()> {
        let current = self.current_episodes(scope).await?;
        let norm = |acl: &[String]| -> BTreeSet<String> {
            acl.iter()
                .map(|a| auth::norm_entry(a))
                .filter(|a| a != FORGOTTEN)
                .collect()
        };
        let mut narrowed = Vec::new();
        for e in held {
            let Some(now) = current.get(&e.episode_id) else {
                continue;
            };
            let (had, want) = (norm(&now.acl), norm(&e.acl));
            let kept: Vec<String> = had.intersection(&want).cloned().collect();
            if kept.len() == had.len() {
                continue;
            }
            narrowed.push(Episode {
                acl: match kept.is_empty() {
                    true => vec![FORGOTTEN.to_string()],
                    false => kept,
                },
                // Ranks past the row it narrows, as `change_acl_all`'s do.
                recorded_at: Some(now_micros().max(now.recorded_at.unwrap_or(i64::MIN) + 1)),
                ..e.clone()
            });
        }
        if !narrowed.is_empty() {
            self.commit_full(scope, MAIN, actor, &narrowed, &[], &[], &[], None)
                .await?;
        }
        Ok(())
    }

    /// The source changed who may read some episodes (a Slack channel went private, a CRM record was
    /// reassigned): commit the new ACL for every committed episode `target` names, keeping each one's
    /// text, source, time and subjects. The newest ACL wins, so readers the new ACL leaves out lose the
    /// episodes (and the facts that cite them) on their next read. Returns how many episodes changed;
    /// episodes already carrying exactly this ACL are left alone, so replaying a feed is harmless.
    pub async fn change_acl(
        &self,
        scope: &Scope,
        actor: &str,
        target: &AclTarget,
        acl: &[String],
    ) -> Result<usize> {
        self.change_acl_all(scope, actor, std::slice::from_ref(target), acl)
            .await
    }

    /// [`Store::change_acl`] for several targets at once: every episode any of them names, read once and
    /// committed together (a commit per 500 episodes), not a commit per target. A dream releasing hundreds of
    /// records at once spent seconds on each.
    pub async fn change_acl_all(
        &self,
        scope: &Scope,
        actor: &str,
        targets: &[AclTarget],
        acl: &[String],
    ) -> Result<usize> {
        anyhow::ensure!(!acl.is_empty(), "an episode needs at least one ACL entry");
        let ns = scope.namespace();
        let want: BTreeSet<String> = acl.iter().map(|a| auth::norm_entry(a)).collect();
        // Episodes named by id are looked up, not matched one target at a time: a list can name thousands.
        let ids: HashSet<&str> = targets
            .iter()
            .filter_map(|t| match t {
                AclTarget::Episode(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let others: Vec<&AclTarget> = targets
            .iter()
            .filter(|t| !matches!(t, AclTarget::Episode(_)))
            .collect();
        let newest = self.current_episodes(scope).await?;
        let mut todo: Vec<Episode> = newest
            .into_values()
            .filter(|e| {
                ids.contains(e.episode_id.as_str())
                    || others.iter().any(|t| t.matches(&e.episode_id, &e.source))
            })
            .filter(|e| {
                e.acl
                    .iter()
                    .map(|a| auth::norm_entry(a))
                    .collect::<BTreeSet<_>>()
                    != want
            })
            .map(|e| Episode {
                acl: acl.to_vec(),
                // Rank past the row this replaces even if its writer's clock ran ahead of ours: a change
                // that lost to a "newer" broad row would leave the episode exposed.
                recorded_at: Some(now_micros().max(e.recorded_at.unwrap_or(i64::MIN) + 1)),
                ..e
            })
            .collect();
        todo.sort_by(|a, b| a.episode_id.cmp(&b.episode_id));
        let table = self.rows(&ns, EPISODES).await?.table;
        let mut changed = 0;
        let mut held: Vec<Episode> = Vec::new();
        for chunk in todo.chunks(500) {
            // Owned inputs per future, as in `write_episodes`: keeps the future `Send` for every lifetime.
            let reads: Vec<_> = chunk
                .iter()
                .map(|e| {
                    let (io, path) = (table.file_io().clone(), body_path(&table, &e.episode_id));
                    async move { Ok::<_, anyhow::Error>(io.new_input(path)?.read().await?.to_vec()) }
                })
                .collect();
            let bodies: Vec<Result<Vec<u8>>> =
                futures::stream::iter(reads).buffered(32).collect().await;
            let mut eps = Vec::with_capacity(chunk.len());
            for (e, body) in chunk.iter().zip(bodies) {
                eps.push(Episode {
                    body: body.with_context(|| format!("read body of episode {}", e.episode_id))?,
                    ..e.clone()
                });
            }
            // Not `commit_on`: these are stored episodes under a new ACL, typed or not (one written before
            // every write was typed must still be re-ACL'd).
            match self
                .commit_full(scope, MAIN, actor, &eps, &[], &[], &[], None)
                .await
            {
                Ok(_) => changed += eps.len(),
                // Some of them would let others read what an entity's owners must first approve
                // (`governance::private_episodes`): those wait, and the rest (a revocation among them) can't.
                Err(e) if governance::OwnerApprovalRequired::refused(&e) => {
                    for ep in eps {
                        match self
                            .commit_full(
                                scope,
                                MAIN,
                                actor,
                                std::slice::from_ref(&ep),
                                &[],
                                &[],
                                &[],
                                None,
                            )
                            .await
                        {
                            Ok(_) => changed += 1,
                            Err(e) if governance::OwnerApprovalRequired::refused(&e) => {
                                held.push(ep)
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        if !held.is_empty() {
            self.revoke_held(scope, actor, &held).await?;
            eprintln!(
                "ACL change in {scope} by {actor}: {changed} episodes changed, {} wait for their facts' owners: {}",
                held.len(),
                held.iter()
                    .map(|e| e.episode_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(
                anyhow::Error::new(governance::OwnerApprovalRequired).context(format!(
                    "{} episodes changed, {} left as they were",
                    changed,
                    held.len()
                )),
            );
        }
        Ok(changed)
    }

    /// The source deleted some records (a Slack message, a channel, a CRM record): retract what `target`
    /// names, on main, for every branch. Every episode it names is gone for every reader and at every time,
    /// and so is every fact citing it (a fact is readable only if all its episodes are). A delete naming one
    /// id or one exact source also holds for that record arriving later: a delete can overtake its record,
    /// and the same id or source arriving again is a late retry of what was deleted. A prefix holds only for
    /// what exists now: a standing delete of a whole family of sources would quietly drop every record
    /// written under it from then on, with no way back. What exists includes the versions a newer one
    /// superseded ([`Store::supersede`]): a delete holds for them for good too, as it did when superseding was
    /// for good. Returns the ids of the committed episodes it retracted (those readers saw), sorted; replaying
    /// a delete retracts nothing and writes nothing. The rows stay in the tables until a purge rewrites them.
    pub async fn retract_episodes(
        &self,
        scope: &Scope,
        actor: &str,
        target: &AclTarget,
    ) -> Result<Vec<String>> {
        self.retract_all(scope, actor, std::slice::from_ref(target))
            .await
    }

    /// [`Store::retract_episodes`] for several targets at once, in one commit: what any of them names.
    pub async fn retract_all(
        &self,
        scope: &Scope,
        actor: &str,
        targets: &[AclTarget],
    ) -> Result<Vec<String>> {
        self.retract_rows(scope, actor, targets, false).await
    }

    /// A record's newer version replaced its other versions (`/ingest`'s `replaces_source`): retire the
    /// committed episodes `targets` name, on main, for every branch, as a delete would, with the facts only they
    /// support. Unlike a delete it isn't for good: a version retired here and written again later is current
    /// again, as a new episode is (and what it supports with it), and the next version replaces it as this one
    /// did. A delete that names it, before or after, holds for good. Returns the ids retired, sorted.
    pub async fn supersede(
        &self,
        scope: &Scope,
        actor: &str,
        targets: &[AclTarget],
    ) -> Result<Vec<String>> {
        self.retract_rows(scope, actor, targets, true).await
    }

    async fn retract_rows(
        &self,
        scope: &Scope,
        actor: &str,
        targets: &[AclTarget],
        superseded: bool,
    ) -> Result<Vec<String>> {
        let mut ids: Vec<String> = self
            .current_episodes(scope)
            .await?
            .into_values()
            .filter(|e| targets.iter().any(|t| t.matches(&e.episode_id, &e.source)))
            .map(|e| e.episode_id)
            .collect();
        ids.sort();
        let named = |id: &str| {
            targets
                .iter()
                .any(|t| matches!(t, AclTarget::Episode(e) if e == id))
        };
        // A delete writes the exact id or source named, unless already committed, then each episode matched by
        // id: what a purge removes, and what stays retracted if it is written again under another source. A
        // supersession names only the episodes it retires.
        let vis = self.visibility(scope, MAIN).await?;
        let mut rows: Vec<(Option<&str>, Option<&str>)> = vec![];
        for target in targets.iter().filter(|_| !superseded) {
            match target {
                AclTarget::Episode(id) if !vis.retraction_targets.contains(&format!("id:{id}")) => {
                    rows.push((Some(id.as_str()), None))
                }
                AclTarget::Source(s)
                    if !vis.retraction_targets.contains(&format!("source:{s}")) =>
                {
                    rows.push((None, Some(s.as_str())))
                }
                _ => {}
            }
        }
        for id in &ids {
            if superseded || !named(id) {
                rows.push((Some(id.as_str()), None));
            }
        }
        // A delete by prefix, or of a source's other versions, also takes the versions a newer one superseded
        // (a source's row or an id's already does): readers don't see them now, but written again they would be.
        let mut dormant: Vec<&str> = vis
            .superseded
            .iter()
            .filter(|(id, source)| {
                !superseded
                    && targets.iter().any(|t| {
                        matches!(
                            t,
                            AclTarget::SourcePrefix(_) | AclTarget::SourceExcept { .. }
                        ) && t.matches(id, source)
                    })
            })
            .map(|(id, _)| id.as_str())
            .collect();
        dormant.sort();
        rows.extend(dormant.into_iter().map(|id| (Some(id), None)));
        if rows.is_empty() {
            return Ok(ids);
        }
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let commit_id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, RETRACTIONS).await?;
        let n = rows.len();
        let col = |i: usize| -> ArrayRef {
            Arc::new(StringArray::from(
                rows.iter().map(|r| [r.0, r.1][i]).collect::<Vec<_>>(),
            ))
        };
        let mark = superseded.then_some(true);
        append(
            &*self.catalog(),
            &table,
            vec![
                (
                    "commit_id",
                    Arc::new(StringArray::from(vec![commit_id.as_str(); n])) as ArrayRef,
                ),
                ("episode_id", col(0)),
                ("source", col(1)),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now_micros()), n)),
                ),
                (
                    "superseded",
                    Arc::new(arrow_array::BooleanArray::from(vec![mark; n])),
                ),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, RETRACTIONS);
        self.append_ledger(&commit_id, scope, MAIN, actor, &|l: &Ledger| {
            Ok(l.heads(MAIN))
        })
        .await?;
        self.visibility(scope, MAIN).await?;
        Ok(ids)
    }

    /// Retract one fact by id, or withdraw every commit an extractor (an actor prefix such as
    /// `extract:anthropic/claude-sonnet-5.5@g1/schema:sales@v2`) has made so far, on main and so
    /// for every reader. A withdrawn extractor's facts and its records of what it read go with its commits, so
    /// the episodes are read again by the next extraction; its summaries go too. Returns how many facts no
    /// reader sees any more.
    pub async fn retract_facts(
        &self,
        scope: &Scope,
        actor: &str,
        fact_id: Option<&str>,
        actor_prefix: Option<&str>,
    ) -> Result<usize> {
        anyhow::ensure!(
            fact_id.is_some() != actor_prefix.is_some(),
            "retract one fact or one extractor"
        );
        if let Some(p) = actor_prefix {
            anyhow::ensure!(
                WITHDRAWABLE
                    .iter()
                    .any(|w| p.starts_with(w) && p.len() > w.len()),
                "only an extractor's commits are withdrawn (an actor starting extract:, backfill:, aggregate: or \
                 dream:, and more than that)"
            );
        }
        // First: a table read before its schema evolves is held stale by the write that follows.
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let before = self.visibility(scope, MAIN).await?;
        let facts = self.rows(&ns, FACTS).await?;
        let gone = |vis: &Vis| -> usize {
            facts
                .rows
                .iter()
                .flat_map(|b| decode_facts(b, "source_episodes"))
                .filter(|(c, f)| {
                    vis.committed.contains(c) && !vis.retracted_facts.contains(&f.fact_id)
                })
                .count()
        };
        let n_before = gone(&before);
        let ids: Vec<&str> = fact_id.into_iter().collect();
        self.retract_writes(scope, actor, &ids, actor_prefix)
            .await?;
        let after = self.visibility(scope, MAIN).await?;
        Ok(n_before.saturating_sub(gone(&after)))
    }

    /// One commit on main retracting each fact in `fact_ids` by id, and the extractor `actor_prefix` names.
    async fn retract_writes(
        &self,
        scope: &Scope,
        actor: &str,
        fact_ids: &[&str],
        actor_prefix: Option<&str>,
    ) -> Result<()> {
        let ns = scope.namespace();
        let commit_id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, RETRACTIONS).await?;
        let mut rows: Vec<(Option<&str>, Option<&str>)> =
            fact_ids.iter().map(|f| (Some(*f), None)).collect();
        if let Some(p) = actor_prefix {
            rows.push((None, Some(p)));
        }
        let n = rows.len();
        let col = |vals: Vec<Option<&str>>| -> ArrayRef { Arc::new(StringArray::from(vals)) };
        append(
            &*self.catalog(),
            &table,
            vec![
                (
                    "commit_id",
                    Arc::new(StringArray::from(vec![commit_id.as_str(); n])) as ArrayRef,
                ),
                ("episode_id", col(vec![None; n])),
                ("source", col(vec![None; n])),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now_micros()), n)),
                ),
                ("fact_id", col(rows.iter().map(|r| r.0).collect())),
                ("actor_prefix", col(rows.iter().map(|r| r.1).collect())),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, RETRACTIONS);
        self.append_ledger(&commit_id, scope, MAIN, actor, &|l: &Ledger| {
            Ok(l.heads(MAIN))
        })
        .await?;
        Ok(())
    }

    /// Merge subject `from` into `into`, on main: every fact `from` holds now moves under `into`, every ref
    /// to `from` now names `into`, `from`'s name becomes an alias of `into` (so a search for either name
    /// finds it), and the rows left under `from` are retracted. Sources, quotes, cites and valid times
    /// travel with the facts, so nothing recorded is lost; a fact `into` already holds is not written twice.
    /// Returns how many facts moved.
    pub async fn merge_subjects(
        &self,
        scope: &Scope,
        actor: &str,
        from: &str,
        into: &str,
    ) -> Result<usize> {
        anyhow::ensure!(from != into, "merge two different subjects");
        self.ensure_scope(scope).await?;
        self.check_owned_subject_merge(scope, from, into).await?;
        let vis = self.visibility(scope, MAIN).await?;
        let of = |id: &str| {
            search::Filter::Or(vec![
                search::Filter::Eq(search::Col::Entity, search::Lit::Str(id.into())),
                search::Filter::Eq(search::Col::ValueRef, search::Lit::Str(id.into())),
                // A ref written before the scope had a schema is a plain value.
                search::Filter::Eq(search::Col::Value, search::Lit::Str(id.into())),
            ])
        };
        // The engine's own view, whoever may read each fact: the whole history, so what `from` held once
        // stays readable under `into`.
        let held_by = |rows: Vec<(String, Fact)>| -> Vec<(String, Fact)> {
            rows.into_iter()
                .filter(|(c, f)| {
                    vis.committed.contains(c) && !vis.retracted_facts.contains(&f.fact_id)
                })
                .map(|(c, f)| (c, vis.with_quote(f)))
                .collect()
        };
        let old = held_by(search::scan(self, scope, &of(from), true).await?);
        let held: HashSet<(String, String)> =
            held_by(search::scan(self, scope, &of(into), true).await?)
                .into_iter()
                .map(|(_, f)| (f.predicate, f.value))
                .collect();
        // Each moved fact is committed by the writer of the original, so withdrawing an extractor still takes
        // what it wrote wherever it now sits; the closures that ended the original carry over to the copy, so
        // each reader still sees the end only when they may see the episode that closed it.
        let mut moved: BTreeMap<String, Vec<Fact>> = BTreeMap::new();
        let mut closures = vec![];
        let mut named = false;
        for (commit, f) in &old {
            let mut g = f.clone();
            g.fact_id = uuid::Uuid::new_v4().to_string();
            if g.entity == from {
                g.entity = into.into();
                if g.predicate == query::schema::NAME {
                    g.predicate = query::schema::ALIAS.into();
                    named = true;
                }
            }
            if g.value_ref.as_deref() == Some(from) || g.value == from {
                g.value = into.into();
            }
            // Typed columns are filled again from the schema on write.
            g.entity_type = None;
            g.value_type = None;
            g.value_num = None;
            g.value_ts = None;
            g.value_ref = None;
            g.unit = None;
            if held.contains(&(g.predicate.clone(), g.value.clone())) {
                continue;
            }
            for (end, episode) in vis.closures.get(&f.fact_id).into_iter().flatten() {
                if vis.acls.contains_key(episode) {
                    closures.push(Closure {
                        fact_id: g.fact_id.clone(),
                        valid_to: *end,
                        episode_id: episode.clone(),
                    });
                }
            }
            let by = vis.actors.get(commit).map_or(actor, String::as_str);
            moved.entry(by.to_string()).or_default().push(g);
        }
        if !named {
            // A subject without a `_name` is still found by the words of its id.
            let words = from
                .split_once(':')
                .map_or(from, |(_, s)| s)
                .replace('_', " ");
            if !held.contains(&(query::schema::ALIAS.into(), words.clone())) {
                let mut alias = Fact::new(into, query::schema::ALIAS, &words);
                alias.source_episodes = old
                    .iter()
                    .flat_map(|(_, f)| f.source_episodes.clone())
                    .collect();
                alias.source_episodes.sort();
                alias.source_episodes.dedup();
                if !alias.source_episodes.is_empty() {
                    moved.entry(actor.to_string()).or_default().push(alias);
                }
            }
        }
        if old.is_empty() && moved.is_empty() {
            return Ok(0);
        }
        for (by, facts) in &moved {
            self.commit(scope, by, &[], facts).await?;
        }
        if !closures.is_empty() {
            self.commit_full(scope, MAIN, actor, &[], &[], &[], &closures, None)
                .await?;
        }
        let gone: Vec<&str> = old.iter().map(|(_, f)| f.fact_id.as_str()).collect();
        self.retract_writes(scope, actor, &gone, None).await?;
        self.visibility(scope, MAIN).await?;
        Ok(old.len())
    }

    /// Set the scope's retention policy, on main, for every branch: episodes older than their source's
    /// rule are gone for every reader from the next read, with the facts citing them, and the rest go as
    /// they age out. The newest policy wins, so loosening it brings back what a purge hasn't yet removed.
    /// Returns the commit id.
    pub async fn set_retention(
        &self,
        scope: &Scope,
        actor: &str,
        policy: &retention::Retention,
    ) -> Result<String> {
        policy.check()?;
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, RETENTION).await?;
        let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", s(&id)),
                ("body", s(&serde_json::to_string(policy)?)),
                ("recorded_at", ts([Some(now_micros())])),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, RETENTION);
        self.append_ledger(&id, scope, MAIN, actor, &|l: &Ledger| Ok(l.heads(MAIN)))
            .await?;
        self.visibility(scope, MAIN).await?;
        Ok(id)
    }

    /// The scope's retention policy (empty: everything is kept).
    pub async fn retention(&self, scope: &Scope) -> Result<retention::Retention> {
        Ok(self.visibility(scope, MAIN).await?.retention.clone())
    }

    /// Attach a supporting quote to every committed fact that has none: the sentence of a source
    /// episode that shares the most words with the fact (see [`find_quote`]), committed to
    /// `fact_quotes` in batches. Facts are not rewritten; reads fill `quote` from that table.
    /// Re-running skips facts that already have one, unless `redo` (recompute every backfilled
    /// quote; the newest wins). Quotes from extraction are never replaced. Returns a summary.
    pub async fn backfill_quotes(&self, scope: &Scope, actor: &str, redo: bool) -> Result<String> {
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        self.invalidate(&ns, FACTS);
        self.invalidate(&ns, QUOTES);
        let vis = self.visibility(scope, MAIN).await?;
        let facts = self.rows(&ns, FACTS).await?;
        let todo: Vec<Fact> = facts
            .rows
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .filter(|(c, f)| {
                vis.committed.contains(c)
                    && f.quote.is_none()
                    && (redo || !vis.quotes.contains_key(&f.fact_id))
            })
            .map(|(_, f)| f)
            .collect();
        let wanted: BTreeSet<String> = todo
            .iter()
            .flat_map(|f| f.source_episodes.iter().cloned())
            .collect();
        let episodes = self.rows(&ns, EPISODES).await?.table;
        let reads = wanted.into_iter().map(|id| {
            let (io, path) = (episodes.file_io().clone(), body_path(&episodes, &id));
            async move {
                let body =
                    with_retries(|| async { Ok(io.new_input(path.clone())?.read().await?) }).await;
                // A body that can't be read just leaves its facts without a quote.
                (
                    id,
                    body.ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
                )
            }
        });
        let bodies: HashMap<String, Option<String>> = futures::stream::iter(reads)
            .buffer_unordered(64)
            .collect()
            .await;
        let found: Vec<(String, String)> = todo
            .iter()
            .filter_map(|f| {
                let text = format!("{} {}", f.entity, f.value);
                f.source_episodes
                    .iter()
                    .filter_map(|id| bodies.get(id)?.as_deref())
                    .find_map(|body| find_quote(body, &text))
                    .map(|q| (f.fact_id.clone(), q))
            })
            .collect();
        for chunk in found.chunks(20_000) {
            let id = uuid::Uuid::new_v4().to_string();
            let table = self.write_target(&ns, QUOTES).await?;
            let col = |g: fn(&(String, String)) -> &str| -> ArrayRef {
                Arc::new(StringArray::from_iter_values(chunk.iter().map(g)))
            };
            let columns = vec![
                (
                    "commit_id",
                    Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                        id.as_str(),
                        chunk.len(),
                    ))) as ArrayRef,
                ),
                ("fact_id", col(|r| &r.0)),
                ("quote", col(|r| &r.1)),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now_micros()), chunk.len())),
                ),
            ];
            append(&*self.catalog(), &table, columns, OnConflict::Recommit).await?;
            self.invalidate(&ns, QUOTES);
            self.append_ledger(&id, scope, MAIN, actor, &|l: &Ledger| Ok(l.heads(MAIN)))
                .await?;
        }
        Ok(format!(
            "backfilled quotes for {} of {} facts without one ({} episodes read)",
            found.len(),
            todo.len(),
            bodies.values().filter(|b| b.is_some()).count()
        ))
    }

    /// The namespaces with a commit on main by an actor whose name starts with `prefix`, recorded after `since` (µs),
    /// and the newest such commit's time (`since` if none): how a writer finds the scopes a request was queued in for it
    /// (`advice-requests/`) from the ledger every server reads, without reading each scope.
    pub async fn committed_by(&self, prefix: &str, since: i64) -> Result<(BTreeSet<String>, i64)> {
        let snap = self.rows(SYSTEM_NS, LEDGER).await?;
        let (mut found, mut newest) = (BTreeSet::new(), since);
        for b in snap.rows.iter() {
            let (actor, branch, sc) = (
                strings(b, "actor"),
                strings(b, "branch"),
                strings(b, "scope"),
            );
            let recorded = timestamps(b, "recorded_at");
            for i in 0..b.num_rows() {
                let at = recorded[i].unwrap_or_default();
                if at > since && branch[i] == MAIN && actor[i].starts_with(prefix) {
                    found.insert(sc[i].clone());
                    newest = newest.max(at);
                }
            }
        }
        Ok((found, newest))
    }

    /// The ledger rows for one scope's namespace.
    async fn ledger(&self, scope: &Scope) -> Result<Ledger> {
        let snap = self.rows(SYSTEM_NS, LEDGER).await?;
        Ok(Ledger::from_batches(&snap.rows, &scope.namespace()))
    }
}

impl Ledger {
    fn from_batches(batches: &[RecordBatch], ns: &str) -> Ledger {
        let mut rows = vec![];
        for b in batches {
            let (id, branch, sc) = (
                strings(b, "commit_id"),
                strings(b, "branch"),
                strings(b, "scope"),
            );
            // Rows written before branches existed have no parents column value.
            let parents = optional_strings(b, "parents");
            let recorded = timestamps(b, "recorded_at");
            for i in 0..b.num_rows() {
                if sc[i] == ns {
                    rows.push(LedgerRow {
                        commit_id: id[i].clone(),
                        branch: branch[i].clone(),
                        parents: split_list(parents[i].as_deref().unwrap_or("")),
                        recorded_at: recorded[i].unwrap_or_default(),
                    });
                }
            }
        }
        Ledger { rows }
    }
}

impl Store {
    /// ACL per committed episode. An episode ingested again with a new ACL (its source
    /// permissions changed) takes the newest committed ACL: a permission revoked at the source
    /// stops granting access. Rows recorded at the same instant (one batch) are unioned.
    fn episode_acls(
        batches: &[RecordBatch],
        committed: &HashSet<String>,
    ) -> HashMap<String, Vec<String>> {
        let mut rows = Vec::new();
        for b in batches {
            let (commit, id, acl) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "acl"),
            );
            let at = timestamps(b, "recorded_at");
            for i in 0..b.num_rows() {
                if committed.contains(&commit[i]) {
                    rows.push((
                        id[i].clone(),
                        at[i].unwrap_or(i64::MIN),
                        split_list(&acl[i]),
                    ));
                }
            }
        }
        latest_acls(rows)
    }

    async fn table(&self, ns: &str, name: &str) -> Result<Table> {
        let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
        self.catalog()
            .load_table(&ident)
            .await
            .with_context(|| format!("load table {ns}.{name}"))
    }
}

/// The newest ACL per episode from (episode, recorded_at, acl) rows; rows tied at the newest time are unioned.
fn latest_acls(rows: Vec<(String, i64, Vec<String>)>) -> HashMap<String, Vec<String>> {
    let mut newest: HashMap<String, (i64, Vec<String>)> = HashMap::new();
    for (id, at, acl) in rows {
        match newest.get_mut(&id) {
            Some((seen, have)) if at == *seen => {
                for a in acl {
                    if !have.contains(&a) {
                        have.push(a);
                    }
                }
            }
            Some((seen, have)) if at > *seen => (*seen, *have) = (at, acl),
            Some(_) => {}
            None => {
                newest.insert(id, (at, acl));
            }
        }
    }
    newest.into_iter().map(|(id, (_, acl))| (id, acl)).collect()
}

/// End-to-end check of the commit model and episode-derived visibility, run unchanged
/// against any catalog. Returns a report; errors on the first broken invariant.
pub async fn smoke(store: &Store, scope: &Scope) -> Result<String> {
    use anyhow::ensure;
    store.ensure_scope(scope).await?;
    let tag = uuid::Uuid::new_v4().to_string();
    let chat = Episode::new(
        "slack:smoke",
        format!("[{tag}] Acme moved renewal to Dec 1").as_bytes(),
        &["user:alice"],
    );
    let crm = Episode::new(
        "note:crm-smoke",
        format!("[{tag}] Acme ARR 1.2M").as_bytes(),
        &["group:sales"],
    );
    ensure!(
        Episode::new("slack:smoke", &chat.body, &[]).episode_id == chat.episode_id,
        "episode ids are not content-addressed"
    );
    let renewal = Fact::new("acme", "renewal_date", "2026-12-01").from_episodes(&[&chat]);
    let arr = Fact::new("acme", "arr_at_renewal", "1.2M").from_episodes(&[&chat, &crm]);
    let commit_id = store
        .commit(
            scope,
            "smoke",
            &[chat.clone(), crm.clone()],
            &[renewal.clone(), arr.clone()],
        )
        .await?;

    // A crash between the data write and the ledger append must leave nothing visible.
    let orphan = Fact::new("acme", "renewal_date", "ORPHAN-never-committed");
    let orphan_ep = Episode::new("slack:smoke", format!("[{tag}] orphan").as_bytes(), &["*"]);
    store
        .write_pending(
            scope,
            std::slice::from_ref(&orphan_ep),
            std::slice::from_ref(&orphan),
        )
        .await?;

    let ids =
        |facts: Vec<Fact>| -> HashSet<String> { facts.into_iter().map(|f| f.fact_id).collect() };
    let alice = Principal::new("alice", &[]);
    let alice_sales = Principal::new("alice", &["sales"]);
    let bob = Principal::new("bob", &["eng"]);

    let seen = ids(store.read(scope, &alice, None).await?);
    ensure!(
        seen.contains(&renewal.fact_id),
        "committed fact not visible to its reader"
    );
    ensure!(
        !seen.contains(&orphan.fact_id),
        "uncommitted fact is visible"
    );
    ensure!(
        !seen.contains(&arr.fact_id),
        "fact visible without access to all its source episodes"
    );
    let seen = ids(store.read(scope, &alice_sales, None).await?);
    ensure!(
        seen.contains(&renewal.fact_id) && seen.contains(&arr.fact_id),
        "fact hidden from a principal who can see all its sources"
    );
    let seen = ids(store.read(scope, &bob, None).await?);
    ensure!(
        !seen.contains(&renewal.fact_id) && !seen.contains(&arr.fact_id),
        "facts leaked to a principal without access to their sources"
    );
    let before = ids(store
        .read(scope, &alice, Some(renewal.valid_from - 1))
        .await?);
    ensure!(
        !before.contains(&renewal.fact_id),
        "fact visible before its valid_from"
    );

    ensure!(
        store.episode_body(scope, &alice, &chat.episode_id).await? == Some(chat.body.clone()),
        "episode body did not round-trip"
    );
    ensure!(
        store
            .episode_body(scope, &bob, &chat.episode_id)
            .await?
            .is_none(),
        "episode body leaked to a principal outside its ACL"
    );
    ensure!(
        store
            .episode_body(scope, &alice, &orphan_ep.episode_id)
            .await?
            .is_none(),
        "uncommitted episode body is readable"
    );
    smoke_branches(store, scope).await?;
    Ok(format!(
        "ok: commit {commit_id} in {}: orphan hidden, as-of respected, source-episode visibility \
         enforced for 3 principals, bodies content-addressed and ACL-gated; branches isolate, \
         merge lands, conflicting merge refused",
        scope.namespace()
    ))
}

async fn smoke_branches(store: &Store, scope: &Scope) -> Result<()> {
    use anyhow::ensure;
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let ent = |s: &str| format!("acme_{tag}_{s}");
    let who = Principal::new("anyone", &[]);
    let ep = Episode::new(
        "slack:smoke",
        format!("[{tag}] branch test").as_bytes(),
        &["*"],
    );
    let fact = |e: &str, p: &str, v: &str| Fact::new(&ent(e), p, v).from_episodes(&[&ep]);
    let keys = |facts: Vec<Fact>| -> HashSet<String> {
        facts
            .into_iter()
            .map(|f| format!("{}.{}={}", f.entity, f.predicate, f.value))
            .collect()
    };
    let key = |e: &str, p: &str, v: &str| format!("{}.{p}={v}", ent(e));

    store
        .commit(
            scope,
            "smoke",
            std::slice::from_ref(&ep),
            &[fact("a", "stage", "discovery")],
        )
        .await?;
    let draft = format!("draft_{tag}");
    store.create_branch(scope, &draft, "smoke").await?;
    ensure!(
        store.create_branch(scope, &draft, "smoke").await.is_err(),
        "duplicate branch name accepted"
    );
    store
        .commit_on(scope, &draft, "smoke", &[], &[fact("a", "owner", "alice")])
        .await?;
    store
        .commit(scope, "smoke", &[], &[fact("b", "stage", "won")])
        .await?;

    let main = keys(store.read(scope, &who, None).await?);
    let on_draft = keys(store.read_on(scope, &draft, &who, None).await?);
    ensure!(
        main.contains(&key("a", "stage", "discovery")) && main.contains(&key("b", "stage", "won")),
        "main is missing its own commits"
    );
    ensure!(
        !main.contains(&key("a", "owner", "alice")),
        "branch write leaked onto main"
    );
    ensure!(
        on_draft.contains(&key("a", "stage", "discovery"))
            && on_draft.contains(&key("a", "owner", "alice")),
        "branch does not see its base plus its own commits"
    );
    ensure!(
        !on_draft.contains(&key("b", "stage", "won")),
        "branch sees a main commit made after it branched"
    );

    store.merge(scope, &draft, "smoke").await?;
    let main = keys(store.read(scope, &who, None).await?);
    ensure!(
        main.contains(&key("a", "owner", "alice")),
        "merged write not visible on main"
    );

    let fork = format!("fork_{tag}");
    store.create_branch(scope, &fork, "smoke").await?;
    store
        .commit_on(scope, &fork, "smoke", &[], &[fact("c", "owner", "carol")])
        .await?;
    store
        .commit(scope, "smoke", &[], &[fact("c", "owner", "dave")])
        .await?;
    let err = store.merge(scope, &fork, "smoke").await;
    ensure!(
        err.as_ref()
            .is_err_and(|e| e.to_string().contains("conflict")),
        "conflicting merge was not refused: {err:?}"
    );
    let main = keys(store.read(scope, &who, None).await?);
    ensure!(
        main.contains(&key("c", "owner", "dave")) && !main.contains(&key("c", "owner", "carol")),
        "refused merge changed main"
    );
    Ok(())
}

fn ledger_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("branch", true, PrimitiveType::String),
        ("actor", true, PrimitiveType::String),
        ("scope", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        ("parents", false, PrimitiveType::String),
    ])
}

fn episodes_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("episode_id", true, PrimitiveType::String),
        ("source", true, PrimitiveType::String),
        ("occurred_at", true, PrimitiveType::Timestamptz),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        ("acl", true, PrimitiveType::String),
        // Entity ids the episode is about (comma-joined); older rows read it as empty.
        ("about", false, PrimitiveType::String),
        // Null (rows written before it existed) reads as true.
        ("index_text", false, PrimitiveType::Boolean),
        // The document this episode is a version of; null for records without versions.
        ("document", false, PrimitiveType::String),
    ])
}

fn closures_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("fact_id", true, PrimitiveType::String),
        ("valid_to", true, PrimitiveType::Timestamptz),
        ("episode_id", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

/// Whether withdrawing `prefix` takes a commit by `actor`: the whole actor, or the actor up to a `/` (so
/// `…@v2` takes `…@v2/sales_call@ab12` and never `…@v20`).
fn withdraws(prefix: &str, actor: &str) -> bool {
    actor
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || prefix.ends_with('/'))
}

/// The facts retracted by id, and the extractors withdrawn (actor prefix, the withdrawal's commit) by
/// committed rows.
fn retracted_writes(
    retractions: &[RecordBatch],
    committed: &dyn Fn(&str) -> bool,
) -> (HashSet<String>, Vec<(String, String)>) {
    let (mut facts, mut withdrawn) = (HashSet::new(), vec![]);
    for b in retractions {
        let commit = strings(b, "commit_id");
        let (fact, prefix) = (
            optional_strings(b, "fact_id"),
            optional_strings(b, "actor_prefix"),
        );
        for i in 0..b.num_rows() {
            if !committed(&commit[i]) {
                continue;
            }
            if let Some(f) = &fact[i] {
                facts.insert(f.clone());
            }
            if let Some(p) = &prefix[i] {
                withdrawn.push((p.clone(), commit[i].clone()));
            }
        }
    }
    (facts, withdrawn)
}

/// The actors whose commits may be withdrawn: the engine's derived writes (facts and their reads, summaries, a
/// dream's links and endings), never a commit that holds a caller's episodes (a dream's commits hold only its own
/// records, `dream:` sources, which go with it).
pub const WITHDRAWABLE: [&str; 4] = ["extract:", "backfill:", "aggregate:", "dream:"];

/// What committed retractions hide ([`retracted`]).
pub struct Retracted {
    /// Every episode no reader sees: one a delete names (its id, or the exact source of any of its rows), and one
    /// a newer version superseded and nothing wrote since.
    pub hidden: HashSet<String>,
    /// What the deletes name (`id:` or `source:` and the name), so a delete isn't written twice.
    pub targets: HashSet<String>,
    /// The hidden episodes only a newer version superseded (no delete names them), each with its source: one
    /// written again is current again, and a delete that names it later holds for good.
    pub superseded: HashMap<String, String>,
    /// Superseded episodes written again since, current again: each with when it came back (the ledger time of
    /// the first commit that wrote it after it was last superseded).
    pub revived: HashMap<String, i64>,
}

/// What committed retractions hide. A delete (any row not marked `superseded`) holds for good: an episode is
/// gone when a delete names its id or the exact source of any of its rows, whenever it was written, so a delete
/// that overtook its record still removes the record and the same record written again stays gone. A row
/// marked `superseded` (a newer version of the source replaced this one) hides the episode only until a commit
/// after it writes the episode again: then it is current again, as a new episode is. Order is the ledger's:
/// `time` gives a commit's ledger time (each commit's is past its parents'), `committed` which commits count.
/// Rows written before the mark existed are all deletes, whatever wrote them.
pub fn retracted(
    retractions: &[RecordBatch],
    episodes: &[RecordBatch],
    committed: &dyn Fn(&str) -> bool,
    time: &dyn Fn(&str) -> Option<i64>,
) -> Retracted {
    let (mut ids, mut sources, mut targets) = (HashSet::new(), HashSet::new(), HashSet::new());
    // Each superseded episode, with when it was last superseded.
    let mut replaced: HashMap<String, i64> = HashMap::new();
    for b in retractions {
        let commit = strings(b, "commit_id");
        let (id, source, superseded) = (
            optional_strings(b, "episode_id"),
            optional_strings(b, "source"),
            optional_bools(b, "superseded"),
        );
        for i in 0..b.num_rows() {
            if !committed(&commit[i]) {
                continue;
            }
            match &id[i] {
                // Only a row naming one episode and nothing else is a supersession; a time the ledger
                // doesn't give keeps it hidden.
                Some(id) if superseded[i] == Some(true) && source[i].is_none() => {
                    let at = time(&commit[i]).unwrap_or(i64::MAX);
                    let t = replaced.entry(id.clone()).or_insert(at);
                    *t = (*t).max(at);
                }
                Some(id) => {
                    ids.insert(id.clone());
                    targets.insert(format!("id:{id}"));
                }
                None => {}
            }
            if let Some(s) = &source[i] {
                sources.insert(s.clone());
                targets.insert(format!("source:{s}"));
            }
        }
    }
    let (mut revived, mut source_of) = (HashMap::new(), HashMap::new());
    if !sources.is_empty() || !replaced.is_empty() {
        for b in episodes {
            let (commit, id, source) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            for i in 0..b.num_rows() {
                // Any row of the episode, whatever its commit: a source that ever carried it deleted it, and a
                // newer row (committed or not) naming another source must not bring it back.
                if sources.contains(&source[i]) {
                    ids.insert(id[i].clone());
                }
                if let Some(at) = replaced.get(&id[i])
                    && committed(&commit[i])
                {
                    source_of.insert(id[i].clone(), source[i].clone());
                    let t = time(&commit[i]).unwrap_or(i64::MIN);
                    if t > *at {
                        let first: &mut i64 = revived.entry(id[i].clone()).or_insert(t);
                        *first = (*first).min(t);
                    }
                }
            }
        }
    }
    // A delete holds whatever else happened to the episode.
    revived.retain(|id, _| !ids.contains(id));
    let superseded: HashMap<String, String> = replaced
        .into_keys()
        .filter(|id| !ids.contains(id) && !revived.contains_key(id))
        .map(|id| {
            let source = source_of.remove(&id).unwrap_or_default();
            (id, source)
        })
        .collect();
    ids.extend(superseded.keys().cloned());
    Retracted {
        hidden: ids,
        targets,
        superseded,
        revived,
    }
}

/// The newest committed retention policy (none: keep everything).
pub fn newest_retention(
    batches: &[RecordBatch],
    committed: &dyn Fn(&str) -> bool,
) -> Result<retention::Retention> {
    let mut newest: Option<(i64, String)> = None;
    for b in batches {
        let (commit, body) = (strings(b, "commit_id"), strings(b, "body"));
        let at = timestamps(b, "recorded_at");
        for i in 0..b.num_rows() {
            let t = at[i].unwrap_or_default();
            if committed(&commit[i]) && newest.as_ref().is_none_or(|(s, _)| t >= *s) {
                newest = Some((t, body[i].clone()));
            }
        }
    }
    Ok(match newest {
        None => retention::Retention::default(),
        Some((_, body)) => serde_json::from_str(&body)?,
    })
}

/// The episodes past `policy` at `now`, and when the next one still kept ages out. Any row of an episode
/// past its age expires it: a newer row naming another source or time must not keep it.
pub fn expired(
    episodes: &[RecordBatch],
    policy: &retention::Retention,
    now: i64,
) -> (HashSet<String>, Option<i64>) {
    let mut gone = HashSet::new();
    let mut next: Option<i64> = None;
    if policy.rules.is_empty() {
        return (gone, next);
    }
    for b in episodes {
        let (id, source) = (strings(b, "episode_id"), strings(b, "source"));
        let at = timestamps(b, "occurred_at");
        for i in 0..b.num_rows() {
            let Some(age) = policy.max_age_us(&source[i]) else {
                continue;
            };
            let end = at[i].unwrap_or_default().saturating_add(age);
            if end <= now {
                gone.insert(id[i].clone());
            } else if next.is_none_or(|n| end < n) {
                next = Some(end);
            }
        }
    }
    (gone, next)
}

fn retention_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("body", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

/// Audiences a brief found no summary for over all the episodes they may read: the next aggregation builds one.
fn agg_requests_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("goal", true, PrimitiveType::String),
        ("subject", true, PrimitiveType::String),
        // `for:` and the reader's ACL tokens, `+`-joined (`for:group:sales+user:amy@acme.com`).
        ("audience", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

fn aggregates_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("goal", true, PrimitiveType::String),
        ("subject", true, PrimitiveType::String),
        // item_status | element | flag
        ("kind", true, PrimitiveType::String),
        ("key", true, PrimitiveType::String),
        ("value", true, PrimitiveType::String),
        ("note", false, PrimitiveType::String),
        // The fact ids it rests on, comma-joined.
        ("cites", false, PrimitiveType::String),
        // Every episode of the batch it was derived from: a reader must see them all to see the row.
        ("episodes", true, PrimitiveType::String),
        ("as_of", true, PrimitiveType::Timestamptz),
        ("aggregator", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        // Who the run was for: `*` (every episode about the subject) or the ACL entry whose episodes it
        // read. Absent on runs from before audiences: those were for everyone.
        ("audience", false, PrimitiveType::String),
        // What the run's model call cost (every row of a run says the same).
        ("input_tokens", false, PrimitiveType::Long),
        ("output_tokens", false, PrimitiveType::Long),
    ])
}

fn qa_log_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        // One answer's rows share it; the newest is the answer as it stands (a later rating).
        ("answer_id", true, PrimitiveType::String),
        ("asker", true, PrimitiveType::String),
        ("goal", false, PrimitiveType::String),
        ("question", true, PrimitiveType::String),
        ("answer", false, PrimitiveType::String),
        // helpful | not_helpful
        ("rating", false, PrimitiveType::String),
        // The fact ids the answer cited, comma-joined, and the episodes they come from.
        ("cited", true, PrimitiveType::String),
        ("episodes", true, PrimitiveType::String),
        ("asked_at", true, PrimitiveType::Timestamptz),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        // A run's reward (`record_run`, `reward_run`), -1 to 1, and why; set when its outcome is known.
        ("reward", false, PrimitiveType::Double),
        ("reward_reason", false, PrimitiveType::String),
        // `run` for a run's entry, `session` for an agent session's (`crate::sessions`); none for an answer.
        ("kind", false, PrimitiveType::String),
        // A session's: the harness it ran in, its arm (`advice`, or `holdout`: advice routed but withheld),
        // the advice situations served and withheld (comma-joined), the episode holding the conversation so far
        // while it is open (the asker's own),
        // when it ended, the judge's verdicts (JSON), and the ACL its transcript episode is kept to.
        ("harness", false, PrimitiveType::String),
        ("arm", false, PrimitiveType::String),
        ("served", false, PrimitiveType::String),
        ("withheld", false, PrimitiveType::String),
        ("conversation", false, PrimitiveType::String),
        ("ended_at", false, PrimitiveType::Timestamptz),
        ("verdicts", false, PrimitiveType::String),
        ("acl", false, PrimitiveType::String),
        // How it ended: `agent` (end_session) or `idle` (the server, once it went idle).
        ("ended_by", false, PrimitiveType::String),
        // The advice ops fired in it mid-run (`get_advice` with an event), as a JSON array of
        // `<kind>:<tool>:<situation>:<op>` strings.
        ("fires", false, PrimitiveType::String),
        // The branch its writes land on, where its org gives each session one (`session_branches`).
        ("branch", false, PrimitiveType::String),
        // The version of each advice note it was served (JSON: note key → the note document's episode id), so its
        // judge reads what the agent read.
        ("served_versions", false, PrimitiveType::String),
        // What each advice lookup in it came to (JSON: the newest 50, each its moment, outcome, the line routed to
        // and the scores), so admins see when advice is served and when it isn't.
        ("lookups", false, PrimitiveType::String),
        // The pull requests it opened or pushed to, read from its transcript, each with what later came of it
        // (JSON: repo, number or branch, how it was linked, and its events: merged, closed, reverted, regression).
        ("prs", false, PrimitiveType::String),
    ])
}

fn feedback_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("fact_id", true, PrimitiveType::String),
        ("giver", true, PrimitiveType::String),
        // positive | negative | very_negative
        ("signal", true, PrimitiveType::String),
        ("reason", false, PrimitiveType::String),
        // The episodes the fact comes from, comma-joined: erasing one erases the feedback.
        ("episodes", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

fn retractions_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        // Exactly one of these per row: an episode (by id or source), a fact, or every commit an extractor
        // (an actor prefix: `extract:<model>@g1/schema:<name>@v2`) made before the row was recorded.
        ("episode_id", false, PrimitiveType::String),
        ("source", false, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        ("fact_id", false, PrimitiveType::String),
        ("actor_prefix", false, PrimitiveType::String),
        // True on an episode id a newer version of its source replaced (`replaces_source`): written again
        // later, it is current again. Null on every delete, and on every row written before it existed.
        ("superseded", false, PrimitiveType::Boolean),
    ])
}

fn installations_schema() -> Result<Schema> {
    schema(&[
        ("at", true, PrimitiveType::Timestamptz),
        ("actor", true, PrimitiveType::String),
        ("op", true, PrimitiveType::String),
        ("installation_id", true, PrimitiveType::String),
        ("provider", false, PrimitiveType::String),
        ("group_ref", false, PrimitiveType::String),
        ("on_behalf_of", false, PrimitiveType::String),
    ])
}

fn usage_hours_schema() -> Result<Schema> {
    schema(&[
        ("hour", true, PrimitiveType::Timestamptz),
        ("written_at", true, PrimitiveType::Timestamptz),
        ("record", true, PrimitiveType::String),
    ])
}

fn key_usage_schema() -> Result<Schema> {
    schema(&[
        ("sha256", true, PrimitiveType::String),
        ("org", true, PrimitiveType::String),
        ("name", true, PrimitiveType::String),
        ("used_at", true, PrimitiveType::Timestamptz),
    ])
}

fn directory_snapshot_schema() -> Result<Schema> {
    schema(&[
        ("at", true, PrimitiveType::Timestamptz),
        ("events", true, PrimitiveType::Long),
        ("digest", true, PrimitiveType::String),
        ("format", true, PrimitiveType::Long),
        ("state", true, PrimitiveType::String),
        ("taken_at", true, PrimitiveType::Timestamptz),
        ("actor", true, PrimitiveType::String),
        // `directory::Snapshot::seal`; none: unsealed.
        ("seal", false, PrimitiveType::String),
    ])
}

fn directory_schema() -> Result<Schema> {
    schema(&[
        ("at", true, PrimitiveType::Timestamptz),
        ("actor", true, PrimitiveType::String),
        ("op", true, PrimitiveType::String),
        ("org", true, PrimitiveType::String),
        ("team", false, PrimitiveType::String),
        ("user", false, PrimitiveType::String),
        ("role", false, PrimitiveType::String),
        ("detail", false, PrimitiveType::String),
        // The change's format (`directory::EVENT_VERSION`); rows from before versions have none: 1.
        ("version", false, PrimitiveType::Long),
    ])
}

fn schemas_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("name", true, PrimitiveType::String),
        ("version", true, PrimitiveType::String),
        ("body", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

fn extractions_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("episode_id", true, PrimitiveType::String),
        ("extractor", true, PrimitiveType::String),
        ("facts", true, PrimitiveType::Long),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        // What the read cost, as the model's API counted it (reads since costs were kept).
        ("input_tokens", false, PrimitiveType::Long),
        ("output_tokens", false, PrimitiveType::Long),
    ])
}

fn quotes_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("fact_id", true, PrimitiveType::String),
        ("quote", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

/// A verbatim span of `body` supporting a fact: the sentence (ending at `.`, `!` or `?` followed
/// by whitespace, so `features.csv_export` and URLs stay whole) or line, cut to 300 bytes, sharing
/// the most distinct words of three or more letters with `fact`. None unless it shares at least
/// two such words, or one when the fact has only one, and is at least 12 characters long.
// ponytail: word overlap, no stemming; an LLM pass would pick better spans for paraphrased facts.
pub fn find_quote(body: &str, fact: &str) -> Option<String> {
    let words = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let want = words(fact);
    let need = want.len().min(2);
    if need == 0 {
        return None;
    }
    let mut best: Option<(usize, &str)> = None;
    let mut spans = vec![];
    let mut start = 0;
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_is_space = chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | '!' | '?') && next_is_space) {
            spans.push(&body[start..i + c.len_utf8()]);
            start = i + c.len_utf8();
        }
    }
    spans.push(&body[start..]);
    for span in spans {
        let mut span = span.trim();
        if span.len() > 300 {
            let mut end = 300;
            while !span.is_char_boundary(end) {
                end -= 1;
            }
            span = &span[..end];
        }
        let n = words(span).intersection(&want).count();
        if n >= need
            && span.chars().filter(|c| c.is_alphanumeric()).count() >= 12
            && best.is_none_or(|(m, _)| n > m)
        {
            best = Some((n, span));
        }
    }
    best.map(|(_, s)| s.to_string())
}

fn facts_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("fact_id", true, PrimitiveType::String),
        ("entity", true, PrimitiveType::String),
        ("predicate", true, PrimitiveType::String),
        ("value", true, PrimitiveType::String),
        ("valid_from", true, PrimitiveType::Timestamptz),
        ("valid_to", false, PrimitiveType::Timestamptz),
        ("recorded_at", true, PrimitiveType::Timestamptz),
        ("source_episodes", true, PrimitiveType::String),
        ("quote", false, PrimitiveType::String),
        // Typed columns (scopes with a schema); older rows read them as null.
        ("entity_type", false, PrimitiveType::String),
        ("value_type", false, PrimitiveType::String),
        ("value_num", false, PrimitiveType::Double),
        ("value_ts", false, PrimitiveType::Timestamptz),
        ("value_ref", false, PrimitiveType::String),
        // Where the fact comes from and how its time is known; older rows read them as null.
        ("unit", false, PrimitiveType::String),
        ("cite", false, PrimitiveType::String),
        ("time_basis", false, PrimitiveType::String),
        // `,a,b,`: see `tags_column`.
        ("tags", false, PrimitiveType::String),
    ])
}

fn schema(cols: &[(&str, bool, PrimitiveType)]) -> Result<Schema> {
    let fields = cols.iter().enumerate().map(|(i, (name, required, ty))| {
        let id = i as i32 + 1;
        let ty = Type::Primitive(ty.clone());
        Arc::new(if *required {
            NestedField::required(id, *name, ty)
        } else {
            NestedField::optional(id, *name, ty)
        })
    });
    Ok(Schema::builder().with_fields(fields).build()?)
}

/// How an append treats a lost race on the table's snapshot.
#[derive(Clone, Copy, PartialEq)]
pub enum OnConflict {
    /// Data tables: appends commute, so re-commit the same files onto the newer snapshot.
    Recommit,
    /// The ledger: fail, so the caller recomputes parents from the newer snapshot (CAS).
    Fail,
}

/// Columns are matched to the table's current schema by name, so a table evolved by a newer
/// writer still takes an older writer's rows (its new optional columns are null) and a column
/// order change never shifts values. A writer's column the table lacks is an error unless it is
/// all null, so typed values are never silently dropped by a table nobody evolved.
pub async fn append(
    catalog: &dyn Catalog,
    table: &Table,
    columns: Vec<(&str, ArrayRef)>,
    on_conflict: OnConflict,
) -> Result<()> {
    // A column the table gained after it was cached here (this process's evolution, or another's): the table
    // as the catalog has it now.
    let fresh;
    let table = match columns.iter().any(|(name, c)| {
        c.null_count() < c.len()
            && table
                .metadata()
                .current_schema()
                .field_by_name(name)
                .is_none()
    }) {
        true => {
            fresh = catalog.load_table(table.identifier()).await?;
            &fresh
        }
        false => table,
    };
    let files = write_data_files(table, columns, None).await?;
    // Every snapshot this append makes carries its id: a commit whose answer was lost is found by it, in any snapshot
    // the table still has (not only the current one: a rewrite may have replaced it since).
    let append_id = uuid::Uuid::new_v4().to_string();
    let mut table = table.clone();
    for attempt in 0u32.. {
        // A manifest path of this try's own: refused for sure, it is deleted (its column bounds hold our values).
        let commit_uuid = uuid::Uuid::now_v7();
        let tx = Transaction::new(&table);
        // No duplicate scan: it reads every manifest of the table on every append, and our files are fresh UUIDs.
        let tx = tx
            .fast_append()
            .with_check_duplicate(false)
            .set_commit_uuid(commit_uuid)
            .set_snapshot_properties(HashMap::from([(
                APPEND_ID_PROP.to_string(),
                append_id.clone(),
            )]))
            .add_data_files(files.clone())
            .apply(tx)?;
        let result = match on_conflict {
            OnConflict::Recommit => tx.commit(catalog).await,
            OnConflict::Fail => {
                let pinned = PinnedCatalog {
                    inner: catalog,
                    pinned: table.clone(),
                };
                tx.commit(&pinned).await
            }
        };
        let e = match result {
            Ok(after) => {
                trim_history(catalog, &table, &after).await;
                return Ok(());
            }
            Err(e) => anyhow::Error::from(e),
        };
        let conflict = is_commit_conflict(&e);
        if attempt >= 100 || !(conflict || is_transient(&e) || commit_state_unknown(&e)) {
            return Err(e);
        }
        // The catalog may have applied a commit whose response was lost (a 5xx, or "commit
        // state unknown"). Resolve that before retrying, or a retry could commit twice.
        let ms = 10 + (uuid::Uuid::new_v4().as_u128() % 40) as u64 * (attempt.min(8) as u64 + 1);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let fresh =
            with_retries(|| async { Ok(catalog.load_table(table.identifier()).await?) }).await?;
        if tagged(&fresh, APPEND_ID_PROP, &append_id) {
            return Ok(());
        }
        // Refused for sure (a 409 with no retry inside the transaction: the table turns them off), so nothing of
        // this try is referenced: its manifest goes.
        let refused = e.chain().any(|c| {
            c.downcast_ref::<iceberg::Error>()
                .is_some_and(|ie| ie.kind() == iceberg::ErrorKind::CatalogCommitConflicts)
        }) && table
            .metadata()
            .properties()
            .get(TableProperties::PROPERTY_COMMIT_NUM_RETRIES)
            .is_some_and(|n| n == "0");
        if refused {
            let _ = table
                .file_io()
                .delete(format!(
                    "{}/metadata/{commit_uuid}-m0.avro",
                    table.metadata().location().trim_end_matches('/')
                ))
                .await;
        }
        match (conflict, on_conflict) {
            // The ledger's compare-and-set lost a race: the caller recomputes parents and writes new files. These
            // go when no try of this append can still land (the first, refused for sure): a forget or an org
            // delete can't reach a file no snapshot references.
            (true, OnConflict::Fail) => {
                if attempt == 0 && refused {
                    for f in &files {
                        let _ = table.file_io().delete(f.file_path()).await;
                    }
                }
                return Err(e);
            }
            // Data appends commute: re-commit the same files onto the newer snapshot.
            (_, OnConflict::Recommit) => table = fresh,
            // Not committed, and no one else moved the ledger: retry against our snapshot.
            (false, OnConflict::Fail) => {}
        }
    }
    unreachable!()
}

/// Where new data files go: the table's data location, or a directory of their own (a rewrite's).
#[derive(Clone)]
enum DataDir {
    Default(DefaultLocationGenerator),
    Under(String),
}

impl iceberg::writer::file_writer::location_generator::LocationGenerator for DataDir {
    fn generate_location(
        &self,
        partition_key: Option<&iceberg::spec::PartitionKey>,
        file_name: &str,
    ) -> String {
        match self {
            DataDir::Default(g) => g.generate_location(partition_key, file_name),
            DataDir::Under(d) => format!("{d}/{file_name}"),
        }
    }
}

/// The snapshot summary property naming the append that made it ([`append`]).
const APPEND_ID_PROP: &str = "memstore.append-id";

/// Whether a snapshot `table` still has carries `key` = `id` in its summary.
pub(crate) fn tagged(table: &Table, key: &str, id: &str) -> bool {
    table.metadata().snapshots().any(|s| {
        s.summary()
            .additional_properties
            .get(key)
            .is_some_and(|v| v == id)
    })
}

/// Write `columns` as new data files of `table` (matched to its current schema by name, as [`append`]
/// takes them), without committing them.
pub(crate) async fn write_data_files(
    table: &Table,
    columns: Vec<(&str, ArrayRef)>,
    dir: Option<&str>,
) -> Result<Vec<iceberg::spec::DataFile>> {
    let arrow_schema = Arc::new(schema_to_arrow_schema(table.metadata().current_schema())?);
    let rows = columns.first().map_or(0, |(_, c)| c.len());
    let mut named: HashMap<&str, ArrayRef> = columns.into_iter().collect();
    let ordered = arrow_schema
        .fields()
        .iter()
        .map(|field| match named.remove(field.name().as_str()) {
            Some(c) => Ok(c),
            None if field.is_nullable() => Ok(arrow_array::new_null_array(field.data_type(), rows)),
            None => anyhow::bail!(
                "no value for required column {} of {}",
                field.name(),
                table.identifier()
            ),
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some((name, _)) = named.iter().find(|(_, c)| c.null_count() < c.len()) {
        anyhow::bail!(
            "{} has no column {name}; run ensure_scope to evolve it",
            table.identifier()
        );
    }
    let batch = RecordBatch::try_new(arrow_schema, ordered)?;
    // A failed write leaves at most an unreferenced file; write a fresh one.
    with_retries(|| async {
        let rolling = RollingFileWriterBuilder::new_with_default_file_size(
            ParquetWriterBuilder::new(
                Default::default(),
                table.metadata().current_schema().clone(),
            ),
            table.file_io().clone(),
            match dir {
                Some(d) => DataDir::Under(d.trim_end_matches('/').to_string()),
                None => DataDir::Default(DefaultLocationGenerator::new(table.metadata())?),
            },
            DefaultFileNameGenerator::new(
                uuid::Uuid::new_v4().to_string(),
                None,
                DataFileFormat::Parquet,
            ),
        );
        let mut writer = DataFileWriterBuilder::new(rolling).build(None).await?;
        writer.write(batch.clone()).await?;
        Ok(writer.close().await?)
    })
    .await
}

/// The catalog could not tell whether a commit was applied.
pub(crate) fn commit_state_unknown(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("commit state is unknown") || text.contains("CommitStateUnknown")
}

/// How much of a table's Iceberg history is kept. Every commit adds a snapshot, and writes a new metadata file
/// that lists every snapshot so far and a manifest list that names every manifest so far, so with nothing
/// removed a table's metadata grows with the square of its commits: a replay's local ledger reached 2,500
/// commits and 2.6 GB of metadata over 10 MB of manifests and 8 MB of rows, and past a catalog's limit on
/// metadata size every write fails. What is kept: the newest `MEMSTORE_MAINTAIN_KEEP` snapshots (default 10;
/// 0 keeps everything) and that many entries of the metadata log. The tables are append-only (no row deletes), so
/// the newest snapshot names every manifest and data file, and an expired snapshot leaves behind only its manifest
/// list; a rewrite (`offboard`) replaces the files in one commit and deletes the replaced manifests and data files
/// itself, from the list it wrote before committing, whether or not this has expired their snapshots by then. That is deleted once the snapshot stopped being
/// current `MEMSTORE_MAINTAIN_GRACE_SECS` ago (default 120, twice the longest a cached handle is used,
/// `MEMSTORE_HARD_STALENESS_MS`): a handle loaded before then may still plan a scan from it.
#[derive(Clone, Copy, Debug)]
pub struct History {
    pub keep: usize,
    pub grace: Duration,
}

impl History {
    pub fn from_env() -> Self {
        static FROM_ENV: std::sync::LazyLock<History> = std::sync::LazyLock::new(|| History {
            keep: std::env::var("MEMSTORE_MAINTAIN_KEEP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            grace: Duration::from_secs(
                std::env::var("MEMSTORE_MAINTAIN_GRACE_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(120),
            ),
        });
        *FROM_ENV
    }

    /// The snapshots of `table` past the newest `keep` on the current snapshot's line.
    fn expirable(&self, table: &Table) -> Vec<i64> {
        if self.keep == 0 {
            return vec![];
        }
        line(table)
            .iter()
            .skip(self.keep)
            .map(|s| s.snapshot_id())
            .collect()
    }
}

/// The current snapshot and its ancestors, newest first.
fn line(table: &Table) -> Vec<iceberg::spec::SnapshotRef> {
    let meta = table.metadata();
    let mut line = vec![];
    let mut at = meta.current_snapshot().cloned();
    while let Some(s) = at {
        at = s
            .parent_snapshot_id()
            .and_then(|p| meta.snapshot_by_id(p).cloned());
        line.push(s);
    }
    line
}

/// Manifest lists of expired snapshots, each deleted once its grace is over (ms since the epoch): checked on every
/// commit's trim. What a process leaves here when it exits stays (the next trim of a local store sweeps it).
static RETIRED: std::sync::LazyLock<Mutex<Vec<(iceberg::io::FileIO, String, i64)>>> =
    std::sync::LazyLock::new(Default::default);

/// Delete the retired manifest lists whose grace is over.
async fn delete_retired() {
    let now = chrono::Utc::now().timestamp_millis();
    let due: Vec<_> = {
        let mut retired = RETIRED.lock().unwrap();
        let (due, wait) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|(_, _, at)| *at <= now);
        *retired = wait;
        due
    };
    for (io, path, _) in due {
        let _ = io.delete(&path).await;
    }
}

/// After a commit: delete the metadata files that left the metadata log with it, and expire the table's history
/// once there are [`History::keep`] snapshots to expire, so it's done once every so many commits, not on each.
/// Best effort: what is left over is deleted by a later commit's round (or never: it is only space).
async fn trim_history(catalog: &dyn Catalog, before: &Table, after: &Table) {
    delete_retired().await;
    forget_metadata(before, after).await;
    let history = History::from_env();
    if history.keep > 0
        && history.expirable(after).len() >= history.keep
        && let Err(e) = expire_history(catalog, after, history).await
    {
        eprintln!(
            "memstore: expiring the history of {} failed (left for a later commit): {e:#}",
            after.identifier()
        );
    }
}

/// The metadata files `before` names (its own and its log) that `after` no longer does, nor an older commit this
/// process saw on the table (callers may append from a handle loaded long before, whose log is older than what the
/// commit dropped). Only what is older than `after` is ever deleted: commits on one table may finish out of order,
/// and a newer one's files are its own. Nothing reads them: a table is loaded from its current metadata file, and no
/// read travels back in time.
async fn forget_metadata(before: &Table, after: &Table) {
    type Seen = HashMap<uuid::Uuid, (i64, HashSet<String>)>;
    static SEEN: std::sync::LazyLock<Mutex<Seen>> = std::sync::LazyLock::new(Default::default);
    let named = |t: &Table| -> HashSet<String> {
        t.metadata()
            .metadata_log()
            .iter()
            .map(|l| l.metadata_file.clone())
            .chain(t.metadata_location().map(str::to_string))
            .collect()
    };
    let kept = named(after);
    let mut gone = named(before);
    let at = after.metadata().last_updated_ms();
    {
        let mut seen = SEEN.lock().unwrap();
        let entry = seen
            .entry(after.metadata().uuid())
            .or_insert((i64::MIN, HashSet::new()));
        if entry.0 < at {
            gone.extend(std::mem::replace(entry, (at, kept.clone())).1);
        }
    }
    for path in gone.difference(&kept) {
        // Gone already (another writer's commit dropped it first) is fine.
        let _ = after.file_io().delete(path).await;
    }
}

/// Expire the snapshots of `table` past the newest `keep` ([`expire_history`]) when it holds more; otherwise only
/// delete what earlier expiries left due (and, on a local table, what was there from before). Returns the snapshots
/// before and after.
pub(crate) async fn trim_to(
    catalog: &dyn Catalog,
    table: &Table,
    keep: usize,
) -> Result<(usize, usize)> {
    let history = History {
        keep,
        ..History::from_env()
    };
    let n = table.metadata().snapshots().count();
    if n > keep {
        return expire_history(catalog, table, history).await;
    }
    delete_retired().await;
    sweep_local(catalog, table, history).await;
    Ok((n, n))
}

/// Expire the snapshots of `table` past what `history` keeps, then delete their manifest lists and the
/// metadata files that left the log. Returns the snapshots before and after.
pub async fn expire_history(
    catalog: &dyn Catalog,
    table: &Table,
    history: History,
) -> Result<(usize, usize)> {
    let before = table.metadata().snapshots().count();
    let mut table = table.clone();
    if history.keep == 0 {
        return Ok((before, before));
    }
    // The metadata log is kept as long as the snapshots: what leaves it is deleted on every commit. Best
    // effort, in a commit of its own, so a catalog that refuses the property still expires.
    let key = TableProperties::PROPERTY_METADATA_PREVIOUS_VERSIONS_MAX;
    let want = history.keep.to_string();
    if table.metadata().properties().get(key) != Some(&want) {
        let tx = Transaction::new(&table);
        match tx
            .update_table_properties()
            .set(key.into(), want)
            .apply(tx)
            .map(|tx| tx.commit(catalog))
        {
            Ok(commit) => match commit.await {
                Ok(after) => {
                    forget_metadata(&table, &after).await;
                    table = after;
                }
                Err(e) => eprintln!(
                    "memstore: setting {key} on {} failed: {e}",
                    table.identifier()
                ),
            },
            Err(e) => eprintln!(
                "memstore: setting {key} on {} failed: {e}",
                table.identifier()
            ),
        }
    }
    for attempt in 0u32.. {
        let ids = history.expirable(&table);
        if ids.is_empty() {
            break;
        }
        let tx = Transaction::new(&table);
        let result = tx
            .expire_snapshots()
            .expire_snapshot_ids(ids.iter().copied())
            // By id alone: no age cutoff of its own.
            .expire_older_than_ms(0)
            .retain_last(history.keep.max(1))
            .apply(tx)?
            .commit(catalog)
            .await;
        match result {
            Ok(after) => {
                let left: HashSet<i64> = after
                    .metadata()
                    .snapshots()
                    .map(|s| s.snapshot_id())
                    .collect();
                // When each stopped being current: when the snapshot after it on the line was made.
                let line = line(&table);
                let until: HashMap<i64, i64> = line
                    .windows(2)
                    .map(|w| (w[1].snapshot_id(), w[0].timestamp_ms()))
                    .collect();
                let now = chrono::Utc::now().timestamp_millis();
                RETIRED.lock().unwrap().extend(
                    ids.iter()
                        .filter(|id| !left.contains(id))
                        .filter_map(|id| table.metadata().snapshot_by_id(*id))
                        .map(|s| {
                            let until = until.get(&s.snapshot_id()).copied().unwrap_or(now);
                            (
                                after.file_io().clone(),
                                s.manifest_list().to_string(),
                                until + history.grace.as_millis() as i64,
                            )
                        }),
                );
                forget_metadata(&table, &after).await;
                table = after;
                break;
            }
            // A writer committed meanwhile: expire against the newer metadata.
            Err(e)
                if attempt < 20
                    && (e.kind() == iceberg::ErrorKind::CatalogCommitConflicts
                        || e.retryable()) =>
            {
                table = catalog.load_table(table.identifier()).await?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    delete_retired().await;
    sweep_local(catalog, &table, history).await;
    Ok((before, table.metadata().snapshots().count()))
}

/// A table on the local filesystem may hold metadata from before its history was trimmed (every metadata file
/// and manifest list since it was created): delete the metadata files numbered below the oldest one its log
/// names, and the manifest lists no snapshot names, unless they wait out their grace in [`RETIRED`]; either only
/// once older than the grace (a newer one may be a commit in flight). An object store has no listing here, so
/// its leftovers stay. Never while an older version's rename-based rewrite has the table aside: the old table keeps
/// its files in the same directory until it is purged. (A rewrite in place writes its data files under `data/`,
/// which this leaves alone.)
async fn sweep_local(catalog: &dyn Catalog, table: &Table, history: History) {
    let meta = table.metadata();
    let Some(dir) = meta.location().strip_prefix("file://") else {
        return;
    };
    let ident = table.identifier();
    let aside = format!("{}{}", ident.name(), offboard::OLD_TAG);
    if meta.properties().contains_key(offboard::REWRITE_PROP)
        || !catalog
            .list_tables(ident.namespace())
            .await
            .is_ok_and(|ts| ts.iter().all(|t| !t.name().starts_with(&aside)))
    {
        return;
    }
    let dir = std::path::PathBuf::from(dir).join("metadata");
    let version = |path: &str| -> Option<u64> {
        let name = path.rsplit('/').next()?;
        name.strip_suffix(".metadata.json")?
            .split('-')
            .next()?
            .parse()
            .ok()
    };
    let Some(oldest) = meta
        .metadata_log()
        .iter()
        .map(|l| l.metadata_file.as_str())
        .chain(table.metadata_location())
        .filter_map(version)
        .min()
    else {
        return;
    };
    let file = |p: &str| p.rsplit('/').next().map(str::to_string);
    let mut lists: HashSet<String> = meta
        .snapshots()
        .filter_map(|s| file(s.manifest_list()))
        .collect();
    lists.extend(
        RETIRED
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, p, _)| file(p)),
    );
    let grace = history.grace;
    let swept = tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // A file younger than the grace may be another table's: a rewrite renames this one aside and
            // creates its successor, numbered from 0, in this directory, after the check above.
            let old = || {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > grace))
            };
            let stale = if name.ends_with(".metadata.json") {
                version(&name).is_some_and(|v| v < oldest) && old()
            } else if name.starts_with("snap-") && name.ends_with(".avro") {
                !lists.contains(&name) && old()
            } else {
                false
            };
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    });
    let _ = swept.await;
}

/// How long a reader or writer waits for a table a rewrite (`offboard`) has renamed aside or not yet filled.
const REWRITE_WAIT: Duration = Duration::from_secs(300);

/// Whether a table of `ns` is renamed aside as `<name>__old_<id>` by a rewrite under way (or stopped).
async fn is_aside(catalog: &dyn Catalog, ns: &NamespaceIdent, name: &str) -> bool {
    let aside = format!("{name}{}", offboard::OLD_TAG);
    catalog
        .list_tables(ns)
        .await
        .is_ok_and(|ts| ts.iter().any(|t| t.name().starts_with(&aside)))
}

/// The ledger is being rewritten (compacted, or an org's rows removed): the table holds only part of its rows.
#[derive(Debug)]
struct Rewriting;

impl std::fmt::Display for Rewriting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the ledger is being rewritten")
    }
}

impl std::error::Error for Rewriting {}

/// Reload a table's metadata and read only the data files not already cached. The ledger is never read while a
/// rewrite has it aside or half filled: a commit would take its parents from part of it, and a reader would
/// miss commits. That waits up to [`REWRITE_WAIT`].
/// The longest a ledger append of this process took to land (µs: from its first read of the ledger to its commit,
/// retries included) since [`take_ledger_wait`] last read it: how long a writer waited around a ledger compaction.
static LEDGER_WAIT_MAX_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_ledger_wait(started: Instant) {
    LEDGER_WAIT_MAX_US.fetch_max(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The longest ledger append of this process since the last call, and start counting again.
pub fn take_ledger_wait() -> Duration {
    Duration::from_micros(LEDGER_WAIT_MAX_US.swap(0, std::sync::atomic::Ordering::Relaxed))
}

async fn refresh(catalog: &dyn Catalog, cache: &Cache, ns: &str, name: &str) -> Result<Snapshot> {
    let waited = Instant::now();
    loop {
        match with_retries(|| refresh_once(catalog, cache, ns, name)).await {
            Err(e)
                if ns == SYSTEM_NS
                    && name == LEDGER
                    && waited.elapsed() < REWRITE_WAIT
                    && (e.is::<Rewriting>()
                        || is_aside(catalog, &NamespaceIdent::new(ns.into()), name).await) =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            r => return r,
        }
    }
}

async fn refresh_once(
    catalog: &dyn Catalog,
    cache: &Cache,
    ns: &str,
    name: &str,
) -> Result<Snapshot> {
    let key = format!("{ns}.{name}");
    let prev = cache.entries.lock().unwrap().get(&key).cloned();
    let generation_now = || {
        cache
            .generations
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(0)
    };
    let started_generation = generation_now();
    let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
    let table = catalog
        .load_table(&ident)
        .await
        .with_context(|| format!("load table {key}"))?;
    if ns == SYSTEM_NS
        && name == LEDGER
        && table
            .metadata()
            .properties()
            .contains_key(offboard::REWRITE_PROP)
    {
        return Err(Rewriting.into());
    }
    let snapshot_id = table.metadata().current_snapshot().map(|s| s.snapshot_id());
    let schema_id = table.metadata().current_schema_id();
    // Cached batches are only reusable under the schema they were decoded with.
    let reusable = prev.filter(|p| p.schema_id == schema_id);
    let (rows, files, planned) = match &reusable {
        Some(p) if p.snapshot_id == snapshot_id => {
            (p.snap.rows.clone(), p.files.clone(), p.planned.clone())
        }
        _ => {
            let appended = match &reusable {
                Some(p) => read_appended(&table, p.snapshot_id, &p.files, &p.planned).await?,
                None => None,
            };
            let (files, planned) = match appended {
                Some(read) => read,
                None => read_planned(&table, reusable.map(|p| p.files).unwrap_or_default()).await?,
            };
            let rows = files.values().flat_map(|b| b.iter().cloned()).collect();
            (Arc::new(rows), files, planned)
        }
    };
    let snap = Snapshot { table, rows };
    let mut entries = cache.entries.lock().unwrap();
    let generation = generation_now();
    entries.insert(
        key,
        CacheEntry {
            snap: snap.clone(),
            fetched: (generation == started_generation).then(Instant::now),
            snapshot_id,
            schema_id,
            generation,
            loaded: Instant::now(),
            files,
            planned,
        },
    );
    Ok(snap)
}

/// A facts table's metadata and its planned data files.
async fn load_plan(catalog: &dyn Catalog, ns: &str) -> Result<(Table, Vec<FileScanTask>)> {
    with_retries(|| load_plan_once(catalog, ns)).await
}

async fn load_plan_once(catalog: &dyn Catalog, ns: &str) -> Result<(Table, Vec<FileScanTask>)> {
    let ident = TableIdent::new(NamespaceIdent::new(ns.into()), FACTS.into());
    let t0 = Instant::now();
    let table = catalog
        .load_table(&ident)
        .await
        .with_context(|| format!("load table {ns}.{FACTS}"))?;
    let loaded = t0.elapsed();
    let tasks = match table.metadata().current_snapshot() {
        None => vec![],
        Some(_) => {
            table
                .scan()
                .select_all()
                .with_concurrency_limit(LOAD_CONCURRENCY)
                .build()?
                .plan_files()
                .await?
                .try_collect()
                .await?
        }
    };
    // A plan that takes long says where: the table's metadata, or listing its files from the manifests.
    if t0.elapsed() > Duration::from_secs(1) {
        eprintln!(
            "memstore: slow plan of {ns}: {} ms ({} ms loading the table, {} snapshots, then {} files planned)",
            t0.elapsed().as_millis(),
            loaded.as_millis(),
            table.metadata().snapshots().count(),
            tasks.len()
        );
    }
    Ok((table, tasks))
}

/// A catalog that answers `load_table` for one table with a fixed, already-loaded version and
/// delegates everything else. `Transaction::commit` re-loads the table and rebases its actions
/// before committing; given this catalog, it commits against exactly the snapshot the caller
/// read, so the catalog's own snapshot requirement turns the commit into a compare-and-set.
#[derive(Debug)]
pub(crate) struct PinnedCatalog<'a> {
    pub(crate) inner: &'a dyn Catalog,
    pub(crate) pinned: Table,
}

#[async_trait::async_trait]
impl Catalog for PinnedCatalog<'_> {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        self.inner.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.create_namespace(ns, props).await
    }
    async fn get_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<iceberg::Namespace> {
        self.inner.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &NamespaceIdent) -> iceberg::Result<bool> {
        self.inner.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.inner.update_namespace(ns, props).await
    }
    async fn drop_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<()> {
        self.inner.drop_namespace(ns).await
    }
    async fn list_tables(&self, ns: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        self.inner.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &NamespaceIdent,
        creation: TableCreation,
    ) -> iceberg::Result<Table> {
        self.inner.create_table(ns, creation).await
    }
    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<Table> {
        if table == self.pinned.identifier() {
            return Ok(self.pinned.clone());
        }
        self.inner.load_table(table).await
    }
    async fn drop_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.drop_table(table).await
    }
    async fn purge_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.purge_table(table).await
    }
    async fn table_exists(&self, table: &TableIdent) -> iceberg::Result<bool> {
        self.inner.table_exists(table).await
    }
    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> iceberg::Result<()> {
        self.inner.rename_table(src, dest).await
    }
    async fn register_table(&self, table: &TableIdent, location: String) -> iceberg::Result<Table> {
        self.inner.register_table(table, location).await
    }
    async fn update_table(&self, commit: iceberg::TableCommit) -> iceberg::Result<Table> {
        self.inner.update_table(commit).await
    }
}

/// A commit lost a race on the table's snapshot (the catalog rejected its requirements).
pub(crate) fn is_commit_conflict(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<iceberg::Error>().is_some_and(|ie| {
            ie.kind() == iceberg::ErrorKind::CatalogCommitConflicts || ie.retryable()
        })
    })
}

/// An (entity, predicate)'s values written since a branch's base, per key.
type Changes = HashMap<(String, String), BTreeSet<String>>;

/// Which side's value a merge keeps for a conflicting key ([`Store::merge_resolving`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Main,
    Branch,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Main => "main",
            Side::Branch => "branch",
        }
    }
}

/// A conflicting key's values on each side since the branch's base, as one reader reads them
/// ([`Store::conflict_values`]).
#[derive(Clone, Debug, Default)]
pub struct ConflictValues {
    pub key: String,
    pub main: Vec<String>,
    pub branch: Vec<String>,
    /// Values from facts the reader may not read, left out.
    pub main_hidden: usize,
    pub branch_hidden: usize,
}

/// The source of a merge's record of the conflicts it resolved: `merge:<branch>`.
pub const MERGE_SOURCE: &str = "merge:";

/// What main and a branch each changed since the branch's base ([`Store::branch_diff`]).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BranchDiff {
    /// How many (entity, predicate) keys main wrote since the base.
    pub main_changed: usize,
    /// How many the branch wrote.
    pub branch_changed: usize,
    /// How many commits the branch made that main doesn't have yet (episodes alone count too): none once merged.
    pub branch_commits: usize,
    /// The keys both changed to different values (`<entity>.<predicate>`), sorted: keys only, since the values may
    /// come from episodes the reader cannot see.
    pub conflicts: Vec<String>,
}

struct SinceBase {
    ledger: Ledger,
    main_new: HashSet<String>,
    branch_new: HashSet<String>,
    main_changes: Changes,
    branch_changes: Changes,
}

impl SinceBase {
    fn conflicts(&self) -> Vec<String> {
        let mut conflicts: Vec<String> = self
            .conflict_pairs()
            .iter()
            .map(|k| format!("{}.{}", k.0, k.1))
            .collect();
        conflicts.sort();
        conflicts
    }

    /// The (entity, predicate) keys both sides changed to different values.
    fn conflict_pairs(&self) -> HashSet<(String, String)> {
        self.branch_changes
            .iter()
            .filter(|(k, theirs)| {
                self.main_changes
                    .get(*k)
                    .is_some_and(|ours| ours != *theirs)
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Keys named `<entity>.<predicate>` that more than one (entity, predicate) either side changed reads as
    /// (a `.` in an entity or a predicate): a resolution can't name one of those apart from the other.
    fn ambiguous(&self) -> HashSet<String> {
        let mut seen: HashMap<String, &(String, String)> = HashMap::new();
        let mut out = HashSet::new();
        for k in self.main_changes.keys().chain(self.branch_changes.keys()) {
            let name = format!("{}.{}", k.0, k.1);
            match seen.get(&name) {
                Some(other) if *other != k => {
                    out.insert(name);
                }
                _ => {
                    seen.insert(name, k);
                }
            }
        }
        out
    }
}

/// Marker error: a merge's heads moved before its commit landed.
#[derive(Debug)]
struct HeadsMoved;

impl std::fmt::Display for HeadsMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("heads moved during merge")
    }
}

impl std::error::Error for HeadsMoved {}

/// A write refused because an episode is about an id that names no type of the scope's schema
/// ([`Store::check_about`]): the caller's mistake, so a server answers it as one (400). Its message names the
/// episode as `episodes[i]`.
#[derive(Debug)]
pub struct AboutRefused(pub String);

impl std::fmt::Display for AboutRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AboutRefused {}

/// Cache key of a data file: its path plus its delete files, so new row deletes force a re-read.
pub(crate) fn file_key(task: &FileScanTask) -> String {
    let mut key = task.data_file_path.clone();
    for d in &task.deletes {
        key.push('|');
        key.push_str(&d.file_path);
    }
    key
}

async fn read_task(table: &Table, task: FileScanTask) -> Result<Vec<RecordBatch>> {
    with_retries(|| async {
        let task = task.clone();
        let stream = table
            .reader_builder()
            .build()
            .read(Box::pin(futures::stream::once(async { Ok(task) })))?
            .stream();
        Ok(stream.try_collect().await?)
    })
    .await
}

/// Object stores and catalogs answer some requests with transient 5xx / throttling errors and
/// expect a retry. Anything else (not found, denied, a commit conflict) is returned at once.
pub fn is_transient(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    [
        "status: 500",
        "status: 502",
        "status: 503",
        "status: 504",
        // Rate limited (BigLake's catalog API answers a burst of calls with it): backed off and tried again.
        "status: 429",
        "Too Many Requests",
        "InternalError",
        "SlowDown",
        "ServiceUnavailable",
        "Connection reset",
        "connection error",
    ]
    .iter()
    .any(|m| text.contains(m))
}

pub(crate) async fn with_retries<T, F, Fut>(op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_millis(50);
    for attempt in 0.. {
        match op().await {
            Err(e) if attempt < 6 && is_transient(&e) => {
                // Jittered (half to all of the step): callers throttled together, a scope's tables or pods
                // restarting at once, don't all come back in the same instant.
                let step = delay.as_millis() as u64;
                let ms =
                    step / 2 + (uuid::Uuid::new_v4().as_u128() % (step / 2 + 1) as u128) as u64;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                delay *= 2;
            }
            r => return r,
        }
    }
    unreachable!()
}

/// Decoded batches per data file of the table's current snapshot, reusing `cached` for files
/// already read. A file's key includes its delete files, so new row deletes force a re-read.
async fn read_files(
    table: &Table,
    cached: HashMap<String, Arc<Vec<RecordBatch>>>,
) -> Result<HashMap<String, Arc<Vec<RecordBatch>>>> {
    Ok(read_planned(table, cached).await?.0)
}

/// [`read_files`], from a full plan of the current snapshot (every manifest it names), and what the next
/// refresh needs to read only what is appended after it.
async fn read_planned(
    table: &Table,
    cached: HashMap<String, Arc<Vec<RecordBatch>>>,
) -> Result<(HashMap<String, Arc<Vec<RecordBatch>>>, Planned)> {
    let Some(current) = table.metadata().current_snapshot() else {
        return Ok((HashMap::new(), Planned::default()));
    };
    let tasks = plan_all(table).await?;
    let planned = Planned {
        manifests: Arc::new(match manifest_list(table, current).await? {
            Some(list) => list
                .entries()
                .iter()
                .map(|m| m.manifest_path.clone())
                .collect(),
            None => HashSet::new(),
        }),
        template: tasks.first().cloned(),
    };
    Ok((read_tasks(table, tasks, cached).await?, planned))
}

/// Object-store reads one table load keeps in flight: its manifests while planning, then its data files. They wait
/// on the network, not the CPU, so the planner's default (one per CPU, 4 on a 3.5-CPU pod) left a ledger of 37,000
/// one-commit files planning for minutes, past the life of the snapshot it was planning; and an unbounded fan-out
/// of the data files asks the store for every file at once.
const LOAD_CONCURRENCY: usize = 64;

/// Every scan task of the table's current snapshot.
async fn plan_all(table: &Table) -> Result<Vec<FileScanTask>> {
    Ok(table
        .scan()
        .select_all()
        .with_concurrency_limit(LOAD_CONCURRENCY)
        .build()?
        .plan_files()
        .await?
        .try_collect()
        .await?)
}

/// The manifest list of `snapshot`; none for an encrypted one.
async fn manifest_list(
    table: &Table,
    snapshot: &iceberg::spec::SnapshotRef,
) -> Result<Option<iceberg::spec::ManifestList>> {
    if snapshot.encryption_key_id().is_some() {
        return Ok(None);
    }
    let bytes = table
        .file_io()
        .new_input(snapshot.manifest_list())?
        .read()
        .await?;
    Ok(Some(iceberg::spec::ManifestList::parse_with_version(
        &bytes,
        table.metadata().format_version(),
    )?))
}

/// The current snapshot's files read from `prev`'s and the manifests appended since, when every snapshot after
/// `prev`'s only appended data files: no file removed or rewritten, no row deleted, no manifest rewritten.
/// None otherwise, and the caller reads a full plan. A forget, an ACL change or a retraction appends rows, so
/// it is read here; a purge or a rewrite replaces files, so it is never read here.
async fn read_appended(
    table: &Table,
    since: Option<i64>,
    prev_files: &HashMap<String, Arc<Vec<RecordBatch>>>,
    prev_planned: &Planned,
) -> Result<Option<(HashMap<String, Arc<Vec<RecordBatch>>>, Planned)>> {
    use iceberg::spec::{DataContentType, ManifestContentType, ManifestStatus, Operation};
    let meta = table.metadata();
    let (Some(current), Some(since), Some(template)) =
        (meta.current_snapshot(), since, &prev_planned.template)
    else {
        return Ok(None);
    };
    // Back from the current snapshot to the cached one, every step an append under the template's schema: a
    // task reads its file with the schema of the snapshot it was planned in, so a file appended after a column
    // was added must not be read with an older schema the template carries.
    let schema = Some(template.schema.schema_id());
    let mut at = current;
    while at.snapshot_id() != since {
        if at.summary().operation != Operation::Append || at.schema_id() != schema {
            return Ok(None);
        }
        match at.parent_snapshot_id().and_then(|p| meta.snapshot_by_id(p)) {
            Some(parent) => at = parent,
            // Not descended from the cached snapshot (or its history expired).
            None => return Ok(None),
        }
    }
    let Some(list) = manifest_list(table, current).await? else {
        return Ok(None);
    };
    let manifests: HashSet<String> = list
        .entries()
        .iter()
        .map(|m| m.manifest_path.clone())
        .collect();
    // A manifest the cached snapshot named is gone: manifests were rewritten.
    if !prev_planned.manifests.is_subset(&manifests) {
        return Ok(None);
    }
    let mut tasks = vec![];
    for m in list.entries() {
        if m.content != ManifestContentType::Data {
            return Ok(None);
        }
        if prev_planned.manifests.contains(&m.manifest_path) {
            continue;
        }
        let manifest = m.load_manifest(table.file_io()).await?;
        for e in manifest.entries() {
            if e.status() == ManifestStatus::Deleted || e.content_type() != DataContentType::Data {
                return Ok(None);
            }
            let f = e.data_file();
            tasks.push(FileScanTask {
                file_size_in_bytes: f.file_size_in_bytes(),
                start: 0,
                length: f.file_size_in_bytes(),
                record_count: Some(f.record_count()),
                data_file_path: f.file_path().to_string(),
                data_file_format: f.file_format(),
                deletes: vec![],
                partition: Some(f.partition().clone()),
                ..template.clone()
            });
        }
    }
    let mut keys: HashSet<String> = prev_files.keys().cloned().collect();
    keys.extend(tasks.iter().map(file_key));
    // MEMSTORE_CHECK_PLAN (set in CI's test run): every incremental read is checked against a full plan, its
    // files and how each new one is read. A divergence aborts the process: a refresh in the background only
    // logs its errors, and the test that hit it must fail.
    if std::env::var_os("MEMSTORE_CHECK_PLAN").is_some() {
        let full = plan_all(table).await?;
        let full_keys: HashSet<String> = full.iter().map(file_key).collect();
        let by_key: HashMap<String, &FileScanTask> =
            full.iter().map(|t| (file_key(t), t)).collect();
        let misread = tasks.iter().find(|t| {
            by_key.get(&file_key(t)).is_none_or(|f| {
                f.schema != t.schema
                    || f.project_field_ids != t.project_field_ids
                    || f.partition != t.partition
                    || f.record_count != t.record_count
            })
        });
        if full_keys != keys || misread.is_some() {
            eprintln!(
                "MEMSTORE_CHECK_PLAN: the incremental plan of {} differs from the full plan: {} files only in it, \
                 {} only in the full plan, new file read differently: {:?}",
                meta.location(),
                keys.difference(&full_keys).count(),
                full_keys.difference(&keys).count(),
                misread.map(|t| &t.data_file_path)
            );
            std::process::abort();
        }
    }
    let planned = Planned {
        manifests: Arc::new(manifests),
        template: Some(template.clone()),
    };
    let mut files = read_tasks(table, tasks, prev_files.clone()).await?;
    // Every file the cached snapshot had is still there (nothing was removed).
    for (k, b) in prev_files {
        files.entry(k.clone()).or_insert_with(|| b.clone());
    }
    Ok(Some((files, planned)))
}

/// Decoded batches per task's file, reusing `cached` for files already read.
pub(crate) async fn read_tasks(
    table: &Table,
    tasks: Vec<FileScanTask>,
    mut cached: HashMap<String, Arc<Vec<RecordBatch>>>,
) -> Result<HashMap<String, Arc<Vec<RecordBatch>>>> {
    let reads = tasks.into_iter().map(|task| {
        let key = file_key(&task);
        let hit = cached.remove(&key);
        async move {
            if let Some(batches) = hit {
                return Ok::<_, anyhow::Error>((key, batches));
            }
            Ok((key, Arc::new(read_task(table, task).await?)))
        }
    });
    Ok(futures::stream::iter(reads)
        .buffer_unordered(LOAD_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .collect())
}

pub fn ts(values: impl IntoIterator<Item = Option<i64>>) -> ArrayRef {
    Arc::new(TimestampMicrosecondArray::from_iter(values).with_timezone("+00:00"))
}

pub fn strings(b: &RecordBatch, col: &str) -> Vec<String> {
    let a = b.column_by_name(col).expect(col).as_string::<i32>();
    (0..a.len()).map(|i| a.value(i).to_string()).collect()
}

pub fn optional_strings(b: &RecordBatch, col: &str) -> Vec<Option<String>> {
    match b.column_by_name(col) {
        None => vec![None; b.num_rows()],
        Some(a) => {
            let a = a.as_string::<i32>();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i).to_string()))
                .collect()
        }
    }
}

pub fn timestamps(b: &RecordBatch, col: &str) -> Vec<Option<i64>> {
    let a = b
        .column_by_name(col)
        .expect(col)
        .as_primitive::<TimestampMicrosecondType>();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i)))
        .collect()
}

/// Episode bodies sit under the episodes table's own location, so storage credentials
/// vended per table (Snowflake Horizon) cover them.
fn body_path(episodes: &Table, episode_id: &str) -> String {
    format!(
        "{}/bodies/{}",
        episodes.metadata().location().trim_end_matches('/'),
        episode_id
    )
}

/// Facts rows of a batch as (commit id, fact). `sources` names the source-episodes column,
/// which differs between the Iceberg table and the search index.
pub fn decode_facts(b: &RecordBatch, sources: &str) -> Vec<(String, Fact)> {
    let s = |n: &str| strings(b, n);
    let (commit, fid, ent, pred, val, src) = (
        s("commit_id"),
        s("fact_id"),
        s("entity"),
        s("predicate"),
        s("value"),
        s(sources),
    );
    let ts = |n: &str| -> Vec<Option<i64>> {
        let a = b.column_by_name(n).expect(n);
        match a.data_type() {
            arrow_schema::DataType::Int64 => {
                let a = a.as_primitive::<arrow_array::types::Int64Type>();
                (0..a.len())
                    .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                    .collect()
            }
            _ => timestamps(b, n),
        }
    };
    let (from, to) = (ts("valid_from"), ts("valid_to"));
    // Rows written before the quote and typed columns existed have none.
    let quote = optional_strings(b, "quote");
    let (etype, vtype, vref) = (
        optional_strings(b, "entity_type"),
        optional_strings(b, "value_type"),
        optional_strings(b, "value_ref"),
    );
    let vnum = optional_f64(b, "value_num");
    let vts = optional_micros(b, "value_ts");
    let (unit, cite, basis) = (
        optional_strings(b, "unit"),
        optional_strings(b, "cite"),
        optional_strings(b, "time_basis"),
    );
    let tags = optional_strings(b, "tags");
    (0..b.num_rows())
        .map(|i| {
            (
                commit[i].clone(),
                Fact {
                    fact_id: fid[i].clone(),
                    entity: ent[i].clone(),
                    predicate: pred[i].clone(),
                    value: val[i].clone(),
                    valid_from: from[i].unwrap_or_default(),
                    valid_to: to[i],
                    source_episodes: split_list(&src[i]),
                    quote: quote[i].clone(),
                    entity_type: etype[i].clone(),
                    value_type: vtype[i].clone(),
                    value_num: vnum[i],
                    value_ts: vts[i],
                    value_ref: vref[i].clone(),
                    unit: unit[i].clone(),
                    cite: cite[i].clone(),
                    time_basis: basis[i].clone(),
                    tags: tags[i].as_deref().map(split_list).unwrap_or_default(),
                },
            )
        })
        .collect()
}

pub fn optional_bools(b: &RecordBatch, col: &str) -> Vec<Option<bool>> {
    match b.column_by_name(col) {
        None => vec![None; b.num_rows()],
        Some(a) => {
            let a = a.as_boolean();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                .collect()
        }
    }
}

pub fn optional_f64(b: &RecordBatch, col: &str) -> Vec<Option<f64>> {
    match b.column_by_name(col) {
        None => vec![None; b.num_rows()],
        Some(a) => {
            let a = a.as_primitive::<arrow_array::types::Float64Type>();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                .collect()
        }
    }
}

/// Microseconds from a timestamp column (the Iceberg table) or an Int64 one (the index).
pub fn optional_micros(b: &RecordBatch, col: &str) -> Vec<Option<i64>> {
    match b.column_by_name(col) {
        None => vec![None; b.num_rows()],
        Some(a) => match a.data_type() {
            arrow_schema::DataType::Int64 => {
                let a = a.as_primitive::<arrow_array::types::Int64Type>();
                (0..a.len())
                    .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                    .collect()
            }
            _ => timestamps(b, col),
        },
    }
}

/// Reject malformed ACL entries and source ids before anything is written.
fn validate_write(episodes: &[Episode], facts: &[Fact]) -> Result<()> {
    for e in episodes {
        for a in &e.acl {
            validate_acl_entry(a)?;
        }
        for id in &e.about {
            anyhow::ensure!(
                !id.trim().is_empty() && !id.contains(','),
                "episode about ids must be non-empty and contain no comma, got {id:?}"
            );
        }
    }
    for f in facts {
        for id in &f.source_episodes {
            anyhow::ensure!(
                id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                "source episode id must be a sha256 hex digest, got {id:?}"
            );
        }
        if let Some(c) = &f.cite {
            serde_json::from_str::<episode_types::Cite>(c).map_err(|e| {
                anyhow::anyhow!("fact {}: cite is not a citation ({e}): {c}", f.fact_id)
            })?;
        }
        if let Some(t) = &f.time_basis {
            anyhow::ensure!(
                TIME_BASES.contains(&t.as_str()),
                "fact {}: time_basis must be one of {}, got {t:?}",
                f.fact_id,
                TIME_BASES.join(", ")
            );
        }
        anyhow::ensure!(
            f.tags.len() <= MAX_TAGS && f.tags.iter().all(|t| valid_tag(t)),
            "fact {}: at most {MAX_TAGS} tags, each lower-case letters, digits, _ and - (at most 32)",
            f.fact_id
        );
    }
    Ok(())
}

/// ACLs are stored comma-joined, so an entry must not be able to smuggle in a second one.
fn validate_acl_entry(a: &str) -> Result<()> {
    let ok_id = auth::is_acl_id;
    let ok = a == "*"
        || match a.split_once(':') {
            Some(("user" | "group" | "role", id)) => ok_id(id),
            _ => false,
        };
    anyhow::ensure!(
        ok,
        "invalid ACL entry {a:?}: expected *, user:<id>, group:<id> or role:<name>"
    );
    Ok(())
}

pub fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

pub fn now_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

/// Rows of `table`'s current snapshot: `columns` only (none: all), of the data files `filter` may
/// match (their column bounds decide, so files wholly outside it are never read).
pub async fn scan_rows(
    table: &Table,
    columns: Option<&[&str]>,
    filter: Option<iceberg::expr::Predicate>,
) -> Result<Vec<RecordBatch>> {
    if table.metadata().current_snapshot().is_none() {
        return Ok(vec![]);
    }
    let mut scan = match columns {
        Some(c) => table.scan().select(c.iter().copied()),
        None => table.scan().select_all(),
    };
    if let Some(f) = filter {
        scan = scan.with_filter(f);
    }
    let tasks: Vec<FileScanTask> = scan.build()?.plan_files().await?.try_collect().await?;
    let reads = tasks.into_iter().map(|t| read_task(table, t));
    Ok(futures::future::try_join_all(reads)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// The directory's changes after `after` (none: all of them), in the order they apply: by time, and
/// changes that share one (only writers from before compare-and-set made such) by their content, so
/// every server folds them alike.
/// One batch of `memory_system.installations` as events.
fn installation_batch(b: &RecordBatch) -> Vec<people::installations::Event> {
    let (at, actor, op, id) = (
        timestamps(b, "at"),
        strings(b, "actor"),
        strings(b, "op"),
        strings(b, "installation_id"),
    );
    let (provider, group_ref, on_behalf_of) = (
        optional_strings(b, "provider"),
        optional_strings(b, "group_ref"),
        optional_strings(b, "on_behalf_of"),
    );
    (0..b.num_rows())
        .map(|i| people::installations::Event {
            at: at[i].unwrap_or_default(),
            actor: actor[i].clone(),
            op: op[i].clone(),
            installation_id: id[i].clone(),
            provider: provider[i].clone(),
            group_ref: group_ref[i].clone(),
            on_behalf_of: on_behalf_of[i].clone(),
        })
        .collect()
}

async fn directory_rows(table: &Table, after: Option<i64>) -> Result<Vec<directory::Event>> {
    let filter = after.map(|a| {
        iceberg::expr::Reference::new("at")
            .greater_than(iceberg::spec::Datum::timestamptz_micros(a))
    });
    let mut events = vec![];
    for b in scan_rows(table, None, filter).await? {
        let (at, actor, op, org) = (
            timestamps(&b, "at"),
            strings(&b, "actor"),
            strings(&b, "op"),
            strings(&b, "org"),
        );
        let (team, user, role, detail) = (
            optional_strings(&b, "team"),
            optional_strings(&b, "user"),
            optional_strings(&b, "role"),
            optional_strings(&b, "detail"),
        );
        let version = match b.column_by_name("version") {
            Some(_) => longs(&b, "version"),
            None => vec![None; b.num_rows()],
        };
        for i in 0..b.num_rows() {
            events.push(directory::Event {
                version: version[i].unwrap_or(1),
                at: at[i].unwrap_or_default(),
                actor: actor[i].clone(),
                op: op[i].clone(),
                org: org[i].clone(),
                team: team[i].clone(),
                user: user[i].clone(),
                role: role[i].clone(),
                detail: detail[i].clone(),
            });
        }
    }
    if let Some(a) = after {
        events.retain(|e| e.at > a);
    }
    events.sort_by(|a, b| {
        type Key<'a> = (i64, &'a str, &'a str, [&'a Option<String>; 4], &'a str);
        fn key(e: &directory::Event) -> Key<'_> {
            let rest = [&e.team, &e.user, &e.role, &e.detail];
            (e.at, &e.op, &e.org, rest, &e.actor)
        }
        key(a).cmp(&key(b))
    });
    Ok(events)
}

fn directory_columns(events: &[directory::Event]) -> Vec<(&'static str, ArrayRef)> {
    let col = |f: &dyn Fn(&directory::Event) -> Option<&str>| -> ArrayRef {
        Arc::new(StringArray::from(events.iter().map(f).collect::<Vec<_>>()))
    };
    vec![
        ("at", ts(events.iter().map(|e| Some(e.at)))),
        ("actor", col(&|e| Some(e.actor.as_str()))),
        ("op", col(&|e| Some(e.op.as_str()))),
        ("org", col(&|e| Some(e.org.as_str()))),
        ("team", col(&|e| e.team.as_deref())),
        ("user", col(&|e| e.user.as_deref())),
        ("role", col(&|e| e.role.as_deref())),
        ("detail", col(&|e| e.detail.as_deref())),
        (
            "version",
            Arc::new(Int64Array::from(
                events.iter().map(|e| Some(e.version)).collect::<Vec<_>>(),
            )),
        ),
    ]
}

/// How many rows `table`'s current snapshot holds, from its metadata (the snapshot's summary, or the
/// data files' record counts); none if it doesn't say.
async fn live_rows(table: &Table) -> Result<Option<u64>> {
    let Some(snap) = table.metadata().current_snapshot() else {
        return Ok(Some(0));
    };
    if let Some(n) = snap
        .summary()
        .additional_properties
        .get("total-records")
        .and_then(|n| n.parse().ok())
    {
        return Ok(Some(n));
    }
    let tasks: Vec<FileScanTask> = table
        .scan()
        .select_all()
        .build()?
        .plan_files()
        .await?
        .try_collect()
        .await?;
    if tasks.iter().any(|t| !t.deletes.is_empty()) {
        return Ok(None);
    }
    Ok(tasks.iter().map(|t| t.record_count).sum())
}

/// A column a table may predate: all none when it does.
pub fn optional_longs(b: &RecordBatch, col: &str) -> Vec<Option<i64>> {
    match b.column_by_name(col) {
        None => vec![None; b.num_rows()],
        Some(_) => longs(b, col),
    }
}

pub fn longs(b: &RecordBatch, col: &str) -> Vec<Option<i64>> {
    let a = b
        .column_by_name(col)
        .expect(col)
        .as_primitive::<arrow_array::types::Int64Type>();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i)))
        .collect()
}

/// A key use: `(sha256, org, name, µs since the epoch)`.
pub type KeyUse = (String, String, String, i64);

#[cfg(test)]
mod principal_tests {
    use super::Principal;

    #[test]
    fn snowflake_users_and_roles_match_exactly_and_fail_closed() {
        let acl = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let alice = Principal::new("ALICE", &["role:SALES", "acme"]);
        assert!(alice.can_see(&acl(&["user:ALICE"])));
        assert!(alice.can_see(&acl(&["role:SALES"])));
        // quoted Snowflake names are case-sensitive: "alice" is not ALICE
        assert!(!alice.can_see(&acl(&["user:alice"])));
        assert!(!alice.can_see(&acl(&["role:sales"])));
        assert!(alice.can_see(&acl(&["group:acme"])));
        assert!(alice.can_see(&acl(&["*"])));
        assert!(!alice.can_see(&acl(&["role:ENG", "user:bob"])));
        // a group named like a role is not the role, and a role is not a group
        assert!(!alice.can_see(&acl(&["group:role:SALES"])));
        assert!(!Principal::new("BOB", &["SALES"]).can_see(&acl(&["role:SALES"])));
        assert!(!alice.can_see(&acl(&[])));
        assert!(!alice.can_see(&acl(&["role:", "user:", "bogus"])));
    }
}
