//! What each source taught the memory (`/browse {op: "sources"}`): for every source, how many of its episodes the
//! caller may read and when the last was written, the facts drawn from them, the entities those facts are about,
//! and the entities it said the most about. A console shows it beside each connected source, and opens exactly those
//! facts in Studio's Explorer (`ls` at `entities` with `from`: [`crate::browse::Narrow`]).
//!
//! A source is a prefix of episodes' sources: `prefixes` names them (a connector knows the prefix of everything it
//! writes: `slack:C09`, `github:acme/app`), one answer each in that order. Without them, every source the caller may
//! read is grouped by its connector and what it read ([`group`]: `slack:C09:#deals:1712.3` is `slack:C09`).
//!
//! Every count is over what the caller may read and nothing else: an episode they can't read is in no count and
//! names no source, and a fact counts only when they may read it (every episode it cites), as every read decides. A
//! prefix with nothing the caller may read answers zeros, the same whether or not someone else's memory is there.
//!
//! Each source also says when its oldest and newest episodes happened (`first_occurred`, `last_occurred`), and, given
//! `since` (µs), how many happened from then on (`since_episodes`): whether the memory holds a window of it, or
//! needs it filled from further back (`ampup-memory-client index` reads it so).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use serde_json::{Value, json};

use crate::browse::bad;
use crate::{Principal, Session, Store};

/// Top entities per source unless the request asks for more or fewer (at most [`MAX_TOP`]).
const TOP: usize = 5;
const MAX_TOP: usize = 20;
/// Prefixes per request, and sources answered without them.
const MAX_PREFIXES: usize = 200;

/// One source's tally.
#[derive(Default)]
struct Tally<'a> {
    episodes: usize,
    last_written: Option<i64>,
    last_occurred: Option<i64>,
    /// When its oldest episode happened: how far back the memory holds it.
    first_occurred: Option<i64>,
    /// Its episodes that happened at or after the request's `since`.
    since: usize,
    facts: usize,
    current: usize,
    last_fact: Option<i64>,
    entities: HashMap<&'a str, usize>,
}

/// Each reader's answer, by memory state ([`crate::kept_answers`]): a page's summary (a console shows it beside
/// each connected source, and asks again every minute), answered at once on a return visit within the hour while
/// the next is worked out, never past a loss.
static KEPT: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::summary(500));

/// Answers `{op: "sources", prefixes?, top?, since?}` as `who`: on main as it is now, kept by memory state (it reads
/// every episode and every fact the caller may read), by the reader (who, and every group they read as) and what
/// they asked; on a branch or at a past state, worked out each time.
pub async fn run(store: &Store, session: &Session, who: &Principal, req: &Value) -> Result<Value> {
    let ask = Ask::of(req)?;
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
    crate::phases::mark("sources state");
    let Some(at) = at else {
        return tally(store, session, who, &ask).await;
    };
    let key = format!(
        "{}\n{}\n{}\n{}",
        crate::kept_answers::reader_key(who),
        ask.prefixes
            .as_ref()
            .map_or_else(|| "*".to_string(), |p| p.join("\u{1f}")),
        ask.top,
        ask.since.map_or_else(|| "-".to_string(), |s| s.to_string())
    );
    let (store2, session2, who2) = (store.clone(), session.clone(), who.clone());
    let answer = KEPT
        .get(
            &session.scope().namespace(),
            &key,
            at,
            true,
            || async move { tally(&store2, &session2, &who2, &ask).await },
        )
        .await?;
    crate::phases::mark("sources answered");
    Ok((*answer).clone())
}

/// What a `sources` request asks.
struct Ask {
    prefixes: Option<Vec<String>>,
    top: usize,
    since: Option<i64>,
}

