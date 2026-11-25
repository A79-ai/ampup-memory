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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Result;
use arrow_array::{ArrayRef, Float64Array, StringArray};
use serde::Serialize;
use serde_json::Value;

use crate::{
    Ledger, MAIN, OnConflict, Principal, QA_LOG, Scope, Store, View, appended, now_micros,
    optional_f64, optional_strings, split_list, strings, timestamps, ts,
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
/// `AMPUP_MEMORY_LOG_PER_DAY`, default 1000.
fn max_per_day() -> usize {
    std::env::var("AMPUP_MEMORY_LOG_PER_DAY")
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

/// Whether `l` is the engine's own record rather than something a person asked or ran: a dream job
/// ([`crate::dream_jobs`]), a digest's send or subscription ([`crate::digest`]), or why a dream or a team share
/// decided something ([`crate::dream::REASON_KIND`]). None is anyone's activity.
pub fn internal(l: &Logged) -> bool {
    matches!(
        l.kind.as_deref(),
        Some(crate::dream_jobs::KIND | crate::digest::KIND | crate::dream::REASON_KIND)
    )
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
    /// How it ended, once it has ([`Ended`]; read it with [`SessionPart::ended`]).
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
    /// The harness's own id for the conversation it ran in (`start_session`'s `thread`): the sessions one
    /// conversation was split into (at a compaction, or after it went idle) share it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Where an eval runs it (`start_session`'s `split`): `train`, which teaches advice by the reward its caller
    /// gives, or `test`, which reads advice and teaches nothing ([`crate::sessions::split`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
}

impl SessionPart {
    /// How the session ended: None while it is open, or when its row names an ending this engine doesn't know.
    pub fn ended(&self) -> Option<Ended> {
        self.ended_by.as_deref().and_then(Ended::parse)
    }

    /// Ends it `how`, as its row says from now on.
    pub fn end_as(&mut self, how: Ended) {
        self.ended_by = Some(how.as_str().into());
    }
}

/// How a session ended: its row's `ended_by`, spelled as [`Ended::as_str`] says. A value no variant names (a row
/// from a newer engine) is kept as it is and read as no ending ([`SessionPart::ended`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Its agent ended it (`end_session`, or its hook).
    Agent,
    /// The server ended it once it went idle: its owner may still come back to it.
    Idle,
    /// Its run kept no transcript (`end_session` with `discard`).
    Discarded,
    /// Nothing was asked in it and nothing kept, whoever ended it.
    Empty,
}

impl Ended {
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::Agent => "agent",
            Ended::Idle => "idle",
            Ended::Discarded => "discarded",
            Ended::Empty => "empty",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(Ended::Agent),
            "idle" => Some(Ended::Idle),
            "discarded" => Some(Ended::Discarded),
            "empty" => Some(Ended::Empty),
            _ => None,
        }
    }

    /// Whether the end is final: an idle end is the only one its owner can resume, and the log ranks a final end
    /// over any later row of the session.
    pub fn is_final(self) -> bool {
        match self {
            Ended::Idle => false,
            Ended::Agent | Ended::Discarded | Ended::Empty => true,
        }
    }

    /// Whether the session is one of its person's day's, the activity's and the judge's: not one that kept nothing
    /// (discarded) or had nothing asked in it (empty).
    pub fn counts(self) -> bool {
        match self {
            Ended::Agent | Ended::Idle => true,
            Ended::Discarded | Ended::Empty => false,
        }
    }
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
            .find(|l| l.answer_id == id && l.asker == who.user() && !internal(l))
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

/// The window the day's most entries is counted over (µs).
const DAY_MICROS: i64 = 86_400_000_000;

