//! What each advice lookup came to: a session's start, a question routed in it (a hook's prompt, an agent's own
//! `get_advice`), a later moment re-reading its note, a hook's step. Which line the lookup was routed to, how likely
//! the decisions model found it, whether the advice read whole fit the very task, and what the session was served,
//! or why nothing was. Kept on the session's row (its newest [`MAX`]), which Studio reads (`/admin {op:
//! "activity"}`): when advice is served and when it isn't, per session and in sum, without joining the server's
//! log and the training log by time.
//!
//! A lookup runs inside [`traced`]. The routing code ([`crate::advice`], [`crate::advice_route`],
//! [`crate::sessions::useful`]) notes its steps here as it goes, and the session step that records the outcome
//! ([`crate::sessions::advise`], [`crate::sessions::opened`], [`crate::sessions::note_lookup`]) turns them into a
//! [`Lookup`]. Outside a traced lookup a note does nothing. A lookup outside any session is said in the log
//! ([`outside`]).

use std::cell::RefCell;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Lookups a session keeps, its newest: a row stays small however long the session runs.
pub const MAX: usize = 50;

/// One lookup, as its session keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lookup {
    /// When it began (µs).
    pub at: i64,
    /// Where in the session it was asked: `start`, `prompt` (a hook's), `agent` (the agent's own call), or the
    /// moment a later ask names (`fork`, `stuck`, `done`, `next`).
    pub moment: String,
    /// What it came to ([`Outcome`]'s names).
    pub outcome: String,
    /// The line it was routed to: a situation's key (`mine:` for the person's own), a compiled skill's
    /// (`skill:…`), another scope's (`<scope>#…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// How likely the decisions model found the best line (0 to 1); none where the chat router routed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// How likely the decisions model found the routed advice, read whole, to be for this very task; at a hook's
    /// step, how relevant and useful it found the lines it would hand on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<f64>,
    /// How likely the decisions model found the request to be in the situation the routed note's `When` describes,
    /// read with where in the session it was asked ([`crate::advice_route::fits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<f64>,
    /// How likely it found one of the note's `Withhold when` cases to describe the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withhold: Option<f64>,
    /// What the person's own advice came to first, when they have some and it served nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
    /// The person's own note (`mine:`) served in the same step before `key`, the team's answer: a later moment
    /// re-reads both ([`crate::sessions::revisit`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beside: Option<String>,
    /// Why it failed, for an `error`; else what went wrong on the way (the decisions model failed and the chat
    /// router routed, a fit check wasn't answered).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ms: u64,
}

/// What a lookup can come to.
pub mod outcome {
    /// A team note.
    pub const SERVED: &str = "served";
    /// The person's own note.
    pub const SERVED_OWN: &str = "served_own";
    /// Procedures with no note: a compiled skill, a situation's linked procedures, another scope's advice.
    pub const SERVED_PROCEDURES: &str = "served_procedures";
    /// Routed and fit, but the session is in the holdout arm: withheld for comparison.
    pub const WITHHELD: &str = "withheld";
    /// The note the session was served already: nothing new.
    pub const AGAIN: &str = "again";
    /// No line was likely enough (the decisions model's best under its bar), or the chat router named none.
    pub const NO_MATCH: &str = "no_match";
    /// A kind of task the map knows, with no note or procedure to serve.
    pub const NO_ADVICE: &str = "no_advice";
    /// Routed, but the advice read whole was judged to be for another task.
    pub const OFF_TASK: &str = "off_task";
    /// Routed to a note whose `When` the request isn't in, or one of whose `Withhold when` cases it is, read with
    /// where in the session it was asked (a follow-up note for a request that starts a task of its own, a task's note
    /// for a message that only steers the work under way): withheld ([`crate::advice_route::fits`]).
    pub const WHEN_MISMATCH: &str = "when_mismatch";
    /// Nothing to route over: no situation, no skill.
    pub const NO_LINES: &str = "no_lines";
    /// A later moment re-read the note the session was served, cut to the moment.
    pub const REVISITED: &str = "revisited";
    /// A later moment with no note to re-read and nothing new to route.
    pub const NOTHING_NEW: &str = "nothing_new";
    /// A hook's step: lines of the served note handed on.
    pub const STEP_HANDED: &str = "step_handed";
    /// A hook's step: lines found, judged not relevant or useful enough to hand on.
    pub const STEP_NOT_USEFUL: &str = "step_not_useful";
    /// A hook's step: the lines of the served note would go on, but the step isn't in the situation the note's
    /// `When` describes (a follow-up note's lines at a step with no new message from the user), or one of its
    /// `Withhold when` cases is it ([`crate::sessions::useful`]).
    pub const STEP_WHEN_MISMATCH: &str = "step_when_mismatch";
    /// A hook's step routed afresh, but what it found was judged not relevant or useful enough now: withheld, and
    /// not recorded as served, so no later step hands its lines on.
    pub const NOT_USEFUL: &str = "not_useful";
    /// A hook's step: nothing of the served note bears on it, or the session had all it hands on.
    pub const STEP_NOTHING: &str = "step_nothing";
    /// A program's session (its harness named `:sdk`) in a scope whose `advice_automated` is off: nothing routed.
    pub const AUTOMATED: &str = "automated";
    /// A hook's step asked with a message that steers the work under way (sent mid-turn, or by the harness or a
    /// helper agent), in a session with no note to re-read: not routed afresh ([`crate::sessions::steers`]).
    pub const FOLLOW_UP: &str = "follow_up";
    /// The lookup failed.
    pub const ERROR: &str = "error";

