//! `/browse`: a scope's memory as a file tree, for a UI to walk. Up to three trees hang off the root:
//!
//! - `sources/…`: episodes by their source split at `:` (`slack:C09:1712.3` is `sources/slack/C09/1712.3`), but for
//!   the engine's own documents (advice, compiled skills, dreams: [`crate::episode_types::ENGINE_SOURCES`]). Those are
//!   what the memory wrote about itself, not what it was told: each version shows on its own page instead (a note's
//!   under `advice/`, a skill's under `skills/`), and an episode op names that page as its path.
//! - `entities/<type>/<key>`: facts by the entity they are about (`deal:42` is `entities/deal/42`).
//! - `skills/<slug>`: the skills compiled from the scope's documents (`crate::compile`), there only when the
//!   caller may read one on the branch read. The folder lists the routing index and each skill with when it
//!   applies; a skill reads as its markdown with its AIP step graph.
//! - `notes`: the notes the caller may read, newest first, as their facts (`crate::note_facts`): what they asked
//!   to be remembered, what AmpUp Memory kept from what they typed (`crate::keep`), and what it shared with
//!   everyone once enough people said the same. A note's fact is read by who may read the note, no one else: a
//!   person's notes are theirs alone, an org's admins included.
//! - `advice/<team|mine>/<category>/<situation>`: the advice the caller may read (`crate::listing::advice`), each a
//!   note to read in full.
//! - `sessions/…`: the caller's own sessions: `ended/<day>` their records (what facts are extracted from) and
//!   `<harness>/<repo>` what a coding session's hooks kept as it went.
//!
//! In those three, a leaf has a `title` (its first line, or a session's first request) to show for its name.
//!
//! A path is a JSON array of segments, so a segment may hold `/`. Every listing is built from what the
//! caller may read and nothing else: an episode they can't see is in no count and names no folder, and a
//! fact shows only when they can see every episode it cites (the same rules as every other read).
//!
//! Requests: `{"op": "ls", "path": [...], "limit"?, "after"?}` lists a folder (children sorted by name,
//! `after` the last name of the previous page) or, at `entities/<type>/<key>`, the entity's facts; at `entities`
//! and `entities/<type>`, `"from"` (source prefixes) and `"by"` (a person, for the org's admins) narrow the
//! entities and their counts to what was learned from those sources or that person ([`Narrow`]), and `"sources":
//! true` at `entities` counts each type's records by source, `"facts": true` at `entities/<type>` lists each record's
//! current facts with it ([`With`]);
//! `{"op": "sources", "prefixes"?, "top"?}` says what each source taught the memory (`crate::learned_from`);
//! `{"op": "entity", "path": [...]}` reads one entity as its page shows it (`crate::entity_page`);
//! `{"op": "episode", "id": "..."}` reads one episode with the facts that cite it; `{"op": "branches"}` names
//! the scope's branches, each with how many compiled skills the caller may read there (null while a branch's state
//! is still being built); `{"op": "schema"}` reads
//! the scope's schema on the branch read, with every version committed there; `{"op": "lessons", …}` what it
//! learned, lesson by lesson, with each one's trail (`crate::lessons`, also `POST /lessons`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::search::{Col, Filter, Lit};
use crate::{Fact, Principal, Scope, Session, Store};

/// Children per page unless the request asks for fewer or more (at most [`MAX_LIMIT`]).
const LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
/// An episode's text is returned up to this many bytes.
const BODY_BYTES: usize = 256 << 10;

/// A request the caller got wrong: shown to them.
#[derive(Debug)]
pub struct BadRequest(pub String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BadRequest {}

/// Nothing at that path the caller may see (whether or not it exists for someone else).
#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing there (or not visible)")
    }
}
impl std::error::Error for NotFound {}

pub(crate) fn bad(msg: impl Into<String>) -> anyhow::Error {
    BadRequest(msg.into()).into()
}

/// One visible episode, without its text.
pub(crate) struct Ep {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) occurred_at: i64,
    /// When it was written to memory.
    pub(crate) recorded_at: i64,
    pub(crate) about: Vec<String>,
}

impl Ep {
    fn segments(&self) -> Vec<&str> {
        self.source.split(':').collect()
    }
}

/// Who is browsing: the caller, and whether they read every person's sessions the advice page lists, not only
/// their own (an org's admins and the operator, on the org's own memory, where the org shows its people's
/// activity: the server decides). Notes are read by their ACL alone, whoever browses.
#[derive(Clone, Copy)]
pub struct Reader<'a> {
    pub who: &'a Principal,
    pub every_session: bool,
}

/// The `/browse` ops, as the Studio's read latency names them (`memstore_studio_read_seconds`): one of these, else
/// `other`, never the caller's own text. `home` and `scopes` the server answers itself; the rest [`run`] does.
const OPS: [&str; 12] = [
    "home",
    "scopes",
    "facts",
    "advice",
    "advice_usage",
    "lessons",
    "ls",
    "sources",
    "episode",
    "entity",
    "branches",
    "schema",
];

/// A `/browse` request's op as a metric's label ([`OPS`]); a request naming none is an `ls`, as [`run`] takes it.
pub fn op_label(req: &Value) -> &'static str {
    let op = req["op"].as_str().unwrap_or("ls");
    OPS.into_iter().find(|o| *o == op).unwrap_or("other")
}