impl Ask {
    fn of(req: &Value) -> Result<Ask> {
        let prefixes: Option<Vec<String>> = match &req["prefixes"] {
            Value::Null => None,
            Value::Array(a) => Some(
                a.iter()
                    .map(|s| s.as_str().filter(|s| !s.is_empty()).map(String::from))
                    .collect::<Option<_>>()
                    .ok_or_else(|| bad("prefixes is a list of source prefixes"))?,
            ),
            _ => return Err(bad("prefixes is a list of source prefixes")),
        };
        if prefixes.as_ref().is_some_and(|p| p.len() > MAX_PREFIXES) {
            return Err(bad(format!("at most {MAX_PREFIXES} prefixes")));
        }
        let top = match &req["top"] {
            Value::Null => TOP,
            v => v
                .as_u64()
                .filter(|n| *n as usize <= MAX_TOP)
                .ok_or_else(|| bad(format!("top is a whole number, 0 to {MAX_TOP}")))?
                as usize,
        };
        // `since` (µs): also count each source's episodes that happened at or after it, so a caller sees what a window holds.
        let since = match &req["since"] {
            Value::Null => None,
            v => Some(
                v.as_i64()
                    .ok_or_else(|| bad("since is a time in microseconds"))?,
            ),
        };
        Ok(Ask {
            prefixes,
            top,
            since,
        })
    }
}

/// Whether each tally is checked against one worked out from every episode and fact decoded as every other read
/// decodes them ([`crate::browse::episodes`], [`crate::Session::read`]), from the same rows at the same state
/// (`AMPUP_MEMORY_SOURCES_ORACLE`: `1` on, `0` off; on by default in debug builds, so every test that asks for
/// sources checks it, and off in release builds). A difference fails a debug build's request; a release build serves
/// the decoded one, and a line says so.
fn oracle() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        match std::env::var("AMPUP_MEMORY_SOURCES_ORACLE").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => cfg!(debug_assertions),
        }
    });
    *ON
}

/// An episode the caller reads, as the tally counts it.
struct EpRow<'a> {
    /// Its place in the state's index ([`crate::Reading::sees`]).
    place: u32,
    source: &'a str,
    occurred_at: i64,
    recorded_at: i64,
}

/// A fact the caller reads, as the tally counts it.
struct FactSeen<'a> {
    entity: &'a str,
    /// Not ended for the caller ([`crate::Vis::valid_to`]).
    current: bool,
    valid_from: i64,
    /// The places of the episodes it cites, in [`Read::places`].
    cited: std::ops::Range<usize>,
}

/// The episodes and facts the caller reads.
struct Read<'a> {
    eps: Vec<EpRow<'a>>,
    facts: Vec<FactSeen<'a>>,
    /// Every fact's cited episodes' places, one after another.
    places: Vec<u32>,
}

/// The answer to `ask`, worked out from every episode and fact `who` reads in `session`: read in place from the
/// tables' rows as cached, each row checked as every read checks it ([`crate::Reading::allows_row`]) but never
/// decoded.
async fn tally(store: &Store, session: &Session, who: &Principal, ask: &Ask) -> Result<Value> {
    let started = std::time::Instant::now();
    let ns = session.scope().namespace();
    let (files, facts) = tokio::try_join!(
        store.episode_files(session.scope()),
        store.rows(&ns, crate::FACTS)
    )?;
    let reading = session.vis().reading(who);
    let mut seen = Read {
        eps: episodes_in_place(&files, &reading),
        facts: vec![],
        places: vec![],
    };
    facts_in_place(&facts.rows, &reading, &mut seen);
    let read = started.elapsed();
    let answer = fold(&seen, ask);
    let folded = started.elapsed();
    let (n_eps, n_facts, n_cited) = (seen.eps.len(), seen.facts.len(), seen.places.len());
    let (n_rows, n_batches) = (
        facts.rows.iter().map(|b| b.num_rows()).sum::<usize>(),
        facts.rows.len(),
    );
    drop(seen);
    let answer = match oracle() {
        true => checked(answer, &files, &facts.rows, session, who, &reading, ask)?,
        false => answer,
    };
    // Worked out away from the request that asked (a kept answer's work), so its steps are said here when slow.
    if folded > std::time::Duration::from_millis(100) {
        eprintln!(
            "slow sources tally: {} in {} ms ({} episodes and {} facts read in {} ms, counted in {} ms; {} fact rows \
             in {} batches, {} citations, {} rows citing what the row before did)",
            session.scope(),
            folded.as_millis(),
            n_eps,
            n_facts,
            read.as_millis(),
            (folded - read).as_millis(),
            n_rows,
            n_batches,
            n_cited,
            reading.reused()
        );
    }
    Ok(answer)
}

