//! Facts in conflict, as their owners settle them (`/admin` ops, beside owner review in [`super::governance`]):
//!
//! - `add_fact` (an org admin): a fact as the admin states it, from a source of its own, with no model: what an
//!   admin knows that the sources don't say, or a second account of a value to set beside the first;
//! - `check_conflicts` (an org admin): reconcile's conflict check over the scope as it is now, without waiting for
//!   the next write (it answers how many it recorded, and the first page of them): two open facts of one thing and attribute that holds one value at a time, with different
//!   values from different sources, are recorded in conflict ([`crate::conflicts`]), as write-time reconcile records
//!   them. Only facts the admin may read are compared, and a read marks a conflict only for readers of both facts;
//! - `list_conflicts` (a member): the conflicts between facts the caller may read, one per thing and attribute
//!   (every value of it in dispute, and the pairs of them recorded in conflict), with the owner of each thing and
//!   whether the caller may settle it, a page at a time (`limit`, at most 200; `after`, the last page's `next`)
//!   with the `total`, and `total_groups` and `total_pairs` beside it; `by: "pair"` lists one per pair instead;
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
            let expires_at = expiry(req)?;
            resolve(store, who, admin, scope, (a, b), choice, expires_at).await
        }
        _ => unreachable!(),
    }
}

/// When an approval made now ends: `expires_days` from now, or the default lifetime when it isn't given.
pub(super) fn expiry(req: &Value) -> Result<i64> {
    use crate::people::directory::Invalid;
    let days = match &req["expires_days"] {
        Value::Null => None,
        v => Some(
            v.as_i64()
                .ok_or_else(|| Invalid("expires_days is a whole number of days".into()))?,
        ),
    };
    crate::governance::approval_expiry(crate::now_micros(), days)
        .map_err(|e| Invalid(format!("{e:#}")).into())
}

/// Records an admin's statement as a decision ([`Store::record_stated`]) once its facts are on main. The facts stand
/// whether or not the record is made, so a failure is said (a warning) rather than refused: a refusal would have the
/// admin state it again, and a second account of the same fact would conflict with the first.
#[allow(clippy::too_many_arguments)]
async fn stated(
    store: &Store,
    who: &Principal,
    scope: &Scope,
    added: &[String],
    kept: Option<&str>,
    ended: &[String],
    expires_at: i64,
) -> Option<String> {
    match store
        .record_stated(scope, who, added, kept, ended, expires_at)
        .await
    {
        Ok(()) => None,
        Err(e) => {
            eprintln!("decision not recorded in {scope}: {e:#}");
            Some("published, but not recorded in the decision log; it won't show there".into())
        }
    }
}

/// The source a fact an admin states is from: its own, unless they name it (`source`), so two facts they add
/// read as two accounts, and two under one name as one source revising itself. Filed under
/// [`crate::origin::ADDED`], which no note may take, so it reads as the admin's own statement (`stated`), not an
/// agent's note: a correction made in Studio ranks as what a person said, beside what it corrects.
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
    format!(
        "{REMEMBER_SOURCE}:{}:{}:{label}",
        who.user(),
        crate::origin::ADDED
    )
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
    let expires_at = expiry(req)?;
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
        .commit_reviewed(
            scope,
            who,
            true,
            &branch,
            from_ref(&ep),
            from_ref(&f),
            &[],
            Some(expires_at),
        )
        .await?;
    // Published on main with no proposal (nobody owns it): the admin's statement is the decision.
    let warning = match review {
        Some(_) => None,
        None => {
            stated(
                store,
                who,
                scope,
                from_ref(&f.fact_id),
                None,
                &[],
                expires_at,
            )
            .await
        }
    };
    Ok(
        json!({"fact_id": f.fact_id, "episode_id": ep.episode_id, "expires_at": expires_at,
        "review": review, "warning": warning}),
    )
}

async fn check(store: &Store, who: &Principal, scope: &Scope) -> Result<Value> {
    let session = store.pin(scope, MAIN).await?;
    let schema = session.schema();
    let mut facts = session.read(store, who, Some(i64::MAX)).await?;
    facts.retain(|f| f.valid_to.is_none() && conflicts::one_value(schema.as_deref(), f));
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
        // The admin who asked reads the thing next: its facts show in dispute on that read, not on the next build.
        store.content_written(scope);
    }
    let recorded = pairs.len();
    // The first page of what it recorded: `list_conflicts` pages through them all.
    pairs.truncate(PAGE);
    Ok(json!({
        "checked": facts.len(),
        "recorded": recorded,
        "conflicts": pairs,
        // Off, reads don't mark them (the org's or team's `conflicts` setting): owners still list and settle them.
        "marked_in_reads": conflicts::on(store, scope).await,
    }))
}

/// The conflicts between open facts `who` may read, one per thing and attribute (or per pair), with the owner of
/// its thing.
/// Most conflicts one page lists, and how many when the caller doesn't say.
const MAX_PAGE: usize = 200;
const PAGE: usize = 50;

