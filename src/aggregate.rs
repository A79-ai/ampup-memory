//! Aggregation (GM4): what a goal's facts add up to, per subject, derived in batches. Extraction writes simple
//! statements and never a status; this is where status comes from. The scope's schema summarises what its own
//! `extract` section extracts the same way, from `extract.aggregates` (`crate::extract::goals::SCHEMA`), as its
//! rows' goal.
//!
//! A goal's `aggregates` names the entity type it summarises (`deal`). A subject's batch is every fact of every
//! episode about it (`about`), of any episode type, so a promise made on a call and its follow-up in Slack are
//! read together. A subject is summarised again once `every` new episodes about it have come in (default 10),
//! or once its new episodes have waited `max_wait_minutes` (default 60), or on demand.
//!
//! Code does what needs no judgement: a one-valued predicate's newer value supersedes the older, a fact past its
//! predicate's freshness is stale, a plan whose date has passed is flagged. One model call per batch does the
//! rest: it links items across episodes (the same commitment restated later), gives each its status, and scores
//! each element of the goal's methodology, every answer citing the fact ids it rests on (citations outside the
//! batch are dropped).
//!
//! The results are rows of the scope's `aggregates` table, committed through the ledger by the aggregator
//! (`aggregate:<model>@a1/goal:<id>@v<n>`, or `.../schema@v<n>`); a subject's newest run replaces its previous one. A row names every
//! episode of its batch, and a reader sees it only if they may read all of them: its notes may draw on any of
//! them. Nothing is rewritten, so a retracted episode or a changed ACL takes effect on the next read.
//!
//! A subject is summarised per audience: once over all its episodes (`*`), and once for each ACL entry that
//! may read only some of them, over those (at most six runs, widest first). A reader gets the widest run they
//! may read whole, so a rep on one call of a two-call deal gets a summary of that call rather than none.
//!
//! A reader whose access no single entry gives (their user and a group) asks for their own: when the widest run
//! they may read covers fewer of the subject's episodes than they may read, the brief says so (`covers`,
//! `partial`) and records a request for their audience (their ACL tokens) in `aggregate_requests`. The next
//! aggregation builds a run over exactly the episodes that audience may read; anyone with the same access reads
//! it too. A request, like any run, draws only on episodes its audience may read, and a reader still sees a run
//! only if they may read all of it. Forgetting a person drops their requests and the runs made for them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{ArrayRef, Int64Array, StringArray};
use serde::Serialize;
use serde_json::{Value, json};

use crate::extract::Extractor;
use crate::extract::goals::{Contract, SCHEMA};
use crate::query::schema::Aggregates;
use crate::query::schema::Card;
use crate::{
    AGG_REQUESTS, AGGREGATES, EPISODES, FACTS, Fact, Ledger, MAIN, OnConflict, Principal, Scope,
    Session, Store, Vis, append, decode_facts, now_micros, optional_strings, split_list, strings,
    timestamps, ts,
};

/// Bumped when the summariser's prompt changes meaning: part of every aggregator's name.
const PROMPT_VERSION: &str = "a1";
/// The summariser's reply may be long: a deal has many items and elements.
const MAX_REPLY_TOKENS: u32 = 16_000;

/// One derived row, as stored and as read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub goal: String,
    pub subject: String,
    /// `item_status`, `element` or `flag`.
    pub kind: String,
    /// The item ids it covers (comma-joined), the element, or the flagged fact.
    pub key: String,
    pub value: String,
    pub note: Option<String>,
    pub cites: Vec<String>,
    /// Every episode of the batch it came from.
    pub episodes: Vec<String>,
    /// Who the run was for: `*` for every episode about the subject, or the ACL entry whose episodes it read.
    pub audience: String,
    pub as_of: i64,
    pub aggregator: String,
    /// What the run's model call cost, in and out tokens (the same on every row of a run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<(i64, i64)>,
}

/// The most runs a subject gets per goal: its widest audiences, the whole subject first.
const MAX_AUDIENCES: usize = 6;
/// The audience that reads every episode about the subject (and every run written before audiences).
const EVERYONE: &str = "*";

/// One (goal, subject, audience)'s runs, by (recorded at, commit).
type RunsByCommit = BTreeMap<(i64, String), Vec<Row>>;

/// What a run did.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Run {
    /// Subjects summarised.
    pub subjects: usize,
    pub rows: usize,
    /// Subjects that waited for more episodes.
    pub waiting: usize,
    pub failed: usize,
    /// Subjects with something new asked for again ([`When::Changed`]) within [`REFRESH_GAP`] of their last run or
    /// attempt: left as they are.
    pub recent: usize,
}

/// Which of a subject's audiences a run summarises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// Those with a full batch (`every` new episodes) or a long wait (`max_wait_minutes`): the hourly pass.
    Due,
    /// Those with anything new since their last run, unless that run or an attempt at one was within
    /// [`REFRESH_GAP`]: a reader's refresh. Nothing new costs nothing.
    Changed,
    /// All of them, new or not (`memstore aggregate --now`).
    All,
}

/// How soon after a subject's last run a refresh may summarise it again: a refresh is a paid model call per
/// audience, and an agent may ask for one on every read.
pub const REFRESH_GAP: i64 = 5 * 60_000_000;

