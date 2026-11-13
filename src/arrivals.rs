//! What arrived in a window of time, from the episodes' own rows rather than a search of what they say: per kind of
//! source (`linear`, `sentry`, `slack`), how many episodes occurred in the window and when the latest did, and which
//! of them no extraction has read for facts yet. Facts are extracted after an episode arrives, and a search finds an
//! episode with no facts only by its words, so "what came in since nine?" is answered here, from what ingest
//! recorded, never from whether a search found it. `search_memory` adds it to a dated search.
//!
//! Only episodes the reader may read count, as they are visible in the session asked about (its ACLs, retractions
//! and retention). An arrival is an episode of source data: not the engine's own documents, sessions, transcripts
//! or notes the server kept, which extraction never reads ([`crate::extract::typed`]).
//!
//! Cost: no body is read to count. The episodes' rows are already held in memory (the passage search reads them on
//! every search), and one pass over their `occurred_at` column picks the window. Whether an episode was read for
//! facts is the extractions table's: each read an extraction commits names its episode. Those rows are indexed by
//! episode once per version of the table and kept, so a call looks up only the episodes in its window. Only the
//! listed episodes (at most [`Ask::list`]) are read, for their titles, once per server: an episode's id is its
//! text's hash.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use arrow_array::cast::AsArray;
use arrow_array::types::{Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, RecordBatch};

use crate::{EPISODES, EXTRACTIONS, Episode, FAILED_TRY, Principal, Session, Store};

/// What to count: the episodes that occurred from `from` to `to` (microseconds since the epoch, both included; None:
/// no bound), of these kinds of source ([`kind_of`]) and episode types (`slack_thread`; empty: any), and how many
/// of those not read for facts to name.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ask<'a> {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub kinds: &'a [&'a str],
    pub types: &'a [&'a str],
    pub list: usize,
}

/// One kind of source's arrivals: how many, the latest one's time, and how many no extraction has read yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kind {
    pub kind: String,
    pub episodes: usize,
    pub latest: i64,
    pub unread: usize,
}

/// An arrival no extraction has read for facts yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Unread {
    pub episode_id: String,
    pub source: String,
    pub occurred_at: i64,
    /// Its first line (an email's subject, an issue's title), once [`titles`] has read it.
    pub title: Option<String>,
}

/// What arrived in a window: per kind of source, the most episodes first; in all; and the newest of those not read
/// for facts, at most [`Ask::list`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arrivals {
    pub kinds: Vec<Kind>,
    pub episodes: usize,
    pub unread: usize,
    pub listed: Vec<Unread>,
}

impl Arrivals {
    /// Another scope's arrivals added to these: counts summed, the newest unread listed.
    pub fn add(&mut self, other: Arrivals, list: usize) {
        self.episodes += other.episodes;
        self.unread += other.unread;
        for k in other.kinds {
            match self.kinds.iter_mut().find(|m| m.kind == k.kind) {
                Some(m) => {
                    m.episodes += k.episodes;
                    m.unread += k.unread;
                    m.latest = m.latest.max(k.latest);
                }
                None => self.kinds.push(k),
            }
        }
        most_first(&mut self.kinds);
        self.listed.extend(other.listed);
        newest_first(&mut self.listed);
        self.listed.truncate(list);
    }
}

/// The kinds with the most episodes first, then the latest.
fn most_first(kinds: &mut [Kind]) {
    kinds.sort_by_key(|k| std::cmp::Reverse((k.episodes, k.latest)));
}

/// Newest first, the same order every time.
fn newest_first(listed: &mut [Unread]) {
    listed.sort_by(|a, b| (b.occurred_at, &a.episode_id).cmp(&(a.occurred_at, &b.episode_id)));
}

/// The kind of source an episode is from: its source up to the first `:` (`linear` of `linear:ENG-12`), lowercase,
/// as passage search groups sources. A note an agent or a script remembered is of the kind of the source it named
/// (`linear` of `mcp:remember_memory:key:acme/feed:linear:ENG-12`), `note` when it named none.
pub fn kind_of(source: &str) -> String {
    let named = crate::profile::author_of(source).map_or(source, |(_, rest)| rest);
    match named
        .split(':')
        .next()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        Some(k) => k.to_lowercase(),
        None => "note".into(),
    }
}

