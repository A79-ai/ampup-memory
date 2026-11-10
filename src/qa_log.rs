//! The answer log (GX4): what agents asked a memory and which facts their answers rested on, so a goal can be
//! tuned on real use (which questions it serves, which facts get cited, which answers were judged unhelpful).
//!
//! `log_answer` (MCP, `POST /log_answer`, the same contract as the engineering brain's) records a question,
//! the fact ids the final answer cited, and optionally the goal it was for, the answer's text and the asker's
//! rating (`helpful` or `not_helpful`). A caller may cite only facts they can read, so the log never names a
//! fact its writer couldn't see. Rating an answer later is another `log_answer` naming its `answer_id`; only
//! the person who logged it may, and the newest row per answer is what the log shows.
//!
//! Rows go to the scope's `qa_log` table through the ledger. Each names the episodes its cited facts come
//! from, so erasing an episode erases the rows that cite it (and an answer logged while it was being erased is
//! never shown, and is swept by the scope's next erasure), and forgetting a person erases what they asked.
//! The log is read by whoever owns the catalog (`memstore qa-log`): questions are the askers' own words.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use arrow_array::{ArrayRef, Float64Array, StringArray};
use serde::Serialize;
use serde_json::Value;

use crate::{
    Ledger, MAIN, OnConflict, Principal, QA_LOG, Scope, Store, append, now_micros, optional_f64,
    optional_strings, split_list, strings, timestamps, ts,
};

/// Longest question kept, in characters.
pub(crate) const MAX_QUESTION: usize = 4_000;

/// `text` as an entry's question: whole when it fits [`MAX_QUESTION`], else its first characters and an ellipsis.
pub(crate) fn as_question(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_QUESTION {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_QUESTION - 1).collect();
    cut.push('…');
    cut
}
/// Longest answer kept, in characters.
const MAX_ANSWER: usize = 20_000;
/// Most fact ids one answer may cite.
const MAX_CITED: usize = 100;
/// Most entries (answers, runs, ratings and rewards) one person may log in a scope in a day:
/// `MEMSTORE_LOG_PER_DAY`, default 1000.
fn max_per_day() -> usize {
    std::env::var("MEMSTORE_LOG_PER_DAY")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1_000)
}

/// A caller's mistake, said back to them as is.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// One logged answer, as the log shows it (its newest row).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Logged {
    pub answer_id: String,
    pub asker: String,
    pub goal: Option<String>,
    pub question: String,
    pub answer: Option<String>,
    pub rating: Option<String>,
    pub cited: Vec<String>,
    /// The episodes the cited facts come from.
    pub episodes: Vec<String>,
    /// When it was first logged (µs).
    pub asked_at: i64,
    /// `run` for a run's entry (`record_run`); none for an answer.
    pub kind: Option<String>,
    /// A run's reward, -1 to 1, once its outcome is known (`reward_run`), and why.
    pub reward: Option<f64>,
    pub reward_reason: Option<String>,
    /// A session's (`kind` `session`, [`crate::sessions`]): what it ran in, its arm, the advice served and
    /// withheld, the conversation so far, when it ended, the judge's verdicts and its transcript's ACL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionPart>,
    /// When its newest row was written (µs): a session's last activity.
    #[serde(skip)]
    pub updated_at: i64,
}

/// A session's own part of its entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SessionPart {
    pub harness: Option<String>,
    /// `advice`, or `holdout` (advice routed but withheld, for comparison).
    pub arm: String,
    pub served: Vec<String>,
    pub withheld: Vec<String>,
    /// The episode holding the conversation so far, while the session is open.
    #[serde(skip)]
    pub conversation: Option<String>,
    pub ended_at: Option<i64>,
    /// The judge's verdicts as JSON, once judged.
    pub verdicts: Option<String>,
    #[serde(skip)]
    pub acl: Vec<String>,
    /// `agent` (end_session) or `idle` (ended by the server).
    pub ended_by: Option<String>,
    /// The advice ops fired mid-run (`crate::sessions::fire`), each `<kind>:<tool>:<situation>:<op>`.
    #[serde(default)]
    pub fires: Vec<String>,
    /// The branch its writes land on, where its org gives each session one (`session_branches`): made on its
    /// first write, merged into main once it ends ([`crate::sessions::branch_of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The version of each note it was served: note key → the note document's episode id when served
    /// ([`crate::sessions::judge`] reads that one, not the note as it is now).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub served_versions: BTreeMap<String, String>,
    /// What each advice lookup in it came to, its newest [`crate::advice_lookups::MAX`]: served, or why not
    /// (`crate::advice_lookups`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lookups: Vec<crate::advice_lookups::Lookup>,
    /// The pull requests it opened or pushed to, each with what later came of it (`crate::session_prs`): an outcome
    /// advice learns from before any model's reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<crate::session_prs::Pr>,
}