/// Answers one `/browse` request as `reader`.
pub async fn run(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    req: &Value,
) -> Result<Value> {
    let who = reader.who;
    match req["op"].as_str().unwrap_or("ls") {
        // The filterable lists (`crate::listing`): `/facts/list` and `/advice/list` are these ops.
        "facts" => crate::listing::facts(store, session, who, req).await,
        "advice" => crate::listing::advice(store, session, who, req).await,
        // The advice page: each team note's use and outcomes, and with `note`, that note's sessions: everyone's for
        // those who read everyone's notes (an org's admins, where it shows people's activity), else the reader's own.
        "advice_usage" => {
            let days = match &req["days"] {
                Value::Null => None,
                v => Some(
                    v.as_u64()
                        .filter(|d| (1..=3650).contains(d))
                        .ok_or_else(|| bad("days is a whole number of days, 1 to 3650"))?,
                ),
            };
            // One person's own notes instead of the reader's: an admin's view of someone (as `/lessons` reads
            // them), or the reader naming themselves. Every person's own notes at once (`"person": "*"`) are the
            // platform operator's view, for support: only a reader of all of the org's memory, whose own sign-in
            // already reads what people keep to themselves, never an org's admin or the operator's admin key.
            let person = match req["person"].as_str().filter(|p| !p.is_empty()) {
                Some(crate::advice::EVERYONE) if !who.reads_all() => {
                    return Err(bad(
                        "only the platform's operator reads every person's own advice",
                    ));
                }
                Some(crate::advice::EVERYONE) => Some(crate::advice::EVERYONE.to_string()),
                Some(p)
                    if !reader.every_session
                        && crate::auth::acl::norm_id(p)
                            != crate::auth::acl::norm_id(who.user()) =>
                {
                    return Err(bad("you read only your own advice and the team's"));
                }
                p => p.map(crate::auth::acl::norm_id),
            };
            // One note's key: that note alone, with its sessions (the list carries only their count).
            let note = match &req["note"] {
                Value::Null => None,
                v => Some(
                    v.as_str()
                        .filter(|k| !k.is_empty() && k.len() <= 1024)
                        .ok_or_else(|| bad("note is an advice note's key"))?
                        .to_string(),
                ),
            };
            usage_kept(store, session.scope(), reader, days, person, note).await
        }
        // What it learned, lesson by lesson, with where each came from and where it was served (`/lessons`).
        "lessons" => crate::lessons::run(store, session, reader, req).await,
        "ls" => {
            let path: Vec<String> = match &req["path"] {
                Value::Null => vec![],
                Value::Array(a) => a
                    .iter()
                    .map(|s| s.as_str().map(String::from))
                    .collect::<Option<_>>()
                    .ok_or_else(|| bad("path is an array of strings"))?,
                _ => return Err(bad("path is an array of strings")),
            };
            let limit = req["limit"]
                .as_u64()
                .map_or(LIMIT, |n| n as usize)
                .clamp(1, MAX_LIMIT);
            let after = req["after"].as_str();
            let narrow = Narrow::of(req, reader)?;
            let with = With::of(req, &path)?;
            // The entities and their types, the sessions, the sources, the notes and the skills, unnarrowed: kept
            // ([`entities_kept`]).
            if narrow.is_empty()
                && match path.first().map(String::as_str) {
                    Some("entities") => path.len() <= 2,
                    Some("sessions" | "sources") => true,
                    Some("skills" | "notes") => path.len() == 1,
                    _ => false,
                }
            {
                return entities_kept(store, session, reader, &path, limit, after, with).await;
            }
            ls(store, session, reader, &path, limit, after, &narrow, with).await
        }
        // What each source taught the memory: its episodes, the facts drawn from them and the entities they are
        // about (`crate::learned_from`).
        "sources" => crate::learned_from::run(store, session, who, req).await,
        "episode" => {
            let id = req["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("episode needs an id"))?;
            episode(store, session, who, id).await
        }
        // One entity's page: its facts with where each came from and who may read it, and what it links to.
        "entity" => {
            let path: Vec<String> = req["path"]
                .as_array()
                .and_then(|a| a.iter().map(|s| s.as_str().map(String::from)).collect())
                .ok_or_else(|| bad("entity needs a path, an array of strings"))?;
            let (page, eps) = tokio::try_join!(
                crate::entity_page::run(store, session, who, &path),
                episodes(store, session, who)
            )?;
            crate::names::attach(store, session, who, page, &eps).await
        }
        // `build: true`: the branches not built are built in the background for the next listing. Not by default:
        // each is a build of the whole memory, and a server keeps only a few branches' states, so a listing that
        // built every branch built most of them for nothing, each time it was asked.
        "branches" => branches(store, session.scope(), who, req["build"] == true).await,
        // The schema the memory is typed with on the branch read, and every version committed there (oldest
        // first): it holds no memory, only the types and what its `extract` section names.
        // `history: false`: only how many versions there are (`versions`), not each of them (a page counting them
        // needn't read every version's types).
        "schema" => {
            let schemas = &session.vis().schemas;
            let mut v = json!({"schema": session.schema().as_deref(), "versions": schemas.len()});
            if req["history"] != false {
                let history: Vec<&crate::query::schema::Schema> =
                    schemas.iter().map(|s| &**s).collect();
                v["history"] = json!(history);
            }
            Ok(v)
        }
        op => Err(bad(format!(
            "unknown op {op:?}: ls, entity, episode, branches, schema, sources, facts, advice, advice_usage, lessons or home"
        ))),
    }
}

/// The scope's branches, main first, each with the compiled skills the caller may read on it: a memory build
/// often compiles on a branch of its own, so a reader on main learns where to look.
async fn branches(store: &Store, scope: &Scope, who: &Principal, build: bool) -> Result<Value> {
    // Listed only: the ledger as this server holds it, and each branch's skills read at once.
    let mut names: Vec<String> = store.branch_heads_held(scope).await?.into_keys().collect();
    crate::phases::mark("branches listed");
    names.sort_by_key(|b| (b.as_str() != crate::MAIN, b.clone()));
    if names.is_empty() {
        names.push(crate::MAIN.to_string());
    }
    // Main as a request's reads pin it; another branch only as a state already built ([`Store::pin_if_built`]): a
    // listing waits for no branch's build. A branch without one has its skills unknown (null), and with `build` is
    // built in the background, one branch at a time, for the next listing.
    let pinned = futures::future::try_join_all(names.iter().map(|b| async move {
        match b == crate::MAIN {
            true => store.pin_lagging(scope, b).await.map(Some),
            false => store.pin_if_built(scope, b).await,
        }
    }))
    .await?;
    crate::phases::mark("branches pinned");
    let mut unbuilt = vec![];
    let mut out: Vec<Value> = Vec::with_capacity(names.len());
    for (b, session) in names.into_iter().zip(pinned) {
        let skills = match session {
            Some(s) => json!(skill_keys(&s, who).len()),
            None => {
                unbuilt.push(b.clone());
                Value::Null
            }
        };
        out.push(json!({"name": b, "skills": skills}));
    }
    if build {
        store.build_later(scope, unbuilt);
    }
    Ok(json!({"branches": out}))
}

/// What a listing of entities carries beside each child, in the one read (the Team page's ontology):
/// - `sources: true` at `entities`: each type's records by the source they were learned from (`{"slack": 12}`), as
///   one read of `entities` narrowed to each source (`from: "<source>:"`) would count them: a record counts under
///   every source of the episodes its facts cite, of those episodes the caller may read.
/// - `facts: true` at `entities/<type>`: each listed record's current facts (no `valid_to`), their predicate and value
///   alone: what a sample of the record shows, in place of listing each record (`entities/<type>/<key>`) after.
#[derive(Clone, Copy, Default)]
pub(crate) struct With {
    sources: bool,
    facts: bool,
}

impl With {
    fn of(req: &Value, path: &[String]) -> Result<With> {
        let flag = |k: &str| match &req[k] {
            Value::Null | Value::Bool(false) => Ok(false),
            Value::Bool(true) => Ok(true),
            _ => Err(bad(format!("{k} is true or false"))),
        };
        let with = With {
            sources: flag("sources")?,
            facts: flag("facts")?,
        };
        let at = |n: usize| path.first().map(String::as_str) == Some("entities") && path.len() == n;
        if with.sources && !at(1) {
            return Err(bad(
                "sources is for a listing of entities' types (path [\"entities\"])",
            ));
        }
        if with.facts && !at(2) {
            return Err(bad(
                "facts is for a listing of one type's entities (path [\"entities\", type])",
            ));
        }
        Ok(with)
    }

    /// As a kept answer's key has it.
    fn key(self) -> &'static str {
        match (self.sources, self.facts) {
            (false, false) => "",
            (true, false) => "sources",
            (false, true) => "facts",
            (true, true) => "sources facts",
        }
    }
}

/// What the Explorer's lists of entities are narrowed to (`ls` at `entities` and `entities/<type>`): the facts
/// learned from the sources starting with one of `from` (`"from": "slack:C09"` or a list), and the facts from what
/// one person (`by`) wrote or said: their notes, their own learned advice and their conversations' transcripts, as
/// Studio's "from person" search reads them (`/admin {op: "user_facts"}`). `by` is for those who read everyone's
/// activity ([`Reader::every_session`]); either reads only what the caller may read, as every list does.
#[derive(Default)]
pub(crate) struct Narrow {
    from: Vec<String>,
    by: Option<String>,
}

impl Narrow {
    fn of(req: &Value, reader: Reader<'_>) -> Result<Narrow> {
        let from: Vec<String> = match &req["from"] {
            Value::Null => vec![],
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .map(|s| s.as_str().map(String::from))
                .collect::<Option<_>>()
                .ok_or_else(|| bad("from is a source prefix or a list of them"))?,
            _ => return Err(bad("from is a source prefix or a list of them")),
        };
        let from: Vec<String> = from.into_iter().filter(|p| !p.is_empty()).collect();
        let by = match &req["by"] {
            Value::Null => None,
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) => Some(s.trim().to_string()),
            _ => return Err(bad("by is a person's id")),
        };
        if by.is_some() && !reader.every_session {
            return Err(bad(
                "by is for the org's admins, on the org's own memory where it shows its people's activity",
            ));
        }
        Ok(Narrow { from, by })
    }

    fn is_empty(&self) -> bool {
        self.from.is_empty() && self.by.is_none()
    }

    /// `facts` (the caller's own reads) less those it doesn't keep.
    async fn keep(
        &self,
        store: &Store,
        session: &Session,
        who: &Principal,
        mut facts: Vec<Fact>,
    ) -> Result<Vec<Fact>> {
        if !self.from.is_empty() {
            let from: std::collections::HashSet<String> = episodes(store, session, who)
                .await?
                .into_iter()
                .filter(|e| self.from.iter().any(|p| e.source.starts_with(p.as_str())))
                .map(|e| e.id)
                .collect();
            facts.retain(|f| f.source_episodes.iter().any(|e| from.contains(e)));
        }
        if let Some(user) = &self.by {
            let log = crate::qa_log::list(store, session.scope(), None).await?;
            let also = crate::activity::transcripts_of(&log, user);
            let (theirs, _) = store
                .facts_from(session.scope(), who, user, &also, usize::MAX)
                .await?;
            let theirs: std::collections::HashSet<&str> =
                theirs.iter().map(|f| f.fact_id.as_str()).collect();
            facts.retain(|f| theirs.contains(f.fact_id.as_str()));
        }
        Ok(facts)
    }
}

/// Each reader's advice page (`advice_usage`), by memory state and answer log.
static USAGE: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::summary(500).however_old());

