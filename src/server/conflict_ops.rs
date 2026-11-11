//! Facts in conflict, as their owners settle them (`/admin` ops, beside owner review in [`super::governance`]):
//!
//! - `add_fact` (an org admin): a fact as the admin states it, from a source of its own, with no model: what an
//!   admin knows that the sources don't say, or a second account of a value to set beside the first;
//! - `check_conflicts` (an org admin): reconcile's conflict check over the scope as it is now, without waiting for
//!   the next write: two open facts of one thing and attribute that holds one value at a time, with different
//!   values from different sources, are recorded in conflict ([`crate::conflicts`]), as write-time reconcile records
//!   them. Only facts the admin may read are compared, and a read marks a conflict only for readers of both facts;
//! - `list_conflicts` (a member): the conflicts between facts the caller may read, with the owner of each thing
//!   and whether the caller may settle it;
//! - `resolve_conflict` (the thing's owner, or an org admin): keep one fact, or state the value, which ends both.
//!   What loses is ended, not deleted: history keeps it. The ending cites the engine's record of the resolution (ids
//!   only), and where the thing is owned it goes through owner review, approved by the one resolving it.

use super::*;
use crate::conflicts;
use crate::{Episode, Fact};
use std::slice::from_ref;

/// The source of a resolution's record ([`crate::episode_types::ENGINE_SOURCES`]: one of reconcile's).
const RESOLUTION_SOURCE: &str = "reconcile:resolution";
/// Where resolutions are recorded, `resolutions/<fact>/<fact>`.
const RESOLUTIONS: &str = "resolutions/";

/// Most facts one check compares, newest first.
const MAX_CHECKED: usize = 200_000;

pub(super) async fn handle(
    store: &Store,
    who: &Principal,
    admin: bool,
    scope: &Scope,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::Invalid;
    let field = |k: &str| req[k].as_str().unwrap_or_default().trim();
    let only_admins = |what: &str| -> Result<()> {
        anyhow::ensure!(admin, "only an org admin may {what}");
        Ok(())
    };
    match field("op") {
        "add_fact" => {
            only_admins("add a fact")?;
            let (entity, predicate, value) = (field("entity"), field("predicate"), field("value"));
            if entity.is_empty() || predicate.is_empty() || value.is_empty() {
                return Err(Invalid("entity, predicate and value are required".into()).into());
            }
            add_fact(store, who, scope, entity, predicate, value, req).await
        }
        "check_conflicts" => {
            only_admins("check for conflicts")?;
            check(store, who, scope).await
        }
        "list_conflicts" => list(store, who, admin, scope, req).await,
        "resolve_conflict" => {
            let facts: Vec<&str> = req["facts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let [a, b] = facts[..] else {
                return Err(Invalid("facts names the two facts in conflict".into()).into());
            };
            let keep = req["keep"].as_str().filter(|k| !k.is_empty());
            let value = req["value"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty());
            let choice = match (keep, value) {
                (Some(k), None) if k == a || k == b => Choice::Keep(k),
                (None, Some(v)) => Choice::Value(v),
                _ => {
                    return Err(Invalid(
                        "resolve keeps one of the two facts (keep) or states the value (value)"
                            .into(),
                    )
                    .into());
                }
            };
            resolve(store, who, admin, scope, (a, b), choice).await
        }
        _ => unreachable!(),
    }
}

/// The source a fact an admin states is from: its own, unless they name it (`source`), so two facts they add
/// read as two accounts, and two under one name as one source revising itself.
fn admin_source(who: &Principal, label: &str) -> String {
    let label = match label {
        "" => uuid::Uuid::new_v4().to_string(),
        l => l
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .take(64)
            .collect(),
    };
    format!("{REMEMBER_SOURCE}:{}:fact:{label}", who.user())
}

