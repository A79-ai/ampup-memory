//! The dream's decisions as an org's builders review them (`/admin` ops, an org admin's, beside owner review in
//! [`super::governance`]). The dream applies what it is sure of and records each decision ([`crate::dream`]); a
//! builder sees what it did and why, and takes back any one of them:
//!
//! - `dream_decisions`: the scope's decisions, newest first, in pages (`limit`, `after`), filtered by `kind` (`end`,
//!   `link`, `name`, `share`), `status` (`applied`, `waiting`, `undone`, `dismissed`, `released`), `entity` (those
//!   resting on a fact about it) and `since` (decided then or later, µs): what each did in plain words, the facts it
//!   rests on (only decisions whose facts the caller may read), the model's reason where one was kept, and who
//!   reviewed it; `total` is how many the filters keep, `counts` the scope's in all (and those applied in the last
//!   seven days). With it, the newest pass that decided something (`last_pass`), when a pass last ran at all
//!   (`last_run`: its last commit in the ledger, a change or not, and the pass it was), and whether one is running.
//! - `undo_dream_decision`: takes back one applied decision: an ended fact is current again, a link or a name is
//!   gone, a team share is the people's own again. `dismiss_dream_decision` does the same for one waiting.
//!   `apply_dream_decision` applies one waiting (or one undone or dismissed before, or one the dream released once
//!   what it rests on is readable by its audience again: no pass decides it again, its parts being weighed). A
//!   released share stays released: what it rests on was forgotten or deleted.
//! - `dream_now`: a pass over the scope now, within the dream's budget (`AMPUP_MEMORY_DREAM_CALLS`) and under its scope
//!   lock, in the background.
//!
//! A decision applies to whoever may read its record (an ending's closure and a name's facts cite it, a link is
//! it), so each of these is an ACL change of that one record: to its audience, or to no one ([`FORGOTTEN`]). Each is
//! recorded as a review ([`dream::REVIEWED`]: the person, the action, and what no pass may decide again), committed
//! in the person's name, and the next pass reads it: it never makes again a decision someone undid or dismissed.

use std::sync::{LazyLock, Mutex};

use super::*;
use crate::dream::{self, ALIASES, ENDS, REVIEW_SOURCE, REVIEWED, WAITING_ACL, WEIGHED_ACL};
use crate::{AclTarget, DREAM_SAME, EPISODES, Episode, FACTS, FORGOTTEN, Fact};

/// Decision records one list reads at most, the newest first.
const MAX_READ: usize = 2_000;
/// What a decision's facts show of a value.
const VALUE_CHARS: usize = 300;
/// How far back from the newest decision of a record that names no pass the rest of its pass reach (µs).
const UNNAMED_PASS: i64 = 3_600_000_000;
/// The week `applied_week` counts (µs).
const WEEK_MICROS: i64 = 7 * 86_400_000_000;

/// The scopes a `dream_now` pass is running over in this process, and the last such pass that failed.
static RUNNING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static FAILED: LazyLock<Mutex<HashMap<String, (i64, String)>>> = LazyLock::new(Default::default);

pub(super) async fn handle(
    state: &Arc<State>,
    store: &Store,
    who: &Principal,
    via: &SignIn,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::{Directory, Invalid, Refused};
    let org = req["org"].as_str().unwrap_or_default();
    crate::check_name("org", org).map_err(|e| Invalid(e.to_string()))?;
    // The list is decided by the directory requests are signed in by (`Directory::behind`); a review, by the
    // directory at most the max staleness old.
    let dir = match req["op"] == "dream_decisions" {
        true => Directory::behind(store).await?,
        false => Directory::load(store).await?,
    };
    crate::phases::mark("dream directory");
    if !dir.is_admin(who.user(), via, org) {
        return Err(Refused(format!(
            "{} may not review org {org}'s dream (only its admins may)",
            who.user()
        ))
        .into());
    }
    let scope = dir.resolve(
        who.user(),
        via,
        Some(org),
        req["team"].as_str().filter(|t| !t.is_empty()),
    )?;
    let reader = as_member(who, org);
    let id = req["id"].as_str().unwrap_or_default();
    let result = match req["op"].as_str().unwrap_or_default() {
        "dream_decisions" => list(store, &scope, &reader, req).await,
        "undo_dream_decision" => review(store, &scope, who, &reader, id, Action::Undo).await,
        "dismiss_dream_decision" => review(store, &scope, who, &reader, id, Action::Dismiss).await,
        "apply_dream_decision" => review(store, &scope, who, &reader, id, Action::Apply).await,
        "dream_now" => now(state, store, &scope, who).await,
        _ => unreachable!(),
    };
    // What a review changed shows on the next list, its maker's first of all: also one that failed part way.
    if req["op"] != "dream_decisions" {
        crate::kept_answers::changed(&scope.namespace());
    }
    Ok(json!({"org": org, "scope": scope.to_string(), "result": result?}))
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Undo,
    Dismiss,
    Apply,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Action::Undo => "undo",
            Action::Dismiss => "dismiss",
            Action::Apply => "apply",
        }
    }
}

