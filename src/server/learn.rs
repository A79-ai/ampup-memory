//! Adding to memory by hand, and what it learned from it: the Studio's "Add to memory" (files dropped in, text pasted)
//! and "Remember this" on an Ask answer, before any source is connected.
//!
//! - `POST /learn {items, for?}` writes each item as an episode through the path `/remember` takes
//!   ([`super::commit_remembered`]: the caller's own memory by default, `for: "team"` everyone's who reads it; signed
//!   with who wrote it; personal identifiers redacted where the scope asks). A pasted text or a file is filed under
//!   [`crate::origin::LEARNED`], so it reads as the person's own statement, as a fact they add in Studio does, not as
//!   an agent's note; an Ask answer kept stays a note (its words are the model's). Each is filed as a starter
//!   document (`starter/added/...`), so extraction reads it wherever it reads anything ([`crate::extract::STARTER`]),
//!   even where it reads nothing else no goal names. The same text added again is the same episode, not a new one.
//!   Answers each item's episode id (`added`), and each item refused with why (`refused`), then wakes extraction
//!   through the ledger ([`crate::extract::ADDED_WAKE`]): it reads what was added within seconds.
//! - `POST /learn/status {episode_ids}` answers what extraction made of them so far: a receipt, per episode,
//!   whether it has been read (`pending`, `read`, `failed`, `not_read`: nothing here reads it, as
//!   [`crate::extract::reads`] decides, the scope's schema included), the facts it gave that the caller may
//!   read, their entities (and whether memory knew them before), and the facts it stated again that memory already
//!   held (where reconcile recorded the restatement: a source independent of the fact's own).
//!
//! An ingest key may use neither: the receipt reads memory. A queued memory (an inbox) refuses: what it would
//! learn comes at the next ingest, with nothing to show now.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use base64::Engine;
use serde_json::{Value, json};
use sha2::Digest;

use super::{State, ToolError};
use crate::search::{Col, Filter};
use crate::{Episode, Principal, Scope, Session, Store};

/// Most items one request adds.
const MAX_ITEMS: usize = 20;
/// Most episodes one status request asks about.
const MAX_STATUS: usize = 50;
/// Largest text one item may hold: what starter memory takes (and about what extraction reads of one episode).
const MAX_TEXT: usize = super::STARTER_MAX_BYTES;
/// What a file's name may end in to be read as text. Anything else is refused, saying what is read.
const TEXT_TYPES: [&str; 12] = [
    "txt", "text", "md", "markdown", "csv", "tsv", "json", "jsonl", "ndjson", "yaml", "yml", "log",
];
/// Where what is added is filed: a starter document per person (a hash of who added it) and item.
const DOCUMENTS: &str = crate::extract::ADDED;

/// Wakes the read of what was just added on this server (`crate::extract::run_added`), so it looks at the ledger now
/// rather than at its next tick.
pub(super) static WAKE: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// Whether this server extracts at all (a model configured, `AMPUP_MEMORY_EXTRACT` not `off`), as it started.
static EXTRACTS: LazyLock<bool> =
    LazyLock::new(|| crate::extract::Extractor::from_env().is_ok_and(|e| e.is_some()));

/// One item to add, read and checked.
struct Item {
    kind: &'static str,
    name: String,
    text: String,
}

