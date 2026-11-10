//! Filterable lists of what a scope remembers and has learned, as `POST /facts/list` and `POST /advice/list`
//! (the `facts` and `advice` ops of `/browse`, which gives them its scope and branch selection, its ACLs and its
//! metering). Both page with a `cursor` over a total order, so pages together are the whole answer.
//!
//! `facts`: the facts the caller may read (over all valid time, once each), narrowed by any of
//! - `entity` (an id or a list), `entity_type`, `predicate` (each a string or a list: any of them);
//! - `source`: source prefixes (`mcp:remember_memory:` for conversations, `slack:`, `ampup:meeting:`): the fact
//!   cites an episode whose source starts with one of them;
//! - `tags`: the fact carries any of them;
//! - `status`: `open` (no end), `ended`, or `any` (default); `valid_from`: `{gte, gt, lte, lt}`, each a time in
//!   microseconds, an RFC 3339 time or a `YYYY-MM-DD` date;
//! - `text`: a case-insensitive substring of the value or its quote;
//!
//! newest `valid_from` first (`order: "oldest"` reverses it). Each fact says which episodes it cites and their
//! sources. `limit` is 50 by default and at most 500.
//!
//! `advice`: the situations whose advice the caller may read, from their own space (`space: "mine"`), the
//! team's (`"team"`) or both (`"all"`, the default), narrowed by `category`, `situation`, `text` (a substring of
//! the situation's `when` or its note), `ratified` (past the evidence bar: only org admins also see the
//! candidates, whose evidence is still being gathered), `min_helped`, `min_served` and `learned` (`{gte, gt, lte,
//! lt}` on when the note was written). With `include: "text"` each entry carries its note. A person's own
//! entries show no evidence: a tally is the team's, and counts other people's sessions. Where the scope curates
//! advice (`crate::advice_curate`), a note withheld by reward says so (`withheld`: its served runs against the rest),
//! and a situation merged into another is listed with its note's history and `merged_into`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::browse::{bad, episodes, fact_json, once_each};
use crate::search::{Cited, Col, Filter, Lit, Op};
use crate::{Fact, Principal, Session, Store};

const LIMIT: usize = 50;
const MAX_FACTS: usize = 500;
const MAX_ADVICE: usize = 200;

/// A field that is a string or a list of strings: the strings, none when it is absent.
fn strings(req: &Value, field: &str) -> Result<Vec<String>> {
    match &req[field] {
        Value::Null => Ok(vec![]),
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) => a
            .iter()
            .map(|s| s.as_str().map(String::from))
            .collect::<Option<_>>()
            .ok_or_else(|| bad(format!("{field} is a string or a list of strings"))),
        _ => Err(bad(format!("{field} is a string or a list of strings"))),
    }
}

/// A time as microseconds: an integer already, an RFC 3339 time or a date.
fn micros(v: &Value, field: &str) -> Result<i64> {
    let parsed = match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => crate::query::schema::parse_time(s).map(|t| t.timestamp_micros()),
        _ => None,
    };
    parsed.ok_or_else(|| {
        bad(format!(
            "{field} is microseconds, an RFC 3339 time or a YYYY-MM-DD date"
        ))
    })
}

/// A window `{gte, gt, lte, lt}` over a time: its bounds as (op, µs).
fn window(req: &Value, field: &str) -> Result<Vec<(Op, i64)>> {
    let w = &req[field];
    if w.is_null() {
        return Ok(vec![]);
    }
    let Some(o) = w.as_object() else {
        return Err(bad(format!("{field} is {{gte, gt, lte, lt}}")));
    };
    let mut out = vec![];
    for (k, v) in o {
        let op = match k.as_str() {
            "gte" => Op::Ge,
            "gt" => Op::Gt,
            "lte" => Op::Le,
            "lt" => Op::Lt,
            _ => return Err(bad(format!("{field} is {{gte, gt, lte, lt}}"))),
        };
        out.push((op, micros(v, &format!("{field}.{k}"))?));
    }
    Ok(out)
}

fn holds(at: i64, bounds: &[(Op, i64)]) -> bool {
    bounds.iter().all(|(op, t)| match op {
        Op::Ge => at >= *t,
        Op::Gt => at > *t,
        Op::Le => at <= *t,
        Op::Lt => at < *t,
        Op::Ne => at != *t,
    })
}

fn limit(req: &Value, max: usize) -> Result<usize> {
    match &req["limit"] {
        Value::Null => Ok(LIMIT),
        v => v
            .as_u64()
            .filter(|n| *n >= 1)
            .map(|n| (n as usize).min(max))
            .ok_or_else(|| bad("limit is a positive integer")),
    }
}

/// The cursor a request continues from: `<time>:<id>`, the last entry of the previous page.
fn cursor(req: &Value) -> Result<Option<(i64, String)>> {
    match &req["cursor"] {
        Value::Null => Ok(None),
        Value::String(c) => c
            .split_once(':')
            .and_then(|(t, id)| Some((t.parse().ok()?, id.to_string())))
            .map(Some)
            .ok_or_else(|| bad("cursor is one a previous page returned")),
        _ => Err(bad("cursor is one a previous page returned")),
    }
}