/// `advice_usage` over the last `days` (none: all), of every note or only `note`, kept by memory state and the scope's
/// answer log ([`crate::kept_answers`]): it reads every note the reader may read and every session logged. Kept by
/// the reader (who, every group they read as, and whether they read everyone's sessions), the note and the days asked
/// for. A window of days ends when the answer was worked out ([`crate::kept_answers::At::windowed`]): one answered
/// while the next is worked out ends its window then, and the next read's ends it now.
async fn usage_kept(
    store: &Store,
    scope: &crate::Scope,
    reader: Reader<'_>,
    days: Option<u64>,
    person: Option<String>,
    note: Option<String>,
) -> Result<Value> {
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    let now = store.lagging_visibility(scope, crate::MAIN).await?;
    let log = crate::kept_answers::Log::read(store, scope).await?;
    let at = crate::kept_answers::At::with_log(mark, &now, log);
    let at = match days {
        Some(_) => at.windowed(),
        None => at,
    };
    let key = format!(
        "{}\n{}\n{}\n{}\n{}",
        crate::kept_answers::reader_key(reader.who),
        reader.every_session,
        person.as_deref().unwrap_or_default(),
        // None for the list: a note's key is never empty.
        note.as_deref().unwrap_or_default(),
        days.map_or_else(|| "all".to_string(), |d| d.to_string())
    );
    let (store2, scope2, who, every) = (
        store.clone(),
        scope.clone(),
        reader.who.clone(),
        reader.every_session,
    );
    let usage = USAGE
        .get(&scope.namespace(), &key, at, true, || async move {
            let since = days.map_or(0, |d| crate::now_micros() - d as i64 * 86_400_000_000);
            crate::advice::usage(
                &store2,
                &scope2,
                &who,
                every,
                since,
                person.as_deref(),
                note.as_deref(),
            )
            .await
        })
        .await?;
    Ok((*usage).clone())
}

/// Each reader's listing of the entity types, of one type's entities, of their sessions, of the sources, of the notes
/// or of the skills, by memory state (none: no such type or folder).
static ENTITIES: std::sync::LazyLock<crate::kept_answers::Kept<Option<Value>>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500).however_old());

/// [`ENTITIES`] for the skills (`ls skills`): the Skills page's list, a page's summary answered at once on a return visit
/// however long after while the next is worked out ([`crate::kept_answers::Kept::however_old`]).
static SKILLS: std::sync::LazyLock<crate::kept_answers::Kept<Option<Value>>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::summary(500).however_old());

/// `ls` of `entities` or `entities/<type>`, of `sessions` or `sources` (and their folders), of `notes` or of `skills`,
/// unnarrowed, kept by memory state ([`crate::kept_answers`]) for main as it is now: listing them reads every fact,
/// every episode or every routing index the reader may read. Kept by the reader (who, and every group they read as)
/// and what they listed (the path, the page); a read of a branch or of a past state lists them itself.
async fn entities_kept(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    path: &[String],
    limit: usize,
    after: Option<&str>,
    with: With,
) -> Result<Value> {
    let at = match session.on_main_now() {
        true => {
            let mark = crate::kept_answers::Mark::read(store, session.scope()).await?;
            let now = store
                .lagging_visibility(session.scope(), crate::MAIN)
                .await?;
            std::ptr::eq(session.vis(), &*now).then(|| crate::kept_answers::At::of(mark, &now))
        }
        false => None,
    };
    let Some(at) = at else {
        return ls(
            store,
            session,
            reader,
            path,
            limit,
            after,
            &Narrow::default(),
            with,
        )
        .await;
    };
    let key = format!(
        "{}\n{}\n{limit}\n{}{}",
        crate::kept_answers::reader_key(reader.who),
        path.join("\u{1f}"),
        after.unwrap_or_default(),
        match with.key() {
            "" => String::new(),
            k => format!("\n{k}"),
        }
    );
    let (store2, session2, who, every) = (
        store.clone(),
        session.clone(),
        reader.who.clone(),
        reader.every_session,
    );
    let kept: &'static crate::kept_answers::Kept<Option<Value>> =
        match path.first().map(String::as_str) {
            Some("skills") => &SKILLS,
            _ => &ENTITIES,
        };
    let (path, after) = (path.to_vec(), after.map(String::from));
    let listed = kept
        .get(
            &session.scope().namespace(),
            &key,
            at,
            true,
            || async move {
                let reader = Reader {
                    who: &who,
                    every_session: every,
                };
                let narrow = Narrow::default();
                match ls(
                    &store2,
                    &session2,
                    reader,
                    &path,
                    limit,
                    after.as_deref(),
                    &narrow,
                    with,
                )
                .await
                {
                    Err(e) if e.is::<NotFound>() => Ok(None),
                    got => got.map(Some),
                }
            },
        )
        .await?;
    match &*listed {
        Some(v) => Ok(v.clone()),
        None => Err(NotFound.into()),
    }
}

/// Listings worked out at once that read every fact or every episode the reader may read
/// (`AMPUP_MEMORY_WHOLE_LISTINGS`, default 3; 0 no cap). One holds an async worker for its CPU for 1-3 s on a large
/// org, decoding and copying each fact or episode, and a page asks for several at once. On staging ampup a burst of
/// seven, when a new server met Studio with nothing kept, held all eight of the runtime's workers for 1.5-7 s, and
/// every other request (`/ready` too) waited that long for a worker before it started. Past the cap a listing waits
/// its turn without holding a worker, so the rest of the server keeps answering.
static WHOLE_LISTINGS: std::sync::LazyLock<Option<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        let n = std::env::var("AMPUP_MEMORY_WHOLE_LISTINGS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);
        (n > 0).then(|| tokio::sync::Semaphore::new(n))
    });

/// One read of every fact and of every episode a reader may see at one memory state, shared by the whole listings
/// worked out side by side ([`Pass::join`]): Studio asks for the entities, notes, sessions and sources and the root's
/// counts at once, and each read every episode (or every fact) again, a second or more of CPU each on a large org.
/// Kept only while a listing that joined it is worked out: none of it outlives them.
#[derive(Default)]
struct Pass {
    facts: tokio::sync::OnceCell<Arc<Vec<Fact>>>,
    episodes: tokio::sync::OnceCell<Arc<Vec<Ep>>>,
}

/// The passes being worked out, by memory state (its visibility's address), scope and reader: each with the state
/// it is of, so an address reused by a later state never answers for it.
type Passes =
    HashMap<(usize, String, String), (std::sync::Weak<crate::Vis>, std::sync::Weak<Pass>)>;

static PASSES: std::sync::LazyLock<std::sync::Mutex<Passes>> =
    std::sync::LazyLock::new(Default::default);

impl Pass {
    /// The pass of `who` at `session`'s state, joined while a listing beside it holds it, else a new one.
    fn join(session: &Session, who: &Principal) -> Arc<Pass> {
        let vis = session.vis_arc();
        let key = (
            Arc::as_ptr(vis) as usize,
            session.scope().namespace(),
            crate::kept_answers::reader_key(who),
        );
        let mut passes = PASSES.lock().unwrap();
        passes.retain(|_, (_, pass)| pass.strong_count() > 0);
        if let Some((of, pass)) = passes.get(&key)
            && of.upgrade().is_some_and(|of| Arc::ptr_eq(&of, vis))
            && let Some(pass) = pass.upgrade()
        {
            return pass;
        }
        let pass = Arc::new(Pass::default());
        passes.insert(key, (Arc::downgrade(vis), Arc::downgrade(&pass)));
        pass
    }

    /// [`facts`], read once for the pass.
    async fn facts(
        &self,
        store: &Store,
        session: &Session,
        who: &Principal,
    ) -> Result<Arc<Vec<Fact>>> {
        let mut read = false;
        let got = self
            .facts
            .get_or_try_init(|| {
                read = true;
                async { facts(store, session, who).await.map(Arc::new) }
            })
            .await?;
        if !read {
            crate::phases::mark("facts from the pass");
        }
        Ok(got.clone())
    }

    /// [`episodes`], read once for the pass.
    async fn episodes(
        &self,
        store: &Store,
        session: &Session,
        who: &Principal,
    ) -> Result<Arc<Vec<Ep>>> {
        let mut read = false;
        let got = self
            .episodes
            .get_or_try_init(|| {
                read = true;
                async { episodes(store, session, who).await.map(Arc::new) }
            })
            .await?;
        if !read {
            crate::phases::mark("episodes from the pass");
        }
        Ok(got.clone())
    }
}