#[allow(clippy::too_many_arguments)]
async fn add_fact(
    store: &Store,
    who: &Principal,
    scope: &Scope,
    entity: &str,
    predicate: &str,
    value: &str,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::Invalid;
    let note = req["note"].as_str().unwrap_or_default().trim();
    let mut body = format!("{entity} {predicate}: {value}");
    if !note.is_empty() {
        body.push_str(&format!("\n{note}"));
    }
    let ep = Episode::sourced(
        &admin_source(who, req["source"].as_str().unwrap_or_default().trim()),
        body.as_bytes(),
        &["*"],
    );
    crate::episode_types::check(&ep).map_err(|e| Invalid(format!("{e:#}")))?;
    let mut f = Fact::new(entity, predicate, value).from_episodes(&[&ep]);
    f.quote = Some(value.chars().take(300).collect());
    if let Some(at) = req["valid_from"].as_str() {
        let at =
            query::schema::parse_instant(at).map_err(|e| Invalid(format!("valid_from: {e}")))?;
        anyhow::ensure!(
            at <= crate::now_micros() + SKEW,
            "valid_from can't be in the future"
        );
        f.valid_from = at;
    }
    if let Some(s) = store.schema(scope, MAIN).await? {
        s.normalize(&mut f.clone())
            .map_err(|e| Invalid(format!("{e:#}")))?;
    }
    let branch = format!(
        "fact-{}-{}",
        &f.fact_id[..8],
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let review = store
        .commit_reviewed(scope, who, true, &branch, from_ref(&ep), from_ref(&f), &[])
        .await?;
    Ok(json!({"fact_id": f.fact_id, "episode_id": ep.episode_id, "review": review}))
}

/// Whether facts of `f`'s attribute hold one value at a time: not a list, a many-valued or a reference predicate
/// of the schema (a new one joins the others), nor one the engine keeps for itself (`_alias`).
fn single_valued(schema: Option<&query::schema::Schema>, f: &Fact) -> bool {
    if f.predicate.starts_with('_') {
        return false;
    }
    let ty = f
        .entity_type
        .clone()
        .or_else(|| {
            schema
                .and_then(|s| s.entity_type(&f.entity))
                .map(String::from)
        })
        .unwrap_or_else(|| {
            f.entity
                .split_once(':')
                .map(|(t, _)| t)
                .unwrap_or("")
                .into()
        });
    schema
        .and_then(|s| s.predicate(&ty, &f.predicate))
        .is_none_or(|p| {
            !p.list
                && p.card != query::schema::Card::Many
                && p.value != query::schema::ValueType::Ref
        })
}

async fn check(store: &Store, who: &Principal, scope: &Scope) -> Result<Value> {
    let session = store.pin(scope, MAIN).await?;
    let schema = session.schema();
    let mut facts = session.read(store, who, Some(i64::MAX)).await?;
    facts.retain(|f| f.valid_to.is_none() && single_valued(schema.as_deref(), f));
    facts.sort_by(|a, b| (b.valid_from, &b.fact_id).cmp(&(a.valid_from, &a.fact_id)));
    facts.truncate(MAX_CHECKED);
    let ids: Vec<String> = facts
        .iter()
        .filter_map(|f| f.source_episodes.first().cloned())
        .collect();
    let sources = session.episode_sources(store, who, &ids).await?;
    let source_of = |f: &Fact| {
        f.source_episodes
            .first()
            .and_then(|e| sources.get(e))
            .map(|s| s.0.clone())
    };
    // Oldest first, one fact per value: the first account of each value stands for it.
    let mut by_key: BTreeMap<(String, String), Vec<&Fact>> = BTreeMap::new();
    for f in facts.iter().rev() {
        let at = by_key
            .entry((f.entity.clone(), f.predicate.clone()))
            .or_default();
        if !at
            .iter()
            .any(|o| crate::extract::squash(&o.value) == crate::extract::squash(&f.value))
        {
            at.push(f);
        }
    }
    let vis = session.vis();
    let mut records = vec![];
    let mut pairs = vec![];
    for ((entity, predicate), fs) in &by_key {
        for (i, new) in fs.iter().enumerate() {
            for old in &fs[..i] {
                let (Some(s1), Some(s2)) = (source_of(old), source_of(new)) else {
                    continue;
                };
                if s1 == s2 || vis.conflicts_of(&old.fact_id).contains(&new.fact_id) {
                    continue;
                }
                let key = format!("{}{}/{}", conflicts::PREFIX, old.fact_id, new.fact_id);
                let body = json!({"conflict": [old.fact_id, new.fact_id]}).to_string();
                let mut e = Episode::sourced(
                    &format!("{}check", conflicts::SOURCE),
                    body.as_bytes(),
                    &[conflicts::ACL],
                )
                .without_text_index()
                .version_of(&key);
                e.occurred_at = new.valid_from;
                records.push(e);
                pairs.push(json!({"entity": entity, "predicate": predicate,
                    "facts": [old.fact_id, new.fact_id], "values": [old.value, new.value]}));
            }
        }
    }
    if !records.is_empty() {
        store
            .commit_derived(scope, who.user(), &records, &[], &[], &[])
            .await?;
    }
    Ok(json!({
        "checked": facts.len(),
        "recorded": pairs.len(),
        "conflicts": pairs,
        // Off, reads don't mark them (the org's or team's `conflicts` setting): owners still list and settle them.
        "marked_in_reads": conflicts::on(store, scope).await,
    }))
}

/// The conflicts between open facts `who` may read, each once, with the owner of its thing.
async fn list(
    store: &Store,
    who: &Principal,
    admin: bool,
    scope: &Scope,
    req: &Value,
) -> Result<Value> {
    let session = store.pin(scope, MAIN).await?;
    let facts = session.read(store, who, Some(i64::MAX)).await?;
    let open: HashMap<&str, &Fact> = facts
        .iter()
        .filter(|f| f.valid_to.is_none())
        .map(|f| (f.fact_id.as_str(), f))
        .collect();
    let owners: HashMap<String, String> = store
        .entity_owners(scope)
        .await?
        .into_iter()
        .map(|o| (o.entity, o.user))
        .collect();
    let me = auth::norm_id(who.user());
    let entity = req["entity"].as_str().filter(|e| !e.is_empty());
    let mine = req["mine"] == true;
    let vis = session.vis();
    let ids: Vec<String> = open
        .values()
        .filter_map(|f| f.source_episodes.first().cloned())
        .collect();
    let sources = session.episode_sources(store, who, &ids).await?;
    let side = |f: &Fact| {
        let ep = f.source_episodes.first();
        json!({"fact_id": f.fact_id, "value": f.value, "since": f.valid_from,
            "since_day": crate::freshness::day(f.valid_from), "quote": f.quote,
            "episode_id": ep, "source": ep.and_then(|e| sources.get(e)).map(|s| &s.0)})
    };
    let mut out = vec![];
    for a in open.values() {
        for b in vis.conflicts_of(&a.fact_id) {
            let Some(b) = open.get(b.as_str()) else {
                continue;
            };
            // Each pair once: the older side first.
            if (a.valid_from, &a.fact_id) > (b.valid_from, &b.fact_id) {
                continue;
            }
            if entity.is_some_and(|e| e != a.entity) {
                continue;
            }
            let owner = owners.get(&a.entity);
            if mine && owner != Some(&me) {
                continue;
            }
            out.push(json!({
                "entity": a.entity, "predicate": a.predicate, "owner": owner,
                "can_resolve": admin || owner == Some(&me),
                "facts": [side(a), side(b)],
            }));
        }
    }
    out.sort_by(|x, y| {
        (x["entity"].as_str(), x["predicate"].as_str())
            .cmp(&(y["entity"].as_str(), y["predicate"].as_str()))
    });
    Ok(json!({"conflicts": out}))
}

enum Choice<'a> {
    /// Keep this fact: the other ends.
    Keep(&'a str),
    /// The value stated anew: both end, and a fact of it begins.
    Value(&'a str),
}

async fn resolve(
    store: &Store,
    who: &Principal,
    admin: bool,
    scope: &Scope,
    (a, b): (&str, &str),
    choice: Choice<'_>,
) -> Result<Value> {
    use crate::people::directory::Invalid;
    let session = store.pin(scope, MAIN).await?;
    let facts = session
        .read_in(
            store,
            who,
            search::Col::FactId,
            &[a.to_string(), b.to_string()],
            Some(i64::MAX),
        )
        .await?;
    // The same answer for a fact that isn't there as for one the caller can't read.
    let (Some(fa), Some(fb)) = (
        facts
            .iter()
            .find(|f| f.fact_id == a && f.valid_to.is_none()),
        facts
            .iter()
            .find(|f| f.fact_id == b && f.valid_to.is_none()),
    ) else {
        return Err(Invalid("no two open facts by those ids you can read".into()).into());
    };
    if !session.vis().conflicts_of(a).iter().any(|o| o == b) {
        return Err(Invalid("those two facts aren't recorded in conflict".into()).into());
    }
    let owner = store
        .entity_owners(scope)
        .await?
        .into_iter()
        .find(|o| o.entity == fa.entity)
        .map(|o| o.user);
    anyhow::ensure!(
        admin || owner.as_deref() == Some(auth::norm_id(who.user()).as_str()),
        "only the owner of {} or an org admin may resolve its conflicts",
        fa.entity
    );
    let now = crate::now_micros();
    let (kept, said): (Option<&Fact>, String) = match choice {
        Choice::Keep(k) => {
            let keep = if k == a { fa } else { fb };
            (Some(keep), keep.value.clone())
        }
        Choice::Value(v) => (None, v.to_string()),
    };
    // It settles the attribute: every other open value of it the resolver reads ends with the losing side (the
    // attribute holds one value at a time), so no older account is left to conflict with what was settled.
    let others = session
        .lookup(store, who, &fa.entity, &fa.predicate, Some(i64::MAX))
        .await?;
    let mut ended: Vec<&Fact> = vec![];
    for f in [fa, fb].into_iter().chain(others.iter()) {
        if f.valid_to.is_none()
            && kept.is_none_or(|k| k.fact_id != f.fact_id)
            && !ended.iter().any(|e| e.fact_id == f.fact_id)
        {
            ended.push(f);
        }
    }
    // A read serves the newest value of an attribute that holds one: a kept value older than one that ends is
    // stated again from now, citing what it came from and the resolution, and its first statement ends with the
    // rest, so it is what reads serve.
    let restated = kept.filter(|k| ended.iter().any(|e| e.valid_from >= k.valid_from));
    if let Some(k) = restated {
        ended.push(k);
    }
    // The record of the resolution, cited by every ending and by the fact it states: the engine's, of ids only.
    // No value is in it, so the closures that cite it (read by everyone in the scope, so they apply to every reader
    // of what they end) say nothing of a fact to anyone who can't read the fact, and no person's words, so it
    // outlives whoever resolved it: forgetting them doesn't reopen what they settled. Who decided is kept by the
    // review it goes through on an owned entity.
    let key = format!("{RESOLUTIONS}{a}/{b}");
    let body =
        json!({"resolved": [a, b], "ended": ended.iter().map(|f| &f.fact_id).collect::<Vec<_>>()})
            .to_string();
    let mut ep = Episode::sourced(RESOLUTION_SOURCE, body.as_bytes(), &["*"])
        .without_text_index()
        .version_of(&key);
    ep.occurred_at = now;
    let mut new = vec![];
    if kept.is_none() || restated.is_some() {
        let mut f = Fact::new(&fa.entity, &fa.predicate, &said).from_episodes(&[&ep]);
        f.quote = Some(said.chars().take(300).collect());
        if let Some(k) = restated {
            f.source_episodes = k.source_episodes.clone();
            f.source_episodes.push(ep.episode_id.clone());
            f.quote = k.quote.clone();
        }
        f.valid_from = now;
        if let Some(s) = session.schema() {
            s.normalize(&mut f.clone())
                .map_err(|e| Invalid(format!("{e:#}")))?;
        }
        new.push(f);
    }
    let closures: Vec<crate::Closure> = ended
        .iter()
        .map(|f| crate::Closure {
            fact_id: f.fact_id.clone(),
            valid_to: now.max(f.valid_from + 1),
            episode_id: ep.episode_id.clone(),
        })
        .collect();
    let branch = format!(
        "resolve-{}-{}",
        &ep.episode_id[..12],
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let review = store
        .commit_reviewed(scope, who, admin, &branch, from_ref(&ep), &new, &closures)
        .await?;
    Ok(json!({
        "kept": kept.map(|f| &f.fact_id),
        "added": new.first().map(|f| &f.fact_id),
        "ended": ended.iter().map(|f| &f.fact_id).collect::<Vec<_>>(),
        "episode_id": ep.episode_id,
        "status": review.as_ref().map_or("published", |r| r.status.as_str()),
        "review": review,
    }))
}