    /// Whether `o` gave the agent advice.
    pub fn served(o: &str) -> bool {
        matches!(
            o,
            SERVED | SERVED_OWN | SERVED_PROCEDURES | REVISITED | STEP_HANDED
        )
    }
}

/// One routing over one space ([`crate::advice::get_in`]'s), as it ended.
#[derive(Clone, Debug)]
struct Step {
    own: bool,
    outcome: &'static str,
    key: Option<String>,
    score: Option<f64>,
    fit: Option<f64>,
    when: Option<(f64, Option<f64>)>,
}

struct Trace {
    moment: String,
    started: Instant,
    at: i64,
    steps: Vec<Step>,
    /// The decisions model's best score, the fit and the note's `When` (and `Withhold when`), noted before the
    /// step they belong to ends.
    score: Option<f64>,
    fit: Option<f64>,
    when: Option<(f64, Option<f64>)>,
    /// What went wrong on the way ([`trouble`]).
    trouble: Option<String>,
}

tokio::task_local! {
    static TRACE: RefCell<Trace>;
}

/// Runs lookup `f`, asked at `moment`, with its steps noted.
pub async fn traced<F: std::future::Future>(moment: &str, f: F) -> F::Output {
    TRACE
        .scope(
            fresh(moment.to_string(), Instant::now(), crate::now_micros()),
            f,
        )
        .await
}

/// A lookup's trace with no step noted yet.
fn fresh(moment: String, started: Instant, at: i64) -> RefCell<Trace> {
    RefCell::new(Trace {
        moment,
        started,
        at,
        steps: vec![],
        score: None,
        fit: None,
        when: None,
        trouble: None,
    })
}

/// `f`, to run on a task of its own (a routing job the caller won't wait for), as part of the lookup it was started
/// in: same moment, same start, its steps noted there.
pub fn carry<F: std::future::Future>(f: F) -> impl std::future::Future<Output = F::Output> {
    let from = TRACE
        .try_with(|t| {
            let t = t.borrow();
            (t.moment.clone(), t.started, t.at)
        })
        .ok();
    async move {
        match from {
            None => f.await,
            Some((moment, started, at)) => TRACE.scope(fresh(moment, started, at), f).await,
        }
    }
}

/// `f`, a routing run at once beside another of the same lookup (a person's own advice beside the team's): its
/// steps are noted apart, so neither's scores land on the other's step, and then put before the lookup's others,
/// where the person's own come when routed first.
pub async fn apart<F: std::future::Future>(f: F) -> F::Output {
    let Ok((moment, started, at)) = TRACE.try_with(|t| {
        let t = t.borrow();
        (t.moment.clone(), t.started, t.at)
    }) else {
        return f.await;
    };
    let (out, (steps, trouble)) = TRACE
        .scope(fresh(moment, started, at), async move {
            let out = f.await;
            let noted = TRACE.with(|t| {
                let mut t = t.borrow_mut();
                (std::mem::take(&mut t.steps), t.trouble.take())
            });
            (out, noted)
        })
        .await;
    let _ = TRACE.try_with(|t| {
        let mut t = t.borrow_mut();
        t.steps.splice(0..0, steps);
        if t.trouble.is_none() {
            t.trouble = trouble;
        }
    });
    out
}

/// The decisions model's best score for the routing under way.
pub fn scored(p: f64) {
    let _ = TRACE.try_with(|t| t.borrow_mut().score = Some(p));
}

/// How well the advice routed fit the very task (or, at a hook's step, how useful its lines were).
pub fn fitted(p: f64) {
    let _ = TRACE.try_with(|t| t.borrow_mut().fit = Some(p));
}

/// How likely the request is in the routed note's `When`, and one of its `Withhold when` cases (when it has some).
pub fn when_fitted(when: f64, withhold: Option<f64>) {
    let _ = TRACE.try_with(|t| t.borrow_mut().when = Some((when, withhold)));
}

/// What routed to `key` was withheld after all (`outcome`), by a check after routing: its step says so, with the
/// scores it was routed and fitted at.
pub fn withheld_after(outcome: &'static str, key: &str) {
    let _ = TRACE.try_with(|t| {
        if let Some(s) = t
            .borrow_mut()
            .steps
            .iter_mut()
            .rev()
            .find(|s| s.key.as_deref() == Some(key))
        {
            s.outcome = outcome;
        }
    });
}

