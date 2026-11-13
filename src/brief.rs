//! The brief: one read that tells an agent where a subject (a deal) stands. It puts together what aggregation
//! derived (each element's score and the items' statuses) with the facts behind them and the subject's current
//! facts, every line citing the moment it comes from, so an agent prepping a call reads a page instead of every
//! episode.
//!
//! Everything in it is what the caller may read: aggregate rows whose every episode they may read, and facts
//! whose every episode they may read. Facts aggregation marked superseded are left out; stale facts and passed
//! plans keep their flag. 4000 tokens (about four characters each) cap the brief. The subject's own facts always get a quarter of it (newest first); the summary fits in
//! the rest, losing closed items first, then evidence beyond the first behind each row. The brief says how
//! much was left out. Each fact is one compact line (its statement, where it was said, a short quote); its id
//! leads back to the rest.

use crate::aggregate::StoreAggregates as _;
use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde_json::{Value, json};

use crate::aggregate::Row;
use crate::{Fact, Principal, Session, Store, search};

/// The brief's size, in tokens.
const DEFAULT_BUDGET: usize = 4000;
/// Facts shown behind each score or item; the rest are counted.
const EVIDENCE: usize = 3;
/// The share of the budget (one part in this many) always left for the subject's own facts.
const FACT_SHARE: usize = 4;
/// Words of a quote the brief keeps.
const QUOTE_WORDS: usize = 24;

/// One fact as the brief shows it, compactly: the statement with its unit, where it was said, a short quote, and
/// how its time is known when it isn't simply as of the episode. The fact id leads back to all of it.
fn fact_json(f: &Fact, flag: Option<&String>) -> Value {
    let value = match &f.unit {
        Some(u) => format!("{} {u}", f.value),
        None => f.value.clone(),
    };
    let mut v = json!({"fact_id": f.fact_id, "says": format!("{} = {value}", f.predicate)});
    if let Some(c) = f
        .cite
        .as_deref()
        .and_then(|c| serde_json::from_str::<crate::episode_types::Cite>(c).ok())
    {
        v["at"] = json!(c.describe());
    }
    if let Some(q) = &f.quote {
        let words: Vec<&str> = q.split_whitespace().collect();
        v["quote"] = json!(if words.len() > QUOTE_WORDS {
            format!("{} …", words[..QUOTE_WORDS].join(" "))
        } else {
            q.clone()
        });
    }
    if let Some(b) = f.time_basis.as_deref().filter(|b| *b != "as_of_source") {
        v["time"] = json!(b);
    }
    if let Some(fl) = flag {
        v["flag"] = json!(fl);
    }
    v
}