/// How old a count may be before a refusal from it also counts again, in the background: an admin clearing empty
/// sessions on another server takes rows off a count, and another server may have counted since. A refusal from an
/// older count is still answered at once (rows are only ever added, so a count only grows until one leaves the day's
/// window), and the next is answered from the new count. Sessions started ([`Cap::Sessions`]) are counted only in a
/// session's start, so their count is not answered past this; their cap is never the lower one by default, each
/// session being an entry of the log too.
const CAPPED_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// How old a count of the day's entries may be and still refuse a session's start before it reads anything, when no
/// count since could be made (the log can't be read): a caller backs off an hour at most ([`capped`]'s refusal says
/// until when), so each of their tries in that time finds a newer count.
const CAPPED_KEPT: std::time::Duration = std::time::Duration::from_secs(3_600);

/// How long before a capped caller's next entry fits the day's entries are counted again, so the try they make
/// then (told to wait until it) is answered from a count that holds what they added meanwhile.
const RECOUNT_LEAD: std::time::Duration = std::time::Duration::from_secs(10);

/// A most a day a session's start is refused at: the log's entries ([`max_per_day`]), or the sessions started
/// (`AMPUP_MEMORY_SESSIONS_PER_DAY`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Cap {
    Entries,
    Sessions,
}

/// Per scope (namespace), user and cap reached: until when (µs) one more fits, the most, and when that was counted.
type Capped = HashMap<(String, String, Cap), (i64, usize, std::time::Instant)>;

/// Who in which scope is at which most a day ([`Capped`]): set by [`entries_cap`], [`count_entries`] and by a
/// session's start, read by [`capped`] before a session's start reads anything.
static CAPPED: std::sync::LazyLock<std::sync::Mutex<Capped>> =
    std::sync::LazyLock::new(Default::default);

/// Notes whether one more entry or session (`cap`) from `user` would pass the day's most (`max`), given when each of
/// theirs in the window was recorded (`times`, oldest first): then until the oldest one that must leave the window has.
pub(crate) fn note_capped(scope: &Scope, user: &str, cap: Cap, times: &[i64], max: usize) {
    let key = (scope.namespace(), user.to_string(), cap);
    let mut capped = CAPPED.lock().unwrap();
    match times.get(times.len().saturating_sub(max)) {
        Some(t) if times.len() >= max => {
            capped.insert(key, (t + DAY_MICROS, max, std::time::Instant::now()));
        }
        _ => {
            capped.remove(&key);
        }
    }
    // Entries past their time go with each note, so the map holds only who is capped now.
    let now = now_micros();
    capped.retain(|_, (until, _, _)| *until > now);
}

/// Forgets every count in `scope`: rows were taken off them here (an admin cleared empty sessions), so a caller
/// counted at a most a day may be under it now. Their next session's start counts again.
pub(crate) fn forget_counts(scope: &Scope) {
    let ns = scope.namespace();
    CAPPED.lock().unwrap().retain(|(n, _, _), _| *n != ns);
}

/// When `user` is at a most a day in `scope` as last counted here: the refusal and the seconds until one more fits
/// (the later of the two caps when both are reached). Reads nothing: for refusing a capped caller's next session
/// before the request does any other work. A count of the entries is answered for up to [`CAPPED_KEPT`], of the
/// sessions for [`CAPPED_RECHECK`]; [`recount_later`] keeps the first fresh.
pub(crate) fn capped(scope: &Scope, user: &str) -> Option<(String, u64)> {
    let now = now_micros();
    let (cap, max, left) = {
        let all = CAPPED.lock().unwrap();
        [Cap::Entries, Cap::Sessions]
            .into_iter()
            .filter_map(|cap| {
                let (until, max, at) = *all.get(&(scope.namespace(), user.to_string(), cap))?;
                let kept = match cap {
                    Cap::Entries => CAPPED_KEPT,
                    Cap::Sessions => CAPPED_RECHECK,
                };
                (until > now && at.elapsed() < kept).then_some((cap, max, until - now))
            })
            .max_by_key(|(_, _, left)| *left)?
    };
    let secs = (left as u64).div_ceil(1_000_000);
    let what = match cap {
        Cap::Entries => "logged",
        Cap::Sessions => "started",
    };
    let things = match cap {
        Cap::Entries => "entries",
        Cap::Sessions => "sessions",
    };
    Some((
        format!(
            "daily cap reached, retry after {secs} s: {user} has {what} {max} or more {things} in {scope} today, \
             the most a day"
        ),
        secs,
    ))
}