/// Records an answer, or (with `answer_id`) rates one `who` logged before. Returns its id.
pub async fn log(store: &Store, scope: &Scope, who: &Principal, args: &Value) -> Result<String> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    let rating = match args["rating"].as_str() {
        None => None,
        Some(r @ ("helpful" | "not_helpful")) => Some(r.to_string()),
        Some(r) => {
            return Err(refuse(format!(
                "rating is helpful or not_helpful, not {r:?}"
            )));
        }
    };
    // A bound on what one person adds a day, ratings included: the log isn't capped like episodes and facts.
    let today = store
        .qa_rows_since(scope, who.user(), now_micros() - 86_400_000_000)
        .await?;
    if today >= max_per_day() {
        return Err(refuse(format!(
            "{} has logged {today} answers in {scope} today, the most a day",
            who.user()
        )));
    }
    if let Some(id) = args["answer_id"].as_str() {
        // A rating of an earlier answer: the asker's own, and nothing else about it changes.
        let Some(rating) = rating else {
            return Err(refuse("rating an answer needs rating".into()));
        };
        let Some(was) = list(store, scope, None)
            .await?
            .into_iter()
            .find(|l| l.answer_id == id && l.asker == who.user())
        else {
            return Err(refuse(format!("no answer {id} that you logged")));
        };
        store
            .write_qa(
                scope,
                who.user(),
                &Logged {
                    rating: Some(rating),
                    ..was
                },
            )
            .await?;
        return Ok(id.to_string());
    }
    let l = checked(store, scope, who, args, rating).await?;
    store.write_qa(scope, who.user(), &l).await?;
    Ok(l.answer_id)
}

/// An entry checked and ready to write: the question, the cited facts (only ones `who` may read), the goal.
async fn checked(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    args: &Value,
    rating: Option<String>,
) -> Result<Logged> {
    let mut l = checked_all(store, scope, who, &[args], None)
        .await?
        .remove(0);
    l.rating = rating;
    Ok(l)
}

/// An entry's own fields, checked without reading the store.
struct Fields<'a> {
    question: &'a str,
    answer: Option<&'a str>,
    cited: BTreeSet<String>,
    goal: Option<&'a str>,
}

fn fields(args: &Value) -> Result<Fields<'_>> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    let question = args["question"].as_str().unwrap_or_default().trim();
    if question.is_empty() || question.chars().count() > MAX_QUESTION {
        return Err(refuse(format!(
            "question is required, at most {MAX_QUESTION} characters"
        )));
    }
    let answer = args["answer"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    if answer.is_some_and(|a| a.chars().count() > MAX_ANSWER) {
        return Err(refuse(format!("answer is at most {MAX_ANSWER} characters")));
    }
    let cited: BTreeSet<String> = match &args["cited"] {
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().map(|s| s.trim().to_string()))
            .collect::<Option<_>>()
            .ok_or_else(|| refuse("cited is a list of fact ids".into()))?,
        Value::Null => BTreeSet::new(),
        _ => return Err(refuse("cited is a list of fact ids".into())),
    };
    if cited.len() > MAX_CITED {
        return Err(refuse(format!("at most {MAX_CITED} cited facts")));
    }
    // A `goal`, from before goals were retired, is accepted and no longer kept.
    let goal = None;
    Ok(Fields {
        question,
        answer,
        cited,
        goal,
    })
}

/// A refusal about the `i`th of a batch's entries, said of it as `<label>[i]: ...`; with no label (one entry, not
/// a batch), as is.
pub(crate) fn of_item(label: Option<&str>, i: usize, e: anyhow::Error) -> anyhow::Error {
    match (label, e.downcast_ref::<Refused>()) {
        (Some(l), Some(r)) => Refused(format!("{l}[{i}]: {}", r.0)).into(),
        _ => e,
    }
}