/// Whether an episode from `source` is source data that arrived: one extraction reads for facts, and not a note
/// of a run, a session or the server's own.
fn arrival(session: &Session, source: &str) -> bool {
    if crate::profile::author_of(source).is_some_and(|(_, rest)| !crate::profile::is_note(rest)) {
        return false;
    }
    let ep = Episode {
        episode_id: String::new(),
        source: source.to_string(),
        occurred_at: 0,
        acl: vec![],
        about: vec![],
        index_text: true,
        body: vec![],
        recorded_at: None,
        document: None,
    };
    crate::extract::typed(session.scope(), &ep)
}

/// What arrived in `session` as `who` may read it, as `ask` asks. `seen` (when reading several scopes) holds the
/// episodes counted already, by id and by source, and gains these: one held in two scopes counts once.
pub async fn of(
    store: &Store,
    session: &Session,
    who: &Principal,
    ask: &Ask<'_>,
    mut seen: Option<&mut HashSet<String>>,
) -> Result<Arrivals> {
    let vis = session.vis();
    let ns = session.scope().namespace();
    let rows = store.rows(&ns, EPISODES).await?.rows;
    // (id, source, occurred_at) of each episode in the window the reader may read, once.
    let mut found: Vec<(&str, &str, i64)> = vec![];
    let mut once: HashSet<&str> = HashSet::new();
    for b in rows.iter() {
        let (Some(at), Some(id), Some(source)) = (
            b.column_by_name("occurred_at"),
            b.column_by_name("episode_id"),
            b.column_by_name("source"),
        ) else {
            continue;
        };
        let at = at.as_primitive::<TimestampMicrosecondType>();
        let (id, source) = (id.as_string::<i32>(), source.as_string::<i32>());
        for i in 0..b.num_rows() {
            if at.is_null(i) {
                continue;
            }
            let t = at.value(i);
            if ask.from.is_some_and(|f| t < f) || ask.to.is_some_and(|u| t > u) {
                continue;
            }
            let (e, s) = (id.value(i), source.value(i));
            if !vis.can_see_episode(e, who) || !once.insert(e) {
                continue;
            }
            found.push((e, s, t));
        }
    }
    let read = read_for_facts(store, &ns).await?;
    let mut kinds: BTreeMap<String, Kind> = BTreeMap::new();
    let mut out = Arrivals::default();
    // (occurred_at, id, source) of each not read for facts.
    let mut unread: Vec<(i64, &str, &str)> = vec![];
    for (e, source, t) in found {
        if !ask.types.is_empty() && !vis.episode_type(e).is_some_and(|k| ask.types.contains(&k)) {
            continue;
        }
        let kind = kind_of(source);
        if (!ask.kinds.is_empty() && !ask.kinds.contains(&kind.as_str()))
            || !arrival(session, source)
        {
            continue;
        }
        if let Some(seen) = seen.as_deref_mut() {
            let keys = [format!("id:{e}"), format!("source:{source}")];
            // A note's source is its author's own label, no record of anything else: only its id counts.
            let by_source = !source.starts_with(crate::REMEMBER_SOURCE);
            if seen.contains(&keys[0]) || (by_source && seen.contains(&keys[1])) {
                continue;
            }
            seen.insert(keys[0].clone());
            if by_source {
                seen.insert(keys[1].clone());
            }
        }
        let done = read
            .get(e)
            .is_some_and(|commits| commits.iter().any(|c| vis.committed.contains(c)));
        let k = kinds.entry(kind.clone()).or_insert_with(|| Kind {
            kind,
            ..Default::default()
        });
        k.episodes += 1;
        k.latest = k.latest.max(t);
        out.episodes += 1;
        if !done {
            k.unread += 1;
            out.unread += 1;
            unread.push((t, e, source));
        }
    }
    out.kinds = kinds.into_values().collect();
    most_first(&mut out.kinds);
    // Newest first, the same order every time.
    unread.sort_by(|a, b| (b.0, a.1).cmp(&(a.0, b.1)));
    out.listed = unread
        .into_iter()
        .take(ask.list)
        .map(|(t, e, source)| Unread {
            episode_id: e.to_string(),
            source: source.to_string(),
            occurred_at: t,
            title: None,
        })
        .collect();
    Ok(out)
}

/// Per namespace, the extractions table's rows as last indexed, and the commits each episode was read for facts in.
type ReadIndex = HashMap<String, (Arc<Vec<RecordBatch>>, Arc<HashMap<String, Vec<String>>>)>;
static READ: LazyLock<Mutex<ReadIndex>> = LazyLock::new(Default::default);