/// Per scope (namespace) and user, the count of their entries being made: one at a time, and a caller that finds one
/// running takes its outcome rather than counting again.
type Counting = HashMap<(String, String), Arc<tokio::sync::Mutex<Option<std::time::Instant>>>>;

static COUNTING: std::sync::LazyLock<std::sync::Mutex<Counting>> =
    std::sync::LazyLock::new(Default::default);

/// Counts `user`'s entries in `scope` today and notes whether one more would pass the most a day ([`note_capped`]),
/// unless a count began after `asked` (one already running when this was asked is waited for and taken instead).
pub(crate) async fn count_entries(
    store: &Store,
    scope: &Scope,
    user: &str,
    asked: std::time::Instant,
) -> Result<()> {
    let gate = COUNTING
        .lock()
        .unwrap()
        .entry((scope.namespace(), user.to_string()))
        .or_default()
        .clone();
    let mut last = gate.lock().await;
    if last.is_some_and(|began| began >= asked) {
        return Ok(());
    }
    let began = std::time::Instant::now();
    let times = store
        .qa_row_times_since(scope, user, now_micros() - DAY_MICROS)
        .await?;
    note_capped(scope, user, Cap::Entries, &times, max_per_day());
    *last = Some(began);
    Ok(())
}

/// When the counts of capped callers' entries are due, per scope (namespace) and user: one sleeping count each.
type Recounts = HashMap<(String, String), Vec<std::time::Instant>>;

static RECOUNTS: std::sync::LazyLock<std::sync::Mutex<Recounts>> =
    std::sync::LazyLock::new(Default::default);

/// After a refusal of `user`'s session at the day's most entries: their entries are counted again in the background,
/// now if their count is past [`CAPPED_RECHECK`], and just before the next one fits if that is within
/// [`CAPPED_KEPT`], so the refusal of their next try never has to count before answering.
pub(crate) fn recount_later(store: &Store, scope: &Scope, user: &str) {
    let key = (scope.namespace(), user.to_string());
    let Some((until, _, at)) = CAPPED
        .lock()
        .unwrap()
        .get(&(key.0.clone(), key.1.clone(), Cap::Entries))
        .copied()
    else {
        return;
    };
    let now = std::time::Instant::now();
    let left = std::time::Duration::from_micros((until - now_micros()).max(0) as u64);
    let mut due = vec![];
    if at.elapsed() >= CAPPED_RECHECK {
        due.push(now);
    }
    if left <= CAPPED_KEPT {
        due.push(now + left.saturating_sub(RECOUNT_LEAD));
    }
    for when in due {
        {
            let mut recounts = RECOUNTS.lock().unwrap();
            let set = recounts.entry(key.clone()).or_default();
            if set
                .iter()
                .any(|w| w.max(&when).duration_since(*w.min(&when)) < RECOUNT_LEAD)
            {
                continue;
            }
            set.push(when);
        }
        let (store, scope, id) = (store.clone(), scope.clone(), key.clone());
        tokio::spawn(async move {
            tokio::time::sleep_until(when.into()).await;
            {
                let mut recounts = RECOUNTS.lock().unwrap();
                if let Some(set) = recounts.get_mut(&id) {
                    set.retain(|w| *w != when);
                    if set.is_empty() {
                        recounts.remove(&id);
                    }
                }
            }
            if let Err(e) = count_entries(&store, &scope, &id.1, when).await {
                eprintln!("counting {}'s entries in {scope} again failed: {e:#}", id.1);
            }
        });
    }
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
    let times = store
        .qa_row_times_since(scope, who.user(), now_micros() - DAY_MICROS)
        .await?;
    let today = times.len();
    let max = max_per_day();
    note_capped(scope, who.user(), Cap::Entries, &times, max);
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

/// Refused when `adding` more imported runs would pass the most a scope imports in a day
/// ([`crate::runs::max_imported_per_day`]), from every import source together. Counted as [`runs_cap`] counts a
/// person's, from every row ever written.
pub async fn imports_cap(store: &Store, scope: &Scope, adding: usize) -> Result<()> {
    let since = now_micros() - DAY_MICROS;
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
            if crate::runs::is_import(&asker[i])
                && kind[i].as_deref() == Some("run")
                && asked[i].is_some_and(|t| t >= since)
            {
                ids.insert(id[i].clone());
            }
        }
    }
    let max = crate::runs::max_imported_per_day();
    if ids.len() + adding > max {
        return Err(Refused(format!(
            "{} runs were imported into {scope} today: {adding} more would pass the most a day, {max}; import the \
             rest tomorrow",
            ids.len()
        ))
        .into());
    }
    Ok(())
}