/// Entries checked as [`checked`] checks one, all before any is written: one read of the facts they cite, one of
/// each goal they name. A refusal names the entry it is about ([`of_item`]).
async fn checked_all(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    entries: &[&Value],
    label: Option<&str>,
) -> Result<Vec<Logged>> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    let fields: Vec<Fields> = entries
        .iter()
        .enumerate()
        .map(|(i, a)| fields(a).map_err(|e| of_item(label, i, e)))
        .collect::<Result<_>>()?;
    // Only facts the caller may read: the log never names one its writer couldn't see. Unknown and unreadable
    // ids are refused alike, so the answer says nothing about facts the caller can't see.
    let all: BTreeSet<String> = fields
        .iter()
        .flat_map(|f| f.cited.iter().cloned())
        .collect();
    let seen = if all.is_empty() {
        Default::default()
    } else {
        store.facts_seen(scope, who, &all).await?
    };
    for (i, f) in fields.iter().enumerate() {
        let not_yours: Vec<String> = f
            .cited
            .iter()
            .filter(|id| !seen.contains_key(*id))
            .cloned()
            .collect();
        if !not_yours.is_empty() {
            return Err(of_item(
                label,
                i,
                refuse(format!(
                    "not facts you can read in {scope}: {}",
                    not_yours.join(", ")
                )),
            ));
        }
    }
    Ok(fields
        .into_iter()
        .map(|f| {
            let episodes: BTreeSet<String> = f
                .cited
                .iter()
                .filter_map(|id| seen.get(id))
                .flat_map(|fact| fact.source_episodes.iter().cloned())
                .collect();
            Logged {
                answer_id: uuid::Uuid::new_v4().to_string(),
                asker: who.user().to_string(),
                goal: f.goal.map(String::from),
                question: f.question.to_string(),
                answer: f.answer.map(String::from),
                cited: f.cited.into_iter().collect(),
                episodes: episodes.into_iter().collect(),
                asked_at: now_micros(),
                ..Default::default()
            }
        })
        .collect())
}

/// Checks an entry as [`log`] would, writing nothing.
pub async fn check(store: &Store, scope: &Scope, who: &Principal, args: &Value) -> Result<()> {
    checked(store, scope, who, args, None).await.map(|_| ())
}

/// Checks a batch's entries as [`log`] would check each, writing nothing; a refusal names its entry
/// ([`of_item`]).
pub async fn check_all(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    entries: &[&Value],
    label: Option<&str>,
) -> Result<()> {
    checked_all(store, scope, who, entries, label)
        .await
        .map(|_| ())
}

/// Refused once `who` has added a day's most rows to the scope's log (answers, runs, sessions and their steps).
pub(crate) async fn daily_cap(store: &Store, scope: &Scope, who: &Principal) -> Result<()> {
    entries_cap(store, scope, who, 1).await
}

/// Refused when `adding` more rows would pass the day's most `who` may add to the scope's log.
pub(crate) async fn entries_cap(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    adding: usize,
) -> Result<()> {
    let today = store
        .qa_rows_since(scope, who.user(), now_micros() - 86_400_000_000)
        .await?;
    let max = max_per_day();
    if today + adding > max {
        let who = who.user();
        return Err(Refused(if adding == 1 {
            format!("{who} has logged {today} entries in {scope} today, the most a day")
        } else {
            format!(
                "{who} has logged {today} entries in {scope} today: {adding} more would pass the most a day, {max}"
            )
        })
        .into());
    }
    Ok(())
}

/// Refused when `adding` more runs would pass the [`max_runs_per_day`] `who` may record in the scope in a day.
/// Counted from every row ever written, forgotten and erased runs included: forgetting a run must not free its
/// place.
pub async fn runs_cap(store: &Store, scope: &Scope, who: &Principal, adding: usize) -> Result<()> {
    let since = now_micros() - 86_400_000_000;
    let rows = store.side_rows(&scope.namespace(), QA_LOG).await?;
    let mut ids = BTreeSet::new();
    for b in rows.iter() {
        let (id, asker, kind, asked) = (
            strings(b, "answer_id"),
            strings(b, "asker"),
            optional_strings(b, "kind"),
            timestamps(b, "asked_at"),
        );
        for i in 0..b.num_rows() {
            if asker[i] == who.user()
                && kind[i].as_deref() == Some("run")
                && asked[i].is_some_and(|t| t >= since)
            {
                ids.insert(id[i].clone());
            }
        }
    }
    let max = max_runs_per_day();
    if ids.len() + adding > max {
        let (who, had) = (who.user(), ids.len());
        return Err(Refused(if adding == 1 {
            format!("{who} has recorded {had} runs in {scope} today, the most a day")
        } else {
            format!(
                "{who} has recorded {had} runs in {scope} today: {adding} more would pass the most a day, {max}"
            )
        })
        .into());
    }
    Ok(())
}

