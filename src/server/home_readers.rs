//! Who opened Studio's Home lately, written down ([`crate::recent_readers`]) so a server that just started works
//! their Home out before it says it is ready ([`prime_readers`], from the warm-up): their first visit after a deploy
//! is answered from it at once, as a return visit is.
//!
//! - **Written down.** Each Home ask on main notes its reader ([`seen`]): who they are and how they signed in, as the
//!   ask had them ([`ReadsAs`]): not their groups, worked out again when they are read. Only a person's own sign-in (or
//!   a local server's one caller), never a superuser's, and only when the groups the ask held are those a warm-up
//!   would work out for them: a key's, a platform's or a recorder's is never written, nor an operator's (whose reads
//!   are no org's to record), nor anything that signs someone in. A super admin's own sign-in is written as theirs,
//!   saying they signed in as one; a warm-up reads all of the scope as them ([`principal_of`]) only while the server
//!   it runs on lists them as one too, and otherwise as the person they sign in as there. Written by this server every
//!   [`flush_every`] at most, at shutdown, and a reader already written only once [`WRITTEN_FOR`] has passed: not a
//!   write per read. How many reads it didn't write down, and why, it logs with each write. Every write ([`store_readers`]) merges with what other servers wrote, keeps only
//!   readers who still read the scope (left, disabled or aged-out ones go), and checks the file again after it.
//! - **Their reads.** Beside each reader, the other Studio reads they made on main that answer from a kept answer
//!   (the listings, the Team page's counts by source and a schema type's records with their facts, the advice list
//!   and page, the lessons, what each source taught, the Skills list, and an admin's Traces and Not answered), as they
//!   asked them ([`read`], [`admin_read`], [`replayable`]), in fixed shapes alone: never a name, a key, a cursor or a
//!   path into memory. An admin's read is made again through `/admin`, as they are signed in there, which checks
//!   again that they may make it. The dream's decisions and the conflicts count need no record: Home's parts read
//!   them through the same kept answers, under the same reader, so a primed Home primes them.
//! - **Primed.** At start, each scope's readers who still read it as they did (the directory still gives them the
//!   scope, they aren't disabled) have their Home, their count and list of proposals and those reads worked out, as they ask
//!   them, [`prime_at_once`] readers at a time. A scope with readers has its skills' summaries read beside them.
//!   The scopes the server is for (its served scope, and on an org's own cell that org and its teams, one with
//!   readers written down even when nothing was written to it lately) are warmed first and primed as soon as their
//!   Home is, before `/ready`, up to its bound. Every other scope's priming shares
//!   one deadline ([`prime_deadline`]): at most [`prime_budget`] after the warm-up began, and never past half the
//!   warm-up's bound on `/ready`; what is left goes on in the background. Those not written down any more (they
//!   left, or are disabled) are taken out of the file then.
//! - **Never another's.** A Home worked out here is kept under its reader's key (who they are, every group they read
//!   as now, how they signed in, and their standing as the directory says now), as every Home worked out ahead is:
//!   only an ask whose own key is the same is answered from it. Their other reads are made as the reader, through
//!   the request's own code, and kept by it under that reader's key as when they ask. A reader whose sign-in, groups
//!   or standing changed since they were written down has another key, so their ask works its answer out as if none
//!   was primed.

use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use super::*;
use crate::recent_readers::RecentReader;

/// How long after a reader was written this server writes them again (their last read moved on).
const WRITTEN_FOR: Duration = Duration::from_secs(3600);