/// Most runs one person may record in a scope in a day: `AMPUP_MEMORY_RUNS_PER_DAY`, default 100. An agent that
/// records every run it makes, for advice to learn from (`crate::advice`), needs more.
fn max_runs_per_day() -> usize {
    std::env::var("AMPUP_MEMORY_RUNS_PER_DAY")
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
/// runs oldest first, keep it. A refusal names its run ([`of_item`]); nothing is written then. `by` wrote them:
/// `who`, or for an import (`who` an import source, [`crate::runs::IMPORT`]) the admin or operator who sent it, whose
/// runs count toward the scope's day's imports rather than a person's caps.
pub async fn log_runs(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    by: &str,
    runs: &[RunEntry<'_>],
    label: Option<&str>,
) -> Result<()> {
    let rewards = runs.iter().filter(|r| r.reward.is_some()).count();
    if crate::runs::is_import(who.user()) {
        imports_cap(store, scope, runs.len()).await?;
    } else {
        runs_cap(store, scope, who, runs.len()).await?;
        if rewards > 0 {
            entries_cap(store, scope, who, runs.len() + rewards).await?;
        }
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
    store.write_qa_rows(scope, by, &rows).await
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
    reward_all(store, scope, who, &[(id, reward, reason, false)], None).await
}

/// Attaches rewards, as [`reward`] attaches one, in one commit: `(run or session id, reward, reason, person)` each,
/// in order (a run rewarded twice keeps the later). `person`: the reward is the person's own verdict on their
/// run or session, given by them at the keyboard (the hooks' `+` / `-` and outcome command), not a program's or
/// an agent's: it is also their rating (`helpful` above 0, `not_helpful` below), which advice learns from as the
/// person's word and the session judge leaves standing. A session's reward is held with its row
/// ([`crate::sessions::rated`]), so one still open keeps it through its later steps and its end, and one ended a
/// moment ago takes it before its row is written. A refusal names its item ([`of_item`]); nothing is written then.
pub async fn reward_all(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    rewards: &[(&str, f64, Option<&str>, bool)],
    label: Option<&str>,
) -> Result<()> {
    let refuse = |m: String| -> anyhow::Error { Refused(m).into() };
    for (i, (_, r, reason, person)) in rewards.iter().enumerate() {
        caller_reason(*reason).map_err(|e| of_item(label, i, e))?;
        if !(-1.0..=1.0).contains(r) {
            return Err(of_item(
                label,
                i,
                refuse(format!("reward is from -1 to 1, not {r}")),
            ));
        }
        if *person && *r == 0.0 {
            return Err(of_item(
                label,
                i,
                refuse("a person's rating is above 0 (it went well) or below (it didn't)".into()),
            ));
        }
    }
    entries_cap(store, scope, who, rewards.len()).await?;
    // With the session rows this server still holds: a session open (or ended a moment ago) isn't in the log yet.
    let mine: BTreeMap<String, Logged> = crate::sessions::list(store, scope)
        .await?
        .into_iter()
        .filter(|l| l.asker == who.user() && matches!(l.kind.as_deref(), Some("run" | "session")))
        .map(|l| (l.answer_id.clone(), l))
        .collect();
    let (mut rows, mut sessions) = (vec![], vec![]);
    for (i, (id, r, reason, person)) in rewards.iter().enumerate() {
        let Some(was) = mine.get(*id) else {
            return Err(of_item(
                label,
                i,
                refuse(format!("no run or session {id} that you recorded")),
            ));
        };
        let rating = match (*person, *r > 0.0) {
            (false, _) => was.rating.clone(),
            (true, true) => Some("helpful".to_string()),
            (true, false) => Some("not_helpful".to_string()),
        };
        let row = Logged {
            reward: Some(*r),
            reward_reason: reason.map(String::from),
            rating,
            ..was.clone()
        };
        match was.kind.as_deref() == Some(crate::sessions::KIND) {
            true => sessions.push(row),
            false => rows.push(row),
        }
    }
    if !rows.is_empty() {
        store.write_qa_rows(scope, who.user(), &rows).await?;
    }
    crate::sessions::rated(store, scope, who.user(), sessions);
    Ok(())
}

/// How a reviewer's reward's reason starts ([`review`]): a person who read the trace graded it, neither its
/// recorder nor a model.
pub const REVIEWED: &str = "reviewed: ";

/// Refuses a reason a caller gives a reward that starts as the server marks its own ([`REVIEWED`], [`JUDGED`],
/// [`crate::sessions::SAID`]): what a reward's reason starts with says who gave it, so a caller writing one could pass
/// its own reward off as a reviewer's.
pub fn caller_reason(reason: Option<&str>) -> Result<()> {
    match reason.map(str::trim_start) {
        Some(r)
            if [REVIEWED, JUDGED, crate::sessions::SAID]
                .iter()
                .any(|m| r.starts_with(m)) =>
        {
            Err(crate::runs::Refused(format!(
                "a reason may not start with {:?}: the server marks its own so",
                &r[..r.find(':').map_or(0, |i| i + 1)]
            ))
            .into())
        }
        _ => Ok(()),
    }
}
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
///
/// Decoding the log's rows is most of what an activity, lessons or advice read costs, and a page asks for it
/// several times at once, so the decoded rows are kept per scope, by data file: a write to the log gives a new
/// snapshot, of which only the files not decoded before are decoded. What may be read
/// (committed, not retracted, not erased or forgotten) is applied to them on every call, from main's visibility
/// as it is now. Never cached by time.
pub async fn list(store: &Store, scope: &Scope, goal: Option<&str>) -> Result<Vec<Logged>> {
    list_where(store, scope, goal, |_| true).await
}

/// [`list`], only the entries `keep` keeps: each answer's newest row is chosen as `list` chooses it, then kept or
/// not, and only those kept are copied. A caller after one session or one person's reads only theirs: a listing
/// copies every entry of the log, which in a large org is a GB, and a session's end read it three times.
pub async fn list_where(
    store: &Store,
    scope: &Scope,
    goal: Option<&str>,
    keep: impl Fn(&Logged) -> bool,
) -> Result<Vec<Logged>> {
    // Main's last state may serve while the next is built: what was recorded since comes with it, so an entry
    // logged a moment ago is listed, and nothing taken away is.
    let view = store.lagging_view(scope, MAIN).await?;
    crate::phases::mark("qa_log view");
    let rows = store.side_rows(&scope.namespace(), QA_LOG).await?;
    let decoded = decoded(scope, rows);
    let out = newest(&view, &decoded, goal, keep);
    crate::phases::mark("qa_log newest");
    Ok(out)
}

/// Loads the log's rows of `scope` and decodes those not decoded before, as [`list`] reads them, without listing:
/// for the warm-up, so the first page that lists sessions on a new server (the Learned page, a session start)
/// doesn't load the table itself (7.7 s on staging, Nov 2026).
pub async fn warm(store: &Store, scope: &Scope) -> Result<()> {
    let rows = store.side_rows(&scope.namespace(), QA_LOG).await?;
    let _ = decoded(scope, rows);
    Ok(())
}

/// One row of the log as written, decoded.
struct Row {
    commit: String,
    /// A session's final end (an agent's, discarded, empty), which outranks a later row that isn't one.
    final_end: bool,
    /// The session's PRs this row holds, when it holds any.
    prs: Option<Vec<crate::session_prs::Pr>>,
    /// A reviewer's grade this row records: (reward, reason).
    review: Option<(f64, String)>,
    logged: Logged,
}

/// Each batch's decoded rows, in the snapshot's order.
type Chunks = Arc<Vec<Arc<Vec<Row>>>>;
/// The decoded rows per scope, with the snapshot they were decoded from (held, so a pointer is never reused).
type Decoded = (Arc<Vec<arrow_array::RecordBatch>>, Chunks);
static DECODED: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Decoded>>> =
    std::sync::LazyLock::new(Default::default);

/// Roughly how many bytes `l` holds: its text, its lists and its session part (serialized length).
pub fn logged_bytes(l: &Logged) -> usize {
    let opt = |o: &Option<String>| o.as_ref().map_or(0, |s| s.len());
    std::mem::size_of::<Logged>()
        + l.answer_id.len()
        + l.asker.len()
        + opt(&l.goal)
        + l.question.len()
        + opt(&l.answer)
        + opt(&l.rating)
        + l.cited
            .iter()
            .chain(&l.episodes)
            .map(|s| s.len() + 24)
            .sum::<usize>()
        + opt(&l.kind)
        + opt(&l.reward_reason)
        + l.session
            .as_ref()
            .map_or(0, |p| serde_json::to_vec(p).map_or(0, |v| v.len()))
}

/// What the decoded log holds, for the server's memory log: scopes, rows, the snapshots' Arrow bytes (kept
/// alive here even once the table cache drops them) and the decoded rows' bytes (every 20th row sized).
pub fn memory_report() -> String {
    let all: Vec<(String, Decoded)> = DECODED
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (mut rows, mut batches, mut decoded, mut largest) = (0, 0, 0, (0, String::new()));
    for (name, (b, d)) in &all {
        let n: usize = d.iter().map(|c| c.len()).sum();
        let arrow: usize = b.iter().map(|b| b.get_array_memory_size()).sum();
        let mut sampled = (0, 0);
        for r in d.iter().flat_map(|c| c.iter()).step_by(20) {
            sampled = (
                sampled.0 + 1,
                sampled.1 + logged_bytes(&r.logged) + r.commit.len(),
            );
        }
        let bytes = (sampled.1 * n).checked_div(sampled.0).unwrap_or(0);
        rows += n;
        batches += arrow;
        decoded += bytes;
        if arrow + bytes > largest.0 {
            largest = (arrow + bytes, name.clone());
        }
    }
    format!(
        "qa_log {} scopes {rows} rows, arrow {} MB, decoded ~{} MB (largest {} {} MB)",
        all.len(),
        batches >> 20,
        decoded >> 20,
        largest.1,
        largest.0 >> 20
    )
}

/// A batch's identity: its first column, shared by every read of the same data file.
fn batch_id(b: &arrow_array::RecordBatch) -> usize {
    Arc::as_ptr(b.column(0)) as *const () as usize
}

fn decoded(scope: &Scope, rows: Arc<Vec<arrow_array::RecordBatch>>) -> Chunks {
    let key = scope.to_string();
    let last = DECODED.lock().unwrap().get(&key).cloned();
    if let Some((r, d)) = &last
        && Arc::ptr_eq(r, &rows)
    {
        crate::phases::mark("qa_log cached");
        return d.clone();
    }
    // The batches decoded before, by identity: `last` holds their snapshot, so no pointer here is reused meanwhile.
    let known: HashMap<usize, Arc<Vec<Row>>> = match &last {
        Some((r, d)) => r.iter().map(batch_id).zip(d.iter().cloned()).collect(),
        None => HashMap::new(),
    };
    let mut fresh = 0;
    let d: Chunks = Arc::new(
        rows.iter()
            .map(|b| {
                known.get(&batch_id(b)).cloned().unwrap_or_else(|| {
                    fresh += 1;
                    Arc::new(decode(std::slice::from_ref(b)))
                })
            })
            .collect(),
    );
    crate::phases::mark(match fresh == rows.len() {
        true => "qa_log decoded",
        false => "qa_log appended",
    });
    DECODED.lock().unwrap().insert(key, (rows, d.clone()));
    d
}

/// Every row of the log, in the order written.
fn decode(rows: &[arrow_array::RecordBatch]) -> Vec<Row> {
    let mut out = vec![];
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
        let (thread, split) = (oo("thread"), oo("split"));
        let ended = if has("ended_at") {
            timestamps(b, "ended_at")
        } else {
            vec![None; n]
        };
        for i in 0..n {
            let at = at[i].unwrap_or_default();
            out.push(Row {
                commit: commit[i].clone(),
                final_end: kind[i].as_deref() == Some("session")
                    && ended[i].is_some()
                    && ended_by[i]
                        .as_deref()
                        .and_then(Ended::parse)
                        .is_some_and(Ended::is_final),
                prs: prs[i]
                    .as_deref()
                    .and_then(|v| serde_json::from_str::<Vec<crate::session_prs::Pr>>(v).ok())
                    .filter(|p| !p.is_empty()),
                review: match (reward[i], reward_reason[i].as_deref()) {
                    (Some(r), Some(why)) if why.starts_with(REVIEWED) => Some((r, why.to_string())),
                    _ => None,
                },
                logged: Logged {
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
                        thread: thread[i].clone(),
                        split: split[i].clone(),
                    }),
                },
            });
        }
    }
    out
}