/// Most runs one person may record in a scope in a day: `MEMSTORE_RUNS_PER_DAY`, default 100. An agent that
/// records every run it makes, for advice to learn from (`crate::advice`), needs more.
fn max_runs_per_day() -> usize {
    std::env::var("MEMSTORE_RUNS_PER_DAY")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(100)
}

/// A run to log ([`log_runs`]).
pub struct RunEntry<'a> {
    pub id: &'a str,
    /// Its entry, as [`log`] takes one: the task as the question, the facts it used as `cited`, its goal, its
    /// outcome as the answer.
    pub entry: &'a Value,
    pub trace_episode: &'a str,
    /// The reward (-1 to 1, checked by the caller) and its reason, when the run comes with its outcome known.
    pub reward: Option<(f64, Option<&'a str>)>,
}

/// Logs runs of `who` (see [`crate::runs`]) in one commit: each its task as the question, the facts it used
/// (checked like an answer's) and its trace episode last among its episodes, so forgetting the trace drops the
/// entry too; then, for each run given with a reward, the reward's row, as [`reward`] would add it later (the
/// newest row is the entry). Runs are asked a microsecond apart in their order, so the log, and advice reading
/// runs oldest first, keep it. A refusal names its run ([`of_item`]); nothing is written then.
pub async fn log_runs(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    runs: &[RunEntry<'_>],
    label: Option<&str>,
) -> Result<()> {
    runs_cap(store, scope, who, runs.len()).await?;
    let rewards = runs.iter().filter(|r| r.reward.is_some()).count();
    if rewards > 0 {
        entries_cap(store, scope, who, runs.len() + rewards).await?;
    }
    let entries: Vec<&Value> = runs.iter().map(|r| r.entry).collect();
    let checked = checked_all(store, scope, who, &entries, label).await?;
    let at = now_micros();
    let (mut rows, mut rewarded) = (Vec::with_capacity(runs.len() + rewards), vec![]);
    for (i, (r, mut l)) in runs.iter().zip(checked).enumerate() {
        l.answer_id = r.id.to_string();
        l.kind = Some("run".into());
        l.asked_at = at + i as i64;
        if !l.episodes.iter().any(|e| e == r.trace_episode) {
            l.episodes.push(r.trace_episode.to_string());
        }
        if let Some((reward, reason)) = r.reward {
            rewarded.push(Logged {
                reward: Some(reward),
                reward_reason: reason.map(String::from),
                ..l.clone()
            });
        }
        rows.push(l);
    }
    rows.extend(rewarded);
    store.write_qa_rows(scope, who.user(), &rows).await
}

/// Attaches a reward (-1 to 1) and its reason to an answer or run `who` logged; the newest row is the entry.
pub async fn reward(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    id: &str,
    reward: f64,
    reason: Option<&str>,
) -> Result<()> {
    reward_all(store, scope, who, &[(id, reward, reason)], None).await
}

/// Attaches rewards, as [`reward`] attaches one, in one commit: `(run or session id, reward, reason)` each, in
/// order (a run rewarded twice keeps the later). A refusal names its item ([`of_item`]); nothing is written then.
pub async fn reward_all(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    rewards: &[(&str, f64, Option<&str>)],
    label: Option<&str>,
) -> Result<()> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    for (i, (_, r, _)) in rewards.iter().enumerate() {
        if !(-1.0..=1.0).contains(r) {
            return Err(of_item(
                label,
                i,
                refuse(format!("reward is from -1 to 1, not {r}")),
            ));
        }
    }
    entries_cap(store, scope, who, rewards.len()).await?;
    let mine: BTreeMap<String, Logged> = list(store, scope, None)
        .await?
        .into_iter()
        .filter(|l| l.asker == who.user() && matches!(l.kind.as_deref(), Some("run" | "session")))
        .map(|l| (l.answer_id.clone(), l))
        .collect();
    let mut rows = Vec::with_capacity(rewards.len());
    for (i, (id, r, reason)) in rewards.iter().enumerate() {
        let Some(was) = mine.get(*id) else {
            return Err(of_item(
                label,
                i,
                refuse(format!("no run or session {id} that you recorded")),
            ));
        };
        rows.push(Logged {
            reward: Some(*r),
            reward_reason: reason.map(String::from),
            ..was.clone()
        });
    }
    store.write_qa_rows(scope, who.user(), &rows).await
}

