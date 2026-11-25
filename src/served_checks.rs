//! Served facts checked against what came of them: a fact memory served in a session that the conversation then
//! shows to be wrong (the person corrects it, or the agent finds it no longer holds) is flagged, as the person's
//! own `negative` feedback ([`crate::feedback`]), without the agent being asked to do anything.
//!
//! Agents rarely say a fact misled them: `feedback_memory` isn't among the tools offered by default, and where hooks
//! end a session the agent never calls `end_session`. But the server sees both halves: which facts its reads served
//! a caller ([`note`], from each read's `Reads`), and the session's transcript once it ends. So once an ended session
//! has settled (the sessions sweep, [`crate::sessions::sweep`]), the facts served to its person between its start
//! and its end are put, with the person's messages and the agent's replies in it, to the decisions model
//! ([`crate::decide`]): one question per fact, whether the conversation shows it wrong. A fact it is sure of
//! ([`SURE`]) gets a flag whose reason names the session (never quotes it: a transcript is its person's alone);
//! nothing else gets anything. Only where advice learns: a team that turned learning off sends no transcript to a
//! model, and an eval's or a program's session is never checked.
//!
//! - **On by default** ([`Mode`]): `AMPUP_MEMORY_SERVED_FACT_CHECKS` is `on` (flagged), `shadow` (checked, each
//!   answer logged, nothing flagged) or `off`; a team's or org's `fact_checks` setting overrides it (true flags,
//!   false turns it off). A flag shows in every reader's tally, so a wrong one marks a true fact for everyone: only
//!   one the model is sure of ([`SURE`]) is flagged, and every answer is logged, by fact id and likelihood, to grade.
//! - **Undone by turning it off**: a flag it gave says so ([`REASON`]), and a tally counts one only where checks
//!   flag ([`counts_flags`]). Set `fact_checks` false (or the server `off`) and every flag it gave there stops
//!   counting at once; set it back and they count again.
//! - **What it sees**: facts served through this server's reads of the caller's own scope (a hook's recall, an
//!   agent's `search_memory`; from a little before the session's start, [`EARLY`], as a hook's recall runs beside its
//!   `start_session`), held in memory on the server that served them for [`window`]
//!   (`AMPUP_MEMORY_SERVED_FACT_HOURS`, default 24) and dropped once checked. A restart, another server, or a fact
//!   the agent repeated from its own context without a read, or a recall a hook kept from an earlier session and
//!   handed this one, is not seen. A person in two sessions at once: the first to be checked takes what was read
//!   while it was open, the other's reads among them. Nor, yet, is a tool result that
//!   contradicts a fact unless the agent says so.
//! - **Whose flag**: the session's person's, as if they gave it: one vote per person per fact (a vote they give by
//!   hand later replaces it, and this replaces one they gave before), within their daily allowance, and erased with
//!   them, or with the fact's episodes, as any feedback is.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::feedback::StoreFeedback;
use crate::qa_log::Logged;
use crate::{Principal, Scope, Store, now_micros};

/// What a flag's reason starts with: how a tally tells the flags these checks gave.
pub const REASON: &str = "shown wrong in session ";
/// How long before a session's start a read still counts as its own (µs): a hook's recall runs beside the
/// `start_session` that opens it, and may land first.
pub const EARLY: i64 = 120_000_000;

/// How sure the decisions model must be that the conversation shows a fact wrong before it is flagged. Picked from
/// a labelled set on the decisions model a production deployment runs (JEV through OpenRouter): 2,052 facts recalled
/// in real sessions and 720 in hard synthetic ones, none shown wrong above 0.41, and 57 of the 62 synthetic corrections at
/// or above it (46 at 0.85).
pub const SURE: f64 = 0.75;
/// Facts asked about per session at most (those served first).
const MAX_FACTS: usize = 40;
/// Callers held at most, and fact ids per caller: past them, the oldest go.
const MAX_CALLERS: usize = 10_000;
const MAX_HELD: usize = 2_000;
/// What the model reads of each message, at most, in characters.
const MESSAGE_CHARS: usize = 1_000;
/// Messages it reads at most per session (the last ones).
const MAX_MESSAGES: usize = 30;
/// The longest reason kept (the feedback table keeps at most 500 characters).
const REASON_CHARS: usize = 480;

const QUESTION: &str = "Fact {n} (in \"facts\") was served to an AI agent from its team's memory during the \
    conversation in \"conversation\". Does the conversation show that fact {n} is wrong or no longer true: the person \
    says so or gives another value for the same thing, or the agent finds and says that it doesn't hold? Not when \
    the fact was merely unused, unhelpful, or about something else than what was discussed.";

/// What the checks do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Nothing is held or checked.
    Off,
    /// Checked, and each answer logged; nothing is flagged.
    Shadow,
    /// Checked, and a fact the model is sure of flagged.
    On,
}