/// An episode, as aggregation reads it.
#[derive(Clone)]
pub(crate) struct Ep {
    pub(crate) id: String,
    pub(crate) source: String,
    occurred_at: i64,
    pub(crate) about: Vec<String>,
    /// Who may read it.
    pub(crate) acl: Vec<String>,
}

/// The engine's view of a scope on main: committed, unretracted episodes, and the facts citing them.
pub(crate) struct View {
    pub(crate) episodes: Vec<Ep>,
    pub(crate) facts: Vec<Fact>,
}

/// The aggregate tables' reads and writes on a [`Store`] (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreAggregates {
    async fn write_aggregates(&self, scope: &Scope, actor: &str, rows: &[Row]) -> Result<String>;
    async fn aggregate_runs(
        &self,
        scope: &Scope,
        vis: &Vis,
    ) -> Result<HashMap<(String, String, String), (i64, Vec<Row>)>>;
    async fn aggregation_view(&self, scope: &Scope) -> Result<View>;
    async fn aggregate_requests(
        &self,
        scope: &Scope,
    ) -> Result<HashMap<(String, String), BTreeSet<String>>>;
    async fn request_aggregate(
        &self,
        scope: &Scope,
        goal: &str,
        subject: &str,
        audience: &str,
    ) -> Result<bool>;
}

impl StoreAggregates for Store {
    async fn aggregation_view(&self, scope: &Scope) -> Result<View> {
        let ns = scope.namespace();
        let (vis, eps, facts) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.rows(&ns, EPISODES),
            self.rows(&ns, FACTS)
        )?;
        let mut episodes: BTreeMap<String, Ep> = BTreeMap::new();
        for b in eps.rows.iter() {
            let (commit, id, source) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            let (at, about) = (timestamps(b, "occurred_at"), optional_strings(b, "about"));
            for i in 0..b.num_rows() {
                if !vis.committed.contains(&commit[i]) || vis.is_retracted(&id[i]) {
                    continue;
                }
                episodes.entry(id[i].clone()).or_insert_with(|| Ep {
                    id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: at[i].unwrap_or_default(),
                    about: about[i].as_deref().map(split_list).unwrap_or_default(),
                    acl: vis.acls.get(&id[i]).cloned().unwrap_or_default(),
                });
            }
        }
        let mut out = vec![];
        for b in facts.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                // A value a merge set aside for the other side's never held on main: left out, or the newest
                // version would be it and the value kept would read as superseded.
                if vis.committed.contains(&commit)
                    && !vis.retracted_facts.contains(&f.fact_id)
                    && !vis.closed_by_merge(&f.fact_id)
                    && !f.source_episodes.is_empty()
                    && f.source_episodes.iter().all(|e| episodes.contains_key(e))
                {
                    out.push(f);
                }
            }
        }
        Ok(View {
            episodes: episodes.into_values().collect(),
            facts: out,
        })
    }

    /// Every committed aggregate row, newest run per (goal, subject, audience).
    async fn aggregate_runs(
        &self,
        scope: &Scope,
        vis: &Vis,
    ) -> Result<HashMap<(String, String, String), (i64, Vec<Row>)>> {
        let rows = self.side_rows(&scope.namespace(), AGGREGATES).await?;
        // Rows of one run share a commit; the newest commit per (goal, subject) is current.
        let mut runs: HashMap<(String, String, String), RunsByCommit> = HashMap::new();
        for b in rows.iter() {
            let s = |c: &str| strings(b, c);
            let (commit, goal, subject, kind, key, value) = (
                s("commit_id"),
                s("goal"),
                s("subject"),
                s("kind"),
                s("key"),
                s("value"),
            );
            let (note, cites, episodes, aggregator) = (
                optional_strings(b, "note"),
                optional_strings(b, "cites"),
                s("episodes"),
                s("aggregator"),
            );
            let (as_of, at) = (timestamps(b, "as_of"), timestamps(b, "recorded_at"));
            let audience = optional_strings(b, "audience");
            for i in 0..b.num_rows() {
                if !vis.committed.contains(&commit[i]) {
                    continue;
                }
                let audience = audience[i].clone().unwrap_or_else(|| EVERYONE.into());
                runs.entry((goal[i].clone(), subject[i].clone(), audience.clone()))
                    .or_default()
                    .entry((at[i].unwrap_or_default(), commit[i].clone()))
                    .or_default()
                    .push(Row {
                        goal: goal[i].clone(),
                        subject: subject[i].clone(),
                        kind: kind[i].clone(),
                        key: key[i].clone(),
                        value: value[i].clone(),
                        note: note[i].clone(),
                        cites: cites[i].as_deref().map(split_list).unwrap_or_default(),
                        episodes: split_list(&episodes[i]),
                        audience,
                        as_of: as_of[i].unwrap_or_default(),
                        aggregator: aggregator[i].clone(),
                        tokens: None,
                    });
            }
        }
        Ok(runs
            .into_iter()
            .filter_map(|(k, by)| {
                by.into_iter()
                    .next_back()
                    .map(|((at, _), rows)| (k, (at, rows)))
            })
            .collect())
    }

    async fn write_aggregates(&self, scope: &Scope, actor: &str, rows: &[Row]) -> Result<String> {
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, AGGREGATES).await?;
        let col = |f: &dyn Fn(&Row) -> String| -> ArrayRef {
            Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let opt = |f: &dyn Fn(&Row) -> Option<String>| -> ArrayRef {
            Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let now = now_micros();
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", col(&|_| id.clone())),
                ("goal", col(&|r| r.goal.clone())),
                ("subject", col(&|r| r.subject.clone())),
                ("kind", col(&|r| r.kind.clone())),
                ("key", col(&|r| r.key.clone())),
                ("value", col(&|r| r.value.clone())),
                ("note", opt(&|r| r.note.clone())),
                (
                    "cites",
                    opt(&|r| (!r.cites.is_empty()).then(|| r.cites.join(","))),
                ),
                ("episodes", col(&|r| r.episodes.join(","))),
                ("audience", opt(&|r| Some(r.audience.clone()))),
                ("as_of", ts(rows.iter().map(|r| Some(r.as_of)))),
                ("aggregator", col(&|r| r.aggregator.clone())),
                (
                    "input_tokens",
                    Arc::new(Int64Array::from_iter(
                        rows.iter().map(|r| r.tokens.map(|t| t.0)),
                    )) as ArrayRef,
                ),
                (
                    "output_tokens",
                    Arc::new(Int64Array::from_iter(
                        rows.iter().map(|r| r.tokens.map(|t| t.1)),
                    )) as ArrayRef,
                ),
                (
                    "recorded_at",
                    ts(std::iter::repeat_n(Some(now), rows.len())),
                ),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, AGGREGATES);
        self.append_ledger(&id, scope, MAIN, actor, &|l: &Ledger| Ok(l.heads(MAIN)))
            .await?;
        Ok(id)
    }

    /// Ask for a summary of `subject` for `audience` (once: an audience asked for already is left as is).
    async fn request_aggregate(
        &self,
        scope: &Scope,
        goal: &str,
        subject: &str,
        audience: &str,
    ) -> Result<bool> {
        let asked = self.aggregate_requests(scope).await?;
        if asked
            .get(&(goal.to_string(), subject.to_string()))
            .is_some_and(|a| a.contains(audience))
        {
            return Ok(false);
        }
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, AGG_REQUESTS).await?;
        let one = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", one(&id)),
                ("goal", one(goal)),
                ("subject", one(subject)),
                ("audience", one(audience)),
                ("recorded_at", ts([Some(now_micros())].into_iter())),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, AGG_REQUESTS);
        self.append_ledger(&id, scope, MAIN, "brief", &|l: &Ledger| Ok(l.heads(MAIN)))
            .await?;
        Ok(true)
    }

    /// The audiences asked for, per (goal, subject), on main.
    async fn aggregate_requests(
        &self,
        scope: &Scope,
    ) -> Result<HashMap<(String, String), BTreeSet<String>>> {
        let ns = scope.namespace();
        let (vis, rows) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.side_rows(&ns, AGG_REQUESTS)
        )?;
        let mut out: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
        for b in rows.iter() {
            let (commit, goal, subject, audience) = (
                strings(b, "commit_id"),
                strings(b, "goal"),
                strings(b, "subject"),
                strings(b, "audience"),
            );
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i]) {
                    out.entry((goal[i].clone(), subject[i].clone()))
                        .or_default()
                        .insert(audience[i].clone());
                }
            }
        }
        Ok(out)
    }
}