/// `POST /learn`: add each item as an episode the caller wrote, in `scope`. Answers `{added: [{name, kind,
/// episode_id, source, bytes, new}], refused: [{name, reason}], scope, readers, extraction}`; `new` is false where
/// the same text was already there (the same episode). Refuses the whole request (a [`ToolError`]) only when it
/// isn't one: no items, too many, or `for` neither "me" nor "team".
pub(super) async fn add(
    state: &State,
    store: &Store,
    scope: &Scope,
    who: &Principal,
    args: &Value,
) -> Result<Value> {
    let items = args["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            ToolError(
                "body is {items: [{kind: file|paste|answer, name?, text? | base64? | question, answer}], for?}"
                    .into(),
            )
        })?;
    if items.len() > MAX_ITEMS {
        return Err(ToolError(format!("at most {MAX_ITEMS} items a request")).into());
    }
    let whose = match args["for"].as_str() {
        None | Some("me") => "me",
        Some("team") => "team",
        Some(other) => {
            return Err(ToolError(format!("for is \"me\" or \"team\", not {other:?}")).into());
        }
    };
    let author = hex(&sha2::Sha256::digest(who.user().as_bytes())[..4]);
    let (mut added, mut refused) = (vec![], vec![]);
    // Each kept, as extraction will see it, to say whether anything reads it.
    let mut kept: Vec<Episode> = vec![];
    let mut readers = String::new();
    let mut fresh = false;
    for raw in items {
        let item = match read(raw) {
            Ok(i) => i,
            Err(why) => {
                refused.push(json!({"name": item_name(raw), "reason": why}));
                continue;
            }
        };
        let (source, key) = filed(&item, &author);
        // Already here, as this caller sees it: the same episode, said so rather than written again.
        let before = store.document(scope, who, &key, None).await?;
        let mut call = json!({"source": source, "body": item.text, "document": key, "for": whose});
        call = super::remember_for(state, who, &call)?;
        let commit = match item.kind {
            "answer" => super::commit_remembered(state, store, scope, who, &call).await,
            _ => super::commit_learned(state, store, scope, who, &call).await,
        };
        match commit {
            Ok((id, r)) => {
                let new = before.as_ref().is_none_or(|(b, _, _)| *b != id);
                fresh |= new;
                readers = r;
                let mut ep = Episode::new(&super::provenance(who, &source), b"", &[]);
                ep.episode_id = id.clone();
                ep.document = Some(key.clone());
                kept.push(ep);
                added.push(
                    json!({"name": item.name, "kind": item.kind, "episode_id": id,
                                  "source": source, "bytes": item.text.len(), "new": new}),
                );
            }
            // What the item itself is refused for (a body no episode type reads, say): that item alone.
            Err(e) => match e.downcast_ref::<ToolError>() {
                Some(m) => refused.push(json!({"name": item.name, "reason": m.0})),
                None => return Err(e),
            },
        }
    }
    let reading = to_read(store, scope, &kept).await?;
    // Something new to read: said in the ledger, where the server that extracts finds it within seconds, whichever
    // server that is (one that only serves never extracts), and that one is woken now if it is this one.
    if fresh && !reading.is_empty() {
        let actor = format!("{}{}", crate::extract::ADDED_WAKE, who.user());
        match store.commit_record(scope, &actor, &[]).await {
            Ok(_) => WAKE.notify_one(),
            Err(e) => eprintln!("learn: {scope}'s extraction couldn't be woken: {e:#}"),
        }
    }
    for a in &mut added {
        a["read"] = json!(reading.contains(a["episode_id"].as_str().unwrap_or_default()));
    }
    let (mode, _) = extraction(store, scope, !reading.is_empty()).await;
    Ok(json!({
        "added": added,
        "refused": refused,
        "scope": scope.to_string(),
        "readers": readers,
        "extraction": mode,
    }))
}

