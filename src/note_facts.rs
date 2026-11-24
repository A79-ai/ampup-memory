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

/// Whether the writer of note `source`, written at `at`, may still have it in `scope`: not disabled, not removed from
/// its org, and the org neither being deleted nor emptied since (`clear_org`). The team's shared notes are the org's.
fn still_theirs(
    dir: &crate::people::directory::Directory,
    scope: &Scope,
    source: &str,
    at: i64,
) -> bool {
    if let Some(org) = scope.org_name()
        && (dir.purging.contains(org)
            || dir.purged.contains(org)
            || dir.orgs.get(org).is_some_and(|o| at < o.cleared_at))
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
            .filter(|(_, source, at)| still_theirs(&dir.dir, scope, source, *at))
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

/// What [`renote`] found, and did unless it was a dry run.
#[derive(Debug, Default, serde::Serialize)]
pub struct Renoted {
    /// Old notes (no fact of their own, or written before the cutoff): each goes only once a note kept from the
    /// person's sessions says all it does.
    pub old_notes: usize,
    /// Sessions whose person's words are read again.
    pub sessions: usize,
    /// The people they are of.
    pub people: usize,
    /// With `preview`: the person's notes now, and what each session read would keep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<serde_json::Value>,
}

/// Which of [`renote`]'s sessions to read, and whether to write.
#[derive(Debug, Default)]
pub struct Renote {
    /// Only this person's sessions.
    pub person: Option<String>,
    /// At most this many sessions a person, their first.
    pub limit: Option<usize>,
    /// Write nothing, delete nothing: answer the person's notes now (each with whether it would be replaced) and
    /// what each session would keep. Needs a `person`.
    pub preview: bool,
}

/// How many people's sessions [`renote`] reads at once (each person's in order, oldest first).
const RENOTE_PARALLEL: usize = 4;

/// Where a session's words are read from, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Read {
    /// Its transcript, kept at its end (`<signed>memstore:session:<id>`, or `session:<id>` before).
    Transcript,
    /// Its conversation so far, kept at each step (`…:conversation`).
    Conversation,
    /// The parts of its transcript the hooks sent (`<signed>memstore:transcript:<harness>:<repo>:<id>`).
    Parts,
    /// Only what opened it (the session's logged question).
    Opening,
}

/// Each session's words by its author and id: where they are best read from, when, and the episodes holding them.
type Words = std::collections::HashMap<(String, String), (Read, i64, Vec<(i64, String)>)>;

/// One session to read: whose, when, where its words are, and its row.
struct ToRead {
    at: i64,
    l: crate::qa_log::Logged,
    read: Read,
    eps: Vec<String>,
}

/// The person's words in `t`.
async fn typed_of(
    store: &crate::Store,
    scope: &crate::Scope,
    t: &ToRead,
) -> anyhow::Result<Vec<crate::keep::Typed>> {
    if t.read == Read::Opening {
        return Ok(crate::sessions::typed_in(&t.l.question, false));
    }
    let bodies = store.episode_bodies(scope, t.eps.clone()).await?;
    let text: Vec<String> = t
        .eps
        .iter()
        .filter_map(|id| bodies.get(id))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    // What its episodes hold, or nothing: never its opening in place of words that were forgotten meanwhile.
    Ok(crate::sessions::typed_in(&text.join("\n"), true))
}