/// What `who` may read of `subject`'s current aggregates as `session` sees them (its branch, or the snapshot it is
/// pinned to): each goal's newest run there, the rows whose every episode they may read.
pub async fn current(
    session: &Session,
    store: &Store,
    who: &Principal,
    subject: &str,
) -> Result<Vec<Row>> {
    Ok(current_covering(session, store, who, subject).await?.0)
}

/// How much of what a reader may read a goal's summary for them covers: its episodes, of those about the subject
/// (as of its last summary over all of them) the reader may read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Coverage {
    pub goal: String,
    pub covers: usize,
    pub of: usize,
}

/// [`current`], and per goal how much of what `who` may read the summary they get covers.
pub async fn current_covering(
    session: &Session,
    store: &Store,
    who: &Principal,
    subject: &str,
) -> Result<(Vec<Row>, Vec<Coverage>)> {
    let vis = session.vis();
    let runs = store.aggregate_runs(session.scope(), vis).await?;
    // Per goal, the widest run the reader may read whole: the one over the most episodes, all of them theirs;
    // and how many of the subject's episodes (the everyone run's) they may read.
    let mut best: BTreeMap<String, (usize, i64, Vec<Row>)> = BTreeMap::new();
    let mut readable: BTreeMap<String, usize> = BTreeMap::new();
    for ((goal, s, audience), (at, rows)) in runs {
        let Some(first) = rows.first() else { continue };
        if s != subject {
            continue;
        }
        if audience == EVERYONE {
            let n = first
                .episodes
                .iter()
                .filter(|e| vis.can_see_episode(e, who))
                .count();
            // Only where the reader may read something: else the count would say a summary exists.
            if n > 0 {
                readable.insert(goal.clone(), n);
            }
        }
        if !first.episodes.iter().all(|e| vis.can_see_episode(e, who)) {
            continue;
        }
        let n = first.episodes.len();
        if best.get(&goal).is_none_or(|(m, t, _)| (n, at) > (*m, *t)) {
            best.insert(goal, (n, at, rows));
        }
    }
    let mut coverage = vec![];
    for (goal, of) in readable {
        let covers = best.get(&goal).map_or(0, |(n, _, _)| *n);
        coverage.push(Coverage {
            goal,
            covers,
            of: of.max(covers),
        });
    }
    let mut out: Vec<Row> = best.into_values().flat_map(|(_, _, rows)| rows).collect();
    out.sort_by(|a, b| (&a.goal, &a.kind, &a.key).cmp(&(&b.goal, &b.kind, &b.key)));
    Ok((out, coverage))
}

