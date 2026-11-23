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
//! - **Off by default**: `AMPUP_MEMORY_SERVED_FACT_CHECKS=on` turns it on. A flag shows in every reader's tally, so a
//!   wrong one marks a true fact for everyone; its flags are sampled before it is turned on anywhere.
//! - **What it sees**: facts served through this server's reads of the caller's own scope (a hook's recall, an
//!   agent's `search_memory`), held in memory on the server that served them for [`window`]
//!   (`AMPUP_MEMORY_SERVED_FACT_HOURS`, default 24) and dropped once checked. A restart, another server, or a fact
//!   the agent repeated from its own context without a read, is not seen. Nor, yet, is a tool result that
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

/// How sure the decisions model must be that the conversation shows a fact wrong before it is flagged.
pub const SURE: f64 = 0.85;
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

/// Whether served facts are checked: `AMPUP_MEMORY_SERVED_FACT_CHECKS=on`.
pub fn on() -> bool {
    std::env::var("AMPUP_MEMORY_SERVED_FACT_CHECKS")
        .is_ok_and(|v| matches!(v.trim(), "on" | "true" | "1"))
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
}

/// Checks the facts served in ended session `l` against its transcript, and flags each the conversation shows wrong
/// as its person's `negative` feedback. Nothing (and no model call) where nothing it served is held, or the person
/// said nothing in it.
pub async fn check(
    store: &Store,
    scope: &Scope,
    d: &crate::decide::Decider,
    l: &Logged,
) -> Result<Checked> {
    let Some(s) = l.session.as_ref() else {
        return Ok(Checked::default());
    };
    let Some(ended) = s.ended_at else {
        return Ok(Checked::default());
    };
    let served = take(scope, &l.asker, l.asked_at, ended);
    let Some(e) = l.episodes.last().filter(|_| !served.is_empty()) else {
        return Ok(Checked::default());
    };
    let body = store
        .episode_bodies(scope, vec![e.clone()])
        .await?
        .remove(e)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let asks = crate::sessions::asks(&body);
    if asks.iter().all(|a| a.said.trim().is_empty()) {
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
    let mut conversation = vec![];
    for a in asks.iter().rev().take(MAX_MESSAGES / 2).rev() {
        if !a.said.trim().is_empty() {
            conversation.push(json!({"person": clip(&a.said, MESSAGE_CHARS)}));
        }
        if !a.reply.trim().is_empty() {
            conversation.push(json!({"agent": clip(&a.reply, MESSAGE_CHARS)}));
        }
    }
    let shown: Vec<Value> = facts
        .iter()
        .enumerate()
        .map(|(i, f)| json!({"n": i + 1, "about": f.entity, "attribute": f.predicate, "value": clip(&f.value, 300)}))
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
    let mut done = Checked {
        asked: facts.len(),
        flagged: 0,
    };
    let allowance = crate::feedback::MAX_PER_DAY;
    let mut given = store
        .feedback_rows_since(scope, &l.asker, now_micros() - 86_400_000_000)
        .await?;
    for (i, f) in facts.iter().enumerate() {
        if !answers
            .get(&format!("f{}", i + 1))
            .is_some_and(|a| a.likely() >= SURE)
        {
            continue;
        }
        if given >= allowance {
            break;
        }
        // Names the session, never quotes it: its transcript is its person's alone, and a reason reaches the
        // memory's owners.
        let reason = clip(
            &format!("shown wrong in session {}", l.answer_id),
            REASON_CHARS,
        );
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