/// Where a decision stands, from its record's ACL and its newest review. The words are its wire status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Its record waits for review ([`WAITING_ACL`]).
    Waiting,
    /// It stands: its record applies to its audience.
    Applied,
    /// Its record applies to no one, taken back by a review: `undo`, `dismiss`, or by the dream itself (`released`,
    /// once what it rests on is gone for its audience).
    Undone,
    Dismissed,
    Released,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::Applied => "applied",
            Status::Undone => "undone",
            Status::Dismissed => "dismissed",
            Status::Released => "released",
        }
    }

    /// Whether a reviewer may take `action` on a decision standing here. A released one may be applied again: no
    /// pass decides it again, its parts being weighed.
    fn allows(self, action: Action) -> bool {
        match self {
            Status::Waiting => matches!(action, Action::Apply | Action::Dismiss),
            Status::Applied => action == Action::Undo,
            Status::Undone | Status::Dismissed | Status::Released => action == Action::Apply,
        }
    }

    /// Never applied: what it would do rather than what it did.
    fn unapplied(self) -> bool {
        matches!(self, Status::Waiting | Status::Dismissed)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One decision as the page shows it.
#[derive(Clone)]
struct Decision {
    /// Its record (a team share: the shared note).
    id: String,
    /// What its review is filed under: the record's key, or `keep/shared/<note>`.
    key: String,
    kind: &'static str,
    status: Status,
    /// Who it applies to once applied.
    audience: String,
    /// When it was decided, and the pass it was decided in (none for a share, or a record from before passes were
    /// named).
    at: i64,
    pass: Option<i64>,
    by: String,
    /// The facts it rests on, each with its part in it, and whether the caller may read them all.
    facts: Vec<(&'static str, Fact, bool)>,
    readable: bool,
    /// A shared note's words, while it is shared.
    text: Option<String>,
    /// What keeps a pass from deciding it again ([`dream::block_of`]).
    block: Option<String>,
    review: Option<Value>,
}

impl Decision {
    /// What may be done with it now: as its status allows, except that a released share stays released. A share is
    /// released when what it rests on was forgotten or deleted, and applying one isn't checked against its parts.
    fn allows(&self, action: Action) -> bool {
        self.status.allows(action) && !(self.status == Status::Released && self.kind == "share")
    }
}

/// What a list of the dream's decisions reads, as one reader reads it at one memory state.
struct Listed {
    /// The decisions the reader may read, newest first.
    decisions: Vec<Decision>,
    /// When a pass last ran, as the ledger has it ([`dream::last_run`]).
    last_run: Option<(i64, Option<i64>)>,
    /// The model's reason for each of them where one was kept, by decision.
    reasons: HashMap<String, String>,
}

/// Each reader's decisions, by memory state ([`crate::kept_answers`]): reading them reads every episode and fact
/// of the scope. Only what the memory holds: whether a pass runs here now, and the counts that depend on the time,
/// are worked out on each list.
static LISTED: LazyLock<crate::kept_answers::Kept<Listed>> =
    LazyLock::new(|| crate::kept_answers::Kept::new(200));

async fn listed(store: &Store, scope: &Scope, reader: &Principal) -> Result<Listed> {
    let started = std::time::Instant::now();
    let read = decisions(store, scope, reader, false).await?;
    let decided = started.elapsed();
    let last_run = dream::last_run(store, scope, &read.vis, &read.eps).await?;
    let ran = started.elapsed();
    let decisions: Vec<Decision> = read.decisions.into_iter().filter(|d| d.readable).collect();
    let ids: HashSet<&str> = decisions.iter().map(|d| d.id.as_str()).collect();
    // Through the last visibility state, as the decisions were read: a reason goes with what it cites. Only the
    // reasons are copied out of the log, never the rest of it (a GB in a large org).
    let reasons: HashMap<String, String> = crate::qa_log::list_where(store, scope, None, |l| {
        l.kind.as_deref() == Some(dream::REASON_KIND) && ids.contains(l.answer_id.as_str())
    })
    .await?
    .into_iter()
    .filter_map(|l| l.answer.map(|a| (l.answer_id, a)))
    .collect();
    // Worked out away from the request that asked (a kept answer's work), so its steps are said here when slow.
    if started.elapsed() > std::time::Duration::from_secs(1) {
        let ms = |d: std::time::Duration| d.as_millis();
        eprintln!(
            "slow dream list: {scope} in {} ms ({} decisions; episodes {}, bodies {}, facts {}, last run {}, reasons {})",
            ms(started.elapsed()),
            decisions.len(),
            ms(read.took[0]),
            ms(read.took[1]),
            ms(read.took[2]),
            ms(ran - decided),
            ms(started.elapsed() - ran)
        );
    }
    Ok(Listed {
        decisions,
        last_run,
        reasons,
    })
}

/// Bodies of the dream's records and of the reviews of them, per scope, by episode id. An episode's id is its body's
/// hash, so a body read once is the same ever after: a list reads from storage only the records new since the last.
/// Only these, the engine's own JSON naming facts by id: a team share's body (a person's words) is read for the
/// page that shows it, each time.
static BODIES: LazyLock<Mutex<HashMap<String, Bodies>>> = LazyLock::new(Default::default);

/// Episode bodies by id.
type Bodies = HashMap<String, Arc<Vec<u8>>>;

/// The bodies of `want` (records and reviews, see [`BODIES`]): held ones as held, the rest read now. Only `want`
/// stays held, so what a list no longer shows is dropped.
async fn record_bodies(store: &Store, scope: &Scope, want: &[String]) -> Result<Bodies> {
    let key = scope.namespace();
    let missing: Vec<String> = {
        let all = BODIES.lock().unwrap();
        let held = all.get(&key);
        want.iter()
            .filter(|id| held.is_none_or(|h| !h.contains_key(*id)))
            .cloned()
            .collect()
    };
    let mut read = HashMap::new();
    for chunk in missing.chunks(256) {
        read.extend(store.episode_bodies(scope, chunk.to_vec()).await?);
    }
    let wanted: HashSet<&str> = want.iter().map(String::as_str).collect();
    let mut all = BODIES.lock().unwrap();
    let held = all.entry(key).or_default();
    held.extend(read.into_iter().map(|(id, b)| (id, Arc::new(b))));
    held.retain(|id, _| wanted.contains(id.as_str()));
    Ok(held.iter().map(|(id, b)| (id.clone(), b.clone())).collect())
}

/// Of a facts batch, the rows a list needs: facts named by id (`ids`), or citing one of `records` (a name a dream
/// gave cites its record). Read from the columns as stored, so only those rows are decoded; None: no row.
fn rows_wanted(
    b: &arrow_array::RecordBatch,
    ids: &HashSet<String>,
    records: &HashSet<&str>,
) -> Option<arrow_array::BooleanArray> {
    use arrow_array::{Array, cast::AsArray};
    let col = |n: &str| b.column_by_name(n).and_then(|c| c.as_string_opt::<i32>());
    let (Some(fid), Some(src)) = (col("fact_id"), col("source_episodes")) else {
        // Stored some other way: every row is decoded, as before.
        return Some(arrow_array::BooleanArray::from(vec![true; b.num_rows()]));
    };
    let mask: arrow_array::BooleanArray = (0..b.num_rows())
        .map(|i| {
            Some(
                (fid.is_valid(i) && ids.contains(fid.value(i)))
                    || (src.is_valid(i) && src.value(i).split(',').any(|e| records.contains(e))),
            )
        })
        .collect();
    (mask.true_count() > 0).then_some(mask)
}

/// The team's shares: a note enough people said, and a fact enough people's own memories showed.
fn is_share(source: &str) -> bool {
    source
        .strip_prefix(crate::REMEMBER_SOURCE)
        .and_then(|s| s.strip_prefix(':'))
        == Some(crate::origin::SHARED)
        || crate::facts_share::is_source(source)
}

/// Whether an episode is a dream's record of a decision: an ending, a link or a name.
fn decided(e: &Episode) -> bool {
    e.document
        .as_deref()
        .is_some_and(|d| d.starts_with(ENDS) || d.starts_with(DREAM_SAME) || d.starts_with(ALIASES))
        && e.source.starts_with(dream::SOURCE)
        && e.source != REVIEW_SOURCE
}

/// Of a scope's current episodes, the decisions' records (the newest [`MAX_READ`], each document's newest
/// version), the newest review of each decision (by what it reviewed: when, and its episode), and the episodes
/// whose bodies a list reads ([`record_bodies`]: every record's but a share's, and every review's). Whoever reads.
#[allow(clippy::type_complexity)]
fn records_of(
    eps: &HashMap<String, Episode>,
) -> (Vec<&Episode>, HashMap<String, (i64, String)>, Vec<String>) {
    let mut records: Vec<&Episode> = eps
        .values()
        .filter(|e| decided(e) || is_share(&e.source))
        .collect();
    records.sort_by_key(|e| std::cmp::Reverse(e.recorded_at.unwrap_or_default()));
    records.truncate(MAX_READ);
    // Each document's newest version: an older one was released (what it rested on went) before this was decided.
    let mut seen: HashSet<&str> = HashSet::new();
    records.retain(|e| e.document.as_deref().is_none_or(|d| seen.insert(d)));
    let mut reviews: HashMap<String, (i64, String)> = HashMap::new();
    for e in eps.values().filter(|e| e.source == REVIEW_SOURCE) {
        if let Some(k) = e.document.as_deref().and_then(|d| d.strip_prefix(REVIEWED)) {
            let at = e.recorded_at.unwrap_or_default();
            let r = reviews
                .entry(k.to_string())
                .or_insert((at, e.episode_id.clone()));
            if at > r.0 {
                *r = (at, e.episode_id.clone());
            }
        }
    }
    let mut want: Vec<String> = records
        .iter()
        .filter(|e| !is_share(&e.source))
        .map(|e| e.episode_id.clone())
        .collect();
    want.extend(reviews.values().map(|(_, id)| id.clone()));
    (records, reviews, want)
}

/// Read what the first list of `scope`'s decisions would read from storage whoever asks (Studio's Home asks for one
/// at every open): the bodies of the dream's records and of the reviews of them, each a read of its own. How many.
pub(super) async fn warm(store: &Store, scope: &Scope) -> Result<usize> {
    let vis = store.lagging_visibility(scope, MAIN).await?;
    let eps = store
        .current_episodes_seen_where(scope, &vis, dream_episode)
        .await?;
    let (_, _, want) = records_of(&eps);
    Ok(record_bodies(store, scope, &want).await?.len())
}

/// What one read of the decisions found, with the state it read them in, and how long it took to read the episodes,
/// the bodies and the facts.
struct Read {
    decisions: Vec<Decision>,
    vis: Arc<crate::Vis>,
    /// The dream's own episodes ([`dream_episode`]): its records, the reviews of them and its weighed records.
    eps: HashMap<String, Episode>,
    took: [std::time::Duration; 3],
}

/// Whether an episode is one a list of the decisions reads, by its source and document: a decision's record
/// ([`decided`]) or a team share's, a review of one, or a pass's weighed record (what [`dream::last_run`] reads).
fn dream_episode(source: &str, document: Option<&str>) -> bool {
    let doc = document.unwrap_or_default();
    (source.starts_with(dream::SOURCE)
        && source != REVIEW_SOURCE
        && [ENDS, DREAM_SAME, ALIASES]
            .iter()
            .any(|p| doc.starts_with(p)))
        || is_share(source)
        || source == REVIEW_SOURCE
        || (dream::pass_actor(source) && doc.starts_with(dream::WEIGHED))
}

/// Every decision of `scope`, newest first, read for `reader`: dream records of an ending, a link or a name (the
/// newest version of each), and the notes the server shared with the team ([`crate::keep::share`]). `fresh`: read
/// from the tables as they are now, for an undo, apply or dismiss, which acts on what it finds. Otherwise, for the
/// page's list, main's last visibility state while the next is built ([`Store::lagging_visibility`]: never one
/// holding what was taken away since) and the tables as this server holds them: the list never waits for another
/// server's write, nor makes every reader here read the tables again. A share's words aren't read here
/// ([`share_texts`]).
async fn decisions(store: &Store, scope: &Scope, reader: &Principal, fresh: bool) -> Result<Read> {
    let ns = scope.namespace();
    let started = std::time::Instant::now();
    let (vis, eps) = match fresh {
        true => {
            store.invalidate(&ns, EPISODES);
            store.invalidate(&ns, FACTS);
            let eps = store.current_episodes(scope).await?;
            (store.visibility(scope, MAIN).await?, eps)
        }
        // Only the dream's episodes decoded: the rest of a large scope's (hundreds of thousands) were decoded and
        // dropped on every list.
        false => {
            let vis = store.lagging_visibility(scope, MAIN).await?;
            let eps = store
                .current_episodes_seen_where(scope, &vis, dream_episode)
                .await?;
            (vis, eps)
        }
    };
    crate::phases::mark("dream episodes");
    let episodes_took = started.elapsed();
    let (records, reviews, want) = records_of(&eps);
    let bodies = record_bodies(store, scope, &want).await?;
    crate::phases::mark("dream bodies");
    let bodies_took = started.elapsed() - episodes_took;
    let json_of = |id: &str| -> Value {
        bodies
            .get(id)
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null)
    };
    // The facts they rest on: by id, and each name a dream gave, by the record it cites.
    let mut wanted: HashSet<String> = HashSet::new();
    let ids: HashSet<&str> = records.iter().map(|e| e.episode_id.as_str()).collect();
    for e in &records {
        let b = json_of(&e.episode_id);
        for k in ["old", "by", "keep", "dup"] {
            if let Some(f) = b[k].as_str() {
                wanted.insert(f.to_string());
            }
        }
    }
    let mut facts: HashMap<String, (String, Fact)> = HashMap::new();
    let mut named: HashMap<String, (String, Fact)> = HashMap::new();
    for b in store.rows(&ns, FACTS).await?.rows.iter() {
        let Some(mask) = rows_wanted(b, &wanted, &ids) else {
            continue;
        };
        let b = arrow_select::filter::filter_record_batch(b, &mask)?;
        for (c, f) in crate::decode_facts(&b, "source_episodes") {
            if !vis.committed.contains(&c) {
                continue;
            }
            if let Some(r) = f.source_episodes.iter().find(|e| ids.contains(e.as_str())) {
                named.insert(r.clone(), (c.clone(), f.clone()));
            }
            if wanted.contains(&f.fact_id) {
                facts.insert(f.fact_id.clone(), (c, f));
            }
        }
    }
    crate::phases::mark("dream facts");
    let facts_took = started.elapsed() - episodes_took - bodies_took;
    // A fact the caller may read: committed, not retracted, and every episode it comes from theirs to read (a
    // dream's record aside: it is the decision, not its evidence).
    let readable = |c: &str, f: &Fact| {
        vis.committed.contains(c)
            && !vis.withheld(f)
            && f.source_episodes
                .iter()
                .all(|e| eps.get(e).is_some_and(decided) || vis.can_see_episode(e, reader))
    };
    let mut out = vec![];
    for e in records {
        let b = json_of(&e.episode_id);
        let acl = &e.acl;
        let gone = !acl.is_empty() && acl.iter().all(|a| a == FORGOTTEN);
        let waiting = acl.iter().any(|a| a == WAITING_ACL);
        let (kind, key, mut parts): (&str, String, Vec<(&'static str, &str)>) =
            if is_share(&e.source) {
                (
                    "share",
                    format!("{}{}", crate::keep::SHARED_KEY, e.episode_id),
                    vec![],
                )
            } else {
                let key = e.document.clone().unwrap_or_default();
                if key.starts_with(ENDS) {
                    ("end", key, vec![("old", "old"), ("by", "by")])
                } else if key.starts_with(DREAM_SAME) {
                    ("link", key, vec![("keep", "keep"), ("dup", "dup")])
                } else {
                    ("name", key, vec![])
                }
            };
        let mut fs: Vec<(&'static str, Fact, bool)> = vec![];
        for (role, field) in parts.drain(..) {
            if let Some((c, f)) = b[field].as_str().and_then(|id| facts.get(id)) {
                fs.push((role, f.clone(), readable(c, f)));
            }
        }
        if kind == "name"
            && let Some((c, f)) = named.get(&e.episode_id)
        {
            fs.push((
                if f.predicate == crate::query::schema::NAME {
                    "name"
                } else {
                    "alias"
                },
                f.clone(),
                readable(c, f),
            ));
        }
        let all_read = match kind {
            // A share is the team's (`*`): any member reads it while it stands.
            "share" => true,
            "end" | "link" => fs.len() == 2 && fs.iter().all(|(_, _, r)| *r),
            _ => fs.len() == 1 && fs[0].2,
        };
        let review = reviews
            .get(&key)
            .map(|(_, id)| json_of(id))
            .filter(|r| !r.is_null());
        let action = review.as_ref().and_then(|r| r["action"].as_str());
        let status = if waiting {
            Status::Waiting
        } else if gone {
            match action {
                Some("undo") => Status::Undone,
                Some("dismiss") => Status::Dismissed,
                _ => Status::Released,
            }
        } else {
            Status::Applied
        };
        let alias = fs.iter().find(|(r, _, _)| *r == "alias" || *r == "name");
        out.push(Decision {
            id: e.episode_id.clone(),
            block: match kind {
                "share" => Some(key.clone()),
                _ => dream::block_of(&key, alias.map(|(_, f, _)| f)),
            },
            key,
            kind,
            status,
            audience: match kind {
                "share" => "*".into(),
                _ => b["audience"].as_str().unwrap_or_default().to_string(),
            },
            at: b["at"]
                .as_i64()
                .unwrap_or_else(|| e.recorded_at.unwrap_or_default()),
            pass: b["pass"].as_i64(),
            by: e.source.clone(),
            facts: fs,
            readable: all_read,
            text: None,
            review,
        });
    }
    out.sort_by_key(|d| std::cmp::Reverse(d.at));
    Ok(Read {
        decisions: out,
        vis,
        eps,
        took: [episodes_took, bodies_took, facts_took],
    })
}

/// The words of each team share in `page` that is still shared, read from storage for the page that shows them.
async fn share_texts(
    store: &Store,
    scope: &Scope,
    page: &[&Decision],
) -> Result<HashMap<String, String>> {
    let ids: Vec<String> = page
        .iter()
        .filter(|d| d.kind == "share" && d.status == Status::Applied)
        .map(|d| d.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(store
        .episode_bodies(scope, ids)
        .await?
        .into_iter()
        .map(|(id, b)| {
            let text = String::from_utf8_lossy(&b)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (id, text)
        })
        .collect())
}

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.chars().count() > VALUE_CHARS {
        true => format!("{}…", s.chars().take(VALUE_CHARS).collect::<String>()),
        false => s,
    }
}

/// What a decision did (or, waiting, would do), in plain words.
fn summary(d: &Decision) -> String {
    let fact = |role: &str| {
        d.facts
            .iter()
            .find(|(r, _, _)| *r == role)
            .map(|(_, f, _)| f)
    };
    // A thing as people name it: its id without its type (`project:billing-export`: billing-export).
    let thing = |f: &Fact| {
        f.entity
            .split_once(':')
            .map_or(f.entity.as_str(), |(_, n)| n)
            .replace(['_', '-'], " ")
    };
    let pred = |f: &Fact| f.predicate.replace('_', " ");
    // One never applied (waiting, or dismissed) says what it would do.
    let waiting = d.status.unapplied();
    match d.kind {
        "end" => match (fact("old"), fact("by")) {
            (Some(o), Some(b)) => format!(
                "{} {}'s {} \"{}\": a newer fact says \"{}\".",
                if waiting { "Would end" } else { "Ended" },
                thing(o),
                pred(o),
                clip(&o.value),
                clip(&b.value)
            ),
            _ => "Ended a fact a newer one replaced.".into(),
        },
        "link" => match (fact("keep"), fact("dup")) {
            (Some(k), Some(_)) => format!(
                "These two facts about {} say the same thing, so the dream {} them: agents are served one.",
                thing(k),
                if waiting { "would link" } else { "linked" }
            ),
            _ => "Linked two facts that say the same thing.".into(),
        },
        "name" => match (fact("alias"), fact("name")) {
            (Some(f), _) => format!(
                "{} {} also \"{}\": one thing under two names, so a search for either finds it.",
                if waiting { "Would name" } else { "Named" },
                thing(f),
                clip(&f.value)
            ),
            (None, Some(f)) => format!(
                "{} {} the name \"{}\", so the other names it goes by lead to it.",
                if waiting { "Would give" } else { "Gave" },
                f.entity,
                clip(&f.value)
            ),
            _ => "Gave a thing another name it goes by.".into(),
        },
        _ => match &d.text {
            Some(t) => format!("Shared with the whole team: \"{}\"", clip(t)),
            None => "Shared a note with the whole team.".into(),
        },
    }
}

fn view(d: &Decision, why: Option<&String>, shared_at: usize) -> Value {
    let facts: Vec<Value> = d
        .facts
        .iter()
        .map(|(role, f, _)| {
            json!({
                "role": role, "id": f.fact_id, "entity": f.entity, "predicate": f.predicate,
                "value": clip(&f.value), "valid_from": f.valid_from,
            })
        })
        .collect();
    let why = why.cloned().or_else(|| match d.kind {
        "share" => Some(format!(
            "At least {shared_at} people said the same, each in their own memory"
        )),
        _ => None,
    });
    json!({
        "id": d.id,
        "kind": d.kind,
        "status": d.status.as_str(),
        "summary": summary(d),
        "why": why,
        "at": d.at,
        "pass": d.pass,
        "by": d.by,
        "audience": d.audience,
        "facts": facts,
        "reviewed": d.review.as_ref().map(|r| json!({"action": r["action"], "by": r["by"], "at": r["at"]})),
        "can_undo": d.allows(Action::Undo),
        "can_apply": d.allows(Action::Apply),
        "can_dismiss": d.allows(Action::Dismiss),
    })
}

async fn list(store: &Store, scope: &Scope, reader: &Principal, req: &Value) -> Result<Value> {
    use crate::people::directory::Invalid;
    let kind = req["kind"].as_str().filter(|k| !k.is_empty());
    if let Some(k) = kind
        && !["end", "link", "name", "share"].contains(&k)
    {
        return Err(Invalid("kind is end, link, name or share".into()).into());
    }
    let statuses: Vec<&str> = match &req["status"] {
        Value::Null => vec![],
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Err(Invalid("status is a status or a list of them".into()).into()),
    };
    let entity = req["entity"].as_str().filter(|e| !e.is_empty());
    let since = match &req["since"] {
        Value::Null => None,
        v => Some(
            v.as_i64()
                .ok_or_else(|| Invalid("since is a time in microseconds".into()))?,
        ),
    };
    let limit = req["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    let vis = store.lagging_visibility(scope, MAIN).await?;
    crate::phases::mark("dream state");
    let kept = LISTED
        .get(
            &scope.namespace(),
            &crate::kept_answers::reader_key(reader),
            crate::kept_answers::At::of(mark, &vis),
            true,
            || {
                let (store, scope, reader) = (store.clone(), scope.clone(), reader.clone());
                async move { listed(&store, &scope, &reader).await }
            },
        )
        .await?;
    crate::phases::mark("dream decisions kept");
    let all = &kept.decisions;
    // The newest pass that decided something, for the banner: the decisions that name it. Records from before
    // passes were named name none: theirs are those decided within an hour of the newest of them (the gate keeps
    // passes a day apart), a pass being minutes.
    let dreamt: Vec<&Decision> = all.iter().filter(|d| d.kind != "share").collect();
    let newest = dreamt.iter().filter_map(|d| d.pass).max();
    let unnamed = dreamt
        .iter()
        .filter(|d| d.pass.is_none())
        .map(|d| d.at)
        .max();
    let of_last: Vec<&Decision> = match (newest, unnamed) {
        (Some(p), _) => dreamt.into_iter().filter(|d| d.pass == Some(p)).collect(),
        (None, Some(t)) => dreamt
            .into_iter()
            .filter(|d| d.pass.is_none() && d.at > t - UNNAMED_PASS)
            .collect(),
        (None, None) => vec![],
    };
    let in_last = (!of_last.is_empty()).then(|| {
        let n = |k: &str| {
            of_last
                .iter()
                .filter(|d| d.kind == k && d.status != Status::Waiting)
                .count()
        };
        json!({
            "at": newest.unwrap_or_else(|| of_last.iter().map(|d| d.at).min().unwrap_or_default()),
            "ended": n("end"),
            "linked": n("link"),
            "named": n("name"),
            "waiting": of_last.iter().filter(|d| d.status == Status::Waiting).count(),
        })
    });
    // When a pass last ran at all, a change or not, and which: its last commit in the ledger, or this process's last
    // pass where that is newer (one that weighed nothing commits nothing; what it did is `dreaming.last`).
    let here = dream::last_pass(scope);
    let ran = match (kept.last_run, &here) {
        (Some((at, _)), Some((t, _))) if *t > at => Some(json!({"at": t, "pass": null})),
        (Some((at, pass)), _) => Some(json!({"at": at, "pass": pass})),
        (None, Some((t, _))) => Some(json!({"at": t, "pass": null})),
        (None, None) => None,
    };
    let week = crate::now_micros() - WEEK_MICROS;
    let counts = json!({
        "waiting": all.iter().filter(|d| d.status == Status::Waiting).count(),
        "applied": all.iter().filter(|d| d.status == Status::Applied).count(),
        "applied_week": all.iter().filter(|d| d.status == Status::Applied && d.at >= week).count(),
        "history": all.iter().filter(|d| !matches!(d.status, Status::Waiting | Status::Applied)).count(),
    });
    let shown: Vec<&Decision> = all
        .iter()
        .filter(|d| kind.is_none_or(|k| d.kind == k))
        .filter(|d| statuses.is_empty() || statuses.contains(&d.status.as_str()))
        .filter(|d| entity.is_none_or(|e| d.facts.iter().any(|(_, f, _)| f.entity == e)))
        .filter(|d| since.is_none_or(|t| d.at >= t))
        .collect();
    let start = match req["after"].as_str().filter(|a| !a.is_empty()) {
        None => 0,
        Some(a) => {
            shown
                .iter()
                .position(|d| d.id == a)
                .ok_or_else(|| Invalid("unknown page cursor".into()))?
                + 1
        }
    };
    let page = &shown[start.min(shown.len())..(start + limit).min(shown.len())];
    let next = (start + limit < shown.len()).then(|| page.last().map(|d| d.id.clone()));
    let total = shown.len();
    let mut texts = share_texts(store, scope, page).await?;
    crate::phases::mark("dream shares read");
    let page: Vec<Decision> = page
        .iter()
        .map(|d| Decision {
            text: texts.remove(&d.id),
            ..(*d).clone()
        })
        .collect();
    let reasons = &kept.reasons;
    let shared_at = crate::keep::people_in(store, scope).await;
    crate::phases::mark("dream people");
    Ok(json!({
        "decisions": page.iter().map(|d| view(d, reasons.get(&d.id), shared_at)).collect::<Vec<_>>(),
        "next_page": next.flatten(),
        "total": total,
        "counts": counts,
        "last_pass": in_last,
        "last_run": ran,
        "scope": scope.to_string(),
        "dreaming": dreaming(scope, here),
    }))
}

/// What this process knows of `scope`'s dream, not its memory: whether it can dream, whether a pass runs here, the
/// last pass here (`here`, [`dream::last_pass`]) and the last that failed here.
fn dreaming(scope: &Scope, here: Option<(i64, Value)>) -> Value {
    let key = scope.to_string();
    let available = dream::internal_scheduler().unwrap_or(false)
        && crate::extract::Extractor::dream_from_env()
            .ok()
            .flatten()
            .is_some();
    json!({
        "available": available,
        "running": RUNNING.lock().unwrap().contains(&key),
        "last": here.map(|(at, counts)| json!({"at": at, "counts": counts})),
        "failed": FAILED.lock().unwrap().get(&key).map(|(at, e)| json!({"at": at, "error": e})),
    })
}

/// A decisions list `list` worked out by another server ([`crate::shared_answers`]) as this one would answer it:
/// its `dreaming` this process's, and its last run this process's last pass where that is newer.
pub(super) fn as_here(scope: &Scope, list: &mut Value) {
    if !list.is_object() {
        return;
    }
    let here = dream::last_pass(scope);
    if let Some((t, _)) = &here
        && list["last_run"]["at"].as_i64().is_none_or(|at| *t > at)
    {
        list["last_run"] = json!({"at": t, "pass": null});
    }
    list["dreaming"] = dreaming(scope, here);
}

/// Apply, undo or dismiss one decision: its record's ACL to its audience or to no one, and a review recorded beside
/// it (in the order below, so a failure between the two leaves the decision as it was).
async fn review(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    reader: &Principal,
    id: &str,
    action: Action,
) -> Result<Value> {
    use crate::people::directory::{Invalid, Refused};
    if id.is_empty() {
        return Err(Invalid("id names the decision".into()).into());
    }
    let read = decisions(store, scope, reader, true).await?;
    let all = read.decisions;
    let Some(d) = all.iter().find(|d| d.id == id && d.readable) else {
        return Err(Refused(format!(
            "no decision {id:?} in {scope} whose facts you may read"
        ))
        .into());
    };
    if !d.allows(action) {
        return Err(Refused(format!(
            "this decision is {}: it can't be {}",
            d.status,
            match action {
                Action::Undo => "undone",
                Action::Dismiss => "dismissed",
                Action::Apply => "applied",
            }
        ))
        .into());
    }
    // Applying one needs what it rests on, still readable by its audience.
    if action == Action::Apply && d.kind != "share" {
        let audience = crate::extract::audience(std::slice::from_ref(&d.audience))
            .pop()
            .ok_or_else(|| Refused("its audience is no longer one".into()))?;
        let vis = store.visibility(scope, MAIN).await?;
        let shown = d.facts.iter().all(|(_, f, _)| {
            f.source_episodes
                .iter()
                .filter(|e| **e != d.id)
                .all(|e| vis.can_see_episode(e, &audience))
        });
        if !shown || d.facts.is_empty() {
            return Err(Refused(
                "what it rests on is gone for its audience: it can't be applied".into(),
            )
            .into());
        }
    }
    let now = crate::now_micros();
    let body = json!({
        "action": action.word(), "decision": d.id, "key": d.key,
        "block": if action == Action::Apply { None } else { d.block.clone() },
        "by": who.user(), "at": now,
    });
    let mut mark = Episode::sourced(REVIEW_SOURCE, body.to_string().as_bytes(), &[WEIGHED_ACL])
        .without_text_index()
        .version_of(&format!("{REVIEWED}{}", d.key));
    mark.occurred_at = now;
    let acl = match action {
        Action::Apply => vec![d.audience.clone()],
        _ => vec![FORGOTTEN.to_string()],
    };
    // Two commits, ordered so that one failing between them leaves the decision as it was, to be retried. A record
    // that applies to no one reads its status from its newest review (`undone`, `dismissed`, else `released`), so
    // taking one back records the review first: until its ACL changes the record still stands (`applied` or
    // `waiting`), and the review's block only keeps a pass from deciding again what it is taking back. Applying one
    // changes the ACL first: it then stands under its old review (`applied`), never an `apply` review over a record
    // that applies to no one, which reads as `released`.
    let reviewed = async {
        store
            .commit_derived(scope, who.user(), &[mark], &[], &[], &[])
            .await
    };
    // A record no one reads (waiting, or taken back) that its audience may read only once the owners of what it
    // rests on approve stays as it is: taking it from no one would leave it under `FORGOTTEN` alone with no review
    // to say why, which reads as `released`.
    let unread = read.eps.get(&d.id).is_some_and(|e| {
        !e.acl.is_empty() && e.acl.iter().all(|a| a == WAITING_ACL || a == FORGOTTEN)
    });
    let changed = async {
        let targets = [AclTarget::Episode(d.id.clone())];
        if !unread {
            return store
                .change_acl_all(scope, who.user(), &targets, &acl)
                .await
                .map(drop);
        }
        let (_, held) = store
            .change_acl_or_hold(scope, who.user(), &targets, &acl)
            .await?;
        match held.is_empty() {
            true => Ok(()),
            false => Err(
                anyhow::Error::new(crate::governance::OwnerApprovalRequired).context(format!(
                    "the owners of what it rests on must approve its audience reading it: it is still {}",
                    d.status
                )),
            ),
        }
    };
    if action == Action::Apply {
        changed.await?;
        reviewed
            .await
            .context("the decision applies, but who applied it wasn't recorded")?;
    } else {
        reviewed.await?;
        changed.await.with_context(|| {
            format!(
                "the decision is still {}: {} it again",
                d.status,
                action.word()
            )
        })?;
    }
    eprintln!(
        "dream review: {} {} a {} decision in {scope}",
        who.user(),
        match action {
            Action::Undo => "undid",
            Action::Dismiss => "dismissed",
            Action::Apply => "applied",
        },
        d.kind
    );
    let after = decisions(store, scope, reader, true).await?.decisions;
    let reasons: HashMap<String, String> = crate::qa_log::list(store, scope, None)
        .await?
        .into_iter()
        .filter(|l| l.kind.as_deref() == Some(dream::REASON_KIND) && l.answer_id == id)
        .filter_map(|l| l.answer.map(|a| (l.answer_id, a)))
        .collect();
    let shared_at = crate::keep::people_in(store, scope).await;
    Ok(after
        .iter()
        .find(|x| x.id == id)
        .map(|x| view(x, reasons.get(id), shared_at))
        .unwrap_or(json!({"id": id})))
}

/// A pass over `scope` now, in the background: within the dream's budget, under its scope lock (a pass already
/// running anywhere holds it, and this one waits its turn), only where this server runs passes itself.
async fn now(state: &Arc<State>, store: &Store, scope: &Scope, who: &Principal) -> Result<Value> {
    use crate::people::directory::Refused;
    if !dream::internal_scheduler()? {
        return Err(Refused(
            "here the dream runs as scheduled jobs: start a reconcile dream instead".into(),
        )
        .into());
    }
    let Some(ex) = crate::extract::Extractor::dream_from_env()? else {
        return Err(Refused("no dreaming model is configured (AMPUP_MEMORY_DREAM)".into()).into());
    };
    let budget = dream::Budget::from_env()?;
    let key = scope.to_string();
    if !RUNNING.lock().unwrap().insert(key.clone()) {
        return Ok(json!({"started": false, "running": true}));
    }
    FAILED.lock().unwrap().remove(&key);
    eprintln!("dream now: {} started a pass over {scope}", who.user());
    let (store, scope) = (store.clone(), scope.clone());
    state.background("dream", async move {
        let pass = dream::pass(&store, &scope, &ex, &budget, false);
        match crate::log_org::with(scope.org_name(), pass).await {
            Ok(r) => eprintln!("dream now over {scope}: {}", r.counts()),
            Err(e) => {
                eprintln!("dream now over {scope} failed: {e:#}");
                FAILED
                    .lock()
                    .unwrap()
                    .insert(key.clone(), (crate::now_micros(), writer::redacted(&e)));
            }
        }
        crate::kept_answers::changed(&scope.namespace());
        RUNNING.lock().unwrap().remove(&key);
    });
    Ok(json!({"started": true, "running": true}))
}