/// A subject with a summary its reader may read, for a listing: when it was summarised, over how many episodes,
/// and how many methodology elements and items it scores.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Listed {
    pub subject: String,
    pub as_of: i64,
    pub episodes: usize,
    pub elements: usize,
    pub items: usize,
}

/// Every subject with a summary `who` may read as `session` sees it, newest summary first: per goal the run
/// [`current_covering`] would serve them (the widest one all of whose episodes they may read).
pub async fn readable(session: &Session, store: &Store, who: &Principal) -> Result<Vec<Listed>> {
    let vis = session.vis();
    let runs = store.aggregate_runs(session.scope(), vis).await?;
    let mut best: BTreeMap<(String, String), (usize, i64, Vec<Row>)> = BTreeMap::new();
    for ((goal, subject, _), (at, rows)) in runs {
        let Some(first) = rows.first() else { continue };
        if !first.episodes.iter().all(|e| vis.can_see_episode(e, who)) {
            continue;
        }
        let n = first.episodes.len();
        let key = (subject, goal);
        if best.get(&key).is_none_or(|(m, t, _)| (n, at) > (*m, *t)) {
            best.insert(key, (n, at, rows));
        }
    }
    let mut out: BTreeMap<String, Listed> = BTreeMap::new();
    for ((subject, _), (n, _, rows)) in best {
        let l = out.entry(subject.clone()).or_insert(Listed {
            subject,
            as_of: 0,
            episodes: 0,
            elements: 0,
            items: 0,
        });
        l.as_of = l.as_of.max(rows.iter().map(|r| r.as_of).max().unwrap_or(0));
        l.episodes = l.episodes.max(n);
        l.elements += rows.iter().filter(|r| r.kind == "element").count();
        l.items += rows.iter().filter(|r| r.kind == "item_status").count();
    }
    let mut out: Vec<Listed> = out.into_values().collect();
    out.sort_by(|a, b| {
        b.as_of
            .cmp(&a.as_of)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    Ok(out)
}

/// A reader's audience, as a request names it: `for:` and their ACL tokens but `*`, as a JSON array (an id may
/// hold any character, so no separator would do).
pub fn audience_of(who: &Principal) -> String {
    let tokens: Vec<&str> = who
        .tokens()
        .iter()
        .map(String::as_str)
        .filter(|t| *t != EVERYONE)
        .collect();
    format!("for:{}", serde_json::to_string(&tokens).unwrap_or_default())
}

pub use crate::offboard::audience_tokens;

/// Whether an episode with `acl` is one `audience` (a request's tokens) may read.
fn audience_reads(audience: &str, acl: &[String]) -> bool {
    let tokens: HashSet<String> = audience_tokens(audience).into_iter().collect();
    !tokens.is_empty()
        && acl
            .iter()
            .any(|a| a == EVERYONE || tokens.contains(&crate::auth::acl::norm_entry(a)))
}

/// Every current run's rows on main, each with when the newest episode about its subject happened (µs): what
/// `goal stats` measures a summary's lag by.
pub async fn all_current(store: &Store, scope: &Scope) -> Result<Vec<(Row, i64)>> {
    let vis = store.visibility(scope, MAIN).await?;
    let (runs, view) = tokio::try_join!(
        store.aggregate_runs(scope, &vis),
        store.aggregation_view(scope)
    )?;
    let mut newest: HashMap<&str, i64> = HashMap::new();
    for e in &view.episodes {
        for s in &e.about {
            let n = newest.entry(s.as_str()).or_default();
            *n = (*n).max(e.occurred_at);
        }
    }
    Ok(runs
        .into_values()
        .flat_map(|(_, rows)| rows)
        .map(|r| {
            let n = newest.get(r.subject.as_str()).copied().unwrap_or(r.as_of);
            (r, n)
        })
        .collect())
}

/// Who a subject's episodes are summarised for: everyone who may read all of them (`*`), and, for each ACL
/// entry that may read only some, those it may read; the same set of episodes once, under the first entry
/// that names it. At most [`MAX_AUDIENCES`], widest first. A reader sees the widest run they may read whole,
/// so someone on one call of a deal still gets that call's summary, and never one that draws on the other.
fn audiences<'a>(eps: &[&'a Ep]) -> Vec<(String, Vec<&'a Ep>)> {
    let mut out: Vec<(String, Vec<&'a Ep>)> = vec![(EVERYONE.into(), eps.to_vec())];
    let mut seen: HashSet<Vec<&str>> = HashSet::from([eps.iter().map(|e| e.id.as_str()).collect()]);
    // Entries as readers are matched against them (an email's case doesn't make another reader).
    let norm = |e: &Ep| -> Vec<String> {
        e.acl
            .iter()
            .map(|a| crate::auth::acl::norm_entry(a))
            .collect()
    };
    let acls: HashMap<&str, Vec<String>> = eps.iter().map(|e| (e.id.as_str(), norm(e))).collect();
    let entries: BTreeSet<&str> = acls
        .values()
        .flatten()
        .map(String::as_str)
        .filter(|a| *a != EVERYONE)
        .collect();
    let mut partial: Vec<(String, Vec<&'a Ep>)> = vec![];
    for entry in entries {
        let some: Vec<&'a Ep> = eps
            .iter()
            .copied()
            .filter(|e| {
                acls[e.id.as_str()]
                    .iter()
                    .any(|a| a == entry || a == EVERYONE)
            })
            .collect();
        if some.is_empty() || !seen.insert(some.iter().map(|e| e.id.as_str()).collect()) {
            continue;
        }
        partial.push((entry.to_string(), some));
    }
    partial.sort_by_key(|(_, some)| std::cmp::Reverse(some.len()));
    if partial.len() >= MAX_AUDIENCES {
        let left: Vec<&str> = partial[MAX_AUDIENCES - 1..]
            .iter()
            .map(|(a, _)| a.as_str())
            .collect();
        eprintln!(
            "aggregate: {} narrower audiences get no summary of their own (at most {MAX_AUDIENCES} runs): {}",
            left.len(),
            left.join(", ")
        );
    }
    out.extend(partial.into_iter().take(MAX_AUDIENCES - 1));
    out
}

/// A predicate's cardinality: from the scope's schema, else from any episode type's catalog; many if unknown.
fn card_of(
    schema: Option<&crate::query::schema::Schema>,
    catalogs: &[crate::query::schema::Schema],
    f: &Fact,
) -> Card {
    let ty = f.entity.split_once(':').map_or("", |(t, _)| t);
    schema
        .and_then(|s| s.predicate(ty, &f.predicate))
        .or_else(|| catalogs.iter().find_map(|c| c.predicate(ty, &f.predicate)))
        .map_or(Card::Many, |p| p.card)
}

fn day(t: i64) -> String {
    chrono::DateTime::from_timestamp_micros(t)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Code's part: superseded, stale and passed facts, as flags keyed by fact id.
fn flags(
    vis: &crate::Vis,
    a: &Aggregates,
    facts: &[Fact],
    schema: Option<&crate::query::schema::Schema>,
    catalogs: &[crate::query::schema::Schema],
    now: i64,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut newest: HashMap<(String, String), &Fact> = HashMap::new();
    for f in facts {
        if card_of(schema, catalogs, f) == Card::One {
            let k = (f.entity.clone(), f.predicate.clone());
            if newest
                .get(&k)
                .is_none_or(|g| vis.rank(g, i64::MAX) < vis.rank(f, i64::MAX))
            {
                newest.insert(k, f);
            }
        }
    }
    let day_us = 86_400_000_000i64;
    for f in facts {
        let k = (f.entity.clone(), f.predicate.clone());
        if newest.get(&k).is_some_and(|g| g.fact_id != f.fact_id) {
            out.insert(f.fact_id.clone(), "superseded".into());
        } else if f.valid_to.is_some_and(|t| t <= now) {
            out.insert(f.fact_id.clone(), "ended".into());
        } else if a.plans.contains(&f.predicate) && f.value_ts.is_some_and(|t| t < now) {
            out.insert(f.fact_id.clone(), format!("date passed ({})", f.value));
        } else if let Some(days) = a.freshness_days.get(&f.predicate) {
            let age = (now - f.valid_from) / day_us;
            if age > *days as i64 {
                out.insert(f.fact_id.clone(), format!("stale: {age} days old"));
            }
        }
    }
    out
}

fn fact_line(f: &Fact, eps: &HashMap<&str, &Ep>, flag: Option<&String>) -> String {
    let when = f
        .source_episodes
        .first()
        .and_then(|e| eps.get(e.as_str()))
        .map(|e| day(e.occurred_at))
        .unwrap_or_default();
    let at = f
        .cite
        .as_deref()
        .and_then(|c| serde_json::from_str::<crate::episode_types::Cite>(c).ok())
        .map(|c| c.describe())
        .unwrap_or_default();
    let v = match &f.unit {
        Some(u) => format!("{} {u}", f.value),
        None => f.value.clone(),
    };
    let mut s = format!(
        "{} [{when} {at}] {} {} = {v}",
        f.fact_id, f.entity, f.predicate
    );
    if let Some(fl) = flag {
        s.push_str(&format!(" ({fl})"));
    }
    if let Some(q) = &f.quote {
        s.push_str(&format!(
            " — \"{}\"",
            q.chars().take(200).collect::<String>()
        ));
    }
    s
}

/// One subject's batch, summarised: code's flags and the model's statuses and scores.
#[allow(clippy::too_many_arguments)]
async fn summarise(
    ex: &Extractor,
    g: &Contract,
    a: &Aggregates,
    subject: &str,
    audience: &str,
    eps: &[&Ep],
    facts: &[Fact],
    flagged: &BTreeMap<String, String>,
    now: i64,
) -> Result<Vec<Row>> {
    // Named by the version of the schema whose `extract` section it summarises.
    let aggregator = format!(
        "aggregate:{}@{PROMPT_VERSION}/schema@v{}",
        ex.model(),
        g.version
    );
    let episodes: Vec<String> = eps.iter().map(|e| e.id.clone()).collect();
    let row =
        |kind: &str, key: String, value: String, note: Option<String>, cites: Vec<String>| Row {
            goal: SCHEMA.to_string(),
            subject: subject.to_string(),
            kind: kind.into(),
            key,
            value,
            note,
            cites,
            episodes: episodes.clone(),
            audience: audience.to_string(),
            as_of: now,
            aggregator: aggregator.clone(),
            tokens: None,
        };
    let mut rows: Vec<Row> = flagged
        .iter()
        .map(|(fid, why)| row("flag", fid.clone(), why.clone(), None, vec![fid.clone()]))
        .collect();
    let by_id: HashMap<&str, &Ep> = eps.iter().map(|e| (e.id.as_str(), *e)).collect();
    let mut ordered: Vec<&Fact> = facts.iter().collect();
    ordered.sort_by_key(|f| {
        (
            f.source_episodes
                .first()
                .and_then(|e| by_id.get(e.as_str()))
                .map(|e| e.occurred_at),
            f.fact_id.clone(),
        )
    });
    let lines: Vec<String> = ordered
        .iter()
        .map(|f| fact_line(f, &by_id, flagged.get(&f.fact_id)))
        .collect();
    let items_of: BTreeSet<String> = facts
        .iter()
        .filter(|f| {
            f.entity
                .split_once(':')
                .is_some_and(|(t, _)| a.items.contains_key(t))
        })
        .map(|f| f.entity.clone())
        .collect();
    let scale = a.scale();
    let prompt = format!(
        "You derive the state of {subject} from facts extracted from many episodes (calls, threads, emails) about \
         it. Goal: {statement} Today is {today}.\n\
         Facts (id [date, where] subject predicate = value (flag) — quote), oldest first:\n{facts}\n\n\
         A. Items. Group the items that are the same thing across episodes (one restated or continued later). Give \
         each group a status as of today from its list, and the fact ids that show it. With no evidence it changed, \
         an item keeps the first status in its list. Lists: {items}.\n\
         B. Elements. Score each element on the scale {scale}, in plain words why, and the fact ids it rests on. \
         Elements and the predicates that feed them: {elements}.\n\
         Rules: {rules}\n\
         Cite only fact ids from the list. Reply with JSON only: {{\"items\": [{{\"items\": [\"<item id>\"], \
         \"status\": \"...\", \"note\": \"...\", \"cites\": []}}], \"elements\": [{{\"element\": \"...\", \"status\": \
         \"...\", \"note\": \"...\", \"cites\": []}}]}}",
        statement = g.statement,
        today = day(now),
        facts = lines.join("\n"),
        items = serde_json::to_string(&a.items)?,
        scale = scale.join(" | "),
        elements = serde_json::to_string(&a.elements)?,
        rules = if a.rules.is_empty() {
            "none".to_string()
        } else {
            a.rules.join(" ")
        },
    );
    // What this run costs, counted apart from any other running beside it.
    let (reply, (tin, tout)) = crate::extract::CALL_TOKENS
        .scope(std::cell::Cell::new((0, 0)), async {
            // A reply with no JSON fails the run, which asks again: the cache keeps only one that parses.
            let r = ex
                .complete_if(&prompt, MAX_REPLY_TOKENS, |r| {
                    matches!((r.find('{'), r.rfind('}')), (Some(a), Some(b))
                        if a <= b && serde_json::from_str::<Value>(&r[a..=b]).is_ok())
                })
                .await;
            (r, crate::extract::CALL_TOKENS.with(|t| t.get()))
        })
        .await;
    let reply = reply?;
    let (Some(s), Some(e)) = (reply.find('{'), reply.rfind('}')) else {
        anyhow::bail!("the summariser's reply has no JSON");
    };
    let v: Value =
        serde_json::from_str(&reply[s..=e]).context("the summariser's reply doesn't parse")?;
    let known: HashSet<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
    let cites = |x: &Value| -> Vec<String> {
        x["cites"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str())
            .filter(|c| known.contains(c))
            .map(String::from)
            .collect()
    };
    let text = |x: &Value, k: &str| {
        x[k].as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mut seen_items = HashSet::new();
    for it in v["items"].as_array().into_iter().flatten() {
        let ids: Vec<String> = it["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_str())
            .filter(|i| items_of.contains(*i) && seen_items.insert(i.to_string()))
            .map(String::from)
            .collect();
        let Some(ty) = ids.first().and_then(|i| i.split_once(':')).map(|(t, _)| t) else {
            continue;
        };
        let status = text(it, "status").unwrap_or_default();
        let allowed = &a.items[ty];
        let status = if allowed.contains(&status) {
            status
        } else {
            allowed[0].clone()
        };
        rows.push(row(
            "item_status",
            ids.join(","),
            status,
            text(it, "note"),
            cites(it),
        ));
    }
    // Items the model left out keep their first status.
    for i in &items_of {
        if !seen_items.contains(i) {
            let ty = i.split_once(':').map_or("", |(t, _)| t);
            rows.push(row(
                "item_status",
                i.clone(),
                a.items[ty][0].clone(),
                Some("no evidence it changed".into()),
                vec![],
            ));
        }
    }
    for el in v["elements"].as_array().into_iter().flatten() {
        let Some(name) = text(el, "element").filter(|n| a.elements.contains_key(n)) else {
            continue;
        };
        let status = text(el, "status")
            .filter(|s| scale.contains(s))
            .unwrap_or_else(|| scale[0].clone());
        rows.push(row("element", name, status, text(el, "note"), cites(el)));
    }
    for name in a.elements.keys() {
        if !rows.iter().any(|r| r.kind == "element" && &r.key == name) {
            rows.push(row(
                "element",
                name.clone(),
                scale[0].clone(),
                Some("no facts yet".into()),
                vec![],
            ));
        }
    }
    for r in &mut rows {
        r.tokens = Some((tin as i64, tout as i64));
    }
    Ok(rows)
}

/// One summarising run per scope at a time, in this process: a refresh and the hourly pass reading the same
/// subjects at once would each pay for the same summary. The pass waits its turn; a refresh that finds one
/// running says so rather than waiting for a whole pass ([`running`]).
fn scope_lock(scope: &Scope) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<HashMap<Scope, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap()
        .entry(scope.clone())
        .or_default()
        .clone()
}

/// When each (scope, subject, audience) was last sent to the model, in this process, whatever came of it: a
/// summary that failed or came back empty writes no run, and must hold off a refresh as one that was written does.
type Attempts = std::sync::Mutex<HashMap<(Scope, String, String), i64>>;
fn attempts() -> &'static Attempts {
    static AT: std::sync::LazyLock<Attempts> = std::sync::LazyLock::new(Default::default);
    &AT
}
fn tried(scope: &Scope, subject: &str, audience: &str) -> Option<i64> {
    let key = (scope.clone(), subject.to_string(), audience.to_string());
    attempts().lock().unwrap().get(&key).copied()
}
fn attempted(scope: &Scope, subject: &str, audience: &str, at: i64) {
    let key = (scope.clone(), subject.to_string(), audience.to_string());
    attempts().lock().unwrap().insert(key, at);
}

/// [`run`], unless one is running for `scope` now: then none. With `reader`, only the audiences whose every
/// episode the reader may read: a reader's refresh spends nothing, and tells nothing, about the others.
pub async fn run_unless_running(
    store: &Store,
    scope: &Scope,
    ex: &Extractor,
    when: When,
    only: Option<&str>,
    reader: Option<&Principal>,
) -> Option<Result<Run>> {
    let lock = scope_lock(scope);
    let _held = lock.try_lock().ok()?;
    Some(summarise_scope(store, scope, ex, when, only, reader, None).await)
}

/// Summarise the subjects of every active goal with `aggregates` that `when` picks (or only `only`). Only goals
/// that extract (`extract: true`): one that doesn't has no facts to summarise. With `report`, commit nothing and
/// describe what would be written.
pub async fn run(
    store: &Store,
    scope: &Scope,
    ex: &Extractor,
    when: When,
    only: Option<&str>,
    report: Option<&mut Vec<Value>>,
) -> Result<Run> {
    let lock = scope_lock(scope);
    let _held = lock.lock().await;
    summarise_scope(store, scope, ex, when, only, None, report).await
}

async fn summarise_scope(
    store: &Store,
    scope: &Scope,
    ex: &Extractor,
    when: When,
    only: Option<&str>,
    reader: Option<&Principal>,
    mut report: Option<&mut Vec<Value>>,
) -> Result<Run> {
    let goals: Vec<Contract> = crate::extract::goals::contract(store, scope)
        .await?
        .into_iter()
        .filter(|g| g.aggregates.is_some())
        .collect();
    let mut done = Run::default();
    if goals.is_empty() {
        return Ok(done);
    }
    // Runs summarised and not yet written: see [`RUNS_PER_COMMIT`].
    let mut pending: Vec<Vec<Row>> = vec![];
    let view = store.aggregation_view(scope).await?;
    let requests = store.aggregate_requests(scope).await?;
    let vis = store.visibility(scope, MAIN).await?;
    let runs = store.aggregate_runs(scope, &vis).await?;
    let schema = store.schema(scope, MAIN).await?;
    let catalogs: Vec<_> = crate::episode_types::all()
        .iter()
        .filter_map(|t| crate::episode_types::catalog(*t).ok())
        .collect();
    let now = now_micros();
    let fact_ids: HashSet<&str> = view.facts.iter().map(|f| f.fact_id.as_str()).collect();
    for g in &goals {
        let a = g.aggregates.as_ref().expect("filtered");
        // The episodes it reads, by the subjects they are about.
        let mut about: BTreeMap<String, Vec<&Ep>> = BTreeMap::new();
        for e in &view.episodes {
            let as_episode = crate::Episode {
                source: e.source.clone(),
                occurred_at: e.occurred_at,
                about: e.about.clone(),
                ..crate::Episode::new(&e.source, b"", &[])
            };
            if !g.sources.matches(&as_episode) {
                continue;
            }
            for s in &e.about {
                if s.split_once(':').is_some_and(|(t, _)| t == a.subject)
                    && only.is_none_or(|o| o == s)
                {
                    about.entry(s.clone()).or_default().push(e);
                }
            }
        }
        for (subject, eps) in about {
            // Each ACL entry's audience, and each audience a reader asked for (over all it may read).
            let mut auds = audiences(&eps);
            if let Some(asked) = requests.get(&(SCHEMA.to_string(), subject.clone())) {
                let mut seen: HashSet<Vec<String>> = auds
                    .iter()
                    .map(|(_, e)| e.iter().map(|x| x.id.clone()).collect())
                    .collect();
                for audience in asked {
                    let some: Vec<&Ep> = eps
                        .iter()
                        .copied()
                        .filter(|e| audience_reads(audience, &e.acl))
                        .collect();
                    if !some.is_empty() && seen.insert(some.iter().map(|x| x.id.clone()).collect())
                    {
                        auds.push((audience.clone(), some));
                    }
                }
            }
            for (audience, eps) in auds {
                if reader.is_some_and(|r| !eps.iter().all(|e| vis.can_see_episode(&e.id, r))) {
                    continue;
                }
                let prev = runs.get(&(SCHEMA.to_string(), subject.clone(), audience.clone()));
                let had: HashSet<&str> = prev
                    .map(|(_, r)| {
                        r.first()
                            .map(|x| x.episodes.iter().map(String::as_str).collect())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let new: Vec<&&Ep> = eps
                    .iter()
                    .filter(|e| !had.contains(e.id.as_str()))
                    .collect();
                // An episode gone from the batch (retracted, or its ACL narrowed) also calls for a new run:
                // the old one names it, so the readers it was for can no longer read it whole.
                let ids_now: HashSet<&str> = eps.iter().map(|e| e.id.as_str()).collect();
                // So does a fact it cited that is gone (retracted, or its extractor withdrawn).
                let lost = had.iter().any(|h| !ids_now.contains(h))
                    || prev.is_some_and(|(_, r)| {
                        r.iter()
                            .flat_map(|x| &x.cites)
                            .any(|c| !fact_ids.contains(c.as_str()))
                    });
                if new.is_empty() && !lost && when != When::All {
                    continue;
                }
                let every = a.every.unwrap_or(10) as usize;
                let wait_us = a.max_wait_minutes.unwrap_or(60) as i64 * 60_000_000;
                // Waited long enough: since the last run, or (never run) since the newest episode happened.
                let since = prev
                    .map(|(at, _)| *at)
                    .unwrap_or_else(|| new.iter().map(|e| e.occurred_at).max().unwrap_or(now));
                let due = match when {
                    When::All => true,
                    When::Changed => {
                        let last = prev
                            .map(|(at, _)| *at)
                            .into_iter()
                            .chain(tried(scope, &subject, &audience));
                        if last.max().is_some_and(|at| now - at < REFRESH_GAP) {
                            done.recent += 1;
                            continue;
                        }
                        true
                    }
                    When::Due => lost || new.len() >= every || now - since >= wait_us,
                };
                if !due {
                    done.waiting += 1;
                    continue;
                }
                let ids: HashSet<&str> = eps.iter().map(|e| e.id.as_str()).collect();
                // Only facts all of whose episodes are in the batch: a row names the batch's episodes as what
                // a reader must see, so nothing may come from an episode outside it.
                let facts: Vec<Fact> = view
                    .facts
                    .iter()
                    .filter(|f| f.source_episodes.iter().all(|e| ids.contains(e.as_str())))
                    .cloned()
                    .collect();
                let flagged = flags(&vis, a, &facts, schema.as_deref(), &catalogs, now);
                attempted(scope, &subject, &audience, now);
                match summarise(ex, g, a, &subject, &audience, &eps, &facts, &flagged, now).await {
                    Ok(rows) => {
                        if let Some(rep) = report.as_deref_mut() {
                            rep.push(json!({"schema": g.version, "subject": subject,
                                            "audience": audience, "episodes": eps.len(),
                                            "facts": facts.len(), "rows": rows}));
                        } else if !rows.is_empty() {
                            pending.push(rows.clone());
                            if pending.len() >= RUNS_PER_COMMIT {
                                write_runs(store, scope, &mut pending).await?;
                            }
                        }
                        done.subjects += 1;
                        done.rows += rows.len();
                    }
                    Err(e) => {
                        eprintln!(
                            "aggregate {subject} for {audience}, schema v{}: {e:#}",
                            g.version
                        );
                        done.failed += 1;
                    }
                }
            }
        }
    }
    write_runs(store, scope, &mut pending).await?;
    Ok(done)
}

/// Runs a pass writes in one commit, at most: each commit is a data file and a manifest of the aggregates
/// table, which every read of it plans and loads (a commit per run had grown one org's to 4,800 files, and a cold
/// `list_briefs` to 90 s). A pass that fails partway loses its unwritten runs, which the next pass does again.
const RUNS_PER_COMMIT: usize = 50;

/// Writes the runs `pending` holds, a commit per aggregator (each commit is signed by one), and empties it. A
/// run's rows keep their subject and audience, so the newest run of each is found per (goal, subject, audience)
/// as before, whatever else its commit holds.
async fn write_runs(store: &Store, scope: &Scope, pending: &mut Vec<Vec<Row>>) -> Result<()> {
    let mut by: std::collections::BTreeMap<String, Vec<Row>> = Default::default();
    for rows in pending.drain(..) {
        if let Some(r) = rows.first() {
            by.entry(r.aggregator.clone()).or_default().extend(rows);
        }
    }
    for (actor, rows) in by {
        store.write_aggregates(scope, &actor, &rows).await?;
    }
    Ok(())
}