/// `POST /learn/status`: the receipt of `episode_ids` as the caller may read them (an id they can't read answers
/// as `unknown`, as one that doesn't exist does). `done` once none is pending.
pub(super) async fn status(
    state: &State,
    store: &Store,
    session: &Session,
    who: &Principal,
    args: &Value,
) -> Result<Value> {
    let ids: Vec<String> = args["episode_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Err(ToolError("body is {episode_ids: [...]}".into()).into());
    }
    if ids.len() > MAX_STATUS {
        return Err(ToolError(format!("at most {MAX_STATUS} episodes a request")).into());
    }
    let scope = session.scope().clone();
    let vis = session.vis();
    let seen: Vec<String> = ids
        .iter()
        .filter(|id| vis.can_see_episode(id, who))
        .cloned()
        .collect();
    let set: HashSet<String> = seen.iter().cloned().collect();
    let reads = match set.is_empty() {
        true => Default::default(),
        false => store.episode_reads(session, &set).await?,
    };
    // The facts citing them that the caller may read, current ones: a column scan for each id, then exactly.
    let mut facts = citing(store, session, who, &seen).await?;
    // A read that found facts, none of which shows here: the read is in the ledger this session pinned, but its
    // facts may be beyond this server's cached copy of the facts table (another server committed them, and the
    // two are refreshed apart). Read that again first, rather than tell the person the read found nothing: the
    // facts were committed with the read, so a copy loaded now holds them.
    let unseen = seen.iter().any(|id| {
        reads.get(id).is_some_and(|rows| {
            rows.iter()
                .any(|(by, n)| *n > 0 && !by.starts_with("compile:"))
        }) && !facts
            .iter()
            .any(|f| f.source_episodes.iter().any(|e| e == id))
    });
    if unseen {
        store.invalidate(&scope.namespace(), crate::FACTS);
        facts = citing(store, session, who, &seen).await?;
    }
    // Each entity the facts are about, and whether memory knew it before: a fact about it the caller may read
    // that cites none of these episodes.
    let entities: Vec<String> = facts
        .iter()
        .map(|f| f.entity.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let known_before: HashSet<String> = match entities.is_empty() {
        true => HashSet::new(),
        false => session
            .read_in(store, who, Col::Entity, &entities, None)
            .await?
            .into_iter()
            .filter(|f| !f.source_episodes.iter().all(|e| set.contains(e)))
            .map(|f| f.entity)
            .collect(),
    };
    // What each stated again that memory already held (reconcile's records), as the caller may read those facts.
    let restated: BTreeMap<&str, Vec<String>> = seen
        .iter()
        .map(|id| {
            (
                id.as_str(),
                vis.restated_by(id).into_iter().map(String::from).collect(),
            )
        })
        .collect();
    let known_ids: Vec<String> = restated.values().flatten().cloned().collect();
    let known_facts = match known_ids.is_empty() {
        true => vec![],
        false => {
            session
                .read_in(store, who, Col::FactId, &known_ids, Some(i64::MAX))
                .await?
        }
    };
    let refusals = crate::extract::refusals(&scope);
    let fact = |f: &crate::Fact| json!({"fact_id": f.fact_id, "entity": f.entity, "predicate": f.predicate, "value": f.value});
    // Where each stands with extraction, as its reads say: read (or citing facts already), given up on, or neither.
    let stands: BTreeMap<&str, &str> = seen
        .iter()
        .map(|id| {
            let rows = reads.get(id).map(Vec::as_slice).unwrap_or_default();
            let read = rows
                .iter()
                .any(|(by, n)| *n != crate::FAILED_TRY && !by.starts_with("compile:"));
            let failed = rows
                .iter()
                .filter(|(by, n)| *n == crate::FAILED_TRY && !by.starts_with("compile:"))
                .count();
            let cited = facts
                .iter()
                .any(|f| f.source_episodes.iter().any(|e| e == id));
            let state = if read || cited {
                "read"
            } else if failed >= crate::EXTRACT_TRIES {
                "failed"
            } else {
                "unread"
            };
            (id.as_str(), state)
        })
        .collect();
    // Those not read yet that extraction will read (pending), as it picks them; the rest it never will (not_read).
    let unread: Vec<String> = stands
        .iter()
        .filter(|(_, s)| **s == "unread")
        .map(|(id, _)| id.to_string())
        .collect();
    let reading = match unread.is_empty() {
        true => HashSet::new(),
        false => {
            let eps: Vec<Episode> = session
                .episode_sources(store, who, &unread)
                .await?
                .into_iter()
                .map(|(id, (source, at))| {
                    let mut e = Episode::new(&source, b"", &[]);
                    e.document = vis.document_of(&id).map(String::from);
                    e.episode_id = id;
                    e.occurred_at = at;
                    e
                })
                .collect();
            to_read(store, &scope, &eps).await?
        }
    };
    let (mode, why) = extraction(
        store,
        &scope,
        !reading.is_empty() || stands.values().any(|s| *s == "read"),
    )
    .await;
    let mut pending = false;
    let items: Vec<Value> = ids
        .iter()
        .map(|id| {
            if !set.contains(id) {
                return json!({"episode_id": id, "status": "unknown"});
            }
            let theirs: Vec<&crate::Fact> = facts
                .iter()
                .filter(|f| f.source_episodes.iter().any(|e| e == id))
                .collect();
            let state = match stands[id.as_str()] {
                "unread" if reading.contains(id) => {
                    pending = true;
                    "pending"
                }
                "unread" => "not_read",
                s => s,
            };
            let mut about: BTreeMap<&str, usize> = BTreeMap::new();
            for f in &theirs {
                *about.entry(f.entity.as_str()).or_default() += 1;
            }
            let known: Vec<Value> = known_facts
                .iter()
                .filter(|f| restated[id.as_str()].contains(&f.fact_id))
                .map(fact)
                .collect();
            let mut v = json!({
                "episode_id": id,
                "status": state,
                "facts": theirs.iter().map(|f| fact(f)).collect::<Vec<_>>(),
                "entities": about.iter().map(|(e, n)| json!({"entity": e, "facts": n,
                    "new": !known_before.contains(*e)})).collect::<Vec<_>>(),
                "known": known,
            });
            // Why its facts were dropped, where this server refused some of the last episode it read.
            if let Some(r) = refusals
                .last_refused
                .as_ref()
                .filter(|r| r.episode_id.as_deref() == Some(id.as_str()))
            {
                v["refused"] = json!({"facts": r.refused, "why": r.why});
            }
            if state == "failed" {
                v["reason"] = json!("extraction gave up on it after repeated failures");
            } else if state == "not_read" {
                v["reason"] = json!(why);
            }
            v
        })
        .collect();
    // On a server that doesn't extract (it doesn't hold the writer lease), what the one that does commits shows here
    // only once this one's cached tables are read again: while a person waits on a receipt, that is at its next
    // look, not when they next age out.
    if pending && !state.writer.holds() {
        fresh(store, &scope);
    }
    Ok(json!({"done": !pending, "extraction": mode, "items": items}))
}

/// The current facts citing any of `ids` that `who` may read, notes' own facts aside: a column scan for each id,
/// then exactly.
async fn citing(
    store: &Store,
    session: &Session,
    who: &Principal,
    ids: &[String],
) -> Result<Vec<crate::Fact>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let cites = Filter::Or(
        ids.iter()
            .map(|id| Filter::Contains(Col::Sources, id.clone()))
            .collect(),
    );
    Ok(session
        .read_where(store, who, &cites, Some(i64::MAX))
        .await?
        .into_iter()
        .filter(|f| !crate::is_note_fact(f))
        .collect())
}

