//! Notes as facts. A note (what a person asked to be remembered, what the server kept from what they said, what
//! the team shares once enough people said the same) is an episode with its ACL; each is also a fact
//! ([`crate::note_fact`]) citing that episode alone. A fact is read only by who may read every episode it cites, so
//! the note's ACL is the fact's: a person's note stays theirs, a shared one is everyone's in its scope, and a
//! forget, an offboarding or a retention rule that takes the note takes its fact. Nothing files notes by path:
//! the Explorer lists them as the facts the reader may read (`crate::browse`), and search finds them as it finds
//! any fact.
//!
//! A note's fact is written with it, in the same commit ([`facts_of`]); [`backfill`] writes the fact of every note
//! written before, or by a path that didn't.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{Episode, Fact, Scope, Store};

/// The most of a note its fact holds, in characters: a note is a sentence or a few, not a document.
const TEXT_MAX: usize = 4000;
/// Who writes the facts of notes written before.
const ACTOR: &str = "memstore:notes";
/// How often a scope's notes are checked for one without its fact, in this process.
const BACKFILL_EVERY: Duration = Duration::from_secs(6 * 3600);
/// The most facts one commit of a backfill writes.
const BACKFILL_CHUNK: usize = 200;

/// Whether `source` is a note's: a person's (kept by the server, or remembered under a plain source; a hook's
/// session note, a transcript, a session's record or a run's trace each name more than one segment) or the team's
/// shared ones.
pub fn is_note(source: &str) -> bool {
    crate::profile::author_of(source).is_some_and(|(author, rest)| {
        if author == crate::keep::SHARER {
            return rest == "shared";
        }
        crate::keep::is_kept(rest) || (!rest.contains(':') && crate::profile::is_note(rest))
    })
}

/// Whether `source` is one of the team's shared notes.
pub fn is_shared(source: &str) -> bool {
    crate::profile::author_of(source)
        .is_some_and(|(author, rest)| author == crate::keep::SHARER && rest == "shared")
}

/// Whether the writer of note `source` may still have it in `scope`: not disabled, not removed from its org, and the
/// org not being deleted. The team's shared notes are the org's.
fn still_theirs(dir: &crate::people::directory::Directory, scope: &Scope, source: &str) -> bool {
    if let Some(org) = scope.org_name()
        && (dir.purging.contains(org) || dir.purged.contains(org))
    {
        return false;
    }
    let Some((author, _)) = crate::profile::author_of(source) else {
        return false;
    };
    if author == crate::keep::SHARER {
        return true;
    }
    let id = crate::people::directory::user_id(author);
    if dir.disabled.get(&id) == Some(&true) {
        return false;
    }
    match scope.org_name() {
        Some(org) => dir.orgs.get(org).is_some_and(|o| !o.removed(&id)),
        None => true,
    }
}

/// The fact of note `id` with `source` and `body`, said at `at`: none for what isn't a note, or says nothing.
pub fn fact_of(id: &str, source: &str, body: &[u8], at: i64) -> Option<Fact> {
    if !is_note(source) {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut value: String = text.chars().take(TEXT_MAX).collect();
    if text.chars().count() > TEXT_MAX {
        value.push('…');
    }
    let mut tags = vec![crate::NOTE.to_string()];
    if is_shared(source) {
        tags.push("team".into());
    }
    Some(crate::note_fact(id, &value, at, tags))
}

/// The facts of the notes among `eps`, to commit with them: a version of a document (starter memory) is none.
pub fn facts_of(eps: &[Episode]) -> Vec<Fact> {
    eps.iter()
        .filter(|e| e.document.is_none())
        .filter_map(|e| fact_of(&e.episode_id, &e.source, &e.body, e.occurred_at))
        .collect()
}

/// When each scope's notes were last checked, in this process.
static CHECKED: LazyLock<Mutex<HashMap<Scope, Instant>>> = LazyLock::new(Default::default);

/// Writes the fact of each note of `scope` that has none: one written before notes were facts, or by a path that
/// didn't write it. A note forgotten (its ACL no one's) or retracted gets none: nothing is written of what a forget
/// took. Once per [`BACKFILL_EVERY`] a scope, in this process. Answers how many were written.
pub async fn backfill(store: &Store, scope: &Scope) -> Result<usize> {
    {
        let checked = CHECKED.lock().unwrap();
        if checked
            .get(scope)
            .is_some_and(|t| t.elapsed() < BACKFILL_EVERY)
        {
            return Ok(0);
        }
    }
    use crate::aggregate::StoreAggregates;
    let view = store.aggregation_view(scope).await?;
    let have: BTreeSet<&str> = view
        .facts
        .iter()
        .filter(|f| crate::is_note_fact(f))
        .flat_map(|f| f.source_episodes.iter().map(String::as_str))
        .collect();
    let missing: Vec<(&str, &str, i64)> = view
        .episodes
        .iter()
        .filter(|e| is_note(&e.source) && !have.contains(e.id.as_str()))
        .filter(|e| !e.acl.is_empty() && !e.acl.iter().any(|a| a == crate::FORGOTTEN))
        .map(|e| (e.id.as_str(), e.source.as_str(), e.occurred_at))
        .collect();
    let mut wrote = 0;
    for chunk in missing.chunks(BACKFILL_CHUNK) {
        let ids = chunk.iter().map(|(id, _, _)| id.to_string()).collect();
        let bodies = store.episode_bodies(scope, ids).await?;
        // Read again just before writing: a person removed from the org (forgotten, offboarded) or disabled since,
        // or the org being deleted, has no fact written of their notes while their erasure runs.
        let dir = store.directory().await?;
        let facts: Vec<Fact> = chunk
            .iter()
            .filter(|(_, source, _)| still_theirs(&dir.dir, scope, source))
            .filter_map(|(id, source, at)| fact_of(id, source, bodies.get(*id)?, *at))
            .collect();
        if facts.is_empty() {
            continue;
        }
        store.commit(scope, ACTOR, &[], &facts).await?;
        wrote += facts.len();
    }
    CHECKED
        .lock()
        .unwrap()
        .insert(scope.clone(), Instant::now());
    if wrote > 0 {
        eprintln!("notes: {scope}: wrote the facts of {wrote} notes written before");
    }
    Ok(wrote)
}