/// How often this server writes the readers it saw (`AMPUP_MEMORY_HOME_READERS_SECS`, default 60).
fn flush_every() -> Duration {
    Duration::from_secs(
        std::env::var("AMPUP_MEMORY_HOME_READERS_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60),
    )
}

/// How many readers' Homes the warm-up works out at once (`AMPUP_MEMORY_PRIME_AT_ONCE`, default 4; 0: none).
fn prime_at_once() -> usize {
    std::env::var("AMPUP_MEMORY_PRIME_AT_ONCE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(4)
}

/// How long the warm-up works readers' Homes out, every scope's together (`AMPUP_MEMORY_PRIME_SECS`, default 60).
/// Those not done by then go on in the background, and later scopes' readers aren't primed.
fn prime_budget() -> Duration {
    Duration::from_secs(
        std::env::var("AMPUP_MEMORY_PRIME_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60),
    )
}

/// When the warm-up that began at `began` stops priming: [`prime_budget`] after it, and no later than half of
/// `ready_within` (the warm-up's bound on `/ready`), so the scopes warmed after priming still have their time.
pub(super) fn prime_deadline(began: Instant, ready_within: Duration) -> tokio::time::Instant {
    tokio::time::Instant::from_std(began + prime_budget().min(ready_within / 2))
}

/// How a reader read, as written down: their sign-in alone, never a credential. Not the groups they read as, nor
/// whether they read as a superuser: [`reader_of`] works their groups out again from the closures and the org as they
/// are when it reads them ([`principal_of`]), and no superuser is written down at all. Every field of the sign-in is
/// kept because each is part of the key a Home is kept under (`{via:?}` in its standing), and the directory's check
/// of what they read turns on its kind, orgs, groups, provider and email: without one, nothing primed would ever
/// match their ask.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadsAs {
    sign_in: SignedIn,
}

/// A person's sign-in ([`SignIn`]) without anything a key, a platform or a recorder signs in with: those aren't
/// written down.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedIn {
    person: bool,
    orgs: Option<std::collections::BTreeSet<String>>,
    groups: Vec<String>,
    verified_email: bool,
    auth0: bool,
    auth0_org: Option<String>,
    mfa: bool,
    unverified: Option<String>,
    local_admin: Option<String>,
    read_only: bool,
    /// They signed in as a super admin (`AMPUP_MEMORY_SUPER_ADMINS`). Read back only while the server reading it lists
    /// them as one too ([`SignedIn::sign_in`]): what is written here never makes anyone one. Not written when false, so
    /// everyone else's sign-in reads as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    super_admin: bool,
}

impl SignedIn {
    /// `via` as written down: none for a sign-in that isn't a person's own (a key, a platform's or its tenant's, a
    /// provisioner's, a recorder's, an operator's), or a local server's one caller.
    fn of(via: &SignIn) -> Option<SignedIn> {
        let SignIn {
            person,
            orgs,
            groups,
            verified_email,
            key,
            auth0,
            auth0_org,
            mfa,
            unverified,
            operator,
            super_admin,
            recorded,
            local_admin,
            provisioner,
            platform,
            tenant,
            read_only,
        } = via.clone();
        if key.is_some()
            || provisioner.is_some()
            || platform.is_some()
            || tenant.is_some()
            || recorded
            || operator
            || !(person || local_admin.is_some())
        {
            return None;
        }
        Some(SignedIn {
            person,
            orgs,
            groups,
            verified_email,
            auth0,
            auth0_org,
            mfa,
            unverified,
            local_admin,
            read_only,
            super_admin,
        })
    }

    /// `user`'s sign-in as written down, a super admin's only while this server lists `user` as one.
    fn sign_in(&self, user: &str) -> SignIn {
        SignIn {
            person: self.person,
            orgs: self.orgs.clone(),
            groups: self.groups.clone(),
            verified_email: self.verified_email,
            key: None,
            auth0: self.auth0,
            auth0_org: self.auth0_org.clone(),
            mfa: self.mfa,
            unverified: self.unverified.clone(),
            operator: false,
            super_admin: self.super_admin && auth::listed_super_admin(user),
            recorded: false,
            local_admin: self.local_admin.clone(),
            provisioner: None,
            platform: None,
            tenant: None,
            read_only: self.read_only,
        }
    }
}

/// A reader this server saw, by scope namespace and then by who they are and how they read.
struct Seen {
    scope: Scope,
    reader: RecentReader,
    /// When this server last wrote them; none: not yet.
    written: Option<Instant>,
}

static SEEN: LazyLock<Mutex<HashMap<String, HashMap<String, Seen>>>> =
    LazyLock::new(Default::default);

/// Whether the writer ([`flush`]) runs.
static FLUSHING: std::sync::Once = std::sync::Once::new();

/// A Home ask on `scope`'s main by `who`, signed in by `via`: noted, to be written down ([`flush`]).
pub(super) fn seen(
    state: &Arc<State>,
    scope: &Scope,
    who: &Principal,
    via: &SignIn,
    superuser: bool,
) {
    note(state, scope, who, via, superuser, None);
}

/// Another Studio read `req` on `scope`'s main by `who`: noted beside them when it answers from a kept answer and
/// names no one else ([`replayable`]; a type's listing only of a type the scope's schema, `schema`, defines), so a new
/// server works it out for them too.
pub(super) fn read(
    state: &Arc<State>,
    scope: &Scope,
    who: &Principal,
    via: &SignIn,
    superuser: bool,
    req: &Value,
    schema: Option<&crate::query::schema::Schema>,
) {
    // An `/admin` read is recorded by `/admin` alone ([`admin_read`]), whatever a scope's read was asked as.
    if let Some(q) = replayable(req)
        && !admin_op(&q)
        && typed_in(&q, schema)
    {
        note(state, scope, who, via, superuser, Some(q));
    }
}

/// A read of org `org`'s memory through `/admin` by `who` (as `/admin` signs them in, before any scope is chosen):
/// noted beside them as a read of the org's scope when it is one a warm-up makes again ([`replayable`]: the lists of
/// Traces and Not answered). `org` is the org's own name, an alias the request named already taken as it.
pub(super) fn admin_read(
    state: &Arc<State>,
    org: &str,
    who: &Principal,
    via: &SignIn,
    superuser: bool,
    req: &Value,
) {
    let Ok(scope) = Scope::org(org) else {
        return;
    };
    if let Some(q) = replayable(req)
        && admin_op(&q)
    {
        note(state, &scope, who, via, superuser, Some(q));
    }
}

/// Whether `q` ([`replayable`]) is read through `/admin`, as its reader is signed in before a scope is chosen
/// ([`signed_in_as`]), rather than through a read of the scope ([`principal_of`]).
fn admin_op(q: &Value) -> bool {
    q["op"] == "activity"
}

/// Whether `q` names only types `schema` defines: a listing of one type's entities (`ls` at `entities/<type>`) is made
/// again only for a type of the scope's schema, a name of its configuration and never one of memory.
fn typed_in(q: &Value, schema: Option<&crate::query::schema::Schema>) -> bool {
    match q["path"].get(1).and_then(Value::as_str) {
        None => true,
        Some(t) => schema.is_some_and(|s| s.types.contains_key(t)),
    }
}

/// A Studio read (`/browse`, `/lessons`, `/advice/list`, the MCP `list_advice`, the Skills list, `/admin`'s activity) a
/// warm-up may make again as its reader: one that answers from a kept answer, in one of a few fixed shapes, as it was
/// asked. Only these fields, each a number, a boolean or one of a few fixed words, never a name, a key, a cursor or a
/// path into memory (so nothing recorded outlives what it named): the top of the entities (with each type's records
/// by source, `sources`), sessions, sources, notes or skills (`ls`, with its `limit`), one type's entities with their
/// facts (`facts`; the type a word, made again only when the scope's schema defines it, [`typed_in`]), the advice list
/// by its space (the rest only filters what is kept), the advice page over some days, the lessons (their space, days,
/// page, trail entries and how they are shown), what the sources taught (`top`, and `prefixes` only when each is a
/// connector's kind alone, [`connector_kind`]), the Skills list (its origin, whether only those
/// proposed), and the org's activity as Traces and Not answered list it (`light`, its window, page size and whether
/// only what wasn't answered: never one person's, one entry, a team's or a later page). Anything else isn't recorded.
fn replayable(req: &Value) -> Option<Value> {
    let o = req.as_object()?;
    let op = o.get("op")?.as_str()?;
    let words = |k: &str, allowed: &[&str]| {
        o.get(k)
            .is_none_or(|v| v.is_null() || v.as_str().is_some_and(|w| allowed.contains(&w)))
    };
    // Each field this op may carry is one of `fields`, and holds a number, a boolean or nothing; `worded` ones are
    // checked apart.
    let only = |fields: &[&str], worded: &[&str]| {
        o.iter().all(|(k, v)| {
            k == "op"
                || worded.contains(&k.as_str())
                || (fields.contains(&k.as_str())
                    && matches!(v, Value::Null | Value::Bool(_) | Value::Number(_)))
        })
    };
    // A type's name as a schema writes one: a short word, never an entity's key.
    let a_type = |t: &str| {
        !t.is_empty()
            && t.len() <= 64
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let spaces = ["mine", "team", "all"];
    let ok = match op {
        "ls" => {
            let flag = |k: &str| o.get(k).is_some_and(|v| v == true);
            only(&["limit", "sources", "facts"], &["path"])
                && match o.get("path").and_then(Value::as_array).map(Vec::as_slice) {
                    Some([f]) => {
                        f.as_str().is_some_and(|f| {
                            ["entities", "sessions", "sources", "notes", "skills"].contains(&f)
                                && (f == "entities" || !flag("sources"))
                        }) && !flag("facts")
                    }
                    Some([e, t]) => {
                        e == "entities" && t.as_str().is_some_and(a_type) && !flag("sources")
                    }
                    _ => false,
                }
        }
        "advice" => {
            return Some(
                json!({"op": "advice", "space": match o.get("space").and_then(Value::as_str) {
                    Some(s) if spaces.contains(&s) => s,
                    _ => "all",
                }}),
            );
        }
        "advice_usage" => only(&["days"], &[]),
        "lessons" => {
            only(
                &[
                    "days", "limit", "page", "trail", "probe", "count", "as_usage",
                ],
                &["space", "show"],
            ) && words("space", &spaces)
                && words("show", &["", "team", "mine"])
        }
        // The console's Sources page asks what each kind of connector taught (`hubspot:`, `granola:`): a prefix that is
        // a connector's kind alone, a word and a colon, names nothing in memory. One that names more (a repo, a
        // channel) isn't recorded.
        "sources" => {
            only(&["top"], &["prefixes"])
                && o.get("prefixes").is_none_or(|p| {
                    p.is_null()
                        || p.as_array().is_some_and(|a| {
                            a.len() <= MAX_KIND_PREFIXES
                                && a.iter().all(|k| k.as_str().is_some_and(connector_kind))
                        })
                })
        }
        "skills_list" => only(&["proposed"], &["origin"]) && words("origin", &["repo", "compiled"]),
        // The org is the scope it is recorded under, never kept in the read.
        "activity" => {
            if o.get("light") != Some(&Value::Bool(true))
                || !o.iter().all(|(k, v)| {
                    k == "org"
                        || k == "op"
                        || ["light", "since_secs", "limit", "gaps_only"].contains(&k.as_str())
                            && matches!(v, Value::Null | Value::Bool(_) | Value::Number(_))
                })
            {
                return None;
            }
            let mut q = req.clone();
            q.as_object_mut()?.remove("org");
            return Some(q);
        }
        _ => false,
    };
    ok.then(|| req.clone())
}

/// At most this many connector kinds in a recorded `sources` read ([`replayable`]).
const MAX_KIND_PREFIXES: usize = 20;

/// A source prefix that is a connector's kind alone (`hubspot:`, `dynamicsCRM:`): a short word and a colon, nothing
/// after it.
fn connector_kind(p: &str) -> bool {
    p.strip_suffix(':').is_some_and(|w| {
        (1..=32).contains(&w.len())
            && w.starts_with(|c: char| c.is_ascii_alphabetic())
            && w.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// Takes out of `scope`'s file those who no longer read it as written ([`store_readers`], with the directory read now:
/// never one read lately, which may not show a reader yet), from the file as it is now (another server may have
/// written since it was read for priming).
async fn prune(state: &Arc<State>, store: &Store, scope: &Scope) -> Result<()> {
    let now = store.recent_readers(scope).await?;
    store_readers(state, store, scope, now).await?;
    Ok(())
}

/// Makes `q` ([`replayable`]) again as `reader`, through the request's own code: the Skills list's, `/admin`'s (as
/// `admin`, signed in by `via`, in `scope`'s org: it checks again that they may read it), or browse's.
#[allow(clippy::too_many_arguments)]
async fn replay(
    state: &Arc<State>,
    store: &Store,
    session: &Session,
    scope: &Scope,
    reader: crate::browse::Reader<'_>,
    admin: &Principal,
    via: &SignIn,
    q: &Value,
) -> Result<()> {
    if admin_op(q) {
        let Some(org) = scope.org_name() else {
            anyhow::bail!("an org's read, of no org");
        };
        let mut body = q.clone();
        body["org"] = json!(org);
        Box::pin(directory_change(
            state,
            admin,
            via,
            false,
            body.to_string().as_bytes(),
        ))
        .await?;
        return Ok(());
    }
    if !typed_in(q, session.schema().as_deref()) {
        anyhow::bail!("a type the schema no longer defines");
    }
    match q["op"].as_str() {
        Some("skills_list") => {
            let origin = match q["origin"].as_str() {
                Some("repo") => Some(crate::skill_review::Origin::Repo),
                Some("compiled") => Some(crate::skill_review::Origin::Compiled),
                _ => None,
            };
            let proposed = q["proposed"].as_bool().unwrap_or(false);
            crate::skill_review::list(store, session, reader.who, origin, proposed).await?;
        }
        _ => {
            Box::pin(crate::browse::run(store, session, reader, q)).await?;
        }
    }
    Ok(())
}

/// [`seen`] and [`read`]: `who` noted as they read `scope`, with `read` among their reads.
fn note(
    state: &Arc<State>,
    scope: &Scope,
    who: &Principal,
    via: &SignIn,
    superuser: bool,
    read: Option<Value>,
) {
    // The writer runs from the first read on, so it says what it left out even when it writes no one.
    FLUSHING.call_once(|| {
        tokio::spawn(crate::loads::by("home_readers", flush(state.clone())));
    });
    use std::sync::atomic::Ordering::Relaxed;
    // A superuser, or a reader of everything other than a super admin's own sign-in (an operator's), isn't written
    // down.
    if superuser || (who.reads_all() && !super_admin_reads(via)) {
        NOT_WRITTEN.all.fetch_add(1, Relaxed);
        return;
    }
    let Some(sign_in) = SignedIn::of(via) else {
        NOT_WRITTEN.not_a_person.fetch_add(1, Relaxed);
        return;
    };
    // Nor one whose groups aren't those a warm-up would work out for them (a scope the request named itself, say):
    // nothing primed for them could match their ask.
    let derived = match read.as_ref().is_some_and(admin_op) {
        true => signed_in_as(state, who.user(), via),
        false => principal_of(state, who.user(), via, scope),
    };
    if crate::kept_answers::reader_key(&derived) != crate::kept_answers::reader_key(who) {
        NOT_WRITTEN.other_key.fetch_add(1, Relaxed);
        return;
    }
    let reads_as = ReadsAs { sign_in };
    let Ok(value) = serde_json::to_value(&reads_as) else {
        return;
    };
    let id = format!("{}\n{value}", who.user());
    let reader = RecentReader {
        user: who.user().to_string(),
        seen: crate::now_micros(),
        reads_as: value,
        reads: read.iter().cloned().collect(),
    };
    {
        let mut all = SEEN.lock().unwrap();
        let readers = all.entry(scope.namespace()).or_default();
        match readers.get_mut(&id) {
            Some(s) => {
                s.reader.seen = reader.seen;
                if let Some(q) = read {
                    // A read not written down yet has them written at the next flush.
                    if !s.reader.reads.contains(&q) {
                        s.written = None;
                    }
                    s.reader.reads.retain(|r| *r != q);
                    s.reader.reads.insert(0, q);
                    s.reader.reads.truncate(crate::recent_readers::MAX_READS);
                }
            }
            None => {
                readers.insert(
                    id,
                    Seen {
                        scope: scope.clone(),
                        reader,
                        written: None,
                    },
                );
            }
        }
    }
}

/// The reads [`note`] didn't write down since the writer last said so, by why: logged with the next write ([`flush`]),
/// as counts alone (never who).
struct NotWritten {
    /// A superuser's, or an operator's (who reads all of the org).
    all: std::sync::atomic::AtomicUsize,
    /// Not a person's own sign-in: a key's, a platform's, a provisioner's, a recorder's.
    not_a_person: std::sync::atomic::AtomicUsize,
    /// Read as other than a warm-up would work them out (a scope the request named itself, groups the closures
    /// don't give them).
    other_key: std::sync::atomic::AtomicUsize,
}

static NOT_WRITTEN: NotWritten = NotWritten {
    all: std::sync::atomic::AtomicUsize::new(0),
    not_a_person: std::sync::atomic::AtomicUsize::new(0),
    other_key: std::sync::atomic::AtomicUsize::new(0),
};

/// Logs the reads [`note`] left out since the last time, when there were any.
fn say_not_written() {
    use std::sync::atomic::Ordering::Relaxed;
    let (all, not_a_person, other_key) = (
        NOT_WRITTEN.all.swap(0, Relaxed),
        NOT_WRITTEN.not_a_person.swap(0, Relaxed),
        NOT_WRITTEN.other_key.swap(0, Relaxed),
    );
    if all + not_a_person + other_key > 0 {
        eprintln!(
            "home readers: {} reads not written down: {all} by a superuser or an operator, {not_a_person} not a person's own sign-in, {other_key} read as other than a warm-up would work them out",
            all + not_a_person + other_key
        );
    }
}

/// Writes the readers this server saw that are due ([`WRITTEN_FOR`]), every [`flush_every`], for as long as it runs.
async fn flush(state: Arc<State>) {
    loop {
        tokio::time::sleep(flush_every()).await;
        say_not_written();
        write_due(&state).await;
    }
}

/// Writes the readers this server saw that are due ([`WRITTEN_FOR`]): every [`flush_every`], and at shutdown.
pub(super) async fn write_due(state: &State) {
    let due: Vec<(Scope, Vec<(String, RecentReader)>)> = {
        let mut all = SEEN.lock().unwrap();
        let since = crate::now_micros() - crate::recent_readers::KEPT_FOR.as_micros() as i64;
        all.retain(|_, readers| {
            readers.retain(|_, s| s.reader.seen >= since);
            !readers.is_empty()
        });
        all.values()
            .filter_map(|readers| {
                let due: Vec<(String, RecentReader)> = readers
                    .iter()
                    .filter(|(_, s)| s.written.is_none_or(|w| w.elapsed() >= WRITTEN_FOR))
                    .map(|(id, s)| (id.clone(), s.reader.clone()))
                    .collect();
                let scope = readers.values().next()?.scope.clone();
                (!due.is_empty()).then_some((scope, due))
            })
            .collect()
    };
    for (scope, due) in due {
        match write(state, &scope, due.iter().map(|(_, r)| r.clone()).collect()).await {
            // The scope is gone (deleted, cleared, purged): nothing of it is held to be written again.
            Ok(None) => {
                SEEN.lock().unwrap().remove(&scope.namespace());
            }
            Ok(Some(kept)) => {
                eprintln!(
                    "home readers of {scope}: {} written down, {} in the file now",
                    due.len(),
                    kept.len()
                );
                let mut all = SEEN.lock().unwrap();
                if let Some(readers) = all.get_mut(&scope.namespace()) {
                    for (id, r) in &due {
                        // One the write left out (they left, are disabled, or were forgotten) is no longer
                        // held here either, so a later write never puts them back.
                        match kept
                            .iter()
                            .any(|k| k.user == r.user && k.reads_as == r.reads_as)
                        {
                            true => {
                                if let Some(s) = readers.get_mut(id) {
                                    s.written = Some(Instant::now());
                                }
                            }
                            false => {
                                readers.remove(id);
                            }
                        }
                    }
                }
            }
            // Tried again at the next flush.
            Err(e) => eprintln!("home readers of {scope} not written: {e:#}"),
        }
    }
}

/// `seen` written into `scope`'s readers as other servers left them: each reader once, as last seen, and only those
/// who still read the scope as they did ([`store_readers`]). What the file holds after; none when the scope is gone.
async fn write(
    state: &State,
    scope: &Scope,
    seen: Vec<RecentReader>,
) -> Result<Option<Vec<RecentReader>>> {
    let store = state.store.read().unwrap().clone();
    // As this server writes them (one written by another version is read as this one would have written it), so
    // the same reader is never written twice.
    let mut readers: Vec<RecentReader> = store
        .recent_readers(scope)
        .await?
        .into_iter()
        .filter_map(canonical)
        .collect();
    // Each reader once, as last seen by any server, with this server's reads before those written already.
    let mut merged: Vec<RecentReader> = vec![];
    for r in readers.drain(..).chain(seen) {
        match merged
            .iter_mut()
            .find(|o| o.user == r.user && o.reads_as == r.reads_as)
        {
            Some(o) => {
                o.seen = o.seen.max(r.seen);
                let older = std::mem::replace(&mut o.reads, r.reads);
                for q in older {
                    if !o.reads.contains(&q) {
                        o.reads.push(q);
                    }
                }
            }
            None => merged.push(r),
        }
    }
    store_readers(state, &store, scope, merged).await
}

/// How many times [`store_readers`] checks a file it wrote again, at most.
const RECHECKS: usize = 3;

/// The directory as it is now (none on a server that serves one scope without it).
async fn directory_now(
    state: &State,
    store: &Store,
) -> Result<Option<crate::people::directory::Directory>> {
    Ok(match state.fixed() {
        true => None,
        false => Some(crate::people::directory::Directory::fresh(store).await?),
    })
}

/// The one way this server writes a scope's readers: `readers`, less those who no longer read the scope so (left,
/// disabled, or let in no more as they signed in, as the directory read now says) and those aged out
/// ([`crate::recent_readers::bounded`]); with none left the file goes. Then the file is checked again against the
/// directory read after the write, and written again without anyone gone meanwhile (a removal, a forget or a disable
/// that landed between the directory's read and the write), until a check finds no one gone: a forget's own last
/// sweep ([`crate::offboard`]) covers what a check made before its removal wrote. What the file holds after; none
/// when the scope is gone (deleted, cleared or purged), and nothing was written.
async fn store_readers(
    state: &State,
    store: &Store,
    scope: &Scope,
    readers: Vec<RecentReader>,
) -> Result<Option<Vec<RecentReader>>> {
    // A type's listing is kept only while the scope's schema defines the type: one it dropped leaves the file here.
    let schema = store.schema(scope, crate::MAIN).await?;
    let still = |dir: Option<&crate::people::directory::Directory>, readers: Vec<RecentReader>| {
        crate::recent_readers::bounded(
            readers
                .into_iter()
                .filter(|r| reader_of(state, dir, scope, r).is_some())
                .map(|mut r| {
                    r.reads.retain(|q| typed_in(q, schema.as_deref()));
                    r
                })
                .collect(),
        )
    };
    let dir = directory_now(state, store).await?;
    let mut written = still(dir.as_ref(), readers);
    if !store.put_recent_readers(scope, written.clone()).await? {
        return Ok(None);
    }
    for _ in 0..RECHECKS {
        let dir = directory_now(state, store).await?;
        let now = store.recent_readers(scope).await?;
        let kept = still(dir.as_ref(), now.clone());
        if kept == now {
            written = kept;
            break;
        }
        if !store.put_recent_readers(scope, kept.clone()).await? {
            return Ok(None);
        }
        written = kept;
    }
    Ok(Some(written))
}

/// `r` as this server writes a reader down; none when what was written doesn't read.
fn canonical(mut r: RecentReader) -> Option<RecentReader> {
    let reads_as: ReadsAs = serde_json::from_value(r.reads_as).ok()?;
    r.reads_as = serde_json::to_value(reads_as).ok()?;
    r.reads = r.reads.iter().filter_map(replayable).collect();
    Some(r)
}

/// Who `user`, signed in by `via`, is as a request signs them in, before any scope is chosen (what `/admin` reads as):
/// the groups worked out as a request works them out (the closures as this server holds them), never as written
/// down. A local server's one caller holds the server's groups.
fn signed_in_as(state: &State, user: &str, via: &SignIn) -> Principal {
    if via.local_admin.is_some() {
        return Principal::from_parts(user.to_string(), state.groups.clone());
    }
    // As the sign-in does: closures hold only for an email the deployment trusts, never one an org's own
    // provider vouches for.
    let org_says_who =
        via.key.is_none() && via.orgs.is_some() && !(via.auth0 && via.verified_email);
    let groups = match org_says_who {
        true => vec![],
        false => state.closures.apply(user, vec![]),
    };
    Principal::from_parts(user.to_string(), groups)
}

/// Who `user`, signed in by `via`, reads `scope` as: [`signed_in_as`], with the org's own group where the directory
/// gave them the scope, as a request's read of the scope holds it, reading all of it for a super admin's own sign-in
/// (`via` says one only while this server lists them as one: [`SignedIn::sign_in`]).
fn principal_of(state: &State, user: &str, via: &SignIn, scope: &Scope) -> Principal {
    let mut who = signed_in_as(state, user, via);
    if !state.fixed()
        && let Some(org) = scope.org_name()
    {
        let all = auth::acl::org_wide(org);
        if !who.groups().contains(&all) {
            who.add_groups([all]);
        }
        if super_admin_reads(via) {
            who = who.reading_all();
        }
    }
    who
}

/// A reader written down as they read `scope`: their principal ([`principal_of`], their groups as they are now) and
/// sign-in, when they still read it so (the directory `dir` gives them it, and neither it nor this server holds them
/// disabled). None otherwise, or when what was written doesn't read. Never a superuser.
fn reader_of(
    state: &State,
    dir: Option<&crate::people::directory::Directory>,
    scope: &Scope,
    r: &RecentReader,
) -> Option<(Principal, SignIn)> {
    let reads_as: ReadsAs = serde_json::from_value(r.reads_as.clone()).ok()?;
    let via = reads_as.sign_in.sign_in(&r.user);
    let mut ids = vec![r.user.clone()];
    ids.extend(via.unverified.clone());
    // The directory's decision where it has one (made through any server), else this server's.
    let disabled = ids.iter().any(|id| {
        let id = auth::acl::disabled_id(id);
        match dir.and_then(|d| d.disabled.get(&id)) {
            Some(d) => *d,
            None => state.closures.any_disabled(std::slice::from_ref(&id)),
        }
    });
    if disabled {
        return None;
    }
    let (org, team) = match scope {
        Scope::Team(id) => match id.split_once('/') {
            Some((o, t)) => (Some(o), Some(t)),
            None => (scope.org_name(), Some(id.as_str())),
        },
        _ => (scope.org_name(), None),
    };
    let reads = match (via.local_admin.is_some(), dir) {
        // A local server's one caller reads the scope it serves, and only on such a server.
        (true, _) => state.fixed() && state.loopback_only && *scope == state.scope,
        (false, Some(dir)) if via.person => dir
            .resolve_reading(&r.user, &via, org, team)
            .is_ok_and(|s| s == *scope),
        _ => false,
    };
    if !reads {
        return None;
    }
    Some((principal_of(state, &r.user, &via, scope), via))
}

/// Works out the Home and the list of proposals of `scope`'s recent readers ([`crate::recent_readers`]) who still
/// read it as they did, so their first asks on this server are answered at once: [`prime_at_once`] at a time, until
/// `until` ([`prime_deadline`]; the rest go on in the background), and beside them the summaries of its skills
/// ([`crate::skill_review::warm`]). Those who don't read it as written any more are taken out of the file. Logs how
/// many and how long, never who.
pub(super) async fn prime_readers(
    state: &Arc<State>,
    store: &Store,
    scope: &Scope,
    until: tokio::time::Instant,
) {
    let at_once = prime_at_once();
    if at_once == 0 {
        return;
    }
    let t = Instant::now();
    let written = match store.recent_readers_held(scope).await {
        Ok(r) if r.is_empty() => {
            eprintln!("warm-up: no recent Home readers of {scope} written down to prime");
            return;
        }
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "warm-up: {scope}'s recent Home readers not read (their first asks work it out): {e:#}"
            );
            return;
        }
    };
    // The closures as the directory holds them now, which the readers' groups are worked out from.
    if let Err(e) = sync_closures(state).await {
        eprintln!("warm-up: the closures not read to prime {scope}'s Home readers with: {e:#}");
        return;
    }
    let dir = match state.fixed() {
        true => None,
        false => match crate::people::directory::Directory::behind(store).await {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("warm-up: no directory to prime {scope}'s Home readers with: {e:#}");
                return;
            }
        },
    };
    // Each reader once, however many times they were written down.
    let mut once = HashSet::new();
    // With the reads they made, as a warm-up makes them again ([`replayable`]).
    type Primed = (Principal, SignIn, Vec<Value>);
    let readers: Vec<Primed> = written
        .iter()
        .filter_map(|r| {
            let (who, via) = reader_of(state, dir.as_ref(), scope, r)?;
            let reads = r
                .reads
                .iter()
                .filter_map(replayable)
                .take(crate::recent_readers::MAX_READS)
                .collect();
            Some((who, via, reads))
        })
        .filter(|(who, via, _)| {
            once.insert(format!("{}\n{via:?}", crate::kept_answers::reader_key(who)))
        })
        .collect();
    let asked: usize = readers.iter().map(|r| r.2.len()).sum();
    // Which reads those are, by op (`ls 20, lessons 1`): whether a page's read was written down at all.
    let asked_ops = {
        let mut ops: BTreeMap<&str, usize> = BTreeMap::new();
        for q in readers.iter().flat_map(|r| &r.2) {
            *ops.entry(q["op"].as_str().unwrap_or("?")).or_default() += 1;
        }
        ops.iter()
            .map(|(op, n)| format!("{op} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (found, still) = (written.len(), readers.len());
    let gone = written
        .iter()
        .filter(|r| reader_of(state, dir.as_ref(), scope, r).is_none())
        .count();
    // Those who left or are disabled leave the file now, not only when a reader of the scope is written next.
    if gone > 0
        && dir.is_some()
        && let Err(e) = prune(state, store, scope).await
    {
        eprintln!("warm-up: {scope}'s recent Home readers not pruned: {e:#}");
    }
    if tokio::time::Instant::now() >= until {
        eprintln!(
            "warm-up: {still} recent Home readers of {scope} not primed: the warm-up's time for priming is spent"
        );
        return;
    }
    // Someone reads this scope in Studio: its Skills list's summaries are read beside their Home.
    let skills = {
        let (store, scope) = (store.clone(), scope.clone());
        tokio::spawn(crate::loads::by("skills_warm", async move {
            let t = Instant::now();
            (crate::skill_review::warm(&store, &scope).await, t.elapsed())
        }))
    };
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let settled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reads_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dreams_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let work = {
        let (done, settled, reads_done, dreams_done) = (
            done.clone(),
            settled.clone(),
            reads_done.clone(),
            dreams_done.clone(),
        );
        let (state, scope) = (state.clone(), scope.clone());
        async move {
            use futures::StreamExt;
            futures::stream::iter(readers)
                .for_each_concurrent(at_once, |(who, via, reads)| {
                    let (state, scope) = (state.clone(), scope.clone());
                    let (done, settled, reads_done, dreams_done) = (
                        done.clone(),
                        settled.clone(),
                        reads_done.clone(),
                        dreams_done.clone(),
                    );
                    async move {
                        let store = state.store.read().unwrap().clone();
                        // No one written down reads as a superuser.
                        let every_session = every_session(&store, &scope, &who, &via, false).await;
                        let reader = crate::browse::Reader {
                            who: &who,
                            every_session,
                        };
                        let primed = async {
                            let whole = home::prime(&state, &scope, reader, &via, false).await?;
                            // Their count of proposals waiting for review (Home's) and their list (Review's, and the
                            // first page opened from Home), as `/admin` counts and lists them for them: kept by who
                            // they are and every group they read as, as there. `/admin` signs a super admin in
                            // without reading all, so neither do these.
                            if let Some(org) = scope.org_name() {
                                let member =
                                    as_member(&signed_in_as(&state, who.user(), &via), org);
                                governance::waiting_kept(&store, &scope, &member).await?;
                                governance::proposals_kept(&store, &scope, &member).await?;
                                // An admin's list of the dream's decisions (Review's), as `/admin` lists it for
                                // them: Home's dream part may have come from another server's shared answer.
                                let admin = signed_in_as(&state, who.user(), &via);
                                if dream_review::prime(&store, &scope, &admin, &via, org).await? {
                                    dreams_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            // Their other Studio reads, as they asked them, on main as a request reads it.
                            if !reads.is_empty() {
                                // As `/admin` signs them in, for its reads.
                                let admin = signed_in_as(&state, who.user(), &via);
                                for q in &reads {
                                    // Main pinned as it is now for each read: an answer is kept only when worked out
                                    // at main's state as it is when the read begins, and a reader's reads take long
                                    // enough at a start for main to move on between the first and the last (a read
                                    // at a state it moved past is worked out and not kept).
                                    let session =
                                        crate::workspace::read_session(&store, &scope, None, None)
                                            .await?;
                                    // One that fails (a folder gone since, an admin's read they may no longer make)
                                    // is left to its next read.
                                    if replay(
                                        &state, &store, &session, &scope, reader, &admin, &via, q,
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        reads_done
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                            }
                            anyhow::Ok(whole)
                        };
                        match primed.await {
                            Ok(whole) => {
                                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if whole {
                                    settled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            Err(e) => eprintln!("warm-up: a Home of {scope} not primed: {e:#}"),
                        }
                    }
                })
                .await
        }
    };
    // Past the budget the rest go on in the background, and the warm-up goes on to the next scope.
    let mut work = tokio::spawn(crate::loads::by("home_prime", work));
    let in_time = tokio::time::timeout_at(until, &mut work).await.is_ok();
    let n = |a: &Arc<std::sync::atomic::AtomicUsize>| a.load(std::sync::atomic::Ordering::Relaxed);
    // Those past the budget say when they are done too, so the log shows whether they were primed at all.
    if !in_time {
        let scope = scope.clone();
        let (done, settled, reads_done, dreams_done) = (
            done.clone(),
            settled.clone(),
            reads_done.clone(),
            dreams_done.clone(),
        );
        let asked_ops = asked_ops.clone();
        tokio::spawn(async move {
            let _ = work.await;
            let n = |a: &Arc<std::sync::atomic::AtomicUsize>| {
                a.load(std::sync::atomic::Ordering::Relaxed)
            };
            eprintln!(
                "warm-up: primed the Home of {} of {still} recent readers of {scope} in the background ({} with every part, {} admins' dream decisions, {} of their {asked} other Studio reads: {asked_ops}) in {} ms",
                n(&done),
                n(&settled),
                n(&dreams_done),
                n(&reads_done),
                t.elapsed().as_millis(),
            );
        });
    }
    eprintln!(
        "warm-up: primed the Home of {} of {still} recent readers of {scope} ({} with every part, {} admins' dream decisions, {} of their {asked} other Studio reads ({asked_ops}), {gone} of {found} written down no longer read it so) in {} ms{}",
        n(&done),
        n(&settled),
        n(&dreams_done),
        n(&reads_done),
        t.elapsed().as_millis(),
        match in_time {
            true => "",
            false => "; the rest go on in the background",
        }
    );
    match tokio::time::timeout_at(until, skills).await {
        Ok(Ok((Ok(n), took))) => eprintln!(
            "warm-up: read the summaries of {n} skills of {scope} in {} ms",
            took.as_millis()
        ),
        Ok(Ok((Err(e), _))) => {
            eprintln!(
                "warm-up: {scope}'s skills not summarised (their first list reads them): {e:#}"
            )
        }
        Ok(Err(e)) => eprintln!("warm-up: {scope}'s skills not summarised: {e}"),
        Err(_) => {
            eprintln!("warm-up: {scope}'s skills are still being summarised in the background")
        }
    }
}
