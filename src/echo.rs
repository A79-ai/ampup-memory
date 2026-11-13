//! Echoes: a fact written back by someone who was just served it. An agent that searches memory, finds a fact and
//! remembers it again ("ingestion is still stalled") adds no evidence, however often it does: each hourly run would
//! otherwise turn one claim into another confirmation of it. So a note's fact that only repeats what its writer was
//! served supports nothing, is no new fact, and refreshes nothing.
//!
//! The server keeps, per scope and caller, the episodes its reads served them in the last [`window`]
//! (`AMPUP_MEMORY_ECHO_MINUTES`, default 60; 0 turns it off): what a search's facts and passages cite, the episodes
//! `get_episode` reads, as the audit's `read` rows name them ([`crate::audit::MAX_READ_IDS`] a read at most). It is
//! held in memory, on the server that served the reads, and never written as such. A note (`remember_memory`,
//! `/remember`) written while it holds any is committed with a record of them: a document of ids only,
//! `served/<note>` ([`PREFIX`]), filed under the note's author ([`TAIL`]) so forgetting them, and retention of their
//! notes, take it too, and read by no reader ([`ACL`]). Nothing extracts it. Forgetting the note forgets it
//! ([`forgotten`]), and an erasure of the note or of any episode it names (a purge of what was deleted or aged out,
//! forgetting a person) deletes it.
//!
//! Reconcile reads the record of each note it reconciles ([`served`]). A fact of the note that gives the value of a
//! known fact (in the same words, or in other words, as the model judges) that cites an episode its writer was served
//! is an echo: dropped, recorded as no restatement ([`crate::corroboration`]), and, where every fact is kept (a
//! schema's extraction), not kept. A fact that gives another value is no echo: what changed still supersedes, under
//! the rules of [`crate::origin`].
//!
//! It links a note to its writer's reads only: what reaches memory another way (a message an agent posted to a chat
//! that a connector delivers, a note written on another server than the one that served the read, a write through
//! the ingest inbox) is no echo here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::{Episode, Principal, REMEMBER_SOURCE, Scope, Store, now_micros};

/// Where a note's record of what its writer was served is filed, `served/<note>`.
pub const PREFIX: &str = crate::SERVED_PREFIX;
/// What follows the author in a record's source (`mcp:remember_memory:<author>:memstore:served`): no note may take
/// it, and the server's checks of what is a note leave it out (it starts `memstore:`).
pub const TAIL: &str = crate::SERVED_SOURCE_TAIL;
/// The entry no reader holds (no sign-in names an address under `.invalid`): a record's ACL, and the reader
/// reconcile reads it as.
const READER: &str = "served@memstore.invalid";
pub const ACL: &str = "user:served@memstore.invalid";
/// Callers held at most, and episodes per caller: past them, the oldest go.
const MAX_CALLERS: usize = 10_000;
const MAX_EPISODES: usize = 1_000;

/// How far back a note's writer's reads count: `AMPUP_MEMORY_ECHO_MINUTES` (default 60), in microseconds. 0: none.
pub fn window() -> i64 {
    let minutes: i64 = std::env::var("AMPUP_MEMORY_ECHO_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(60);
    minutes.max(0) * 60_000_000
}

/// A caller's recent reads: each one's time and the episodes it served, oldest first.
type Reads = VecDeque<(i64, Vec<String>)>;

/// The episodes each caller was served recently, by (scope, caller).
static SERVED: LazyLock<Mutex<HashMap<(String, String), Reads>>> = LazyLock::new(Default::default);

/// Note that a read of `user`'s in `scope` served these episodes, now.
pub fn note(scope: &Scope, user: &str, episodes: &[String]) {
    let w = window();
    if w == 0 || episodes.is_empty() {
        return;
    }
    let now = now_micros();
    let mut held = SERVED.lock().unwrap();
    if held.len() >= MAX_CALLERS {
        held.retain(|_, reads| reads.back().is_some_and(|(t, _)| now - t <= w));
        if held.len() >= MAX_CALLERS {
            let oldest = held
                .iter()
                .min_by_key(|(_, reads)| reads.back().map_or(0, |(t, _)| *t))
                .map(|(k, _)| k.clone());
            held.remove(&oldest.unwrap());
        }
    }
    let reads = held
        .entry((scope.to_string(), user.to_string()))
        .or_default();
    reads.push_back((now, episodes.to_vec()));
    while reads.front().is_some_and(|(t, _)| now - t > w)
        || reads.iter().map(|(_, e)| e.len()).sum::<usize>() > MAX_EPISODES
    {
        reads.pop_front();
    }
}

/// The episodes `user` was served in `scope` within the window before now, each once.
pub fn before(scope: &Scope, user: &str) -> Vec<String> {
    let w = window();
    let now = now_micros();
    let held = SERVED.lock().unwrap();
    let mut out: Vec<String> = vec![];
    for (_, eps) in held
        .get(&(scope.to_string(), user.to_string()))
        .into_iter()
        .flatten()
        .filter(|(t, _)| now - t <= w)
    {
        for e in eps {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
    }
    out
}

/// The record that the writer of `note`, `author`, was served `episodes` just before writing it.
pub fn record(note: &Episode, author: &str, episodes: &[String]) -> Episode {
    let body = json!({"note": note.episode_id, "served": episodes}).to_string();
    let mut e = Episode::sourced(
        &format!("{REMEMBER_SOURCE}:{author}:{TAIL}"),
        body.as_bytes(),
        &[ACL],
    )
    .without_text_index()
    .version_of(&format!("{PREFIX}{}", note.episode_id));
    e.occurred_at = note.occurred_at;
    e
}

/// Whether `source` is a record's ([`record`]).
pub fn is_record(source: &str) -> bool {
    crate::is_served_source(source)
}

/// The episodes the writer of `ep` was served just before writing it, as its record says: none for an episode
/// that is no note, or a note written with none.
pub async fn served(store: &Store, scope: &Scope, ep: &Episode) -> Result<HashSet<String>> {
    if !ep.source.starts_with(REMEMBER_SOURCE) || is_record(&ep.source) {
        return Ok(HashSet::new());
    }
    let reader = Principal::new(READER, &[]);
    let key = format!("{PREFIX}{}", ep.episode_id);
    let Some((_, _, body)) = store.document(scope, &reader, &key, None).await? else {
        return Ok(HashSet::new());
    };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Ok(v["served"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_str().map(String::from))
        .collect())
}

/// After `author` forgot their note `id` (`forget_memory`): the record of what they were served before writing it
/// is forgotten too, every version of it. How many it forgot.
pub async fn forgotten(store: &Store, scope: &Scope, author: &str, id: &str) -> Result<usize> {
    let key = format!("{PREFIX}{id}");
    let versions: Vec<String> = {
        let session = store.pin(scope, crate::MAIN).await?;
        session
            .vis()
            .document_versions(&key)
            .into_iter()
            .filter(|(k, _)| *k == key)
            .flat_map(|(_, v)| v.into_iter().map(String::from))
            .collect()
    };
    let mut n = 0;
    for v in versions {
        if store
            .forget(scope, author, &v, |s| {
                is_record(s) && crate::remembered_by(s, author)
            })
            .await?
        {
            n += 1;
        }
    }
    Ok(n)
}
