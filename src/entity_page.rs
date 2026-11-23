//! `/browse {op: "entity", path: ["entities", <type>, <key>]}`: one entity as its page shows it, everything read as
//! the caller and nothing else. Beside the facts `ls` gives (history included), it answers:
//!
//! - per fact: where its words came from ([`crate::origin`]), how many independent sources the caller may read
//!   back it ([`crate::corroboration::count`]), who said it (the speaker, author or sender its citation names, or a
//!   note's author), the entity its value names, and the facts it is recorded in conflict with that the caller may
//!   read (open ones only: a conflict between facts the caller can't both read is no conflict to them);
//! - `episodes`: each episode those facts cite, with its source, when it happened, its type and who may read it
//!   (its ACL: `*`, `user:…`, `group:…`, `role:…`). Every one is an episode the caller may read: a fact shows only
//!   when they may read every episode it cites;
//! - `links`: the entities its open facts name (`out`), and the open facts of other entities naming it (`in`), by
//!   attribute, each once, each with its `title` when a name fact the caller may read names it;
//! - `title` and `since`: its own name and when its first fact holds from, as `ls` lists them.
//!
//! The same 404 as `ls` when the caller may read nothing about it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use serde_json::{Value, json};

use crate::browse::{NotFound, fact_json, once_each};
use crate::episode_types::Cite;
use crate::query::schema::{NAMED_BY, display_names};
use crate::search::{Col, Filter, Lit};
use crate::{Fact, Principal, Session, Store};

/// Most entities its links list each way.
const MAX_LINKS: usize = 200;
/// Cited episodes titled at most, the newest first.
const TITLED: usize = 200;

/// An entity id's type and key, as `/browse` splits it (`deal:42` is `deal`, `42`; an id without one is `_`).
fn split(e: &str) -> (&str, &str) {
    e.split_once(':').unwrap_or(("_", e))
}

/// Whether `v` reads as an entity id: a type, a colon, a key.
pub(crate) fn entity_like(v: &str) -> bool {
    let Some((t, k)) = v.split_once(':') else {
        return false;
    };
    !t.is_empty()
        && !k.is_empty()
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !k.contains(char::is_whitespace)
}

/// Who said what a fact holds, when its citation or its episode's source names them: a call's speaker, a message's
/// author, an email's sender, a note's author. None for a record or a document, whose source is who says it.
fn said_by(f: &Fact, source: Option<&str>) -> Option<String> {
    let cited = f
        .cite
        .as_deref()
        .and_then(|c| serde_json::from_str::<Cite>(c).ok());
    match cited {
        Some(Cite::Turn { speaker, .. }) if !speaker.is_empty() => return Some(speaker),
        Some(Cite::Message { author, .. }) if !author.is_empty() => return Some(author),
        Some(Cite::Email { from, .. }) if !from.is_empty() => return Some(from),
        _ => {}
    }
    let sayer = crate::origin::sayer(source?);
    sayer
        .strip_prefix(crate::REMEMBER_SOURCE)
        .and_then(|s| s.strip_prefix(':'))
        .filter(|s| !s.is_empty())
        .map(String::from)
}