/// What the skills compiler's reads are recorded as ([`crate::compile::compiler_id_with`]).
const COMPILER: &str = "compile:";

/// The commits in which each episode of `ns` was read for facts: a read an extraction recorded (none found is a read
/// too), not a failed try, and not the skills compiler's, which reads episodes for skills, not facts. A read counts
/// where its commit is committed in the session asked about.
async fn read_for_facts(store: &Store, ns: &str) -> Result<Arc<HashMap<String, Vec<String>>>> {
    let rows = store.side_rows(ns, EXTRACTIONS).await?;
    if let Some((held, index)) = READ.lock().unwrap().get(ns)
        && Arc::ptr_eq(held, &rows)
    {
        return Ok(index.clone());
    }
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for b in rows.iter() {
        let (Some(commit), Some(ep), Some(by), Some(facts)) = (
            b.column_by_name("commit_id"),
            b.column_by_name("episode_id"),
            b.column_by_name("extractor"),
            b.column_by_name("facts"),
        ) else {
            continue;
        };
        let (commit, ep, by) = (
            commit.as_string::<i32>(),
            ep.as_string::<i32>(),
            by.as_string::<i32>(),
        );
        let facts = facts.as_primitive::<Int64Type>();
        for i in 0..b.num_rows() {
            if by.value(i).starts_with(COMPILER)
                || (!facts.is_null(i) && facts.value(i) == FAILED_TRY)
            {
                continue;
            }
            let commits = index.entry(ep.value(i).to_string()).or_default();
            if !commits.iter().any(|c| c == commit.value(i)) {
                commits.push(commit.value(i).to_string());
            }
        }
    }
    let index = Arc::new(index);
    READ.lock()
        .unwrap()
        .insert(ns.to_string(), (rows, index.clone()));
    Ok(index)
}

/// Titles read once per server, by episode id: an episode's id is its text's hash, so its title never changes.
static TITLES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
/// Titles held at most; past it, they are read again.
const MAX_TITLES: usize = 10_000;
/// A title's characters at most.
const TITLE_CHARS: usize = 120;

/// The listed episodes' titles ([`title`]), from `session`'s scope. Only the episodes the reader may read in it are
/// read.
pub async fn titles(
    store: &Store,
    session: &Session,
    who: &Principal,
    listed: &mut [Unread],
) -> Result<()> {
    let missing: Vec<String> = {
        let held = TITLES.lock().unwrap();
        listed
            .iter()
            .filter(|u| u.title.is_none() && !held.contains_key(&u.episode_id))
            .filter(|u| session.vis().can_see_episode(&u.episode_id, who))
            .map(|u| u.episode_id.clone())
            .collect()
    };
    if !missing.is_empty() {
        let bodies = store.episode_bodies(session.scope(), missing).await?;
        let mut held = TITLES.lock().unwrap();
        if held.len() + bodies.len() > MAX_TITLES {
            held.clear();
        }
        for (id, body) in bodies {
            let Some(u) = listed.iter().find(|u| u.episode_id == id) else {
                continue;
            };
            held.insert(id, title(&u.source, &body));
        }
    }
    let held = TITLES.lock().unwrap();
    for u in listed.iter_mut().filter(|u| u.title.is_none()) {
        if session.vis().can_see_episode(&u.episode_id, who) {
            u.title = held.get(&u.episode_id).cloned();
        }
    }
    Ok(())
}

/// An episode's title: its first line, or of a record kept as JSON (a thread, a call), what its type says the
/// title is, else its first line as its type reads it.
fn title(source: &str, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let first = match text.trim_start().starts_with('{') {
        true => {
            let mut ep = Episode::new(source, body, &[]);
            ep.source = source.to_string();
            let typed = crate::episode_types::of(&ep);
            typed
                .map(|t| t.header(&ep))
                .and_then(|h| h.title.or(h.subject))
                .filter(|t| !t.trim().is_empty())
                .or_else(|| {
                    typed
                        .and_then(|t| t.units(&ep).ok())
                        .and_then(|u| u.into_iter().map(|u| u.text).find(|t| !t.trim().is_empty()))
                })
                .unwrap_or_default()
        }
        false => text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_string(),
    };
    let first = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = first.trim_start_matches('#').trim();
    match first.char_indices().nth(TITLE_CHARS) {
        Some((i, _)) => format!("{}…", &first[..i]),
        None => first.to_string(),
    }
}
