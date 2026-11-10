//! Facts that disagree. Where two facts give one thing's attribute values that can't both be taken as its value,
//! and neither plainly corrects the other (two sources that disagree, an estimate beside a reported figure, a figure
//! on another basis), write-time reconcile records the two as in conflict instead of ending one: both stay, and a
//! read that serves one says what the other says, from where and when, so a reader never takes one for the other.
//!
//! A conflict is a record of its own: an engine document of ids only, `conflicts/<known fact>/<new fact>`, from a
//! [`SOURCE`] source, committed with the facts reconcile kept. No reader reads the record itself ([`ACL`]): a read
//! serves a conflict only to a reader who may read both facts at the time read ([`marks`]), so one who can't read
//! a side sees nothing of it, and a side forgotten, retracted, aged out or ended (a closure, a correction that
//! supersedes it) ends the conflict for everyone it took from. Withdrawing the extractor takes its records too.
//!
//! Reconcile records conflicts and reads mark them only where the scope asks ([`on`]): it changes what readers see
//! and the reconcile prompt.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::{Value, json};

use crate::{Fact, Principal, Session, Store, search};

/// Where conflicts are recorded, `conflicts/<known fact>/<new fact>`.
pub const PREFIX: &str = "conflicts/";
/// The source of a conflict record ([`crate::episode_types::ENGINE_SOURCES`]): `reconcile:<extractor>`.
pub const SOURCE: &str = "reconcile:";
/// The ACL of a conflict record: an entry no reader holds (no sign-in names an address under `.invalid`). Reads
/// serve what it records only to readers of both its facts.
pub const ACL: &str = "user:conflicts@memstore.invalid";
/// The environment variable that turns conflicts on for every scope that doesn't set `conflicts` itself.
pub const ENV: &str = "MEMSTORE_CONFLICTS";

/// How much of the other value a mark quotes.
const VALUE_CHARS: usize = 160;

/// What a read says beside a value another fact disagrees with: the other fact, its value, where it is from (its
/// first episode's source) and when it began.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub fact_id: String,
    pub value: String,
    pub since: i64,
    pub episode_id: String,
    pub source: String,
}

impl Conflict {
    /// `conflict: "~$252M, an analyst's estimate" (gdrive:analyst-note, 2026-08-15)`.
    pub fn note(&self) -> String {
        let mut v: String = self.value.chars().take(VALUE_CHARS).collect();
        if v.len() < self.value.len() {
            v.push('…');
        }
        format!(
            "conflict: \"{v}\" ({}, {})",
            self.source,
            crate::freshness::day(self.since)
        )
    }

    /// `{"fact_id", "value", "since", "episode_id", "source", "note"}`, as JSON answers carry it (`conflict`).
    pub fn to_json(&self) -> Value {
        json!({
            "fact_id": self.fact_id, "value": self.value, "since": crate::freshness::day(self.since),
            "episode_id": self.episode_id, "source": self.source, "note": self.note(),
        })
    }
}

/// Whether the environment turns conflicts on (`MEMSTORE_CONFLICTS=1`, `true` or `on`) where no setting says.
pub fn from_env() -> bool {
    std::env::var(ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// Whether `scope` records conflicts and its reads mark them: its team's `conflicts` setting, else its org's, else
/// [`ENV`]. Off unless one of them turns it on.
pub async fn on(store: &Store, scope: &crate::Scope) -> bool {
    match store.directory().await {
        Ok(d) => d.dir.marks_conflicts(scope),
        Err(_) => from_env(),
    }
}

/// The recorded conflicts, from the committed documents' versions by key: each fact with the facts it was recorded
/// in conflict with, both ways. Only `records` count, the committed episodes from a [`SOURCE`] source that aren't
/// retracted: a document a caller files under [`PREFIX`] records nothing.
pub(crate) fn recorded(
    documents: &HashMap<String, Vec<(i64, String)>>,
    records: &HashSet<String>,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (key, versions) in documents {
        let Some((a, b)) = key.strip_prefix(PREFIX).and_then(|r| r.split_once('/')) else {
            continue;
        };
        if a == b || !versions.iter().any(|(_, id)| records.contains(id)) {
            continue;
        }
        for (x, y) in [(a, b), (b, a)] {
            let to = out.entry(x.to_string()).or_default();
            if !to.iter().any(|t| t == y) {
                to.push(y.to_string());
            }
        }
    }
    out
}

/// The conflicts `who` is shown on `facts`, by fact id, as `session` reads them at `valid_at` (`i64::MAX`: now; None,
/// a read over history: none). The other fact must be one the reader may read, valid at that time for them (not
/// ended by a closure they can see): otherwise nothing of it is said. Of several, the newest.
pub async fn marks(
    store: &Store,
    session: &Session,
    who: &Principal,
    facts: &[&Fact],
    valid_at: Option<i64>,
) -> Result<HashMap<String, Conflict>> {
    let Some(valid_at) = valid_at else {
        return Ok(HashMap::new());
    };
    let vis = session.vis();
    let pairs: Vec<(&str, &str)> = facts
        .iter()
        .flat_map(|f| {
            vis.conflicts_of(&f.fact_id)
                .iter()
                .map(|o| (f.fact_id.as_str(), o.as_str()))
        })
        .collect();
    if pairs.is_empty() {
        return Ok(HashMap::new());
    }
    let mut ids: Vec<String> = pairs.iter().map(|(_, o)| o.to_string()).collect();
    ids.sort();
    ids.dedup();
    let others = session
        .read_in(store, who, search::Col::FactId, &ids, Some(valid_at))
        .await?;
    let by_id: HashMap<&str, &Fact> = others.iter().map(|f| (f.fact_id.as_str(), f)).collect();
    let episodes: Vec<String> = others
        .iter()
        .filter_map(|f| f.source_episodes.first().cloned())
        .collect();
    let sources = session.episode_sources(store, who, &episodes).await?;
    let mut out: HashMap<String, Conflict> = HashMap::new();
    for (shown, other) in pairs {
        let Some(o) = by_id.get(other) else {
            continue;
        };
        let Some((episode, (source, _))) = o
            .source_episodes
            .first()
            .and_then(|e| Some((e, sources.get(e)?)))
        else {
            continue;
        };
        if out
            .get(shown)
            .is_some_and(|c| (c.since, &c.fact_id) >= (o.valid_from, &o.fact_id))
        {
            continue;
        }
        out.insert(
            shown.to_string(),
            Conflict {
                fact_id: o.fact_id.clone(),
                value: o.value.clone(),
                since: o.valid_from,
                episode_id: episode.clone(),
                source: source.clone(),
            },
        );
    }
    Ok(out)
}