pub async fn run(
    store: &Store,
    session: &Session,
    who: &Principal,
    path: &[String],
) -> Result<Value> {
    let [root, t, k] = path else {
        return Err(NotFound.into());
    };
    if root != "entities" {
        return Err(NotFound.into());
    }
    let (t, k) = (t.as_str(), k.as_str());
    let mut ids = vec![format!("{t}:{k}")];
    if t == "_" && !k.contains(':') {
        ids.push(k.to_string());
    }
    let read = once_each(session.read_in(store, who, Col::Entity, &ids, None).await?);
    let mut facts: Vec<&Fact> = read.iter().filter(|f| split(&f.entity) == (t, k)).collect();
    let Some(first) = facts.first() else {
        return Err(NotFound.into());
    };
    let entity = first.entity.clone();
    facts.sort_by(|a, b| {
        (&a.predicate, a.valid_from, &a.fact_id).cmp(&(&b.predicate, b.valid_from, &b.fact_id))
    });
    let vis = session.vis();

    // The episodes they cite, as the caller may read them.
    let cited: Vec<String> = facts
        .iter()
        .flat_map(|f| f.source_episodes.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sources = session.episode_sources(store, who, &cited).await?;
    // What each cited episode is called by its type (a call by its recorder's title): the newest first, at most
    // a page's worth, read once per server ([`crate::arrivals::titled`]).
    let mut newest: Vec<(&String, i64)> = cited
        .iter()
        .filter_map(|id| Some((id, sources.get(id)?.1)))
        .collect();
    newest.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    let ask: Vec<(String, String)> = newest
        .iter()
        .take(TITLED)
        .filter_map(|(id, _)| Some(((*id).clone(), sources.get(*id)?.0.clone())))
        .collect();
    let titled = crate::arrivals::titled(store, session, who, &ask).await?;
    let episodes: BTreeMap<&str, Value> = cited
        .iter()
        .filter_map(|id| {
            let (source, at) = sources.get(id)?;
            let readers: Vec<&String> = vis
                .acls
                .get(id)
                .into_iter()
                .flatten()
                .filter(|a| a.as_str() != crate::FORGOTTEN)
                .collect();
            Some((
                id.as_str(),
                json!({"source": source, "occurred_at": at, "type": vis.episode_type(id),
                       "title": titled.get(id).and_then(|t| t.shown()),
                       "origin": vis.episode_origin(id).map(|o| o.name()), "readers": readers}),
            ))
        })
        .collect();

    // Conflicts the caller may see: both facts open and readable to them, on this entity, of an attribute that holds
    // one answer ([`crate::conflicts::one_value`]), as the review list and reads take them: two of a deal's
    // requirements, recorded in conflict before reconcile checked, are both true, never in dispute.
    let schema = session.schema();
    let open: HashSet<&str> = facts
        .iter()
        .filter(|f| f.valid_to.is_none() && crate::conflicts::one_value(schema.as_deref(), f))
        .map(|f| f.fact_id.as_str())
        .collect();
    let out_facts: Vec<Value> = facts
        .iter()
        .map(|f| {
            let mut v = fact_json(f);
            if let Some(o) = vis.fact_origin(f) {
                v["origin"] = json!(o.name());
            }
            v["corroboration"] = json!(crate::corroboration::count(&[vis], who, &[*f]));
            let source = f
                .source_episodes
                .first()
                .and_then(|e| sources.get(e))
                .map(|(s, _)| s.as_str());
            if let Some(by) = said_by(f, source) {
                v["said_by"] = json!(by);
            }
            if let Some(r) = &f.value_ref {
                v["value_ref"] = json!(r);
            }
            if open.contains(f.fact_id.as_str()) {
                let against: Vec<&String> = vis
                    .conflicts_of(&f.fact_id)
                    .iter()
                    .filter(|o| open.contains(o.as_str()))
                    .collect();
                if !against.is_empty() {
                    v["in_conflict_with"] = json!(against);
                }
            }
            v
        })
        .collect();

    // What its open facts name: a ref, or a value that is the id of an entity the caller may read.
    let current: Vec<&&Fact> = facts.iter().filter(|f| f.valid_to.is_none()).collect();
    let named: Vec<String> = current
        .iter()
        .filter(|f| f.value_ref.is_none() && f.value != entity && entity_like(&f.value))
        .map(|f| f.value.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_LINKS)
        .collect();
    let known: HashSet<String> = if named.is_empty() {
        HashSet::new()
    } else {
        session
            .read_in(store, who, Col::Entity, &named, Some(i64::MAX))
            .await?
            .into_iter()
            .map(|f| f.entity)
            .collect()
    };
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    for f in &current {
        let to = f
            .value_ref
            .clone()
            .or_else(|| known.contains(&f.value).then(|| f.value.clone()));
        if let Some(to) = to.filter(|to| *to != entity) {
            out.insert((f.predicate.clone(), to));
        }
    }
    // What names it: other entities' open facts whose value is it.
    let naming = Filter::Or(vec![
        Filter::Eq(Col::ValueRef, Lit::Str(entity.clone())),
        Filter::Eq(Col::Value, Lit::Str(entity.clone())),
    ]);
    let mut incoming: BTreeSet<(String, String)> = BTreeSet::new();
    for f in session
        .read_where(store, who, &naming, Some(i64::MAX))
        .await?
    {
        if f.entity != entity && f.valid_to.is_none() && !crate::is_note_fact(&f) {
            incoming.insert((f.predicate, f.entity));
        }
    }
    // The names of what it links to, in one read of their name facts the caller may read now.
    let linked: Vec<String> = out
        .iter()
        .take(MAX_LINKS)
        .chain(incoming.iter().take(MAX_LINKS))
        .map(|(_, e)| e.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let now = crate::now_micros();
    let names = if linked.is_empty() {
        HashMap::new()
    } else {
        let lit = |v: &[String]| v.iter().map(|s| Lit::Str(s.clone())).collect::<Vec<_>>();
        let named_by: Vec<String> = NAMED_BY.iter().map(|p| p.to_string()).collect();
        let filter = Filter::And(vec![
            Filter::In(Col::Entity, lit(&linked)),
            Filter::In(Col::Predicate, lit(&named_by)),
        ]);
        display_names(
            &session
                .read_where(store, who, &filter, Some(i64::MAX))
                .await?,
            now,
        )
    };
    let link = |(p, e): &(String, String)| {
        let mut v = json!({"predicate": p, "entity": e});
        if let Some(t) = names.get(e).and_then(|s| s.title.as_ref()) {
            v["title"] = json!(t);
        }
        v
    };
    let links = json!({
        "out": out.iter().take(MAX_LINKS).map(link).collect::<Vec<_>>(),
        "in": incoming.iter().take(MAX_LINKS).map(link).collect::<Vec<_>>(),
        "more": out.len() > MAX_LINKS || incoming.len() > MAX_LINKS,
    });
    let mut page = json!({"path": path, "entity": entity, "facts": out_facts, "episodes": episodes,
                          "links": links});
    if let Some(s) = display_names(facts.iter().copied(), now).get(&entity) {
        if let Some(t) = &s.title {
            page["title"] = json!(t);
        }
        page["since"] = json!(s.since);
    }
    Ok(page)
}