/// `answer`, checked against the tally of the same rows decoded ([`oracle`]): the decoded one if they differ (an
/// error in a debug build).
fn checked(
    answer: Value,
    files: &HashMap<String, std::sync::Arc<Vec<RecordBatch>>>,
    rows: &[RecordBatch],
    session: &Session,
    who: &Principal,
    reading: &crate::Reading,
    ask: &Ask,
) -> Result<Value> {
    let eps = crate::browse::episodes_in(files, session, who);
    let mut facts = crate::browse::once_each(session.read_rows(rows, who, None));
    facts.retain(|f| !crate::is_note_fact(f));
    // Each episode told by its place, as the tally tells it: every one the decoded reads let through has one, and
    // one that hasn't is a difference too.
    let unplaced = std::cell::Cell::new(0usize);
    let place = |e: &str| {
        reading.sees(e).unwrap_or_else(|| {
            unplaced.set(unplaced.get() + 1);
            u32::MAX
        })
    };
    let mut seen = Read {
        eps: eps
            .iter()
            .map(|e| EpRow {
                place: place(&e.id),
                source: &e.source,
                occurred_at: e.occurred_at,
                recorded_at: e.recorded_at,
            })
            .collect(),
        facts: vec![],
        places: vec![],
    };
    for f in &facts {
        let start = seen.places.len();
        seen.places
            .extend(f.source_episodes.iter().map(|e| place(e)));
        seen.facts.push(FactSeen {
            entity: &f.entity,
            current: f.valid_to.is_none(),
            valid_from: f.valid_from,
            cited: start..seen.places.len(),
        });
    }
    let decoded = fold(&seen, ask);
    if decoded == answer && unplaced.get() == 0 {
        return Ok(answer);
    }
    // Counts only: a line outlives a forget, so it names no entity.
    let sums = |v: &Value| {
        let s = v["sources"].as_array().cloned().unwrap_or_default();
        let n = |k: &str| s.iter().filter_map(|x| x[k].as_u64()).sum::<u64>();
        format!(
            "{} sources, {} episodes, {} facts, {} current",
            s.len(),
            n("episodes"),
            n("facts"),
            n("current")
        )
    };
    let why = format!(
        "sources oracle: the tally of {} read in place ({}) differs from the decoded one ({}; {} episodes unplaced)",
        session.scope(),
        sums(&answer),
        sums(&decoded),
        unplaced.get()
    );
    // A test (a debug build) fails on it; a server that turned the oracle on serves the decoded one.
    anyhow::ensure!(!cfg!(debug_assertions), "{why}");
    eprintln!("{why}; serving the decoded one");
    Ok(decoded)
}

/// A column of strings.
fn text<'a>(b: &'a RecordBatch, col: &str) -> &'a arrow_array::StringArray {
    b.column_by_name(col).expect(col).as_string::<i32>()
}