/// The brief on `subject`, as `who` may read it in `session`.
pub async fn brief(
    session: &Session,
    store: &Store,
    who: &Principal,
    subject: &str,
) -> Result<Value> {
    let (rows, coverage) = crate::aggregate::current_covering(session, store, who, subject).await?;
    // A summary over fewer episodes than the reader may read: ask for one over all of them (on main, as it is
    // now), built by the next aggregation. A failure to ask costs only that: the brief is served as is.
    let partial: Vec<&crate::aggregate::Coverage> =
        coverage.iter().filter(|c| c.covers < c.of).collect();
    if !partial.is_empty() && session.on_main_now() {
        let audience = crate::aggregate::audience_of(who);
        for c in &partial {
            if let Err(e) = store
                .request_aggregate(session.scope(), &c.goal, subject, &audience)
                .await
            {
                eprintln!("brief: asking for {subject}'s summary for its reader failed: {e:#}");
            }
        }
    }
    // Only the facts the brief shows: those about the subject and its items, and those the rows cite.
    let mut about: Vec<String> = rows
        .iter()
        .filter(|r| r.kind == "item_status")
        .flat_map(|r| r.key.split(',').map(String::from))
        .collect();
    about.push(subject.to_string());
    about.sort();
    about.dedup();
    let mut cited: Vec<String> = rows.iter().flat_map(|r| r.cites.iter().cloned()).collect();
    cited.sort();
    cited.dedup();
    let (facts, cited) = tokio::try_join!(
        session.read_in(store, who, search::Col::Entity, &about, None),
        session.read_in(store, who, search::Col::FactId, &cited, None),
    )?;
    let by_id: HashMap<&str, &Fact> = cited.iter().map(|f| (f.fact_id.as_str(), f)).collect();
    let stored: HashMap<&str, &String> = rows
        .iter()
        .filter(|r| r.kind == "flag")
        .map(|r| (r.key.as_str(), &r.value))
        .collect();
    // Where the scope marks freshness (`crate::freshness`), what is stale or overdue now, rather than when the
    // subject was last summarised: it replaces a stored flag but `superseded` and `ended`.
    let now = session.now();
    let schema = session.vis().schema.clone();
    let schemas: Vec<&crate::query::schema::Schema> = schema.as_deref().into_iter().collect();
    let marking =
        crate::freshness::any(&schemas) && crate::freshness::on(store, session.scope()).await;
    let current: Vec<&Fact> = facts
        .iter()
        .filter(|f| f.valid_to.is_none_or(|t| t > now))
        .collect();
    let mut marks = match marking {
        true => {
            let said = crate::corroboration::Said {
                vis: vec![session.vis()],
                who,
            };
            crate::freshness::marks(&schemas, &current, now, &said)
        }
        false => HashMap::new(),
    };
    // And what the claims of absence among them come to, whatever the scope's setting (`crate::absence`).
    let checked = crate::absence::Checked::default();
    marks.extend(crate::absence::marks(store, &[session], who, &current, now, &checked).await);
    // And what another fact says against one now, where the scope keeps conflicts (`crate::conflicts`).
    let disputed = match crate::conflicts::on(store, session.scope()).await {
        true => {
            let current: Vec<&Fact> = facts
                .iter()
                .chain(&cited)
                .filter(|f| f.valid_to.is_none_or(|t| t > now))
                .collect();
            crate::conflicts::marks(store, session, who, &current, Some(now)).await?
        }
        false => HashMap::new(),
    };
    let flags: HashMap<&str, String> = facts
        .iter()
        .chain(&cited)
        .filter_map(|f| {
            let kept = stored
                .get(f.fact_id.as_str())
                .filter(|s| s.as_str() == "superseded" || s.as_str() == "ended");
            let flag = match (kept, marks.get(&f.fact_id)) {
                (Some(s), _) => Some(s.to_string()),
                (None, Some(m)) => Some(m.note(&f.predicate)),
                (None, None) => stored.get(f.fact_id.as_str()).map(|s| s.to_string()),
            };
            // A conflict is said beside whatever else is.
            let flag = match (flag, disputed.get(&f.fact_id)) {
                (Some(fl), Some(c)) => Some(format!("{fl}; {}", c.note())),
                (None, Some(c)) => Some(c.note()),
                (fl, None) => fl,
            };
            flag.map(|fl| (f.fact_id.as_str(), fl))
        })
        .collect();
    // At most EVIDENCE facts behind a row, leaving out those about `skip` (an item's own facts, which the item
    // already states), and how many more it cites.
    let evidence = |r: &Row, skip: &[&str]| -> (Vec<Value>, usize) {
        let all: Vec<&&Fact> = r
            .cites
            .iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .filter(|f| !skip.contains(&f.entity.as_str()))
            .collect();
        let shown = all
            .iter()
            .take(EVIDENCE)
            .map(|f| fact_json(f, flags.get(f.fact_id.as_str())))
            .collect();
        (shown, all.len().saturating_sub(EVIDENCE))
    };
    // The schema's contract (its `extract` section) behind the rows, for its scale.
    let goals: BTreeMap<String, crate::extract::goals::Contract> =
        crate::extract::goals::session_schema_contract(session)
            .into_iter()
            .map(|c| (crate::extract::goals::SCHEMA.to_string(), c))
            .collect();
    let mut scorecard = vec![];
    let mut gaps = vec![];
    let (mut open, mut closed) = (vec![], vec![]);
    for r in &rows {
        match r.kind.as_str() {
            "element" => {
                let lowest = goals
                    .get(&r.goal)
                    .and_then(|g| g.aggregates.as_ref())
                    .map(|a| a.scale()[0].clone())
                    .unwrap_or_else(|| "none".into());
                if r.value == lowest {
                    gaps.push(json!({"element": r.key, "note": r.note}));
                }
                let (ev, more) = evidence(r, &[]);
                let mut e =
                    json!({"element": r.key, "status": r.value, "note": r.note, "evidence": ev});
                if more > 0 {
                    e["more_evidence"] = json!(more);
                }
                scorecard.push(e);
            }
            "item_status" => {
                let ids: Vec<&str> = r.key.split(',').collect();
                // The item's own facts (action, owner, due ...), from each of its ids, the newest last.
                let mut said: BTreeMap<String, Value> = BTreeMap::new();
                let mut item_facts: Vec<&Fact> = facts
                    .iter()
                    .filter(|f| ids.contains(&f.entity.as_str()))
                    .collect();
                item_facts.sort_by_key(|f| f.valid_from);
                for f in item_facts {
                    said.insert(f.predicate.clone(), json!(f.value));
                }
                let first = goals
                    .get(&r.goal)
                    .and_then(|g| g.aggregates.as_ref())
                    .and_then(|a| {
                        ids.first()
                            .and_then(|i| i.split_once(':'))
                            .and_then(|(t, _)| a.items.get(t))
                    })
                    .and_then(|s| s.first().cloned());
                let (ev, more) = evidence(r, &ids);
                let mut item =
                    json!({"items": ids, "status": r.value, "note": r.note, "said": said});
                // Still open once its plan's date has passed: overdue since then.
                if marking && first.as_deref() == Some(r.value.as_str()) {
                    let of: Vec<&Fact> = facts
                        .iter()
                        .filter(|f| ids.contains(&f.entity.as_str()))
                        .filter(|f| f.valid_to.is_none_or(|t| t > now))
                        .collect();
                    if let Some((since, plan)) = crate::freshness::passed_plan(&schemas, &of, now) {
                        item["freshness"] =
                            crate::freshness::Mark::Overdue { since, plan }.to_json("status");
                    }
                }
                if !ev.is_empty() {
                    item["evidence"] = json!(ev);
                }
                if more > 0 {
                    item["more_evidence"] = json!(more);
                }
                if first.as_deref() == Some(r.value.as_str()) {
                    open.push(item);
                } else {
                    closed.push(item);
                }
            }
            _ => {}
        }
    }
    // The subject's own facts, newest first, without those superseded.
    let mut own: Vec<&Fact> = facts
        .iter()
        .filter(|f| {
            f.entity == subject
                && stored
                    .get(f.fact_id.as_str())
                    .is_none_or(|fl| fl.as_str() != "superseded")
        })
        .collect();
    own.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
    let mut own: Vec<Value> = own
        .iter()
        .map(|f| fact_json(f, flags.get(f.fact_id.as_str())))
        .collect();
    let summarised = !rows.is_empty();
    let mut out = json!({
        "subject": subject,
        "summarised": summarised,
        "aggregator": rows.first().map(|r| r.aggregator.clone()),
        "as_of": rows.first().map(|r| r.as_of),
        // When the summary was made, as a time a reader reads; `get_brief` with `refresh` makes it again.
        "summarised_at": rows.first().and_then(|r| chrono::DateTime::from_timestamp_micros(r.as_of)).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "covers": coverage.iter().map(|c| json!({"goal": shown_goal(&c.goal), "episodes": c.covers, "of": c.of})).collect::<Vec<_>>(),
        "scorecard": scorecard,
        "gaps": gaps,
        "open_items": open,
        "closed_items": closed,
        "facts": [],
    });
    if !partial.is_empty() {
        out["partial"] = json!(
            "The summary covers fewer of the subject's episodes than you may read: one over all of them is on its way."
        );
    }
    if !summarised {
        out["note"] = json!(
            "No summary you may read: the subject isn't aggregated yet (subjects are summarised every hour; get_brief with refresh summarises it now), or its summary draws on an episode you can't read."
        );
    }
    // Within the brief's read budget.
    let budget = Some(DEFAULT_BUDGET * 4);
    let size = |v: &Value| v.to_string().len();
    let mut omitted = 0;
    if let Some(cap) = budget {
        // The subject's own facts get what the summary leaves, and never less than a share of the
        // budget, so a full scorecard can't crowd out every fact. Newest are kept.
        let reserve = (cap / FACT_SHARE).max(cap.saturating_sub(size(&out)));
        let mut used = 0;
        let keep = own
            .iter()
            .take_while(|f| {
                used += size(f) + 1;
                used <= reserve
            })
            .count();
        omitted += own.len() - keep;
        let all = own.clone();
        own.truncate(keep);
        let room = cap.saturating_sub(own.iter().map(|f| size(f) + 1).sum::<usize>());
        // Then the summary fits in the rest: closed items go first, then evidence beyond the first
        // behind each score and item (counted in its `more_evidence`).
        while size(&out) > room
            && out["closed_items"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        {
            out["closed_items"].as_array_mut().unwrap().pop();
            omitted += 1;
        }
        // A row at a time, down to two pieces of evidence and then one, until the summary fits.
        'trim: for keep in (1..EVIDENCE).rev() {
            for list in ["scorecard", "open_items"] {
                let n = out[list].as_array().map_or(0, Vec::len);
                for i in 0..n {
                    if size(&out) <= room {
                        break 'trim;
                    }
                    let row = &mut out[list][i];
                    let Some(ev) = row.get_mut("evidence").and_then(Value::as_array_mut) else {
                        continue;
                    };
                    if ev.len() > keep {
                        let cut = ev.len() - keep;
                        ev.truncate(keep);
                        let more = row["more_evidence"].as_u64().unwrap_or(0) + cut as u64;
                        row["more_evidence"] = json!(more);
                    }
                }
            }
        }
        // Whatever room the summary left goes back to facts.
        let mut total = size(&out) + own.iter().map(|f| size(f) + 1).sum::<usize>();
        while let Some(f) = all.get(own.len()) {
            if total + size(f) + 1 > cap {
                break;
            }
            total += size(f) + 1;
            own.push(f.clone());
            omitted -= 1;
        }
    }
    out["facts"] = json!(own);
    if omitted > 0 {
        out["omitted"] = json!(omitted);
    }
    Ok(out)
}

/// A summary's goal as the brief names it: the schema's contract is `schema`.
fn shown_goal(goal: &str) -> &str {
    if goal == crate::extract::goals::SCHEMA {
        "schema"
    } else {
        goal
    }
}