/// How often a scope's tables are read again for a receipt waiting on another server's extraction: each is a
/// catalog load.
const FRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// Have `scope`'s next read see what was committed since (the ledger, its reads and facts), at most every
/// [`FRESH_EVERY`].
fn fresh(store: &Store, scope: &Scope) {
    static LAST: LazyLock<std::sync::Mutex<BTreeMap<String, std::time::Instant>>> =
        LazyLock::new(Default::default);
    let ns = scope.namespace();
    {
        let mut last = LAST.lock().unwrap();
        if last.get(&ns).is_some_and(|t| t.elapsed() < FRESH_EVERY) {
            return;
        }
        last.insert(ns.clone(), std::time::Instant::now());
    }
    store.invalidate(crate::SYSTEM_NS, "ledger");
    store.invalidate(&ns, crate::EXTRACTIONS);
    store.invalidate(&ns, crate::FACTS);
}

/// Of `eps` (as extraction sees them: source, document, when), those extraction will read for facts
/// ([`crate::extract::reads`]); none where this server has no extraction model.
async fn to_read(store: &Store, scope: &Scope, eps: &[Episode]) -> Result<HashSet<String>> {
    match *EXTRACTS {
        true => crate::extract::reads(store, scope, eps).await,
        false => Ok(HashSet::new()),
    }
}

/// Whether what is added here is read for facts, and why not where it isn't: `on` where any of it is read or will be
/// (`reads`), else `none` (no extraction model on this server), `off` (the scope's admins turned extraction off and
/// its schema reads none of it), or `on` with nothing here reading these.
async fn extraction(store: &Store, scope: &Scope, reads: bool) -> (&'static str, &'static str) {
    if reads {
        return ("on", "");
    }
    if !*EXTRACTS {
        return ("none", "this server has no extraction model");
    }
    match crate::people::directory::Directory::load(store).await {
        Ok(d) if !d.extracts_starter(scope) => ("off", "extraction is turned off for this memory"),
        _ => ("on", "nothing here reads it for facts"),
    }
}