/// A column of times in microseconds: timestamps, or plain integers (as a search index holds them).
enum Micros<'a> {
    Stamps(&'a arrow_array::TimestampMicrosecondArray),
    Ints(&'a arrow_array::Int64Array),
}

impl<'a> Micros<'a> {
    fn of(b: &'a RecordBatch, col: &str) -> Micros<'a> {
        let a = b.column_by_name(col).expect(col);
        match a.data_type() {
            arrow_schema::DataType::Int64 => Micros::Ints(a.as_primitive()),
            _ => Micros::Stamps(a.as_primitive()),
        }
    }

    fn at(&self, i: usize) -> Option<i64> {
        match self {
            Micros::Stamps(a) => (!a.is_null(i)).then(|| a.value(i)),
            Micros::Ints(a) => (!a.is_null(i)).then(|| a.value(i)),
        }
    }
}

/// Every committed episode the reading shows, once each, as [`crate::browse::episodes`] reads them (the first row
/// met of one written again), borrowed from the episodes table's files.
fn episodes_in_place<'a>(
    files: &'a HashMap<String, std::sync::Arc<Vec<RecordBatch>>>,
    reading: &crate::Reading,
) -> Vec<EpRow<'a>> {
    let mut once: HashSet<u32> = HashSet::new();
    let mut out = vec![];
    for b in files.values().flat_map(|bs| bs.iter()) {
        let (commit, id, source) = (
            text(b, "commit_id"),
            text(b, "episode_id"),
            text(b, "source"),
        );
        let (at, recorded) = (Micros::of(b, "occurred_at"), Micros::of(b, "recorded_at"));
        for i in 0..b.num_rows() {
            if !reading.committed(commit.value(i)) {
                continue;
            }
            let Some(place) = reading.sees(id.value(i)).filter(|p| once.insert(*p)) else {
                continue;
            };
            out.push(EpRow {
                place,
                source: source.value(i),
                occurred_at: at.at(i).unwrap_or_default(),
                recorded_at: recorded.at(i).unwrap_or_default(),
            });
        }
    }
    out
}

/// Every fact the reading lets through, as [`crate::browse::facts`] reads them (each the first row allowed of its
/// id, notes' own facts left out), borrowed from the facts table's rows, into `out`.
fn facts_in_place<'a>(rows: &'a [RecordBatch], reading: &crate::Reading, out: &mut Read<'a>) {
    let mut once: HashSet<&str> = HashSet::with_capacity(rows.iter().map(|b| b.num_rows()).sum());
    let mut cited: Vec<&str> = vec![];
    let mut places: Vec<u32> = vec![];
    for b in rows {
        let (commit, id, entity, predicate, value, sources) = (
            text(b, "commit_id"),
            text(b, "fact_id"),
            text(b, "entity"),
            text(b, "predicate"),
            text(b, "value"),
            text(b, "source_episodes"),
        );
        // Rows written before quotes existed have none.
        let quote = b.column_by_name("quote").map(|a| a.as_string::<i32>());
        let (from, to) = (Micros::of(b, "valid_from"), Micros::of(b, "valid_to"));
        for i in 0..b.num_rows() {
            let fact_id = id.value(i);
            cited.clear();
            cited.extend(sources.value(i).split(',').filter(|e| !e.is_empty()));
            let row = crate::FactRow {
                commit: commit.value(i),
                fact_id,
                valid_from: from.at(i).unwrap_or_default(),
                valid_to: to.at(i),
                source_episodes: &cited,
                value: value.value(i),
                quote: quote.filter(|q| !q.is_null(i)).map(|q| q.value(i)),
            };
            let Some(ends) = reading.allows_row(&row, &mut places) else {
                continue;
            };
            if !once.insert(fact_id)
                || crate::is_note_row(fact_id, entity.value(i), predicate.value(i), &cited)
            {
                continue;
            }
            let start = out.places.len();
            out.places.extend_from_slice(&places);
            out.facts.push(FactSeen {
                entity: entity.value(i),
                current: ends.is_none(),
                valid_from: row.valid_from,
                cited: start..out.places.len(),
            });
        }
    }
}