/// How a reviewer's reward's reason starts ([`review`]): a person who read the trace graded it, neither its
/// recorder nor a model.
pub const REVIEWED: &str = "reviewed: ";
/// Longest reason a reviewer gives, in characters.
const REVIEW_REASON: usize = 500;

/// Whether `l`'s reward is a reviewer's ([`review`]).
pub fn reviewed(l: &Logged) -> bool {
    l.reward.is_some()
        && l.reward_reason
            .as_deref()
            .is_some_and(|r| r.starts_with(REVIEWED))
}

/// A reviewer's grade of a run, or of a session that has ended, whoever in `scope` recorded it (the org admins'
/// `/admin {op: "review_conversation"}`; the caller has checked who may): `reward` is 1 or -1, and a -1 comes with
/// what went wrong, which is what a note is learned from. It is a row of the entry, written by `actor`, its reason
/// starting [`REVIEWED`]. The newest review stands over every row of the entry that isn't one ([`list`]): a row
/// rewritten from a read taken before the review landed (a session's step, the judge's verdicts, a PR event)
/// never drops it, and a recorder's later `reward_run` doesn't replace it; a later review does. A session still
/// open is refused: it is graded once it has ended. Returns the entry as it now stands.
pub async fn review(
    store: &Store,
    scope: &Scope,
    actor: &str,
    id: &str,
    reward: f64,
    reason: Option<&str>,
) -> Result<Logged> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    if reward != 1.0 && reward != -1.0 {
        return Err(refuse(format!(
            "a review's reward is 1 or -1, not {reward}"
        )));
    }
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| r.chars().count() > REVIEW_REASON) {
        return Err(refuse(format!(
            "a review's reason is at most {REVIEW_REASON} characters"
        )));
    }
    let reason = match reason {
        Some(r) => r,
        None if reward < 0.0 => {
            return Err(refuse(
                "a -1 says what went wrong (reason): it is what advice learns from".into(),
            ));
        }
        None => "right, as a person who read the trace found it",
    };
    // Fresh, and with the session rows this server holds for its next flush over the log's: a session that has
    // ended here is reviewable before its end is written.
    store.invalidate(&scope.namespace(), QA_LOG);
    let Some(was) = crate::sessions::list(store, scope)
        .await?
        .into_iter()
        .find(|l| l.answer_id == id && matches!(l.kind.as_deref(), Some("run" | "session")))
    else {
        return Err(refuse(format!("no run or session {id:?} in {scope}")));
    };
    if was.session.as_ref().is_some_and(|s| s.ended_at.is_none()) {
        return Err(refuse(format!(
            "session {id} is still open: review it once it has ended"
        )));
    }
    let row = Logged {
        reward: Some(reward),
        reward_reason: Some(format!("{REVIEWED}{reason}")),
        ..was
    };
    store.write_qa(scope, actor, &row).await?;
    Ok(row)
}

/// What a judgement of a run says (`crate::advice_loop`): its reward, none when the judge couldn't tell, and
/// why, starting with [`JUDGED`].
pub struct Judgement {
    pub run: String,
    pub reward: Option<f64>,
    pub reason: String,
}

/// How a judged reward's reason starts, so the log shows it as a model's opinion, not the recorder's.
pub const JUDGED: &str = "judged: ";

/// Writes the engine's judgements of runs nobody rewarded, each as a row of the run's own recorder (its
/// asker), so it counts where the recorder's reward would. Unlike [`reward`], nothing checks that a caller is
/// the asker: there is no caller. Only the engine's own judge calls this, and only a run that still has no
/// reward, rating or judgement when its row is written (the log is read fresh here, just before) gets one, so
/// it never replaces what a recorder said. A reward the recorder gives later is a newer row, and the newest row
/// is the entry ([`list`]). The row counts toward the recorder's day of entries, once per run at most.
/// Returns the judgements written.
pub async fn judged(store: &Store, scope: &Scope, judgements: &[Judgement]) -> Result<usize> {
    store.invalidate(&scope.namespace(), QA_LOG);
    let open: BTreeMap<String, Logged> = list(store, scope, None)
        .await?
        .into_iter()
        .filter(|l| {
            l.kind.as_deref() == Some("run")
                && l.reward.is_none()
                && l.rating.is_none()
                && !l
                    .reward_reason
                    .as_deref()
                    .is_some_and(|r| r.starts_with(JUDGED))
        })
        .map(|l| (l.answer_id.clone(), l))
        .collect();
    let mut written = 0;
    for j in judgements {
        let Some(was) = open.get(&j.run) else {
            continue;
        };
        if !j.reason.starts_with(JUDGED) || j.reward.is_some_and(|r| !(-1.0..=1.0).contains(&r)) {
            continue;
        }
        store
            .write_qa(
                scope,
                "advice:judge",
                &Logged {
                    reward: j.reward,
                    reward_reason: Some(j.reason.clone()),
                    ..was.clone()
                },
            )
            .await?;
        written += 1;
    }
    Ok(written)
}

