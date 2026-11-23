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

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::browse::{bad, episodes, facts};
use crate::{Principal, Session, Store};

/// Top entities per source unless the request asks for more or fewer (at most [`MAX_TOP`]).
const TOP: usize = 5;
const MAX_TOP: usize = 20;
/// Prefixes per request, and sources answered without them.
const MAX_PREFIXES: usize = 200;

/// One source's tally.
#[derive(Default)]
struct Tally {
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
    entities: HashMap<String, usize>,
}

/// Answers `{op: "sources", prefixes?, top?}` as `who`.
pub async fn run(store: &Store, session: &Session, who: &Principal, req: &Value) -> Result<Value> {
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
    let (eps, facts) = tokio::try_join!(episodes(store, session, who), facts(store, session, who))?;

    // Each source's key, and which keys each episode the caller reads counts toward.
    let mut keys: Vec<String> = prefixes.clone().unwrap_or_default();
    let mut index: HashMap<String, usize> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.clone(), i))
        .collect();
    let mut of_episode: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut tallies: Vec<Tally> = keys.iter().map(|_| Tally::default()).collect();
    for e in &eps {
        let at: Vec<usize> = match &prefixes {
            Some(p) => p
                .iter()
                .enumerate()
                .filter(|(_, p)| e.source.starts_with(p.as_str()))
                .map(|(i, _)| i)
                .collect(),
            None => {
                let g = group(&e.source);
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
        if !at.is_empty() {
            of_episode.insert(e.id.as_str(), at);
        }
    }
    for f in &facts {
        let mut at: Vec<usize> = f
            .source_episodes
            .iter()
            .filter_map(|e| of_episode.get(e.as_str()))
            .flatten()
            .copied()
            .collect();
        at.sort_unstable();
        at.dedup();
        for i in at {
            let t = &mut tallies[i];
            t.facts += 1;
            t.current += usize::from(f.valid_to.is_none());
            t.last_fact = t.last_fact.max(Some(f.valid_from));
            *t.entities.entry(f.entity.clone()).or_default() += 1;
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
            let mut most: Vec<(&String, &usize)> = t.entities.iter().collect();
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
    Ok(json!({"sources": sources}))
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