/// The answers `vis` lets be read, each as its newest row; only `goal`'s with one; newest first.
fn newest(
    view: &View,
    chunks: &[Arc<Vec<Row>>],
    goal: Option<&str>,
    keep: impl Fn(&Logged) -> bool,
) -> Vec<Logged> {
    let rows: Vec<&Row> = chunks.iter().flat_map(|c| c.iter()).collect();
    // Each answer's newest row, except that a session its agent ended is final: no row without that end (a
    // conversation kept, or the sweep's idle end, from a read taken before the end landed) supersedes it.
    let mut newest: BTreeMap<&str, ((bool, i64), usize)> = BTreeMap::new();
    // A session's PRs as every row of it holds them (`crate::session_prs::merge`): they only grow, and a row written
    // from a read taken before another pod's row landed would otherwise drop what that row added.
    let mut prs_of: BTreeMap<&str, Vec<crate::session_prs::Pr>> = BTreeMap::new();
    // Each entry's newest review ([`review`]): (when, reward, reason). It stands over any row that isn't one, so a
    // row written from a read taken before the review landed doesn't undo it.
    let mut review_of: BTreeMap<&str, (i64, f64, &str)> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        let l = &r.logged;
        // An answer citing an episode retracted or erased since is gone with it, even one logged while the
        // erasure ran (its rows are swept by the next erasure in the scope), as is one citing a retracted fact.
        if !view.is_committed(&r.commit)
            || l.episodes.iter().any(|e| view.is_gone(e))
            || l.cited.iter().any(|f| view.vis.retracted_facts.contains(f))
        {
            continue;
        }
        let id = l.answer_id.as_str();
        if let Some(p) = &r.prs {
            crate::session_prs::merge(prs_of.entry(id).or_default(), p.clone());
        }
        let at = l.updated_at;
        if let Some((rw, why)) = &r.review
            && review_of.get(id).is_none_or(|(t, _, _)| *t < at)
        {
            review_of.insert(id, (at, *rw, why));
        }
        let rank = (r.final_end, at);
        if newest.get(id).is_some_and(|(k, _)| *k >= rank) {
            continue;
        }
        newest.insert(id, (rank, i));
    }
    let mut out: Vec<Logged> = newest
        .into_values()
        .map(|(_, i)| &rows[i].logged)
        .filter(|l| goal.is_none_or(|g| l.goal.as_deref() == Some(g)) && keep(l))
        .map(|l| {
            let mut l = l.clone();
            if let (Some(s), Some(p)) = (l.session.as_mut(), prs_of.remove(l.answer_id.as_str())) {
                crate::session_prs::merge(&mut s.prs, p);
            }
            if let Some((_, r, why)) = review_of.remove(l.answer_id.as_str()) {
                l.reward = Some(r);
                l.reward_reason = Some(why.to_string());
            }
            l
        })
        .collect();
    out.sort_by_key(|l| std::cmp::Reverse(l.asked_at));
    out
}