/// The log on main, newest answer first, each answer as its newest row; only `goal`'s with one.
pub async fn list(store: &Store, scope: &Scope, goal: Option<&str>) -> Result<Vec<Logged>> {
    let vis = store.visibility(scope, MAIN).await?;
    let rows = store.side_rows(&scope.namespace(), QA_LOG).await?;
    // Each answer's newest row, except that a session its agent ended is final: no row without that end (a
    // conversation kept, or the sweep's idle end, from a read taken before the end landed) supersedes it.
    let mut newest: BTreeMap<String, ((bool, i64), Logged)> = BTreeMap::new();
    // A session's PRs as every row of it holds them (`crate::session_prs::merge`): they only grow, and a row written
    // from a read taken before another pod's row landed would otherwise drop what that row added.
    let mut prs_of: BTreeMap<String, Vec<crate::session_prs::Pr>> = BTreeMap::new();
    // Each entry's newest review ([`review`]): (when, reward, reason). It stands over any row that isn't one, so a
    // row written from a read taken before the review landed doesn't undo it.
    let mut review_of: BTreeMap<String, (i64, f64, String)> = BTreeMap::new();
    for b in rows.iter() {
        let s = |c: &str| strings(b, c);
        let o = |c: &str| optional_strings(b, c);
        let (commit, id, asker, question, cited, episodes) = (
            s("commit_id"),
            s("answer_id"),
            s("asker"),
            s("question"),
            s("cited"),
            s("episodes"),
        );
        let (g, answer, rating) = (o("goal"), o("answer"), o("rating"));
        let (reward, reward_reason, kind) =
            (optional_f64(b, "reward"), o("reward_reason"), o("kind"));
        let (asked, at) = (timestamps(b, "asked_at"), timestamps(b, "recorded_at"));
        // A session's columns, absent from a table no engine with sessions has evolved yet.
        let n = b.num_rows();
        let has = |c: &str| b.schema().column_with_name(c).is_some();
        let oo = |c: &str| if has(c) { o(c) } else { vec![None; n] };
        let (harness, arm, served, withheld) =
            (oo("harness"), oo("arm"), oo("served"), oo("withheld"));
        let (conversation, verdicts, acl) = (oo("conversation"), oo("verdicts"), oo("acl"));
        let (ended_by, fires, branch) = (oo("ended_by"), oo("fires"), oo("branch"));
        let (served_versions, lookups, prs) = (oo("served_versions"), oo("lookups"), oo("prs"));
        let ended = if has("ended_at") {
            timestamps(b, "ended_at")
        } else {
            vec![None; n]
        };
        for i in 0..n {
            // An answer citing an episode retracted or erased since is gone with it, even one logged while
            // the erasure ran (its rows are swept by the next erasure in the scope), as is one citing a
            // retracted fact.
            if !vis.committed.contains(&commit[i])
                || split_list(&episodes[i]).iter().any(|e| vis.is_gone(e))
                || split_list(&cited[i])
                    .iter()
                    .any(|f| vis.retracted_facts.contains(f))
            {
                continue;
            }
            if let Some(p) = prs[i]
                .as_deref()
                .and_then(|v| serde_json::from_str::<Vec<crate::session_prs::Pr>>(v).ok())
                .filter(|p| !p.is_empty())
            {
                crate::session_prs::merge(prs_of.entry(id[i].clone()).or_default(), p);
            }
            let at = at[i].unwrap_or_default();
            if let (Some(r), Some(why)) = (reward[i], reward_reason[i].as_deref())
                && why.starts_with(REVIEWED)
                && review_of.get(&id[i]).is_none_or(|(t, _, _)| *t < at)
            {
                review_of.insert(id[i].clone(), (at, r, why.to_string()));
            }
            // A session's final end (an agent's, discarded, empty) outranks a later row that isn't one.
            let rank = (
                kind[i].as_deref() == Some("session")
                    && ended[i].is_some()
                    && matches!(
                        ended_by[i].as_deref(),
                        Some("agent" | "discarded" | "empty")
                    ),
                at,
            );
            if newest.get(&id[i]).is_some_and(|(r, _)| *r >= rank) {
                continue;
            }
            newest.insert(
                id[i].clone(),
                (
                    rank,
                    Logged {
                        answer_id: id[i].clone(),
                        asker: asker[i].clone(),
                        goal: g[i].clone(),
                        question: question[i].clone(),
                        answer: answer[i].clone(),
                        rating: rating[i].clone(),
                        cited: split_list(&cited[i]),
                        episodes: split_list(&episodes[i]),
                        asked_at: asked[i].unwrap_or_default(),
                        kind: kind[i].clone(),
                        reward: reward[i],
                        reward_reason: reward_reason[i].clone(),
                        updated_at: at,
                        session: (kind[i].as_deref() == Some("session")).then(|| SessionPart {
                            harness: harness[i].clone(),
                            arm: arm[i].clone().unwrap_or_else(|| "advice".into()),
                            served: served[i].as_deref().map(split_list).unwrap_or_default(),
                            withheld: withheld[i].as_deref().map(split_list).unwrap_or_default(),
                            conversation: conversation[i].clone(),
                            ended_at: ended[i],
                            verdicts: verdicts[i].clone(),
                            acl: acl[i].as_deref().map(split_list).unwrap_or_default(),
                            ended_by: ended_by[i].clone(),
                            fires: fires[i]
                                .as_deref()
                                .and_then(|f| serde_json::from_str(f).ok())
                                .unwrap_or_default(),
                            branch: branch[i].clone(),
                            served_versions: served_versions[i]
                                .as_deref()
                                .and_then(|v| serde_json::from_str(v).ok())
                                .unwrap_or_default(),
                            lookups: lookups[i]
                                .as_deref()
                                .and_then(|v| serde_json::from_str(v).ok())
                                .unwrap_or_default(),
                            prs: prs[i]
                                .as_deref()
                                .and_then(|v| serde_json::from_str(v).ok())
                                .unwrap_or_default(),
                        }),
                    },
                ),
            );
        }
    }
    let mut out: Vec<Logged> = newest
        .into_values()
        .map(|(_, mut l)| {
            if let (Some(s), Some(p)) = (l.session.as_mut(), prs_of.remove(&l.answer_id)) {
                crate::session_prs::merge(&mut s.prs, p);
            }
            if let Some((_, r, why)) = review_of.remove(&l.answer_id) {
                l.reward = Some(r);
                l.reward_reason = Some(why);
            }
            l
        })
        .filter(|l| goal.is_none_or(|g| l.goal.as_deref() == Some(g)))
        .collect();
    out.sort_by_key(|l| std::cmp::Reverse(l.asked_at));
    Ok(out)
}