/// An item as asked, read: its text (a file's bytes as UTF-8), named. Err is why it is refused.
fn read(v: &Value) -> std::result::Result<Item, String> {
    let kind = match v["kind"].as_str().unwrap_or("file") {
        "file" => "file",
        "paste" => "paste",
        "answer" => "answer",
        other => return Err(format!("kind is file, paste or answer, not {other:?}")),
    };
    let name = item_name(v);
    let text = match kind {
        "answer" => {
            let (q, a) = (
                v["question"].as_str().unwrap_or_default().trim(),
                v["answer"].as_str().unwrap_or_default().trim(),
            );
            if q.is_empty() || a.is_empty() {
                return Err("an answer to keep needs its question and its answer".into());
            }
            format!("Question: {q}\n\nAnswer: {a}\n")
        }
        "file" => {
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "pdf" {
                return Err("PDFs aren't read here yet: copy its text and paste it instead".into());
            }
            if !TEXT_TYPES.contains(&ext.as_str()) {
                return Err(format!(
                    "{} files aren't read: text, Markdown, CSV, JSON and YAML are ({})",
                    if ext.is_empty() {
                        "untyped".to_string()
                    } else {
                        format!(".{ext}")
                    },
                    TEXT_TYPES.map(|t| format!(".{t}")).join(", ")
                ));
            }
            match (v["text"].as_str(), v["base64"].as_str()) {
                (Some(t), _) => t.to_string(),
                (None, Some(b)) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b.trim())
                        .map_err(|_| "its base64 doesn't decode".to_string())?;
                    String::from_utf8(bytes).map_err(|_| {
                        "it isn't text (not UTF-8): save it as text first".to_string()
                    })?
                }
                (None, None) => return Err("a file is sent as text or base64".into()),
            }
        }
        _ => v["text"].as_str().unwrap_or_default().to_string(),
    };
    // A byte-order mark and NULs say nothing, and NUL can't be stored as text.
    let text = text.trim_start_matches('\u{feff}').replace('\0', "");
    if text.trim().is_empty() {
        return Err("it's empty".into());
    }
    if text.len() > MAX_TEXT {
        return Err(format!(
            "it's {} KB: at most {} KB is added at a time, so split it",
            text.len().div_ceil(1024),
            MAX_TEXT / 1024
        ));
    }
    Ok(Item { kind, name, text })
}

/// An item's name as given (a file's, without its folders), else a name for its kind.
fn item_name(v: &Value) -> String {
    let given = v["name"].as_str().unwrap_or_default();
    let base = given.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect();
    match (clean.is_empty(), v["kind"].as_str()) {
        (false, _) => clean,
        // An answer is named by the question it answers.
        (true, Some("answer")) => match v["question"].as_str().map(str::trim) {
            Some(q) if !q.is_empty() => q.chars().filter(|c| !c.is_control()).take(120).collect(),
            _ => "Ask answer".into(),
        },
        (true, Some("paste")) => "Pasted text".into(),
        _ => "Untitled".into(),
    }
}

/// Where an item is filed: its source (what the Explorer lists it under, below who wrote it) and its document key.
/// A file is a document by its name, so adding a changed one again is its next version; pasted text and an answer
/// are one each by their text. A file and pasted text are filed under [`crate::origin::LEARNED`], the person's own.
fn filed(item: &Item, author: &str) -> (String, String) {
    // `:` separates a source's parts and `/` a document key's.
    let part = |s: &str| -> String {
        let dashed: String = s
            .chars()
            .map(|c| {
                if c == ':' || c == '/' || c.is_whitespace() {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        dashed
            .split('-')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .take(80)
            .collect()
    };
    let digest = hex(&sha2::Sha256::digest(item.text.as_bytes())[..6]);
    let learned = crate::origin::LEARNED;
    match item.kind {
        "file" => (
            format!("{learned}:upload:{}", part(&item.name)),
            format!("{DOCUMENTS}{author}/file/{}", part(&item.name)),
        ),
        kind => {
            // Named by its first words, as a reader would recognise it.
            let words: Vec<&str> = item.text.split_whitespace().take(8).collect();
            let label = match kind {
                "answer" => item
                    .text
                    .strip_prefix("Question: ")
                    .unwrap_or(&item.text)
                    .split_whitespace()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => words.join(" "),
            };
            let label = part(&label.to_lowercase());
            let family = if kind == "answer" { "ask" } else { "paste" };
            // What a person pasted is theirs (`crate::origin::LEARNED`); an answer kept is the model's words.
            let filed = match kind {
                "answer" => String::new(),
                _ => format!("{learned}:"),
            };
            (
                format!(
                    "{filed}{family}:{}",
                    if label.is_empty() {
                        digest.clone()
                    } else {
                        label
                    }
                ),
                format!("{DOCUMENTS}{author}/{family}/{digest}"),
            )
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