/// A turn at working out a listing of every fact or episode ([`WHOLE_LISTINGS`]), held while it is worked out: none
/// with no cap. A wait for one is marked in the phases of the request, or of the background work it is
/// ([`crate::kept_answers`] logs slow work with its phases).
async fn whole_listing_turn() -> Option<tokio::sync::SemaphorePermit<'static>> {
    let turns = WHOLE_LISTINGS.as_ref()?;
    if let Ok(turn) = turns.try_acquire() {
        return Some(turn);
    }
    // Never closed, so acquire can't fail.
    let turn = turns.acquire().await.unwrap();
    crate::phases::mark("listing turn waited");
    Some(turn)
}

#[allow(clippy::too_many_arguments)]
async fn ls(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    path: &[String],
    limit: usize,
    after: Option<&str>,
    narrow: &Narrow,
    with: With,
) -> Result<Value> {
    let who = reader.who;
    // The listings that read every fact or every episode share those reads with the others worked out beside them
    // ([`Pass`]), joined before their turn so one waiting for it keeps the reads of those before it.
    let whole = match path.first().map(String::as_str) {
        Some("entities") => path.len() < 3,
        Some("notes" | "sessions" | "sources") => true,
        _ => false,
    };
    let pass = match whole {
        true => Pass::join(session, who),
        false => Default::default(),
    };
    // They take a turn too ([`WHOLE_LISTINGS`]); the root (its counts are worked out in the background,
    // [`count_root`]), one entity, the skills and the advice don't.
    let _turn = match path.first().map(String::as_str) {
        Some("entities") if path.len() < 3 => {
            crate::phases::mark("ls entities");
            whole_listing_turn().await
        }
        Some(listing @ ("notes" | "sessions" | "sources")) => {
            crate::phases::mark(match listing {
                "notes" => "ls notes",
                "sessions" => "ls sessions",
                _ => "ls sources",
            });
            whole_listing_turn().await
        }
        _ => None,
    };
    let page = |mut children: Vec<Value>| {
        children.retain(|c| after.is_none_or(|a| c["name"].as_str().unwrap_or_default() > a));
        let more = children.len() > limit;
        children.truncate(limit);
        let next = more.then(|| children.last().map(|c| c["name"].clone()));
        json!({"path": path, "children": children, "next": next.flatten()})
    };
    match path.first().map(String::as_str) {
        None => {
            // Until the caller's counts are worked out every folder shows, without a count: one that turns out
            // empty drops from a later listing.
            let children: Vec<Value> = match root_counts(store, session, who).await {
                Some(counts) => counts
                    .iter()
                    .filter(|(name, n)| *n != 0 || matches!(*name, "entities" | "sources"))
                    .map(|(name, n)| json!({"name": name, "kind": "folder", "path": [name], "count": n}))
                    .collect(),
                None => ROOT_FOLDERS
                    .iter()
                    .map(|name| json!({"name": name, "kind": "folder", "path": [name]}))
                    .collect(),
            };
            Ok(page(children))
        }
        Some("skills") => skills(store, session, who, path, page).await,
        // The notes: no folders, each one its fact.
        Some("notes") if path.len() == 1 => {
            let eps = pass.episodes(store, session, who).await?;
            let children = notes(store, session, who, &eps)
                .await?
                .iter()
                .map(|f| {
                    let id = f.source_episodes.first().map_or("", String::as_str);
                    let name = format!("{}-{}", day(f.valid_from), &id[..id.len().min(8)]);
                    json!({"name": name, "kind": "episode", "path": path, "episode_id": id,
                           "fact_id": f.fact_id, "occurred_at": f.valid_from, "title": title(&f.value),
                           "team": f.tags.iter().any(|t| t == "team")})
                })
                .collect();
            Ok(page(children))
        }
        Some("sessions") => {
            let eps = pass.episodes(store, session, who).await?;
            let leaves = filed(&eps, who);
            let children = folder(path, &leaves)?;
            titled(store, session.scope(), page(children)).await
        }
        Some("advice") => {
            let advice = advice(store, session, who).await?;
            let leaves: Vec<(Vec<String>, Value)> = advice
                .into_iter()
                .map(|a| {
                    let at = vec![
                        a["space"].as_str().unwrap_or("team").to_string(),
                        a["category"]
                            .as_str()
                            .unwrap_or("uncategorized")
                            .to_string(),
                        a["situation"].as_str().unwrap_or("general").to_string(),
                    ];
                    (at, a)
                })
                .collect();
            let at = &path[1..];
            if let [space, category, situation] = at {
                // One piece of advice: its note in full, with how it has done.
                let found = leaves
                    .into_iter()
                    .find(|(p, _)| p == &[space.clone(), category.clone(), situation.clone()]);
                let Some((_, a)) = found else {
                    return Err(NotFound.into());
                };
                // Every version of its note the caller may read, newest first.
                let doc = a["note_id"]
                    .as_str()
                    .and_then(|id| session.vis().document_of(id))
                    .map(str::to_string);
                let history = match doc {
                    Some(k) => history(store, session, who, &k).await?,
                    None => vec![],
                };
                return Ok(json!({"path": path, "advice": a, "history": history}));
            }
            let mut folders: BTreeMap<&str, (usize, Option<&Value>)> = BTreeMap::new();
            for (p, a) in &leaves {
                if p.starts_with(at) {
                    let e = folders.entry(p[at.len()].as_str()).or_default();
                    e.0 += 1;
                    e.1 = Some(a);
                }
            }
            if folders.is_empty() && !at.is_empty() {
                return Err(NotFound.into());
            }
            let children = folders
                .into_iter()
                .map(|(name, (n, a))| {
                    let mut p = path.to_vec();
                    p.push(name.to_string());
                    match (at.len(), a) {
                        (2, Some(a)) => json!({"name": name, "kind": "advice", "path": p, "title": name,
                            "status": advice_status(a), "runs": a["runs"], "learned_at": a["learned_at"]}),
                        _ => json!({"name": name, "kind": "folder", "path": p, "count": n}),
                    }
                })
                .collect();
            Ok(page(children))
        }
        Some("sources") => {
            let at: Vec<&str> = path[1..].iter().map(String::as_str).collect();
            let eps = pass.episodes(store, session, who).await?;
            let under: Vec<&Ep> = eps
                .iter()
                .filter(|e| listed_source(&e.source) && e.segments().starts_with(&at))
                .collect();
            if under.is_empty() && !at.is_empty() {
                return Err(NotFound.into());
            }
            // Each next segment, with the episodes under it; episodes whose source ends here are listed
            // by id.
            let mut folders: BTreeMap<&str, Vec<&Ep>> = BTreeMap::new();
            let mut here = vec![];
            for e in under {
                match e.segments().get(at.len()) {
                    Some(s) => folders.entry(s).or_default().push(e),
                    None => here.push(e),
                }
            }
            let mut children: Vec<Value> = folders
                .into_iter()
                .map(|(name, eps)| {
                    let mut p = path.to_vec();
                    p.push(name.to_string());
                    // A folder holding just one episode whose source ends at it is that episode.
                    match eps.as_slice() {
                        [e] if e.segments().len() == at.len() + 1 => {
                            json!({"name": name, "kind": "episode", "path": p, "episode_id": e.id,
                                   "occurred_at": e.occurred_at})
                        }
                        _ => json!({"name": name, "kind": "folder", "path": p, "count": eps.len(),
                                    "latest": eps.iter().map(|e| e.occurred_at).max()}),
                    }
                })
                .chain(here.into_iter().map(|e| {
                    json!({"name": e.id, "kind": "episode", "path": path, "episode_id": e.id,
                           "occurred_at": e.occurred_at})
                }))
                .collect();
            children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            sources_titled(store, session, who, page(children), &eps).await
        }
        Some("entities") if path.len() == 3 => {
            // One entity's facts: read by the entity index, not out of every fact.
            let (t, k) = (path[1].as_str(), path[2].as_str());
            let mut ids = vec![format!("{t}:{k}")];
            if t == "_" && !k.contains(':') {
                ids.push(k.to_string());
            }
            let facts = once_each(session.read_in(store, who, Col::Entity, &ids, None).await?);
            let mut fs: Vec<&Fact> = facts
                .iter()
                .filter(|f| split_entity(&f.entity) == (t, k))
                .collect();
            if fs.is_empty() {
                return Err(NotFound.into());
            }
            // The fact id last, so facts stated at the same moment keep one order however they were read.
            fs.sort_by(|a, b| {
                (&a.predicate, a.valid_from, &a.fact_id).cmp(&(
                    &b.predicate,
                    b.valid_from,
                    &b.fact_id,
                ))
            });
            Ok(json!({"path": path, "entity": fs[0].entity,
                      "facts": fs.iter().map(|f| fact_json(f)).collect::<Vec<_>>()}))
        }
        Some("entities") => {
            // Named from every fact the caller may read, the narrowing aside: a name from another source is still
            // theirs to read.
            let names = |facts: &[Fact]| match path.len() == 2 {
                true => crate::query::schema::display_names(facts, crate::now_micros()),
                false => Default::default(),
            };
            // Narrowed, its own read (the narrowing keeps what it reads); else the pass's.
            let (facts, shown) = match narrow.is_empty() {
                true => {
                    let facts = pass.facts(store, session, who).await?;
                    let shown = names(&facts);
                    (facts, shown)
                }
                false => {
                    let facts = facts(store, session, who).await?;
                    let shown = names(&facts);
                    (
                        Arc::new(narrow.keep(store, session, who, facts).await?),
                        shown,
                    )
                }
            };
            let entities = by_entity(&facts);
            match &path[1..] {
                [] => {
                    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
                    for (t, _) in entities.keys() {
                        *types.entry(t).or_default() += 1;
                    }
                    let by_source = match with.sources {
                        true => Some(by_source(
                            &entities,
                            &pass.episodes(store, session, who).await?,
                        )),
                        false => None,
                    };
                    Ok(page(
                        types
                            .into_iter()
                            .map(|(t, n)| {
                                let mut c = json!({"name": t, "kind": "folder", "path": ["entities", t], "count": n});
                                if let Some(by) = &by_source {
                                    c["sources"] = json!(by.get(t).cloned().unwrap_or_default());
                                }
                                c
                            })
                            .collect(),
                    ))
                }
                [t] => {
                    let children: Vec<Value> = entities
                        .iter()
                        .filter(|((et, _), _)| et == t)
                        .map(|((_, k), fs)| {
                            let mut c = json!({"name": k, "kind": "entity", "path": ["entities", t, k],
                                   "entity": fs[0].entity, "count": fs.len()});
                            if let Some(s) = shown.get(&fs[0].entity) {
                                if let Some(title) = &s.title {
                                    c["title"] = json!(title);
                                }
                                c["since"] = json!(s.since);
                            }
                            c
                        })
                        .collect();
                    // Narrowed, a type with nothing from that source or person is an empty list, not a
                    // missing folder.
                    if children.is_empty() && narrow.is_empty() {
                        return Err(NotFound.into());
                    }
                    let mut listed = page(children);
                    if with.facts {
                        for c in listed["children"].as_array_mut().into_iter().flatten() {
                            let k = c["name"].as_str().unwrap_or_default();
                            let held: Vec<Value> = entities
                                .get(&(t.as_str(), k))
                                .into_iter()
                                .flatten()
                                .filter(|f| f.valid_to.is_none())
                                .map(|f| json!({"predicate": f.predicate, "value": f.value}))
                                .collect();
                            c["facts"] = json!(held);
                        }
                    }
                    named(store, session, who, listed).await
                }
                _ => Err(NotFound.into()),
            }
        }
        Some(_) => Err(NotFound.into()),
    }
}