/// The QA log's reads and writes on a [`Store`] (an inherent impl until the store moved to memstore-core).
pub(crate) trait StoreQaLog {
    async fn write_qa_rows(&self, scope: &Scope, actor: &str, rows: &[Logged]) -> Result<()>;
    async fn write_qa(&self, scope: &Scope, actor: &str, l: &Logged) -> Result<()>;
    async fn qa_rows_since(&self, scope: &Scope, asker: &str, since: i64) -> Result<usize> {
        Ok(self.qa_row_times_since(scope, asker, since).await?.len())
    }
    async fn qa_row_times_since(&self, scope: &Scope, asker: &str, since: i64) -> Result<Vec<i64>>;
}

impl StoreQaLog for Store {
    /// When each row `asker` added to the log since `since` (µs) was recorded, oldest first, ratings included; not
    /// the one row ending a session `empty` (an admin's `clear_empty_sessions` writes one per session it clears, in
    /// the asker's name). Its start was a row of its own, which counts, as does any other row of it. Nor an ended
    /// backfilled session's rows (`crate::sessions::BACKFILL`: past sessions sent long after they ran), which have a
    /// day's most of their own, so sending them never refuses what the person does now. One left open (its end
    /// failed) counts like any other: steps in it are the person's own activity.
    async fn qa_row_times_since(&self, scope: &Scope, asker: &str, since: i64) -> Result<Vec<i64>> {
        let rows = self.side_rows(&scope.namespace(), QA_LOG).await?;
        let mut ended_empty = std::collections::HashSet::new();
        let mut n = vec![];
        for b in rows.iter() {
            let ended_by = optional_strings(b, "ended_by");
            let harness = optional_strings(b, "harness");
            // A batch from before sessions had an end has no such column: none of its rows ended.
            let ended_at = match b.schema().column_with_name("ended_at") {
                Some(_) => timestamps(b, "ended_at"),
                None => vec![None; b.num_rows()],
            };
            for (((((a, t), e), id), h), end) in strings(b, "asker")
                .iter()
                .zip(timestamps(b, "recorded_at"))
                .zip(&ended_by)
                .zip(strings(b, "answer_id"))
                .zip(&harness)
                .zip(&ended_at)
            {
                if a.as_str() != asker || !t.is_some_and(|t| t >= since) {
                    continue;
                }
                if end.is_some() && h.as_deref().is_some_and(crate::sessions::is_backfill) {
                    continue;
                }
                if e.as_deref().and_then(Ended::parse) == Some(Ended::Empty)
                    && ended_empty.insert(id)
                {
                    continue;
                }
                n.extend(t);
            }
        }
        n.sort_unstable();
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
        let before = self.generation(&ns, QA_LOG);
        let after = appended(
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
                ("thread", opt(&|l| sp(l, &|s| s.thread.clone()))),
                ("split", opt(&|l| sp(l, &|s| s.split.clone()))),
            ],
            OnConflict::Recommit,
        )
        .await?;
        // The session log is read at every session's step: the rows just written are cached here, not read back
        // from the catalog by the next step.
        self.wrote(&ns, QA_LOG, after, before).await;
        // A session ended `empty` (an admin's clear) leaves its asker's day count: counts kept here are counted again.
        if rows.iter().any(|l| {
            l.session
                .as_ref()
                .is_some_and(|s| s.ended() == Some(Ended::Empty))
        }) {
            forget_counts(scope);
        }
        // Rows one person writes for another's entries (an admin clearing empty sessions, naming their tasks or
        // marking them automated, a reviewer's grade) change what the activity and advice pages show of them: what
        // was kept of those pages is worked out again on their next read here.
        if rows.iter().any(|l| l.asker != actor) {
            crate::kept_answers::changed(&ns);
        }
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
