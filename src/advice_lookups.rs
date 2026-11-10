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
    /// What the person's own advice came to first, when they have some and it served nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
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
}

struct Trace {
    moment: String,
    started: Instant,
    at: i64,
    steps: Vec<Step>,
    /// The decisions model's best score and the fit, noted before the step they belong to ends.
    score: Option<f64>,
    fit: Option<f64>,
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
            RefCell::new(Trace {
                moment: moment.to_string(),
                started: Instant::now(),
                at: crate::now_micros(),
                steps: vec![],
                score: None,
                fit: None,
                trouble: None,
            }),
            f,
        )
        .await
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
            Some((moment, started, at)) => {
                TRACE
                    .scope(
                        RefCell::new(Trace {
                            moment,
                            started,
                            at,
                            steps: vec![],
                            score: None,
                            fit: None,
                            trouble: None,
                        }),
                        f,
                    )
                    .await
            }
        }
    }
}

/// The decisions model's best score for the routing under way.
pub fn scored(p: f64) {
    let _ = TRACE.try_with(|t| t.borrow_mut().score = Some(p));
}

/// How well the advice routed fit the very task (or, at a hook's step, how useful its lines were).
pub fn fitted(p: f64) {
    let _ = TRACE.try_with(|t| t.borrow_mut().fit = Some(p));
}

/// Something went wrong on the way that the outcome alone doesn't say: kept with the lookup.
pub fn trouble(what: &str) {
    let _ = TRACE.try_with(|t| t.borrow_mut().trouble = Some(what.chars().take(300).collect()));
}

/// One space's routing ended at `outcome` (`routed` when it found advice to serve), on line `key`.
pub fn step(own: bool, outcome: &'static str, key: Option<String>) {
    let _ = TRACE.try_with(|t| {
        let mut t = t.borrow_mut();
        let (score, fit) = (t.score.take(), t.fit.take());
        t.steps.push(Step {
            own,
            outcome,
            key,
            score,
            fit,
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
                own: own.filter(|o| o != ROUTED),
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
        eprintln!(
            "advice lookup outside a session: {}{}{}{} ({} ms)",
            l.outcome,
            l.key
                .as_deref()
                .map(|k| format!(" {k}"))
                .unwrap_or_default(),
            l.score
                .map(|p| format!(", score {p:.2}"))
                .unwrap_or_default(),
            l.fit.map(|p| format!(", fit {p:.2}")).unwrap_or_default(),
            l.ms
        );
    }
}
