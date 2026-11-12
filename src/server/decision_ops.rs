//! The decision log (`/admin` op `decisions`, an org admin's): every decision that published facts to main
//! ([`crate::governance::Approval`]), with how far it has landed since.
//!
//! - Agents: the first read after the decision that served what it published (the audit's `read` rows name the
//!   episodes a read's facts came from), who read it, and in which scopes. A statement or resolution cites an
//!   episode of its own, so its first serve is exact; a published proposal's facts cite their source, which other
//!   facts of that source cite too, so its first serve is a source's (`precision`).
//! - People: what sources said of the same thing and attribute after the decision, the new value against the
//!   value it replaced, matched on the value's words (a paraphrase isn't matched). Adopted once at least
//!   [`MIN_MENTIONS`] such statements were made and [`ADOPTED_SHARE`] of them say the new value.
//!
//! Each decision's approval ends at its `expires_at`, and the log marks it `expired` after: nothing un-publishes or
//! re-reviews its facts on their own yet.

use super::*;
use crate::Fact;
use crate::audit::{self, Kind};
use crate::governance::Approval;

/// Statements after a decision needed before its share counts.
const MIN_MENTIONS: usize = 3;
/// The share of them saying the new value at which people have adopted it.
const ADOPTED_SHARE: f64 = 0.8;
const DAY_US: f64 = 86_400_000_000.0;

pub(super) async fn handle(
    state: &State,
    store: &Store,
    who: &Principal,
    via: &SignIn,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::{Directory, Invalid};
    let org = req["org"].as_str().unwrap_or_default();
    crate::check_name("org", org).map_err(|e| Invalid(e.to_string()))?;
    let dir = Directory::load(store).await?;
    anyhow::ensure!(
        dir.is_admin(who.user(), via, org),
        "{} may not read org {org}'s decision log (only its admins may)",
        who.user()
    );
    let scope = dir.resolve(
        who.user(),
        via,
        Some(org),
        req["team"].as_str().filter(|t| !t.is_empty()),
    )?;
    let reader = as_member(who, org);
    let approvals = store.approvals(&scope).await?;
    // The reads so far, this server's unwritten ones too.
    if let Err(e) = state.audit.flush_for_read(store).await {
        eprintln!("audit: events not written yet (kept, retried): {e:#}");
    }
    let since = approvals.first().map_or(i64::MAX, |a| a.at);
    let reads: Vec<audit::Entry> = if approvals.is_empty() {
        vec![]
    } else {
        audit::collect(store, org, since, Some(Kind::Read), state.audit.buffered())
            .await?
            .0
    };
    let session = store.pin(&scope, MAIN).await?;
    let ids: Vec<String> = approvals
        .iter()
        .flat_map(|a| a.added.iter().chain(&a.kept).chain(&a.ended).cloned())
        .collect();
    let facts: HashMap<String, Fact> = session
        .read_in(store, &reader, search::Col::FactId, &ids, None)
        .await?
        .into_iter()
        .map(|f| (f.fact_id.clone(), f))
        .collect();
    let now = crate::now_micros();
    let mut out = vec![];
    for a in &approvals {
        out.push(entry(store, &session, &reader, a, &facts, &reads, now).await?);
    }
    out.reverse();
    let firsts: Vec<f64> = out
        .iter()
        .filter_map(|d| d["agents"]["days_to_first_serve"].as_f64())
        .collect();
    let adopted = out
        .iter()
        .filter(|d| d["people"]["adopted_at"].is_string())
        .count();
    Ok(json!({
        "org": org,
        "scope": scope.to_string(),
        "result": {
        "summary": {
            "decisions": out.len(),
            "served": firsts.len(),
            "median_days_to_first_serve": median(firsts),
            "adopted_by_people": adopted,
            "expired": out.iter().filter(|d| d["expired"] == true).count(),
        },
        "decisions": out,
        },
    }))
}