async fn episode(store: &Store, session: &Session, who: &Principal, id: &str) -> Result<Value> {
    // The facts citing it: those whose sources name it (a scan of that one column), then exactly.
    let cites_it = Filter::Contains(Col::Sources, id.to_string());
    let citing = session.read_where(store, who, &cites_it, None);
    let (eps, facts) = tokio::try_join!(episodes(store, session, who), citing)?;
    let facts = once_each(facts);
    let Some(e) = eps.iter().find(|e| e.id == id) else {
        return Err(NotFound.into());
    };
    // Read with the caller's own access, again: an ACL change since the listing hides it.
    let Some(body) = session.episode_body(store, who, id).await? else {
        return Err(NotFound.into());
    };
    let truncated = body.len() > BODY_BYTES;
    let text = String::from_utf8_lossy(&body[..body.len().min(BODY_BYTES)]).into_owned();
    // Where the Explorer lists it: a note (one its fact cites) under `notes`, anything else under `sources/`.
    let path = if crate::note_facts::is_note(&e.source) {
        vec!["notes".to_string()]
    } else if !listed_source(&e.source) {
        engine_path(store, session, who, &e.id).await?
    } else {
        std::iter::once("sources")
            .chain(e.segments())
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let cites: Vec<Value> = facts
        .iter()
        .filter(|f| f.source_episodes.iter().any(|s| s == id))
        .map(fact_json)
        .collect();
    let titled =
        crate::arrivals::titled(store, session, who, &[(e.id.clone(), e.source.clone())]).await?;
    let title = titled.get(&e.id).and_then(|t| t.shown());
    let v = json!({"episode_id": e.id, "source": e.source, "path": path, "occurred_at": e.occurred_at,
                   "title": title, "about": e.about, "body": text, "truncated": truncated, "facts": cites});
    crate::names::attach(store, session, who, v, &eps).await
}

/// A source's page of children with each episode on it titled by what its type says it is (a call by its
/// recorder's title), else by its first line ([`crate::arrivals::Titled::shown`]).
async fn sources_titled(
    store: &Store,
    session: &Session,
    who: &Principal,
    mut listed: Value,
    eps: &[Ep],
) -> Result<Value> {
    let on_page: HashSet<&str> = listed["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["episode_id"].as_str())
        .collect();
    let ask: Vec<(String, String)> = eps
        .iter()
        .filter(|e| on_page.contains(e.id.as_str()))
        .map(|e| (e.id.clone(), e.source.clone()))
        .collect();
    if ask.is_empty() {
        return Ok(listed);
    }
    let titled = crate::arrivals::titled(store, session, who, &ask).await?;
    for c in listed["children"].as_array_mut().into_iter().flatten() {
        let Some(t) = c["episode_id"].as_str().and_then(|id| titled.get(id)) else {
            continue;
        };
        if let Some(name) = t.shown() {
            c["title"] = json!(name);
        }
    }
    Ok(listed)
}

/// An entity type's page of entities with each one named ([`crate::names`]) titled by its name.
async fn named(
    store: &Store,
    session: &Session,
    who: &Principal,
    mut listed: Value,
) -> Result<Value> {
    let ids: Vec<String> = listed["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["entity"].as_str().map(String::from))
        .filter(|e| crate::names::opaque(e))
        .collect();
    if ids.is_empty() {
        return Ok(listed);
    }
    let eps = episodes(store, session, who).await?;
    let names = crate::names::of(store, session, who, &ids, &eps).await?;
    for c in listed["children"].as_array_mut().into_iter().flatten() {
        if let Some(n) = c["entity"].as_str().and_then(|e| names.get(e)) {
            c["title"] = json!(n);
        }
    }
    Ok(listed)
}

/// The compiled skills the caller may read: (slug, document key). The routing indexes (one per audience,
/// `skills/index` or `skills/index~<hash>`) are not skills.
fn skill_keys(session: &Session, who: &Principal) -> Vec<(String, String)> {
    session
        .documents(who, crate::compile::SKILLS, None)
        .into_iter()
        .filter(|k| !crate::compile::is_index(k))
        .filter_map(|k| {
            let slug = k.strip_prefix(crate::compile::SKILLS)?.to_string();
            (!slug.is_empty() && !slug.contains('/')).then_some((slug, k))
        })
        .collect()
}

/// The routing indexes the caller may read, by name (`index`, `index~<hash>`), as document keys.
fn index_keys(session: &Session, who: &Principal) -> Vec<(String, String)> {
    session
        .documents(who, crate::compile::SKILLS, None)
        .into_iter()
        .filter(|k| crate::compile::is_index(k))
        .filter_map(|k| Some((k.strip_prefix(crate::compile::SKILLS)?.to_string(), k)))
        .collect()
}

/// `skills/…`: the routing indexes and the compiled skills, or one of them read in full.
async fn skills(
    store: &Store,
    session: &Session,
    who: &Principal,
    path: &[String],
    page: impl Fn(Vec<Value>) -> Value,
) -> Result<Value> {
    match &path[1..] {
        [] => {
            let skills = skill_keys(session, who);
            // When each skill applies, from the index lines (`- `slug`: when (`aip key`)`) of every index
            // the caller may read.
            // The indexes are read at once: each is a body read from object storage.
            let indexes = futures::future::try_join_all(index_keys(session, who).into_iter().map(
                |(name, key)| async move {
                    let doc = session.document(store, who, &key, None).await?;
                    anyhow::Ok(doc.map(|(_, at, body)| (name, at, body)))
                },
            ))
            .await?;
            crate::phases::mark("skills indexes read");
            let mut when: BTreeMap<String, String> = BTreeMap::new();
            let mut children = vec![];
            for (name, at, body) in indexes.into_iter().flatten() {
                let mut lines = 0;
                for line in String::from_utf8_lossy(&body).lines() {
                    let Some((slug, rest)) =
                        line.strip_prefix("- `").and_then(|l| l.split_once("`: "))
                    else {
                        continue;
                    };
                    lines += 1;
                    let w = rest.rsplit_once(" (`").map_or(rest, |(w, _)| w).trim();
                    when.entry(slug.to_string())
                        .or_insert_with(|| w.to_string());
                }
                children.push(
                    json!({"name": name, "kind": "skill_index", "path": ["skills", name],
                                     "count": lines, "occurred_at": at}),
                );
            }
            children.extend(skills.iter().map(|(slug, _)| {
                json!({"name": slug, "kind": "skill", "path": ["skills", slug],
                       "when": when.get(slug)})
            }));
            children.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Ok(page(children))
        }
        [name] => {
            let key = crate::compile::skill_key(name);
            let index = crate::compile::is_index(&key);
            let Some((id, at, body)) = session.document(store, who, &key, None).await? else {
                return Err(NotFound.into());
            };
            // A skill's first line is the compiler's own record of it (`<!-- memstore skill {...} -->`).
            let text = String::from_utf8_lossy(&body);
            let markdown = match text.split_once('\n') {
                Some((first, rest)) if first.starts_with("<!-- memstore skill ") => rest,
                _ => text.as_ref(),
            };
            let mut v = json!({"path": path, "name": name,
                               "kind": if index { "skill_index" } else { "skill" },
                               "markdown": markdown, "episode_id": id, "occurred_at": at,
                               "history": history(store, session, who, &key).await?});
            // The episodes it was compiled from that the caller may read, each with its name; the rest
            // are only counted.
            if let Some((meta, _)) = crate::compile::parse_skill(&body) {
                let vis = session.vis();
                let (seen, hidden): (Vec<_>, Vec<_>) = meta
                    .sources
                    .iter()
                    .enumerate()
                    .partition(|(_, e)| vis.can_see_episode(e, who));
                v["sources"] = json!(
                    seen.iter()
                        .map(|(i, e)| json!({"episode_id": e, "name": meta.names.get(*i)}))
                        .collect::<Vec<_>>()
                );
                if !hidden.is_empty() {
                    v["sources_hidden"] = json!(hidden.len());
                }
            }
            if !index {
                let graph = session
                    .document(store, who, &crate::compile::aip_key(name), None)
                    .await?;
                v["aip"] = graph.map_or(Value::Null, |(_, _, b)| {
                    serde_json::from_slice(&b)
                        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
                });
            }
            Ok(v)
        }
        _ => Err(NotFound.into()),
    }
}

pub(crate) fn fact_json(f: &Fact) -> Value {
    let mut v = json!({"fact_id": f.fact_id, "entity": f.entity, "predicate": f.predicate, "value": f.value,
           "valid_from": f.valid_from, "valid_to": f.valid_to, "source_episodes": f.source_episodes,
           "quote": f.quote});
    for (k, x) in [("unit", &f.unit), ("time_basis", &f.time_basis)] {
        if let Some(x) = x {
            v[k] = json!(x);
        }
    }
    if let Some(c) = crate::cite_json(f) {
        v["cite"] = c;
    }
    v
}

/// An entity id's type and key: `deal:42` is (`deal`, `42`); an id without a type has type `_`.
fn split_entity(e: &str) -> (&str, &str) {
    e.split_once(':').unwrap_or(("_", e))
}

/// Visible facts by entity (type, key).
fn by_entity(facts: &[Fact]) -> BTreeMap<(&str, &str), Vec<&Fact>> {
    let mut out: BTreeMap<(&str, &str), Vec<&Fact>> = BTreeMap::new();
    for f in facts {
        out.entry(split_entity(&f.entity)).or_default().push(f);
    }
    out
}

/// Each type's records by the source they were learned from ([`With::sources`]): a record counts once under each
/// source (the first part of its name, `slack` of `slack:C09`) of the episodes its facts cite, of `eps`, the episodes
/// the caller may read; an engine's own episodes name no source.
fn by_source(
    entities: &BTreeMap<(&str, &str), Vec<&Fact>>,
    eps: &[Ep],
) -> BTreeMap<String, BTreeMap<String, usize>> {
    let source: HashMap<&str, &str> = eps
        .iter()
        .filter(|e| listed_source(&e.source))
        .filter_map(|e| Some((e.id.as_str(), e.source.split_once(':')?.0)))
        .collect();
    let mut out: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for ((t, _), fs) in entities {
        let from: std::collections::BTreeSet<&str> = fs
            .iter()
            .flat_map(|f| f.source_episodes.iter())
            .filter_map(|e| source.get(e.as_str()).copied())
            .collect();
        for s in from {
            *out.entry(t.to_string())
                .or_default()
                .entry(s.to_string())
                .or_default() += 1;
        }
    }
    out
}

/// Every fact the caller may read, over all valid time (history included), once each: a corpus loaded
/// twice into one catalog commits each fact twice, under the same id.
/// The facts `entities/` files: every one the caller may read but the notes', which `notes` lists.
pub(crate) async fn facts(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Fact>> {
    // As [`Session::read`], its two steps marked apart: the table's rows (kept by the store), then each decoded.
    let rows = store
        .rows(&session.scope().namespace(), crate::FACTS)
        .await?;
    crate::phases::mark("facts table read");
    // Seconds of CPU on a large org: off the async workers ([`crate::running::cpu`]).
    let many = rows.rows.iter().map(|b| b.num_rows()).sum::<usize>() >= crate::running::CPU_ROWS;
    let facts = crate::running::cpu(many, || {
        let read = session.read_rows(&rows.rows, who, None);
        crate::phases::mark("facts decoded");
        let mut facts = once_each(read);
        facts.retain(|f| !crate::is_note_fact(f));
        facts
    });
    crate::phases::mark("facts once each");
    Ok(facts)
}

/// Each fact once, by id.
pub(crate) fn once_each(mut facts: Vec<Fact>) -> Vec<Fact> {
    let mut seen = std::collections::HashSet::new();
    facts.retain(|f| seen.insert(f.fact_id.clone()));
    facts
}

/// The advice the caller may read, each with its note's text.
async fn advice(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Value>> {
    let req = json!({"op": "advice", "include": "text"});
    let listed = crate::listing::advice(store, session, who, &req).await?;
    Ok(listed["advice"].as_array().cloned().unwrap_or_default())
}

/// The root's folders and how many each holds as the caller reads them, by name.
type Counts = Vec<(&'static str, usize)>;

/// The root's folders, by name, as [`count_root`] lists them.
const ROOT_FOLDERS: [&str; 6] = [
    "advice", "entities", "notes", "sessions", "skills", "sources",
];

/// How long a reader's counts are kept for certain once there are [`MAX_ROOTS`] readers: older ones go first.
const ROOT_STALE: std::time::Duration = std::time::Duration::from_secs(600);
/// Readers whose counts are kept, at most.
const MAX_ROOTS: usize = 1000;

struct RootCounts {
    /// The state they were counted at (or, while none are kept yet, are being counted at): its ledger heads and
    /// its removals ([`crate::Vis::removals`]). A loss that commits nothing (an erasure or a retention policy
    /// rewrites the tables, the heads stay) is a state of its own: the counts before it are not its counts.
    state: (Vec<String>, u64),
    /// For counts read on main as it is now, main's state with the scope's loss count and facts table read before
    /// it was pinned ([`crate::kept_answers::At`]): counts from before a loss this server counted (an erasure, a
    /// forget of a fact) are never answered after it, though neither the heads nor the removals moved.
    main: Option<crate::kept_answers::At>,
    at: std::time::Instant,
    /// None until the reader's first count is done.
    counts: Option<Counts>,
    refreshing: bool,
}

/// Each reader's last root counts, by scope, whether read on main as it is now, and the reader's closure: never
/// shared between readers.
static ROOTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, RootCounts>>> =
    std::sync::LazyLock::new(Default::default);

/// The root's counts for the caller, if they are known yet. Counting reads every fact the caller may read, which
/// takes long in a large org, so it never runs on the request: the counts are kept by reader, and at the same
/// ledger state they are the same. On main as it is now, counts from an earlier state answer, however old, while
/// the new state's are counted in the background, but only while memory lost nothing since they were counted (the
/// same removals, loss count and facts generation: [`crate::kept_answers`]): past a loss they answer None, so a
/// page waiting for them (Home's) shows the new state's once counted, never what was taken away. With none kept for
/// the caller (a first visit, a restart, or a read off main whose state moved) this answers None and counts in the
/// background for a later read.
async fn root_counts(store: &Store, session: &Session, who: &Principal) -> Option<Counts> {
    let state = (session.vis().heads().to_vec(), session.vis().removals);
    let now_main = session.on_main_now();
    let main = match now_main {
        true => main_at(store, session).await,
        false => None,
    };
    let key = format!(
        "{}\n{now_main}\n{}\n{}",
        session.scope().namespace(),
        who.user(),
        who.tokens().join(",")
    );
    let mut kept = ROOTS.lock().unwrap();
    let (answer, refresh) = match kept.get_mut(&key) {
        Some(c) => {
            // Lost since they were counted: a loss this server counted, or one main's state shows. Without a mark
            // to compare (a read off main, or a state not main's now), the removals alone.
            let lost = match (&c.main, &main) {
                (Some(was), Some(now)) => !was.lost_nothing(now),
                _ => c.state.1 != state.1,
            };
            let current = c.state == state && !lost;
            let answer = if current || now_main && !lost {
                c.counts.clone()
            } else {
                None
            };
            // Counts from an earlier state: shown, never shared as this state's.
            if !current && answer.is_some() {
                crate::kept_answers::not_sharable();
            }
            let refresh = !c.refreshing && !(current && c.counts.is_some());
            if refresh {
                c.refreshing = true;
            }
            (answer, refresh)
        }
        None => {
            keep_root(
                &mut kept,
                key.clone(),
                state.clone(),
                main.clone(),
                None,
                true,
            );
            (None, true)
        }
    };
    drop(kept);
    if refresh {
        let (store, session, who) = (store.clone(), session.clone(), who.clone());
        tokio::spawn(async move {
            let got = count_root(&store, &session, &who).await;
            let mut kept = ROOTS.lock().unwrap();
            match got {
                Ok(counts) => keep_root(&mut kept, key, state, main, Some(counts), false),
                Err(e) => {
                    eprintln!("root counts: {}: {e:#}", session.scope().namespace());
                    // Counts kept from before still answer; a first count that failed is tried again on the
                    // next read.
                    match kept.get(&key).map(|c| c.counts.is_some()) {
                        Some(true) => kept.get_mut(&key).unwrap().refreshing = false,
                        Some(false) => drop(kept.remove(&key)),
                        None => {}
                    }
                }
            }
        });
    }
    answer
}

/// Main's state as `session` reads it, with the scope's loss count and facts table read before it ([`At::of`]), when
/// `session` reads main's state as it is now: None when a read fails, or `session` pinned a state main has moved on
/// from since (its counts then answer by its removals alone).
///
/// [`At::of`]: crate::kept_answers::At::of
async fn main_at(store: &Store, session: &Session) -> Option<crate::kept_answers::At> {
    let mark = crate::kept_answers::Mark::read(store, session.scope())
        .await
        .ok()?;
    let now = store
        .lagging_visibility(session.scope(), crate::MAIN)
        .await
        .ok()?;
    std::ptr::eq(session.vis(), &*now).then(|| crate::kept_answers::At::of(mark, &now))
}

fn keep_root(
    kept: &mut std::collections::HashMap<String, RootCounts>,
    key: String,
    state: (Vec<String>, u64),
    main: Option<crate::kept_answers::At>,
    counts: Option<Counts>,
    refreshing: bool,
) {
    if kept.len() >= MAX_ROOTS && !kept.contains_key(&key) {
        kept.retain(|_, c| c.at.elapsed() < ROOT_STALE);
        if kept.len() >= MAX_ROOTS {
            kept.clear();
        }
    }
    let at = std::time::Instant::now();
    kept.insert(
        key,
        RootCounts {
            state,
            main,
            at,
            counts,
            refreshing,
        },
    );
}

/// Whether `sources/` lists an episode from `source`: anything but the engine's own documents
/// ([`crate::episode_types::engine_source`]), whose versions show on their own pages.
fn listed_source(source: &str) -> bool {
    !crate::episode_types::engine_source(source)
}

/// Versions of a document's history shown, at most.
const HISTORY: usize = 50;

/// Every version of document `key` the caller may read, newest first, each with when it was written and its first
/// line: a note's or a skill's history, each version readable with the `episode` op.
async fn history(
    store: &Store,
    session: &Session,
    who: &Principal,
    key: &str,
) -> Result<Vec<Value>> {
    let versions = session.versions(store, who, key).await?;
    Ok(versions
        .iter()
        .take(HISTORY)
        .map(|(id, at, body)| {
            let text = String::from_utf8_lossy(body);
            // A skill's first line is the compiler's record of it, not its title.
            let shown = match text.split_once('\n') {
                Some((first, rest)) if first.starts_with("<!-- memstore skill ") => rest,
                _ => text.as_ref(),
            };
            json!({"episode_id": id, "occurred_at": at, "title": title(shown)})
        })
        .collect())
}

/// Where the Explorer shows a version of one of the engine's own documents: the piece of advice or the skill it is a
/// version of, else the folder of its kind (`advice` for the advice index and map, `skills` for a skills index).
/// Anything else the engine wrote (a dream's, a merge's or reconcile's record) has no page and is named by its source
/// alone, under no folder.
async fn engine_path(
    store: &Store,
    session: &Session,
    who: &Principal,
    id: &str,
) -> Result<Vec<String>> {
    let Some(doc) = session.vis().document_of(id).map(str::to_string) else {
        return Ok(vec![]);
    };
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Some(slug) = doc
        .strip_prefix(crate::compile::SKILLS)
        .or_else(|| doc.strip_prefix(crate::compile::AIP))
    {
        return Ok(match crate::compile::is_index(&doc) {
            true => strings(&["skills"]),
            false => strings(&["skills", slug]),
        });
    }
    if doc.starts_with(crate::advice::PREFIX) || doc.starts_with(crate::advice::MINE) {
        let vis = session.vis();
        let found = advice(store, session, who)
            .await?
            .into_iter()
            .find(|a| a["note_id"].as_str().and_then(|n| vis.document_of(n)) == Some(doc.as_str()));
        return Ok(match found {
            Some(a) => {
                ["space", "category", "situation"]
                    .iter()
                    .fold(strings(&["advice"]), |mut p, k| {
                        p.push(a[*k].as_str().unwrap_or_default().to_string());
                        p
                    })
            }
            None => strings(&["advice"]),
        });
    }
    Ok(vec![])
}

/// Counts the root's folders: the episodes (and from them the notes), the facts and the advice read side by side.
async fn count_root(store: &Store, session: &Session, who: &Principal) -> Result<Counts> {
    // It reads every fact and every episode, as the listings do ([`Pass`], [`WHOLE_LISTINGS`]).
    let pass = Pass::join(session, who);
    let _turn = whole_listing_turn().await;
    let read_eps = async {
        let eps = pass.episodes(store, session, who).await?;
        let notes = notes(store, session, who, &eps).await?.len();
        crate::phases::mark("ls episodes+notes");
        anyhow::Ok((eps, notes))
    };
    let read_facts = async {
        let n = by_entity(&pass.facts(store, session, who).await?).len();
        crate::phases::mark("ls facts");
        anyhow::Ok(n)
    };
    let read_advice = async {
        let n = advice(store, session, who).await?.len();
        crate::phases::mark("ls advice");
        anyhow::Ok(n)
    };
    let ((eps, notes), entities, advice) = tokio::try_join!(read_eps, read_facts, read_advice)?;
    Ok(vec![
        ("advice", advice),
        ("entities", entities),
        ("notes", notes),
        ("sessions", filed(&eps, who).len()),
        ("skills", skill_keys(session, who).len()),
        (
            "sources",
            eps.iter().filter(|e| listed_source(&e.source)).count(),
        ),
    ])
}

/// How a piece of advice stands, in a word for a list: `merged`, `held`, `served` or, served all the same, `not
/// ratified` (outcomes or people don't back it yet).
fn advice_status(a: &Value) -> &'static str {
    if !a["merged_into"].is_null() {
        "merged"
    } else if a["held"] == true || !a["withheld"].is_null() {
        "held"
    } else if !a["needs_backing"].is_null() {
        "not ratified"
    } else {
        "served"
    }
}

/// The notes the caller may read, as their facts ([`crate::note_facts`]), newest first: each read with the caller's
/// own access as every fact is, so a person's note is theirs alone and a shared one everyone's in the scope. Only
/// a fact citing a note (among `eps`, the episodes the caller reads) is one.
async fn notes(store: &Store, session: &Session, who: &Principal, eps: &[Ep]) -> Result<Vec<Fact>> {
    let note = Filter::Eq(Col::Predicate, Lit::Str(crate::NOTE.to_string()));
    let notes_at: std::collections::BTreeSet<&str> = eps
        .iter()
        .filter(|e| crate::note_facts::is_note(&e.source))
        .map(|e| e.id.as_str())
        .collect();
    let mut notes: Vec<Fact> = once_each(
        session
            .read_where(store, who, &note, Some(i64::MAX))
            .await?,
    )
    .into_iter()
    .filter(crate::is_note_fact)
    .filter(|f| notes_at.contains(f.source_episodes[0].as_str()))
    .collect();
    notes.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
    Ok(notes)
}

/// The caller's own sessions among `eps`, each with the folders it is filed under (below `sessions/`): `ended/<day>`
/// a session's record, `<harness>/<repo>` what a coding session's hooks kept as it went. One entry per session, its
/// latest save: a session's conversation is kept again at every step (each a new version of one document), and its
/// record and transcript come beside it, so listing every episode showed one session dozens of times.
fn filed<'a>(eps: &'a [Ep], who: &Principal) -> Vec<(Vec<String>, &'a Ep)> {
    let me = who.user();
    let mut latest: BTreeMap<(bool, &str, &str), &Ep> = BTreeMap::new();
    for e in eps {
        let Some((author, rest)) = crate::profile::author_of(&e.source) else {
            continue;
        };
        if !author.eq_ignore_ascii_case(me) {
            continue;
        }
        // (ended, the folder's key, the session): an ended session's records by its id; a hook's note by its source.
        let key =
            if crate::session_record_source(&e.source).is_some() || rest.starts_with("session:") {
                (true, "", session_of(rest))
            } else if crate::keep::hook_session_note(&e.source) {
                (false, rest, "")
            } else {
                continue;
            };
        let held = latest.entry(key).or_insert(e);
        if (e.occurred_at, e.recorded_at, &e.id) > (held.occurred_at, held.recorded_at, &held.id) {
            *held = e;
        }
    }
    latest
        .into_iter()
        .map(|((ended, rest, _), e)| {
            let at: Vec<String> = if ended {
                vec!["ended".into(), day(e.occurred_at)]
            } else {
                let parts: Vec<&str> = rest.split(':').collect();
                vec![parts[0].into(), parts[1].into()]
            };
            (at, e)
        })
        .collect()
}

/// The session a record of it is of, from its source past its author: `memstore:session:<id>` (and its
/// `…:conversation`), `memstore:transcript:<harness>:<repo>:<id>`, or the older `session:<id>`.
fn session_of(rest: &str) -> &str {
    if let Some(r) = rest.strip_prefix(crate::TRANSCRIPT_SOURCE) {
        return r.rsplit_once(':').map_or(r, |(_, id)| id);
    }
    let r = rest
        .strip_prefix(crate::SESSION_SOURCE)
        .or_else(|| rest.strip_prefix("session:"))
        .unwrap_or(rest);
    r.split(':').next().unwrap_or(r)
}

/// A note's or a session's title: its first line that says something, at most 90 characters.
fn title(text: &str) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .map(|l| {
            l.strip_prefix("User:")
                .or(l.strip_prefix("Task:"))
                .unwrap_or(l)
                .trim()
        })
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut title: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        title.push('…');
    }
    title
}

/// A time's day, `YYYY-MM-DD` (UTC).
fn day(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map_or_else(|| "undated".into(), |t| t.format("%Y-%m-%d").to_string())
}

/// The children at `path` (its root first) of a derived tree's `leaves`: a folder per next segment with how
/// many are under it, and each leaf filed right here, newest first by name (`<day>-<id8>`).
fn folder(path: &[String], leaves: &[(Vec<String>, &Ep)]) -> Result<Vec<Value>> {
    let at = &path[1..];
    let mut folders: BTreeMap<&str, (usize, i64)> = BTreeMap::new();
    let mut here = vec![];
    for (p, e) in leaves.iter().filter(|(p, _)| p.starts_with(at)) {
        match p.get(at.len()) {
            Some(s) => {
                let f = folders.entry(s).or_default();
                f.0 += 1;
                f.1 = f.1.max(e.occurred_at);
            }
            None => here.push(*e),
        }
    }
    if folders.is_empty() && here.is_empty() && !at.is_empty() {
        return Err(NotFound.into());
    }
    let mut children: Vec<Value> = folders
        .into_iter()
        .map(|(name, (n, latest))| {
            let mut p = path.to_vec();
            p.push(name.to_string());
            json!({"name": name, "kind": "folder", "path": p, "count": n, "latest": latest})
        })
        .collect();
    here.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
    children.extend(here.into_iter().map(|e| {
        let name = format!("{}-{}", day(e.occurred_at), &e.id[..e.id.len().min(8)]);
        json!({"name": name, "kind": "episode", "path": path, "episode_id": e.id, "occurred_at": e.occurred_at})
    }));
    Ok(children)
}

/// Page `listed`, with each session on it titled by its first request.
async fn titled(store: &Store, scope: &Scope, mut listed: Value) -> Result<Value> {
    let ids: Vec<String> = listed["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["episode_id"].as_str().map(String::from))
        .collect();
    if ids.is_empty() {
        return Ok(listed);
    }
    let bodies = store.episode_bodies(scope, ids).await?;
    for c in listed["children"].as_array_mut().into_iter().flatten() {
        let Some(b) = c["episode_id"].as_str().and_then(|id| bodies.get(id)) else {
            continue;
        };
        c["title"] = json!(title(&String::from_utf8_lossy(b)));
    }
    Ok(listed)
}

/// Every committed episode the session sees that the caller may read, once each.
pub(crate) async fn episodes(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Ep>> {
    let files = store.episode_files(session.scope()).await?;
    crate::phases::mark("episode files read");
    let many = files
        .values()
        .flat_map(|bs| bs.iter())
        .map(|b| b.num_rows())
        .sum::<usize>()
        >= crate::running::CPU_ROWS;
    let eps = crate::running::cpu(many, || episodes_in(&files, session, who));
    crate::phases::mark("episodes listed");
    Ok(eps)
}

/// [`episodes`] of the episodes table's files as already read ([`Store::episode_files`]).
pub(crate) fn episodes_in(
    files: &std::collections::HashMap<String, Arc<Vec<arrow_array::RecordBatch>>>,
    session: &Session,
    who: &Principal,
) -> Vec<Ep> {
    let vis = session.vis();
    let mut out: BTreeMap<String, Ep> = BTreeMap::new();
    for bs in files.values() {
        for b in bs.iter() {
            let (commit, id, source) = (
                crate::strings(b, "commit_id"),
                crate::strings(b, "episode_id"),
                crate::strings(b, "source"),
            );
            let at = crate::timestamps(b, "occurred_at");
            let recorded = crate::timestamps(b, "recorded_at");
            let about = crate::optional_strings(b, "about");
            for i in 0..b.num_rows() {
                if !vis.committed.contains(&commit[i]) || !vis.can_see_episode(&id[i], who) {
                    continue;
                }
                out.entry(id[i].clone()).or_insert_with(|| Ep {
                    id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: at[i].unwrap_or_default(),
                    recorded_at: recorded[i].unwrap_or_default(),
                    about: crate::split_list(about[i].as_deref().unwrap_or("")),
                });
            }
        }
    }
    out.into_values().collect()
}

/// Browsing a [`Store`]'s scopes (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreBrowse {
    async fn scopes(&self) -> Result<Vec<Scope>>;
}

impl StoreBrowse for Store {
    /// Every scope in the catalog, from its namespaces (see [`Scope::namespace`]); org and team names hold no
    /// `__`, so each namespace names one scope.
    async fn scopes(&self) -> Result<Vec<Scope>> {
        let mut out: Vec<Scope> = self
            .catalog()
            .list_namespaces(None)
            .await?
            .into_iter()
            .filter_map(|n| scope_of(&n.to_url_string()))
            .collect();
        out.sort();
        Ok(out)
    }
}

fn scope_of(ns: &str) -> Option<Scope> {
    let s = match ns.strip_prefix("memory_")? {
        "org" => Scope::Org,
        rest => match (
            rest.strip_prefix("org_"),
            rest.strip_prefix("team_"),
            rest.strip_prefix("user_"),
        ) {
            (Some(o), _, _) => match o.split_once("__team_") {
                Some((org, team)) => Scope::team(org, team).ok()?,
                None => Scope::org(o).ok()?,
            },
            (_, Some(t), _) => Scope::Team(t.into()),
            (_, _, Some(u)) => Scope::User(u.into()),
            _ => return None,
        },
    };
    (s.namespace() == ns).then_some(s)
}