impl Mode {
    /// The server's: `AMPUP_MEMORY_SERVED_FACT_CHECKS` (`off`, `shadow` or `on`; `on` unset or unknown).
    pub fn server() -> Mode {
        match std::env::var("AMPUP_MEMORY_SERVED_FACT_CHECKS")
            .unwrap_or_default()
            .trim()
        {
            "off" | "false" | "0" => Mode::Off,
            "shadow" => Mode::Shadow,
            _ => Mode::On,
        }
    }

    /// `scope`'s: its team's or org's `fact_checks` setting (true on, false off), else the server's. Off while the
    /// directory can't be read: a scope that turned checks off is never checked, nor its old flags counted, on a
    /// guess.
    pub async fn of(store: &Store, scope: &Scope) -> Mode {
        let set = match store.directory().await {
            Ok(d) => d.dir.fact_checks(scope),
            Err(_) => return Mode::Off,
        };
        match set {
            Some(true) => Mode::On,
            Some(false) => Mode::Off,
            None => Mode::server(),
        }
    }
}

/// Whether reads hold the facts they serve for a check: unless the server's mode is off (a scope that turned it
/// off holds them till they age out, unchecked).
pub fn on() -> bool {
    Mode::server() != Mode::Off
}

/// Whether a tally in `scope` counts the flags these checks gave: only where they flag.
pub async fn counts_flags(store: &Store, scope: &Scope) -> bool {
    Mode::of(store, scope).await == Mode::On
}