async fn entry(
    store: &Store,
    session: &Session,
    reader: &Principal,
    a: &Approval,
    facts: &HashMap<String, Fact>,
    reads: &[audit::Entry],
    now: i64,
) -> Result<Value> {
    let new: Vec<&Fact> = a
        .added
        .iter()
        .chain(&a.kept)
        .filter_map(|id| facts.get(id))
        .collect();
    let old: Vec<&Fact> = a.ended.iter().filter_map(|id| facts.get(id)).collect();
    // What only the decision's facts cite: a read that names one of these served what the decision published.
    let old_eps: HashSet<&str> = old
        .iter()
        .flat_map(|f| f.source_episodes.iter().map(String::as_str))
        .collect();
    let marks: HashSet<&str> = new
        .iter()
        .flat_map(|f| f.source_episodes.iter().map(String::as_str))
        .filter(|e| !old_eps.contains(e))
        .collect();
    let mut first: Option<&audit::Entry> = None;
    let mut served = 0;
    let mut scopes: BTreeMap<String, i64> = BTreeMap::new();
    for r in reads.iter().filter(|r| r.at >= a.at) {
        let hit = r
            .detail
            .as_deref()
            .is_some_and(|d| d.split(',').any(|e| marks.contains(e)));
        if !hit {
            continue;
        }
        served += 1;
        if first.is_none_or(|f| r.at < f.at) {
            first = Some(r);
        }
        if let Some(s) = &r.scope {
            let at = scopes.entry(s.clone()).or_insert(r.at);
            *at = (*at).min(r.at);
        }
    }
    let agents = json!({
        "first_served_at": first.map(|r| audit::rfc3339(r.at)),
        "first_served_by": first.map(|r| &r.actor),
        "days_to_first_serve": first.map(|r| round((r.at - a.at) as f64 / DAY_US)),
        "reads_since": served,
        "served_in": scopes.into_iter().map(|(s, at)| (s, json!(audit::rfc3339(at)))).collect::<serde_json::Map<_, _>>(),
        "precision": if a.kind == "stated" { "exact" } else { "source" },
    });
    let subject = new.first().or(old.first());
    let people = match subject {
        Some(s) => people(store, session, reader, a, s, &new, &old).await?,
        None => json!(null),
    };
    Ok(json!({
        "id": a.id,
        "kind": a.kind,
        "deciders": a.deciders,
        "decided_at": audit::rfc3339(a.at),
        "expires_at": a.expires_at.map(audit::rfc3339),
        "expired": a.expires_at.is_some_and(|e| e <= now),
        // What it decided, as this admin may read it (null where they can't read the facts).
        "entity": subject.map(|f| &f.entity),
        "predicate": subject.map(|f| &f.predicate),
        "value": new.first().map(|f| &f.value),
        "was": old.iter().map(|f| &f.value).collect::<Vec<_>>(),
        "facts": {"added": a.added, "kept": a.kept, "ended": a.ended},
        "agents": agents,
        "people": people,
    }))
}

/// What sources said of `s`'s thing and attribute after the decision: the new value against the old.
async fn people(
    store: &Store,
    session: &Session,
    reader: &Principal,
    a: &Approval,
    s: &Fact,
    new: &[&Fact],
    old: &[&Fact],
) -> Result<Value> {
    let key = |v: &str| crate::extract::squash(v);
    let new_vals: HashSet<String> = new.iter().map(|f| key(&f.value)).collect();
    let old_vals: HashSet<String> = old
        .iter()
        .map(|f| key(&f.value))
        .filter(|v| !new_vals.contains(v))
        .collect();
    let decided: HashSet<&str> = a
        .added
        .iter()
        .chain(&a.kept)
        .chain(&a.ended)
        .map(String::as_str)
        .collect();
    let mut said: Vec<(i64, bool)> = session
        .lookup(store, reader, &s.entity, &s.predicate, None)
        .await?
        .into_iter()
        .filter(|f| f.valid_from > a.at && !decided.contains(f.fact_id.as_str()))
        .filter_map(|f| {
            let v = key(&f.value);
            if new_vals.contains(&v) {
                Some((f.valid_from, true))
            } else if old_vals.contains(&v) {
                Some((f.valid_from, false))
            } else {
                None
            }
        })
        .collect();
    said.sort();
    let (mut n, mut yes, mut adopted_at) = (0usize, 0usize, None);
    for (at, is_new) in &said {
        n += 1;
        yes += usize::from(*is_new);
        if adopted_at.is_none() && n >= MIN_MENTIONS && yes as f64 / n as f64 >= ADOPTED_SHARE {
            adopted_at = Some(*at);
        }
    }
    // Adoption that held: a later slide below the share undoes it.
    if n > 0 && (yes as f64 / n as f64) < ADOPTED_SHARE {
        adopted_at = None;
    }
    Ok(json!({
        "mentions_new": yes,
        "mentions_old": n - yes,
        "share_new": (n > 0).then(|| round(yes as f64 / n as f64)),
        "adopted_at": adopted_at.map(audit::rfc3339),
        "days_to_adoption": adopted_at.map(|t| round((t - a.at) as f64 / DAY_US)),
        "match": "value words; a paraphrase isn't matched",
    }))
}

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    let m = xs.len() / 2;
    Some(round(if xs.len().is_multiple_of(2) {
        (xs[m - 1] + xs[m]) / 2.0
    } else {
        xs[m]
    }))
}
