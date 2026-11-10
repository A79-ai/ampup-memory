//! Feedback on a memory (G12, GM11): a reader says a fact was helpful or wrong, and why. Search shows each
//! fact's tally, so an agent (and later the ranking, GM10) can weigh a fact the team has flagged.
//!
//! `feedback_memory` (MCP) and `POST /feedback {fact_id, signal, reason?}`: `signal` is `positive`,
//! `negative` or `very_negative`. One vote per person per fact: a later one replaces theirs. A caller may
//! give feedback only on a fact they can read, and the answer is the same for a fact that doesn't exist, so
//! it says nothing of facts they can't see. Only a person votes (a service key, which anyone holding several
//! could vote with once each, may not). Search shows a fact's tally to those who may read the fact, never who
//! gave it; the reasons are kept for the memory's owners (the improvement loop), not shown in search, where
//! anonymous text would reach every agent that hits the fact.
//!
//! Rows go to the scope's `feedback` table through the ledger. Each names the episodes the fact comes from, so
//! erasing an episode erases the feedback on its facts, and forgetting a person erases the feedback they gave.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::Result;
use arrow_array::{ArrayRef, StringArray};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    FEEDBACK, Ledger, MAIN, OnConflict, Principal, Scope, Store, append, now_micros,
    optional_strings, split_list, strings, timestamps, ts,
};

/// Longest reason kept, in characters.
const MAX_REASON: usize = 500;
/// Most votes (changes included) one person may give in a scope in a day.
const MAX_PER_DAY: usize = 100;
/// The signals, best to worst.
pub const SIGNALS: [&str; 3] = ["positive", "negative", "very_negative"];

/// A caller's mistake, said back to them as is.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A fact's feedback as its readers see it: how many of each signal, and the reasons given (newest first).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    pub positive: usize,
    pub negative: usize,
    pub very_negative: usize,
    /// The reasons given, newest first: for the memory's owners, never put in search results.
    #[serde(skip)]
    pub reasons: Vec<String>,
}

impl Tally {
    /// As search shows it.
    pub fn to_json(&self) -> Value {
        json!(self)
    }
}

/// Records `who`'s feedback on a fact they can read. Returns the fact id.
pub async fn give(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    person: bool,
    args: &Value,
) -> Result<String> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    if !person {
        return Err(refuse(format!(
            "feedback is a person's vote: {} is a key",
            who.user()
        )));
    }
    let fact_id = args["fact_id"].as_str().unwrap_or_default().trim();
    if fact_id.is_empty() {
        return Err(refuse("fact_id is required".into()));
    }
    let signal = args["signal"].as_str().unwrap_or_default();
    if !SIGNALS.contains(&signal) {
        return Err(refuse(format!(
            "signal is positive, negative or very_negative, not {signal:?}"
        )));
    }
    let reason = args["reason"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if reason.is_some_and(|r| r.chars().count() > MAX_REASON) {
        return Err(refuse(format!("reason is at most {MAX_REASON} characters")));
    }
    let today = store
        .feedback_rows_since(scope, who.user(), now_micros() - 86_400_000_000)
        .await?;
    if today >= MAX_PER_DAY {
        return Err(refuse(format!(
            "{} has given feedback {today} times in {scope} today, the most a day",
            who.user()
        )));
    }
    // Only a fact the caller may read; one that doesn't exist answers the same.
    let Some(fact) = store.fact_seen(scope, who, fact_id).await? else {
        return Err(refuse(format!("no fact {fact_id} you can read in {scope}")));
    };
    store
        .write_feedback(
            scope,
            who.user(),
            fact_id,
            signal,
            reason,
            &fact.source_episodes,
        )
        .await?;
    Ok(fact_id.to_string())
}