/// How long served facts are held for a check, in microseconds: `AMPUP_MEMORY_SERVED_FACT_HOURS` (default 24).
pub fn window() -> i64 {
    let hours: i64 = std::env::var("AMPUP_MEMORY_SERVED_FACT_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(24);
    hours.max(0) * 3_600_000_000
}

/// Each read's time and the facts it served, oldest first.
type Served = VecDeque<(i64, Vec<String>)>;

/// The facts each caller was served, by (scope, caller).
static SERVED: LazyLock<Mutex<HashMap<(String, String), Served>>> = LazyLock::new(Default::default);

/// Note that a read of `user`'s in `scope` served these facts, now.
pub fn note(scope: &Scope, user: &str, facts: &[String]) {
    if facts.is_empty() || !on() {
        return;
    }
    let (now, w) = (now_micros(), window());
    let mut held = SERVED.lock().unwrap();
    if held.len() >= MAX_CALLERS {
        held.retain(|_, s| s.back().is_some_and(|(t, _)| now - t <= w));
        if held.len() >= MAX_CALLERS
            && let Some(k) = held
                .iter()
                .min_by_key(|(_, s)| s.back().map_or(0, |(t, _)| *t))
                .map(|(k, _)| k.clone())
        {
            held.remove(&k);
        }
    }
    let s = held
        .entry((scope.to_string(), user.to_string()))
        .or_default();
    s.push_back((now, facts.to_vec()));
    while s.front().is_some_and(|(t, _)| now - t > w)
        || s.iter().map(|(_, f)| f.len()).sum::<usize>() > MAX_HELD
    {
        s.pop_front();
    }
}

/// Whether any facts served to `user` in `scope` wait for a check.
pub fn holds(scope: &Scope, user: &str) -> bool {
    SERVED
        .lock()
        .unwrap()
        .contains_key(&(scope.to_string(), user.to_string()))
}

/// The facts `user` was served in `scope` from `from` to `to` (µs), each once in the order first served, taken out
/// of what is held, so no session is checked twice. What was served outside it stays for its own session (or ages
/// out).
fn take(scope: &Scope, user: &str, from: i64, to: i64) -> Vec<String> {
    let mut held = SERVED.lock().unwrap();
    let key = (scope.to_string(), user.to_string());
    let Some(s) = held.get_mut(&key) else {
        return vec![];
    };
    let mut out: Vec<String> = vec![];
    for (_, facts) in s.iter().filter(|(t, _)| *t >= from && *t <= to) {
        for f in facts {
            if !out.contains(f) {
                out.push(f.clone());
            }
        }
    }
    s.retain(|(t, _)| *t < from || *t > to);
    if s.is_empty() {
        held.remove(&key);
    }
    out
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// What a check of a session came to.
#[derive(Debug, Default, PartialEq)]
pub struct Checked {
    /// Facts put to the model.
    pub asked: usize,
    /// Facts flagged.
    pub flagged: usize,
    /// Facts the model was sure are wrong ([`SURE`]): flagged where checks flag, else only logged.
    pub sure: usize,
}

/// A fact as the model is shown it.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Shown {
    pub about: String,
    pub attribute: String,
    pub value: String,
}

/// How likely the model finds it that `conversation` (each message `{"person": ..}` or `{"agent": ..}`, in order)
/// shows each of `facts` wrong, in their order: what [`check`] decides on, and what an eval of it grades.
pub async fn judge(
    d: &crate::decide::Decider,
    facts: &[Shown],
    conversation: &[Value],
) -> Result<Vec<f64>> {
    let shown: Vec<Value> = facts
        .iter()
        .enumerate()
        .map(|(i, f)| json!({"n": i + 1, "about": f.about, "attribute": f.attribute, "value": clip(&f.value, 300)}))
        .collect();
    let questions = (1..=facts.len())
        .map(|n| {
            (
                format!("f{n}"),
                crate::decide::Question::Likely(QUESTION.replace("{n}", &n.to_string())),
            )
        })
        .collect();
    let answers = d
        .decide(
            &json!({"facts": shown, "conversation": conversation}),
            questions,
        )
        .await?;
    Ok((1..=facts.len())
        .map(|n| answers.get(&format!("f{n}")).map_or(0.0, |a| a.likely()))
        .collect())
}

/// A conversation as [`judge`] reads it, from a transcript's text ([`crate::sessions::asks`]): its last
/// [`MAX_MESSAGES`] messages, each cut to [`MESSAGE_CHARS`]. Empty where the person said nothing.
pub fn conversation(transcript: &str) -> Vec<Value> {
    let asks = crate::sessions::asks(transcript);
    if asks.iter().all(|a| a.said.trim().is_empty()) {
        return vec![];
    }
    let mut out = vec![];
    for a in asks.iter().rev().take(MAX_MESSAGES / 2).rev() {
        if !a.said.trim().is_empty() {
            out.push(json!({"person": clip(&a.said, MESSAGE_CHARS)}));
        }
        if !a.reply.trim().is_empty() {
            out.push(json!({"agent": clip(&a.reply, MESSAGE_CHARS)}));
        }
    }
    out
}

/// Checks the facts served in ended session `l` (from `from`, µs: a little before its start, [`EARLY`], but not
/// before its person's previous session ended) against its transcript, and flags each the conversation shows wrong
/// as its person's `negative` feedback. Nothing (and no model call) where nothing it served is held, or the person
/// said nothing in it.
pub async fn check(
    store: &Store,
    scope: &Scope,
    d: &crate::decide::Decider,
    l: &Logged,
    from: i64,
) -> Result<Checked> {
    let Some(s) = l.session.as_ref() else {
        return Ok(Checked::default());
    };
    let Some(ended) = s.ended_at else {
        return Ok(Checked::default());
    };
    let served = take(scope, &l.asker, from, ended);
    let Some(e) = l.episodes.last().filter(|_| !served.is_empty()) else {
        return Ok(Checked::default());
    };
    // Its transcript, held for the next flush when it ended a moment ago (the sweep that ended it idle checks it).
    let body = crate::sessions::bodies(store, scope, vec![e.clone()])
        .await?
        .remove(e)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let conversation = conversation(&body);
    if conversation.is_empty() {
        return Ok(Checked::default());
    }
    // The facts as the person reads them now: one gone, or out of their reach, isn't asked about.
    let who = Principal::from_parts(l.asker.clone(), vec![]);
    let mut facts = vec![];
    for id in served.iter().take(MAX_FACTS) {
        if let Some(f) = store.fact_seen(scope, &who, id).await?
            && f.valid_to.is_none()
        {
            facts.push(f);
        }
    }
    if facts.is_empty() {
        return Ok(Checked::default());
    }
    let shown: Vec<Shown> = facts
        .iter()
        .map(|f| Shown {
            about: f.entity.clone(),
            attribute: f.predicate.clone(),
            value: f.value.clone(),
        })
        .collect();
    let likely = judge(d, &shown, &conversation).await?;
    let mode = Mode::of(store, scope).await;
    let mut done = Checked {
        asked: facts.len(),
        flagged: 0,
        sure: likely.iter().filter(|p| **p >= SURE).count(),
    };
    // Each answer, by fact id and likelihood alone (never what was said): what a shadow run is graded on.
    eprintln!(
        "served_checks {scope}: session {} mode {mode:?} answers {}",
        l.answer_id,
        facts
            .iter()
            .zip(&likely)
            .map(|(f, p)| format!("{}={p:.2}", f.fact_id))
            .collect::<Vec<_>>()
            .join(",")
    );
    if mode != Mode::On {
        return Ok(done);
    }
    let allowance = crate::feedback::MAX_PER_DAY;
    let mut given = store
        .feedback_rows_since(scope, &l.asker, now_micros() - 86_400_000_000)
        .await?;
    for (i, f) in facts.iter().enumerate() {
        if likely[i] < SURE {
            continue;
        }
        if given >= allowance {
            break;
        }
        // Names the session, never quotes it: its transcript is its person's alone, and a reason reaches the
        // memory's owners.
        let reason = clip(&format!("{REASON}{}", l.answer_id), REASON_CHARS);
        store
            .write_feedback(
                scope,
                &l.asker,
                &f.fact_id,
                "negative",
                Some(&reason),
                &f.source_episodes,
            )
            .await?;
        given += 1;
        done.flagged += 1;
    }
    Ok(done)
}