/// The QA log's reads and writes on a [`Store`] (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreQaLog {
    async fn write_qa_rows(&self, scope: &Scope, actor: &str, rows: &[Logged]) -> Result<()>;
    async fn write_qa(&self, scope: &Scope, actor: &str, l: &Logged) -> Result<()>;
    async fn qa_rows_since(&self, scope: &Scope, asker: &str, since: i64) -> Result<usize>;
}

impl StoreQaLog for Store {
    /// Rows `asker` added to the log since `since` (µs), ratings included; not the one row ending a session
    /// `empty` (an admin's `clear_empty_sessions` writes one per session it clears, in the asker's name). Its
    /// start was a row of its own, which counts, as does any other row of it.
    async fn qa_rows_since(&self, scope: &Scope, asker: &str, since: i64) -> Result<usize> {
        let rows = self.side_rows(&scope.namespace(), QA_LOG).await?;
        let mut ended_empty = std::collections::HashSet::new();
        let mut n = 0;
        for b in rows.iter() {
            let ended_by = optional_strings(b, "ended_by");
            for (((a, t), e), id) in strings(b, "asker")
                .iter()
                .zip(timestamps(b, "recorded_at"))
                .zip(&ended_by)
                .zip(strings(b, "answer_id"))
            {
                if a.as_str() != asker || !t.is_some_and(|t| t >= since) {
                    continue;
                }
                if e.as_deref() == Some("empty") && ended_empty.insert(id) {
                    continue;
                }
                n += 1;
            }
        }
        Ok(n)
    }