/// Something went wrong on the way that the outcome alone doesn't say: kept with the lookup.
pub fn trouble(what: &str) {
    let _ = TRACE.try_with(|t| t.borrow_mut().trouble = Some(what.chars().take(300).collect()));
}

/// One space's routing ended at `outcome` (`routed` when it found advice to serve), on line `key`.
pub fn step(own: bool, outcome: &'static str, key: Option<String>) {
    let _ = TRACE.try_with(|t| {
        let mut t = t.borrow_mut();
        let (score, fit, when) = (t.score.take(), t.fit.take(), t.when.take());
        t.steps.push(Step {
            own,
            outcome,
            key,
            score,
            fit,
            when,
        });
    });
}

/// Where a routing that found advice ends ([`step`]).
pub const ROUTED: &str = "routed";

/// The lookup under way, come to `outcome` on `key` (the session's serving of what routing found); none outside a
/// traced lookup. The routing step behind it gives its scores, and a person's own advice that served nothing says
/// what it came to.
pub fn finish(outcome: &str, key: Option<&str>) -> Option<Lookup> {
    TRACE
        .try_with(|t| {
            let t = t.borrow();
            // Nothing routed and nothing served: nothing was asked (a session opened with no task yet).
            if outcome == outcome::NO_MATCH && t.steps.is_empty() && t.score.is_none() {
                return None;
            }
            let own = t
                .steps
                .iter()
                .find(|s| s.own)
                .filter(|s| {
                    s.outcome != ROUTED || !key.is_some_and(|k| Some(k) == s.key.as_deref())
                })
                .map(|s| s.outcome.to_string());
            // The step the outcome rests on: the one that routed to `key`, else the last (the team's, which
            // decides when the person's own served nothing).
            let step = t
                .steps
                .iter()
                .rev()
                .find(|s| key.is_some() && s.key.as_deref() == key)
                .or(t.steps.last());
            // Nothing served: the last step says why, when the session's serving didn't.
            let (outcome, key) = match (outcome, step) {
                (outcome::NO_MATCH, Some(s)) if s.outcome != ROUTED => {
                    (s.outcome.to_string(), s.key.clone())
                }
                (o, s) => (
                    o.to_string(),
                    key.map(String::from)
                        .or_else(|| s.and_then(|s| s.key.clone())),
                ),
            };
            Some(Lookup {
                at: t.at,
                moment: t.moment.clone(),
                outcome,
                key,
                score: step.and_then(|s| s.score).or(t.score),
                fit: step.and_then(|s| s.fit).or(t.fit),
                when: step.and_then(|s| s.when).or(t.when).map(|(w, _)| w),
                withhold: step.and_then(|s| s.when).or(t.when).and_then(|(_, h)| h),
                own: own.filter(|o| o != ROUTED),
                beside: None,
                error: t.trouble.clone(),
                ms: t.started.elapsed().as_millis() as u64,
            })
        })
        .ok()
        .flatten()
}

/// The lookup under way failed with `e`.
pub fn failed(e: &anyhow::Error) -> Option<Lookup> {
    let mut l = finish(outcome::ERROR, None)?;
    let m = format!("{e:#}");
    l.error = Some(m.chars().take(300).collect());
    Some(l)
}

/// Adds `l` to a session's lookups, keeping the newest [`MAX`].
pub fn keep(lookups: &mut Vec<Lookup>, l: Lookup) {
    lookups.push(l);
    if lookups.len() > MAX {
        let extra = lookups.len() - MAX;
        lookups.drain(..extra);
    }
}

/// Says in the log what a lookup outside any session came to (`POST /advice`, `get_advice` with no session): no
/// row keeps it.
pub fn outside(found: Option<&crate::advice::Advised>) {
    let (o, key) = match found {
        Some(a) if a.key.starts_with(crate::advice::MINE_KEY) => {
            (outcome::SERVED_OWN, Some(a.key.as_str()))
        }
        Some(a) if a.note => (outcome::SERVED, Some(a.key.as_str())),
        Some(a) => (outcome::SERVED_PROCEDURES, Some(a.key.as_str())),
        None => (outcome::NO_MATCH, None),
    };
    if let Some(l) = finish(o, key) {
        let own = found.and_then(|a| a.own.as_ref());
        eprintln!(
            "advice lookup outside a session: {}{}{}{}{} ({} ms)",
            l.outcome,
            l.key
                .as_deref()
                .map(|k| format!(" {k}"))
                .unwrap_or_default(),
            own.map(|o| format!(" (with their own {})", o.key))
                .unwrap_or_default(),
            l.score
                .map(|p| format!(", score {p:.2}"))
                .unwrap_or_default(),
            l.fit.map(|p| format!(", fit {p:.2}")).unwrap_or_default(),
            l.ms
        );
    }
}