/// Notes again from what people said: reads each person's sessions again, oldest first (its transcript, else its
/// conversation so far, else the transcript parts the hooks sent, else what opened it), keeping what they show they
/// want honoured as a note of theirs with its fact, as each session's end would ([`crate::keep::from_user_but`]).
/// Then each old note of `scope` (one with no fact of its own, or with `written_before` (microseconds) one written
/// before it whatever its fact) goes only where a note the reading kept for the same person says all it does
/// ([`crate::keep::repeats`]): a note no session says again stays, with the fact it has. The old notes are not what
/// the reading compares with, so what they say is kept fresh if the sessions say it; one kept again word for word
/// has the same id and stays. A recorder key's or a program's session is not read, nor one of someone removed,
/// disabled or whose org is going, nor a note the person forgot kept again. With `dry_run` nothing is written: it
/// answers what it would do. The reading and the replacing run in the background; they log their progress.
pub async fn renote(
    store: &crate::Store,
    scope: &crate::Scope,
    actor: &str,
    written_before: Option<i64>,
    dry_run: bool,
    which: Renote,
) -> anyhow::Result<Renoted> {
    use crate::aggregate::StoreAggregates;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    anyhow::ensure!(
        !which.preview || which.person.is_some(),
        "a preview is of one person's sessions: name the person"
    );
    let view = store.aggregation_view(scope).await?;
    let have: BTreeSet<&str> = view
        .facts
        .iter()
        .filter(|f| crate::is_note_fact(f))
        .flat_map(|f| f.source_episodes.iter().map(String::as_str))
        .collect();
    // The old notes, by their author.
    let mut old: HashMap<String, Vec<String>> = HashMap::new();
    for e in &view.episodes {
        if !is_note(&e.source)
            || is_shared(&e.source)
            || (have.contains(e.id.as_str()) && !written_before.is_some_and(|t| e.occurred_at < t))
        {
            continue;
        }
        if let Some((author, _)) = crate::profile::author_of(&e.source) {
            old.entry(author.to_string())
                .or_default()
                .push(e.id.clone());
        }
    }
    let going: HashSet<String> = old.values().flatten().cloned().collect();
    // Each session's words, by its author and id: where they are best read from, and the episodes holding them.
    let mut words: Words = HashMap::new();
    // Sessions some of whose words their person forgot: none of it is read, its opening neither.
    let mut forgot: HashSet<(String, String)> = HashSet::new();
    for e in &view.episodes {
        let Some((author, rest)) = crate::profile::author_of(&e.source) else {
            continue;
        };
        let (id, read) = if let Some(r) = rest.strip_prefix(crate::TRANSCRIPT_SOURCE) {
            // `<harness>:<repo>:<session>`: the session is what follows the last `:`.
            match r.rsplit_once(':') {
                Some((_, id)) => (id, Read::Parts),
                None => continue,
            }
        } else if let Some(r) = rest
            .strip_prefix(crate::SESSION_SOURCE)
            .or_else(|| rest.strip_prefix("session:"))
        {
            match r.strip_suffix(":conversation") {
                Some(id) => (id, Read::Conversation),
                None => (r, Read::Transcript),
            }
        } else {
            continue;
        };
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            continue;
        }
        if e.acl.is_empty() || e.acl.iter().any(|a| a == crate::FORGOTTEN) {
            forgot.insert((author.to_string(), id.to_string()));
            continue;
        }
        let w = words
            .entry((author.to_string(), id.to_string()))
            .or_insert((read, e.occurred_at, vec![]));
        if read < w.0 {
            *w = (read, e.occurred_at, vec![]);
        }
        if read == w.0 {
            match read {
                // All its parts, in order.
                Read::Parts => w.2.push((e.occurred_at, e.id.clone())),
                // The newest.
                _ if e.occurred_at >= w.1 || w.2.is_empty() => {
                    w.1 = e.occurred_at;
                    w.2 = vec![(e.occurred_at, e.id.clone())];
                }
                _ => {}
            }
        }
    }
    let dir = store.directory().await?;
    let mut by_person: BTreeMap<String, Vec<ToRead>> = BTreeMap::new();
    for l in crate::sessions::list(store, scope).await? {
        if which.person.as_ref().is_some_and(|p| *p != l.asker) {
            continue;
        }
        if l.session.is_none()
            || crate::sessions::recorded(&l)
            || crate::sessions::automated(&l)
            || !crate::sessions::still_theirs(&dir.dir, scope, &l)
        {
            continue;
        }
        // Only what the session's own asker wrote, signed as theirs, none of it forgotten.
        let key = (l.asker.clone(), l.answer_id.clone());
        if forgot.contains(&key) {
            continue;
        }
        let (read, eps) = match words.get(&key) {
            Some((read, _, eps)) => {
                let mut eps = eps.clone();
                eps.sort();
                (*read, eps.into_iter().map(|(_, id)| id).collect())
            }
            None if !l.question.trim().is_empty() => (Read::Opening, vec![]),
            None => continue,
        };
        by_person.entry(l.asker.clone()).or_default().push(ToRead {
            at: l.asked_at,
            l,
            read,
            eps,
        });
    }
    for sessions in by_person.values_mut() {
        sessions.sort_by_key(|t| t.at);
        sessions.truncate(which.limit.unwrap_or(usize::MAX));
    }
    let r = |preview| Renoted {
        old_notes: going.len(),
        sessions: by_person.values().map(Vec::len).sum(),
        people: by_person.len(),
        preview,
    };
    if dry_run && !which.preview {
        return Ok(r(None));
    }
    if which.preview {
        let person = which.person.as_deref().unwrap_or_default();
        let signed = format!("{}:{person}:", crate::REMEMBER_SOURCE);
        let mut read = vec![];
        let mut kept_texts = vec![];
        for t in by_person.values().flatten() {
            let typed = typed_of(store, scope, t).await?;
            let said = typed.len();
            let would = std::sync::Mutex::new(vec![]);
            let theirs = |dir: &crate::people::directory::Directory| {
                crate::sessions::still_theirs(dir, scope, &t.l)
            };
            crate::keep::from_user_but(
                store,
                scope,
                &t.l.asker,
                &signed,
                typed,
                theirs,
                &going,
                Some(&would),
            )
            .await?;
            let would = would.into_inner().unwrap();
            kept_texts.extend(
                would
                    .iter()
                    .map(|w| w.split_once(": ").map_or(w.clone(), |(_, t)| t.to_string())),
            );
            read.push(
                serde_json::json!({"session": t.l.answer_id, "asked_at": t.at,
                "harness": t.l.session.as_ref().and_then(|s| s.harness.clone()),
                "read": t.read, "messages": said, "would_keep": would}),
            );
        }
        let mine: Vec<String> = view
            .episodes
            .iter()
            .filter(|e| e.source.starts_with(&signed) && is_note(&e.source))
            .filter(|e| !e.acl.iter().any(|a| a == crate::FORGOTTEN))
            .map(|e| e.id.clone())
            .collect();
        let bodies = store.episode_bodies(scope, mine).await?;
        let now: Vec<serde_json::Value> = view
            .episodes
            .iter()
            .filter(|e| bodies.contains_key(&e.id))
            .map(|e| {
                let text = String::from_utf8_lossy(&bodies[&e.id]).trim().to_string();
                let replaced = going.contains(&e.id)
                    && kept_texts.iter().any(|k| crate::keep::repeats(k, &text));
                serde_json::json!({"filed": &e.source[signed.len()..], "old": going.contains(&e.id),
                    "replaced": replaced, "text": text})
            })
            .collect();
        return Ok(r(Some(serde_json::json!({"now": now, "sessions": read}))));
    }
    let answer = r(None);
    eprintln!(
        "renote: {scope}: reading {} sessions of {} people again, then replacing what they say of {} old notes",
        answer.sessions, answer.people, answer.old_notes
    );
    let (store, scope, actor) = (store.clone(), scope.clone(), actor.to_string());
    let org = scope.org_name().map(str::to_string);
    tokio::spawn(crate::log_org::with(org.as_deref(), async move {
        use futures::StreamExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let total = answer.sessions;
        let done = AtomicUsize::new(0);
        let kept: std::sync::Mutex<HashMap<String, Vec<String>>> = Default::default();
        futures::stream::iter(by_person)
            .for_each_concurrent(RENOTE_PARALLEL, |(person, sessions)| {
                let (store, scope, done, kept, going) = (&store, &scope, &done, &kept, &going);
                async move {
                    let signed = format!("{}:{person}:", crate::REMEMBER_SOURCE);
                    for t in sessions {
                        let read = async {
                            let typed = typed_of(store, scope, &t).await?;
                            let theirs = |dir: &crate::people::directory::Directory| {
                                crate::sessions::still_theirs(dir, scope, &t.l)
                            };
                            crate::keep::from_user_but(
                                store, scope, &t.l.asker, &signed, typed, theirs, going, None,
                            )
                            .await
                        };
                        match read.await {
                            Ok(ids) => kept
                                .lock()
                                .unwrap()
                                .entry(person.clone())
                                .or_default()
                                .extend(ids),
                            Err(e) => eprintln!("renote: {scope}: a session not read: {e:#}"),
                        }
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % 25 == 0 || d == total {
                            eprintln!("renote: {scope}: read {d} of {total} sessions");
                        }
                    }
                }
            })
            .await;
        // Each old note goes where a note kept for its author now says all it does.
        let kept = kept.into_inner().unwrap();
        let mut replaced = vec![];
        for (person, olds) in &old {
            let Some(new) = kept.get(person).filter(|n| !n.is_empty()) else {
                continue;
            };
            let ids: Vec<String> = new.iter().chain(olds).cloned().collect();
            let bodies = match store.episode_bodies(&scope, ids).await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("renote: {scope}: notes not compared: {e:#}");
                    continue;
                }
            };
            let text = |id: &String| {
                bodies
                    .get(id)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
            };
            let new: Vec<(&String, String)> =
                new.iter().filter_map(|id| Some((id, text(id)?))).collect();
            for o in olds {
                let Some(ot) = text(o) else { continue };
                if new
                    .iter()
                    .any(|(id, nt)| *id != o && crate::keep::repeats(nt, &ot))
                {
                    replaced.push(crate::AclTarget::Episode(o.clone()));
                }
            }
        }
        let n = replaced.len();
        match store.retract_all(&scope, &actor, &replaced).await {
            Ok(_) => eprintln!(
                "renote: {scope}: done: {n} old notes replaced, {} kept as they were",
                answer.old_notes - n
            ),
            Err(e) => eprintln!("renote: {scope}: the replaced notes not deleted: {e:#}"),
        }
    }));
    Ok(answer)
}