    async fn write_qa(&self, scope: &Scope, actor: &str, l: &Logged) -> Result<()> {
        self.write_qa_rows(scope, actor, std::slice::from_ref(l))
            .await
    }

    /// Rows in one commit, recorded a microsecond apart in their order, so a later row of an entry (its reward)
    /// is the newer one.
    async fn write_qa_rows(&self, scope: &Scope, actor: &str, rows: &[Logged]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.ensure_scope(scope).await?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, QA_LOG).await?;
        let at = now_micros();
        let col = |f: &dyn Fn(&Logged) -> String| -> ArrayRef {
            Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let opt = |f: &dyn Fn(&Logged) -> Option<String>| -> ArrayRef {
            Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let sp =
            |l: &Logged, f: &dyn Fn(&SessionPart) -> Option<String>| l.session.as_ref().and_then(f);
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", col(&|_| id.clone())),
                ("answer_id", col(&|l| l.answer_id.clone())),
                ("asker", col(&|l| l.asker.clone())),
                ("goal", opt(&|l| l.goal.clone())),
                ("question", col(&|l| l.question.clone())),
                ("answer", opt(&|l| l.answer.clone())),
                ("rating", opt(&|l| l.rating.clone())),
                ("cited", col(&|l| l.cited.join(","))),
                ("episodes", col(&|l| l.episodes.join(","))),
                ("asked_at", ts(rows.iter().map(|l| Some(l.asked_at)))),
                (
                    "recorded_at",
                    ts((0..rows.len()).map(|i| Some(at + i as i64))),
                ),
                (
                    "reward",
                    Arc::new(Float64Array::from(
                        rows.iter().map(|l| l.reward).collect::<Vec<_>>(),
                    )) as ArrayRef,
                ),
                ("reward_reason", opt(&|l| l.reward_reason.clone())),
                ("kind", opt(&|l| l.kind.clone())),
                ("harness", opt(&|l| sp(l, &|s| s.harness.clone()))),
                ("arm", opt(&|l| sp(l, &|s| Some(s.arm.clone())))),
                ("served", opt(&|l| sp(l, &|s| Some(s.served.join(","))))),
                ("withheld", opt(&|l| sp(l, &|s| Some(s.withheld.join(","))))),
                ("conversation", opt(&|l| sp(l, &|s| s.conversation.clone()))),
                (
                    "ended_at",
                    ts(rows
                        .iter()
                        .map(|l| l.session.as_ref().and_then(|s| s.ended_at))),
                ),
                ("verdicts", opt(&|l| sp(l, &|s| s.verdicts.clone()))),
                ("acl", opt(&|l| sp(l, &|s| Some(s.acl.join(","))))),
                ("ended_by", opt(&|l| sp(l, &|s| s.ended_by.clone()))),
                (
                    "fires",
                    opt(&|l| {
                        sp(l, &|s| {
                            (!s.fires.is_empty())
                                .then(|| serde_json::to_string(&s.fires).unwrap_or_default())
                        })
                    }),
                ),
                ("branch", opt(&|l| sp(l, &|s| s.branch.clone()))),
                (
                    "served_versions",
                    opt(&|l| {
                        sp(l, &|s| {
                            (!s.served_versions.is_empty()).then(|| {
                                serde_json::to_string(&s.served_versions).unwrap_or_default()
                            })
                        })
                    }),
                ),
                (
                    "lookups",
                    opt(&|l| {
                        sp(l, &|s| {
                            (!s.lookups.is_empty())
                                .then(|| serde_json::to_string(&s.lookups).unwrap_or_default())
                        })
                    }),
                ),
                (
                    "prs",
                    opt(&|l| {
                        sp(l, &|s| {
                            (!s.prs.is_empty())
                                .then(|| serde_json::to_string(&s.prs).unwrap_or_default())
                        })
                    }),
                ),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, QA_LOG);
        self.append_ledger(
            &id,
            scope,
            MAIN,
            &format!("log_answer:{actor}"),
            &|l: &Ledger| Ok(l.heads(MAIN)),
        )
        .await?;
        Ok(())
    }
}