/// `facts`: see the module's description.
pub async fn facts(
    store: &Store,
    session: &Session,
    who: &Principal,
    req: &Value,
) -> Result<Value> {
    let (entities, types, predicates, prefixes, tags) = (
        strings(req, "entity")?,
        strings(req, "entity_type")?,
        strings(req, "predicate")?,
        strings(req, "source")?,
        strings(req, "tags")?,
    );
    let status = req["status"].as_str().unwrap_or("any");
    if !["open", "ended", "any"].contains(&status) {
        return Err(bad("status is open, ended or any"));
    }
    let oldest = match req["order"].as_str() {
        None | Some("newest") => false,
        Some("oldest") => true,
        Some(_) => return Err(bad("order is newest or oldest")),
    };
    let from = window(req, "valid_from")?;
    let text = req["text"].as_str().map(str::to_lowercase);
    let (limit, after) = (limit(req, MAX_FACTS)?, cursor(req)?);

    // The episodes the caller may read, for the sources filter and to say what each fact cites.
    let eps: BTreeMap<String, String> = episodes(store, session, who)
        .await?
        .into_iter()
        .map(|e| (e.id, e.source))
        .collect();

    // What the index can narrow on goes into one filter; the rest is checked on what comes back.
    let mut all = vec![];
    if !predicates.is_empty() {
        all.push(Filter::In(
            Col::Predicate,
            predicates.iter().cloned().map(Lit::Str).collect(),
        ));
    }
    if !entities.is_empty() {
        all.push(Filter::In(
            Col::Entity,
            entities.iter().cloned().map(Lit::Str).collect(),
        ));
    }
    if !tags.is_empty() {
        all.push(Filter::Or(
            tags.iter()
                .map(|t| Filter::Contains(Col::Tags, format!(",{t},")))
                .collect(),
        ));
    }
    for (op, t) in &from {
        all.push(Filter::Cmp(Col::ValidFrom, *op, Lit::Int(*t)));
    }
    match status {
        "open" => all.push(Filter::Null(Col::ValidTo)),
        "ended" => all.push(Filter::NotNull(Col::ValidTo)),
        _ => {}
    }
    if !prefixes.is_empty() {
        let ids: BTreeSet<String> = eps
            .iter()
            .filter(|(_, s)| prefixes.iter().any(|p| s.starts_with(p.as_str())))
            .map(|(id, _)| id.clone())
            .collect();
        all.push(Filter::Cites(Cited {
            label: format!("source {}", prefixes.join(" ")),
            ids: Arc::new(ids),
        }));
    }
    let found = if all.is_empty() {
        session.read(store, who, None).await?
    } else {
        session
            .read_where(store, who, &Filter::And(all), None)
            .await?
    };
    let type_of = |f: &Fact| {
        f.entity_type
            .clone()
            .unwrap_or_else(|| f.entity.split_once(':').map_or("_", |(t, _)| t).to_string())
    };
    let mut facts: Vec<Fact> = once_each(found)
        .into_iter()
        .filter(|f| types.is_empty() || types.contains(&type_of(f)))
        .filter(|f| {
            text.as_ref().is_none_or(|t| {
                f.value.to_lowercase().contains(t)
                    || f.quote
                        .as_ref()
                        .is_some_and(|q| q.to_lowercase().contains(t))
            })
        })
        .collect();
    let total = facts.len();
    // A total order: newest first (or oldest), ties by id.
    facts.sort_by(|a, b| {
        let by_time = if oldest {
            a.valid_from.cmp(&b.valid_from)
        } else {
            b.valid_from.cmp(&a.valid_from)
        };
        by_time.then_with(|| a.fact_id.cmp(&b.fact_id))
    });
    if let Some((t, id)) = &after {
        facts.retain(|f| {
            let key = (f.valid_from, &f.fact_id);
            let at = (*t, id);
            if oldest {
                key > at
            } else {
                key.0 < at.0 || (key.0 == at.0 && key.1 > at.1)
            }
        });
    }
    let more = facts.len() > limit;
    facts.truncate(limit);
    let next = more
        .then(|| {
            facts
                .last()
                .map(|f| format!("{}:{}", f.valid_from, f.fact_id))
        })
        .flatten();
    let page: Vec<Value> = facts
        .iter()
        .map(|f| {
            let mut v = fact_json(f);
            v["entity_type"] = json!(type_of(f));
            v["tags"] = json!(f.tags);
            v["sources"] = json!(
                f.source_episodes
                    .iter()
                    .map(|id| json!({"episode_id": id, "source": eps.get(id)}))
                    .collect::<Vec<_>>()
            );
            v
        })
        .collect();
    Ok(json!({"facts": page, "count": total, "cursor": next}))
}