async fn list(
    store: &Store,
    who: &Principal,
    admin: bool,
    scope: &Scope,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::Invalid;
    let limit = match &req["limit"] {
        Value::Null => PAGE,
        v => v
            .as_u64()
            .filter(|n| (1..=MAX_PAGE as u64).contains(n))
            .ok_or_else(|| Invalid(format!("limit is a whole number from 1 to {MAX_PAGE}")))?
            as usize,
    };
    let after = req["after"].as_str().filter(|a| !a.is_empty());
    // Shown and filtered by, not acted on: as this server holds them.
    let owners: HashMap<String, String> = store
        .entity_owners_held(scope)
        .await?
        .into_iter()
        .map(|o| (o.entity, o.user))
        .collect();
    crate::phases::mark("conflicts owners");
    let me = auth::norm_id(who.user());
    let mine = req["mine"] == true;
    // Only the conflicts on what the caller owns, and they own nothing: none, without reading a fact.
    if mine && !owners.values().any(|u| *u == me) {
        return Ok(json!({"conflicts": [], "total": 0, "next": null}));
    }
    // Main's last visibility state while the next is built: a list never waits for a build of what another
    // conversation just wrote, and never holds what was taken away (a resolution's ending of a fact included).
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    let session = store.pin_lagging(scope, MAIN).await?;
    crate::phases::mark("conflicts pinned");
    let vis = session.vis();
    // The caller's open facts in conflict, kept by memory state ([`crate::kept_answers`]): reading them reads every
    // fact of the scope.
    let facts = CONFLICTED
        .get(
            &scope.namespace(),
            &crate::kept_answers::reader_key(who),
            crate::kept_answers::At::of(mark, vis),
            true,
            || {
                let (store, session, who) = (store.clone(), session.clone(), who.clone());
                async move { open_conflicted(&store, &session, &who).await }
            },
        )
        .await?;
    crate::phases::mark("conflicts read");
    let open: HashMap<&str, &Fact> = facts.iter().map(|f| (f.fact_id.as_str(), f)).collect();
    let entity = req["entity"].as_str().filter(|e| !e.is_empty());
    let mut pairs: Vec<(String, &Fact, &Fact)> = vec![];
    for a in open.values() {
        if entity.is_some_and(|e| e != a.entity) {
            continue;
        }
        if mine && owners.get(&a.entity) != Some(&me) {
            continue;
        }
        for b in vis.conflicts_of(&a.fact_id) {
            let Some(b) = open.get(b.as_str()) else {
                continue;
            };
            // Each pair once: the older side first.
            if (a.valid_from, &a.fact_id) > (b.valid_from, &b.fact_id) {
                continue;
            }
            // The page's order and its cursor: by thing, attribute, then the pair.
            let key = [&a.entity, &a.predicate, &a.fact_id, &b.fact_id]
                .map(|s| s.as_str())
                .join("\u{1f}");
            pairs.push((key, a, b));
        }
    }
    pairs.sort_by(|x, y| x.0.cmp(&y.0));
    // One conflict per thing and attribute, whatever how many values disagree on it: an attribute nine sources
    // give nine values of is one dispute to settle, not the 36 pairs among them (`by: "pair"` lists the pairs).
    // Each group is what the caller may read of it: only the pairs whose both sides they read.
    let mut groups: Vec<(String, Vec<(&Fact, &Fact)>)> = vec![];
    for &(_, a, b) in &pairs {
        let key = [&a.entity, &a.predicate].map(|s| s.as_str()).join("\u{1f}");
        if let Some((_, g)) = groups.last_mut().filter(|(k, _)| *k == key) {
            g.push((a, b));
        } else {
            groups.push((key, vec![(a, b)]));
        }
    }
    let (total_pairs, total_groups) = (pairs.len(), groups.len());
    let by_pair = match req["by"].as_str().unwrap_or_default() {
        "" | "group" => false,
        "pair" => true,
        _ => return Err(Invalid("by is group (the default) or pair".into()).into()),
    };
    // Each listed conflict: its sides, the older first, and the pairs among them recorded in conflict. A group's
    // first two sides are its first recorded pair, so a reader that settles the first two settles it all
    // (`resolve_conflict` ends every other open value of the attribute).
    let mut listed: Vec<Listed> = match by_pair {
        true => pairs
            .iter()
            .map(|(k, a, b)| (k.clone(), vec![*a, *b], vec![(*a, *b)]))
            .collect(),
        false => groups
            .into_iter()
            .map(|(k, g)| {
                let mut sides: Vec<&Fact> = vec![g[0].0, g[0].1];
                let mut rest: Vec<&Fact> = g
                    .iter()
                    .flat_map(|(a, b)| [*a, *b])
                    .filter(|f| !sides.iter().any(|s| s.fact_id == f.fact_id))
                    .collect();
                rest.sort_by(|x, y| (x.valid_from, &x.fact_id).cmp(&(y.valid_from, &y.fact_id)));
                rest.dedup_by(|x, y| x.fact_id == y.fact_id);
                sides.extend(rest);
                (k, sides, g)
            })
            .collect(),
    };
    let total = listed.len();
    let from = after.map_or(0, |a| listed.partition_point(|p| p.0.as_str() <= a));
    let to = (from + limit).min(total);
    let next = (to < total).then(|| listed[to - 1].0.clone());
    let page: Vec<_> = listed.drain(from..to).collect();
    // Where each side was said, for the page's facts only.
    let eps: Vec<String> = page
        .iter()
        .flat_map(|(_, sides, _)| sides.iter())
        .filter_map(|f| f.source_episodes.first().cloned())
        .collect();
    let sources = session.episode_sources(store, who, &eps).await?;
    crate::phases::mark("conflicts sources");
    let side = |f: &Fact| {
        let ep = f.source_episodes.first();
        json!({"fact_id": f.fact_id, "value": f.value, "since": f.valid_from,
            "since_day": crate::freshness::day(f.valid_from), "quote": f.quote,
            "episode_id": ep, "source": ep.and_then(|e| sources.get(e)).map(|s| &s.0)})
    };
    let out: Vec<Value> = page
        .iter()
        .map(|(_, sides, recorded)| {
            let a = sides[0];
            let owner = owners.get(&a.entity);
            json!({
                "entity": a.entity, "predicate": a.predicate, "owner": owner,
                "can_resolve": admin || owner == Some(&me),
                "facts": sides.iter().map(|f| side(f)).collect::<Vec<_>>(),
                "pairs": recorded.iter().map(|(a, b)| [&a.fact_id, &b.fact_id]).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({"conflicts": out, "total": total, "next": next,
        "total_groups": total_groups, "total_pairs": total_pairs}))
}

/// Each reader's open facts recorded in conflict, by memory state.
static CONFLICTED: std::sync::LazyLock<crate::kept_answers::Kept<Vec<Fact>>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

/// The facts recorded in conflict that `who` may read and that are open, of an attribute that holds one value.
async fn open_conflicted(store: &Store, session: &Session, who: &Principal) -> Result<Vec<Fact>> {
    // Only the facts recorded in conflict, not every fact of the scope.
    let ids: Vec<String> = session.vis().conflicted().map(String::from).collect();
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let schema = session.schema();
    let mut facts = session
        .read_in(store, who, search::Col::FactId, &ids, Some(i64::MAX))
        .await?;
    facts.retain(|f| f.valid_to.is_none() && conflicts::one_value(schema.as_deref(), f));
    Ok(facts)
}

/// A listed conflict: its cursor key, its sides (the first two a recorded pair) and the pairs recorded among them.
type Listed<'a> = (String, Vec<&'a Fact>, Vec<(&'a Fact, &'a Fact)>);

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
    expires_at: i64,
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
    // A value the resolver states is their statement, as a fact they add is ([`admin_source`]): the fact cites
    // their words first, then the record. Forgetting them takes their words, not the resolution (the endings cite
    // only the record).
    let mut episodes = vec![];
    if kept.is_none() {
        let body = format!("{} {}: {said}", fa.entity, fa.predicate);
        let mut theirs =
            Episode::sourced(&admin_source(who, "resolution"), body.as_bytes(), &["*"]);
        theirs.occurred_at = now;
        crate::episode_types::check(&theirs).map_err(|e| Invalid(format!("{e:#}")))?;
        episodes.push(theirs);
    }
    episodes.push(ep.clone());
    if kept.is_none() || restated.is_some() {
        let cited: Vec<&Episode> = episodes.iter().collect();
        let mut f = Fact::new(&fa.entity, &fa.predicate, &said).from_episodes(&cited);
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
        .commit_reviewed(
            scope,
            who,
            admin,
            &branch,
            &episodes,
            &new,
            &closures,
            Some(expires_at),
        )
        .await?;
    let mut warning = None;
    if review.is_none() && admin {
        // Settled on main with no proposal: nobody owns the thing, so only an admin could resolve it, and their
        // resolution is the decision. (An owner's resolution of what they own always goes through review.)
        let added: Vec<String> = new.iter().map(|f| f.fact_id.clone()).collect();
        let ended: Vec<String> = ended.iter().map(|f| f.fact_id.clone()).collect();
        let kept = kept
            .filter(|_| restated.is_none())
            .map(|f| f.fact_id.as_str());
        warning = stated(store, who, scope, &added, kept, &ended, expires_at).await;
    }
    Ok(json!({
        "kept": kept.map(|f| &f.fact_id),
        "added": new.first().map(|f| &f.fact_id),
        "ended": ended.iter().map(|f| &f.fact_id).collect::<Vec<_>>(),
        "episode_id": ep.episode_id,
        "status": review.as_ref().map_or("published", |r| r.status.as_str()),
        "expires_at": expires_at,
        "warning": warning,
        "review": review,
    }))
}