/// The answer to `ask` from the episodes and facts the caller reads.
fn fold<'a>(read: &Read<'a>, ask: &Ask) -> Value {
    let (prefixes, top, since) = (&ask.prefixes, ask.top, ask.since);

    // Each source's key, and which keys each episode the caller reads counts toward.
    let mut keys: Vec<String> = prefixes.clone().unwrap_or_default();
    let mut index: HashMap<String, usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.clone(), i))
        .collect();
    // By place: the sources each episode counts toward.
    let mut of_episode: Vec<Vec<usize>> = vec![];
    let mut tallies: Vec<Tally> = keys.iter().map(|_| Tally::default()).collect();
    for e in &read.eps {
        let at: Vec<usize> = match prefixes {
            Some(p) => p
                .iter()
                .enumerate()
                .filter(|(_, p)| e.source.starts_with(p.as_str()))
                .map(|(i, _)| i)
                .collect(),
            None => {
                let g = group(e.source);
                let i = *index.entry(g.clone()).or_insert_with(|| {
                    keys.push(g);
                    tallies.push(Tally::default());
                    keys.len() - 1
                });
                vec![i]
            }
        };
        for &i in &at {
            let t = &mut tallies[i];
            t.episodes += 1;
            t.last_written = t.last_written.max(Some(e.recorded_at));
            t.last_occurred = t.last_occurred.max(Some(e.occurred_at));
            t.first_occurred = Some(
                t.first_occurred
                    .map_or(e.occurred_at, |f| f.min(e.occurred_at)),
            );
            t.since += usize::from(since.is_some_and(|s| e.occurred_at >= s));
        }
        let p = e.place as usize;
        if !at.is_empty() && p < u32::MAX as usize {
            if of_episode.len() <= p {
                of_episode.resize_with(p + 1, Vec::new);
            }
            of_episode[p] = at;
        }
    }
    let mut at: Vec<usize> = vec![];
    for f in &read.facts {
        at.clear();
        for &p in &read.places[f.cited.clone()] {
            if let Some(i) = of_episode.get(p as usize) {
                at.extend(i);
            }
        }
        at.sort_unstable();
        at.dedup();
        for &i in &at {
            let t = &mut tallies[i];
            t.facts += 1;
            t.current += usize::from(f.current);
            t.last_fact = t.last_fact.max(Some(f.valid_from));
            *t.entities.entry(f.entity).or_default() += 1;
        }
    }

    let mut out: Vec<(String, Tally)> = keys.into_iter().zip(tallies).collect();
    // Grouped, the sources that taught the most first; asked by prefix, in the order asked.
    if prefixes.is_none() {
        out.sort_by(|(a, x), (b, y)| (y.facts, y.episodes, a).cmp(&(x.facts, x.episodes, b)));
        out.truncate(MAX_PREFIXES);
    }
    let sources: Vec<Value> = out
        .into_iter()
        .map(|(source, t)| {
            let mut most: Vec<(&&str, &usize)> = t.entities.iter().collect();
            most.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.cmp(b)));
            let top_entities: Vec<Value> = most
                .into_iter()
                .take(top)
                .map(|(e, n)| json!({"entity": e, "facts": n}))
                .collect();
            let mut v = json!({
                "source": source,
                "type": crate::episode_types::name_of_source(&source),
                "episodes": t.episodes,
                "last_written": t.last_written,
                "last_occurred": t.last_occurred,
                "facts": t.facts,
                "current": t.current,
                "last_fact": t.last_fact,
                "entities": t.entities.len(),
                "top_entities": top_entities,
                "first_occurred": t.first_occurred,
            });
            if since.is_some() {
                v["since_episodes"] = json!(t.since);
            }
            v
        })
        .collect();
    json!({"sources": sources})
}

/// The source an episode's `source` is grouped under when no prefixes are asked for: the connector and what it read,
/// its first two segments (`slack:C09:#deals:1712.3` is `slack:C09`), less an item's number (`github:acme/app#12`
/// is `github:acme/app`); a source that is a URL is grouped by everything before its last path segment
/// (`meeting:https://app.example.com/meetings/42` is `meeting:https://app.example.com/meetings`).
pub fn group(source: &str) -> String {
    let Some((kind, rest)) = source.split_once(':') else {
        return source.to_string();
    };
    if let Some(url) = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))
    {
        let scheme = &rest[..rest.len() - url.len()];
        return match url.rsplit_once('/') {
            Some((head, _)) => format!("{kind}:{scheme}{head}"),
            None => format!("{kind}:{scheme}{url}"),
        };
    }
    let second = rest.split(':').next().unwrap_or_default();
    let second = second.split('#').next().unwrap_or_default();
    format!("{kind}:{second}")
}