/// `advice`: see the module's description. `admin`: the caller administers the org, so the team's candidates
/// (notes short of the evidence bar) are theirs to review too.
pub async fn advice(
    store: &Store,
    session: &Session,
    who: &Principal,
    req: &Value,
    admin: bool,
) -> Result<Value> {
    let space = req["space"].as_str().unwrap_or("all");
    if !["mine", "team", "all"].contains(&space) {
        return Err(bad("space is mine, team or all"));
    }
    let (categories, situations) = (strings(req, "category")?, strings(req, "situation")?);
    let text = req["text"].as_str().map(str::to_lowercase);
    let ratified = match &req["ratified"] {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        _ => return Err(bad("ratified is true or false")),
    };
    let count = |field: &str| -> Result<usize> {
        match &req[field] {
            Value::Null => Ok(0),
            v => v
                .as_u64()
                .map(|n| n as usize)
                .ok_or_else(|| bad(format!("{field} is a whole number"))),
        }
    };
    let (min_helped, min_served) = (count("min_helped")?, count("min_served")?);
    let learned = window(req, "learned")?;
    let with_text = match req["include"].as_str() {
        None => false,
        Some("text") => true,
        Some(_) => return Err(bad("include is text")),
    };
    let (limit, after) = (limit(req, MAX_ADVICE)?, cursor(req)?);

    let mut entries: Vec<(i64, Value)> = vec![];
    let mut add = |space: &str,
                   l: crate::advice::Listed,
                   evidence: Option<&crate::sessions::Tally>,
                   ratified_now: bool| {
        let at = l.note.as_ref().map_or(0, |(_, at, _)| *at);
        let note = l.note.as_ref().map(|(_, _, t)| t.as_str()).unwrap_or("");
        if !categories.is_empty() && !categories.contains(&l.category)
            || !situations.is_empty() && !situations.contains(&l.situation)
            || text.as_ref().is_some_and(|t| {
                !l.when.to_lowercase().contains(t) && !note.to_lowercase().contains(t)
            })
            || ratified.is_some_and(|r| r != ratified_now)
            || !learned.is_empty() && !holds(at, &learned)
            || evidence.map_or(0, |e| e.helped) < min_helped
            || evidence.map_or(0, |e| e.served) < min_served
        {
            return;
        }
        let mut v = json!({"space": space, "key": l.key, "category": l.category, "situation": l.situation,
            "when": l.when, "skills": l.skills, "learned_at": l.note.as_ref().map(|(_, at, _)| at),
            "note_id": l.note.as_ref().map(|(id, _, _)| id), "note_chars": note.chars().count(),
            "runs": l.runs, "wrong": l.wrong, "ratified": ratified_now, "evidence": evidence});
        // Whether the holdout proves it by outcomes people gave (reported only; null until there are enough).
        if let Some(e) = evidence {
            v["proven"] = json!(e.proven());
        }
        // Where the scope curates advice: a note withheld by reward (and why), or a situation merged into another.
        if let Some(w) = &l.withheld {
            v["withheld"] = json!(w);
        }
        if l.held {
            v["held"] = json!(true);
        }
        if let Some(why) = &l.needs_backing {
            v["needs_backing"] = json!(why);
        }
        // A team situation whose task is the conversation itself: the team serves it no note.
        if l.conversation {
            v["conversation"] = json!(true);
        }
        if let Some(m) = &l.merged_into {
            v["merged_into"] = json!(m);
        }
        if with_text {
            v["text"] = json!(note);
        }
        entries.push((at, v));
    };
    if space != "team" {
        let mine = crate::advice::Space::mine(who.user());
        for l in crate::advice::listed(session, store, who, &mine).await? {
            // A person's own: served to them at once, and nobody else's tally is theirs to see.
            if min_helped == 0 && min_served == 0 {
                add("mine", l, None, true);
            }
        }
    }
    if space != "mine" {
        let scope = session.scope();
        let bar = crate::sessions::evidence_bar(store, scope).await?;
        let tallies = crate::sessions::evidence(store, scope).await?;
        for l in crate::advice::listed(session, store, who, &crate::advice::Space::team()).await? {
            let tally = tallies.get(&l.key).cloned().unwrap_or_default();
            let past = tally.ratified(bar);
            // Everyone reads what the team serves; the candidates are for the org's admins.
            if past || admin {
                add("team", l, Some(&tally), past);
            }
        }
    }
    entries.sort_by(|(a, av), (b, bv)| {
        Reverse(a)
            .cmp(&Reverse(b))
            .then_with(|| av["space"].as_str().cmp(&bv["space"].as_str()))
            .then_with(|| av["key"].as_str().cmp(&bv["key"].as_str()))
    });
    let total = entries.len();
    if let Some((t, id)) = &after {
        // The same order: older than the cursor's time, or the same time and a later key.
        entries.retain(|(at, v)| {
            let key = format!(
                "{}{}",
                v["space"].as_str().unwrap_or(""),
                v["key"].as_str().unwrap_or("")
            );
            *at < *t || (*at == *t && key.as_str() > id.as_str())
        });
    }
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next = more
        .then(|| {
            entries.last().map(|(at, v)| {
                format!(
                    "{at}:{}{}",
                    v["space"].as_str().unwrap_or(""),
                    v["key"].as_str().unwrap_or("")
                )
            })
        })
        .flatten();
    Ok(
        json!({"advice": entries.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
              "count": total, "cursor": next}),
    )
}