/// The feedback on `facts` as `who` may see it: for each fact they can read that has any, its tally. A fact
/// whose episodes they can't all read shows none.
pub async fn tallies(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    facts: &[(String, Vec<String>)],
) -> Result<HashMap<String, Tally>> {
    if facts.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = store.side_rows(&scope.namespace(), FEEDBACK).await?;
    if rows.is_empty() {
        return Ok(HashMap::new());
    }
    let vis = store.visibility(scope, MAIN).await?;
    let wanted: HashMap<&str, &Vec<String>> =
        facts.iter().map(|(f, eps)| (f.as_str(), eps)).collect();
    // The newest vote per (fact, giver).
    let mut newest: BTreeMap<(String, String), (i64, String, Option<String>)> = BTreeMap::new();
    for b in rows.iter() {
        let (commit, fact, giver, signal, episodes) = (
            strings(b, "commit_id"),
            strings(b, "fact_id"),
            strings(b, "giver"),
            strings(b, "signal"),
            strings(b, "episodes"),
        );
        let reason = optional_strings(b, "reason");
        let at = timestamps(b, "recorded_at");
        for i in 0..b.num_rows() {
            if !wanted.contains_key(fact[i].as_str())
                || !vis.committed.contains(&commit[i])
                || split_list(&episodes[i]).iter().any(|e| vis.is_gone(e))
            {
                continue;
            }
            let t = at[i].unwrap_or_default();
            let key = (fact[i].clone(), crate::auth::norm_id(&giver[i]));
            if newest.get(&key).is_some_and(|(seen, _, _)| *seen >= t) {
                continue;
            }
            newest.insert(key, (t, signal[i].clone(), reason[i].clone()));
        }
    }
    let mut out: HashMap<String, (Tally, Vec<(i64, String)>)> = HashMap::new();
    for ((fact, _), (t, signal, reason)) in newest {
        // Shown only with the fact itself: to a reader who may read every episode it cites.
        let readable = wanted[fact.as_str()]
            .iter()
            .all(|e| vis.can_see_episode(e, who));
        if !readable {
            continue;
        }
        let (tally, reasons) = out.entry(fact).or_default();
        match signal.as_str() {
            "positive" => tally.positive += 1,
            "negative" => tally.negative += 1,
            _ => tally.very_negative += 1,
        }
        if let Some(r) = reason {
            reasons.push((t, r));
        }
    }
    Ok(out
        .into_iter()
        .map(|(f, (mut tally, mut reasons))| {
            reasons.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
            tally.reasons = reasons.into_iter().map(|(_, r)| r).collect();
            (f, tally)
        })
        .collect())
}

/// The feedback table's reads and writes on a [`Store`] (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreFeedback {
    async fn write_feedback(
        &self,
        scope: &Scope,
        giver: &str,
        fact_id: &str,
        signal: &str,
        reason: Option<&str>,
        episodes: &[String],
    ) -> Result<()>;
    async fn feedback_rows_since(&self, scope: &Scope, giver: &str, since: i64) -> Result<usize>;
}

impl StoreFeedback for Store {
    /// Votes `giver` gave in the scope since `since` (µs).
    async fn feedback_rows_since(&self, scope: &Scope, giver: &str, since: i64) -> Result<usize> {
        let rows = self.side_rows(&scope.namespace(), FEEDBACK).await?;
        Ok(rows
            .iter()
            .map(|b| {
                strings(b, "giver")
                    .iter()
                    .zip(timestamps(b, "recorded_at"))
                    .filter(|(g, t)| g.as_str() == giver && t.is_some_and(|t| t >= since))
                    .count()
            })
            .sum())
    }

    async fn write_feedback(
        &self,
        scope: &Scope,
        giver: &str,
        fact_id: &str,
        signal: &str,
        reason: Option<&str>,
        episodes: &[String],
    ) -> Result<()> {
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, FEEDBACK).await?;
        let one = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v.to_string()])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", one(&id)),
                ("fact_id", one(fact_id)),
                ("giver", one(giver)),
                ("signal", one(signal)),
                (
                    "reason",
                    Arc::new(StringArray::from(vec![reason.map(String::from)])),
                ),
                ("episodes", one(&episodes.join(","))),
                ("recorded_at", ts([Some(now_micros())].into_iter())),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, FEEDBACK);
        self.append_ledger(
            &id,
            scope,
            MAIN,
            &format!("feedback:{giver}"),
            &|l: &Ledger| Ok(l.heads(MAIN)),
        )
        .await?;
        Ok(())
    }
}
