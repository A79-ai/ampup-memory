//! What the training log ([`crate::extract::train_log`]) holds, kept to what the memory still holds and turned into
//! training examples.
//!
//! - **The sweep** removes the calls that read an episode or a run the memory has since forgotten. A forgotten episode
//!   (`forget_memory`) is purged at once by the server; the others go by other paths (a person offboarded, a
//!   retention policy, a retraction, an ACL narrowed until no one the episode was shared with can read it), none of
//!   which know of the log, so the sweep asks the memory, as each of the episode's original readers, whether it can
//!   still be read, and the answer log whether the run still exists. A server with a log runs it every
//!   `AMPUP_MEMORY_TRAIN_LOG_SWEEP_SECS` (default 3600) and after the operations that remove a person's memory.
//! - **The export** writes a scope's calls as JSON lines, one training example each: `messages` (the prompt, then the
//!   reply the model should give) and `meta` (the call, what it read, the caller's outcomes, what later came of it,
//!   and a `split`). It keeps a call only if its reply was whole, usable and parsed as its role requires, its caller's
//!   checks did not error, and everything it read is still there; and, unless asked, only what every reader of the
//!   org may read (episodes shared with `*`): a model trained on a call's prompt can say what the prompt held, so a
//!   call that read a private episode or a run's trace is for a model only those readers will use.
//!
//! What later came of a call is joined here from the memory: a note's `outcome` is what the runs that were served
//! its situation after it was written were rewarded. The `split` is by what the call read (an episode, a situation),
//! so every call that read the same episode lands in the same one of train (80%), dev (10%) and test (10%).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::extract::train_log::{self, Marker};
use crate::{Scope, Store};

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Swept {
    /// Calls removed, because what they read is gone.
    pub purged: usize,
    /// Markers removed whose call was already gone.
    pub dangling: usize,
    /// Markers whose episode or run is still there.
    pub kept: usize,
    /// Markers that couldn't be checked (the memory couldn't be read): left alone.
    pub unknown: usize,
    /// Markers of another cell's scopes: that cell's servers sweep them.
    pub elsewhere: usize,
}

/// Whether anyone the episode was shared with can still read it.
async fn episode_alive(store: &Store, scope: &Scope, id: &str, acl: &[String]) -> Result<bool> {
    for who in crate::extract::audience(acl) {
        if store.episode_body(scope, &who, id).await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How many days a call that read nothing the memory can forget is kept (`AMPUP_MEMORY_TRAIN_LOG_UNTRACKED_DAYS`,
/// default 30).
fn untracked_days() -> i64 {
    std::env::var("AMPUP_MEMORY_TRAIN_LOG_UNTRACKED_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Whether the day (`YYYY-MM-DD`) a call was made is within what is kept.
fn recent(day: &str) -> bool {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map(|d| (chrono::Utc::now().date_naive() - d).num_days() < untracked_days())
        .unwrap_or(false)
}

/// The ids of the scope's answer-log entries: its runs and answers.
async fn answers(store: &Store, scope: &Scope) -> Result<HashSet<String>> {
    Ok(crate::qa_log::list(store, scope, None)
        .await?
        .into_iter()
        .map(|l| l.answer_id)
        .collect())
}

/// Removes from the log what the memory has forgotten, in `org` (every org with `None`).
pub async fn sweep(store: &Store, org: Option<&str>) -> Result<Swept> {
    let mut done = Swept::default();
    let mut answered: HashMap<Scope, Option<HashSet<String>>> = HashMap::new();
    let mut first_unknown: Option<String> = None;
    for m in train_log::markers(org).await? {
        // Another cell's memory can't be read here (`memstore_core::cell`): its own servers sweep it.
        if !crate::server::cell::mine(&m.scope) {
            done.elsewhere += 1;
            continue;
        }
        if !train_log::call_exists(&m).await? {
            train_log::purge_call(&m).await?;
            done.dangling += 1;
            continue;
        }
        let alive: Result<bool> = match m.kind.as_str() {
            "episode" => episode_alive(store, &m.scope, &m.id, &m.acl).await,
            "run" => {
                if !answered.contains_key(&m.scope) {
                    let a = answers(store, &m.scope).await.ok();
                    answered.insert(m.scope.clone(), a);
                }
                match &answered[&m.scope] {
                    Some(a) => Ok(a.contains(&m.id)),
                    None => Err(anyhow::anyhow!("the answer log can't be read")),
                }
            }
            "untracked" => Ok(recent(&m.id)),
            _ => Ok(true),
        };
        match alive {
            Ok(true) => done.kept += 1,
            Ok(false) => {
                purge(&m).await?;
                done.purged += 1;
            }
            Err(e) => {
                first_unknown.get_or_insert_with(|| format!("{} {}: {e:#}", m.kind, m.id));
                done.unknown += 1;
            }
        }
    }
    // One line a sweep, however many couldn't be checked: each is checked again by the next sweep.
    if let Some(first) = first_unknown {
        eprintln!(
            "train log sweep: {} markers couldn't be checked, left as they are (the first: {first})",
            done.unknown
        );
    }
    Ok(done)
}

async fn purge(m: &Marker) -> Result<()> {
    train_log::purge_call(m).await
}

/// Whether a call's reply is what its role's model is trained to give.
fn reply_ok(role: &str, reply: &str) -> bool {
    let json = || {
        let (a, b) = (reply.find('{'), reply.rfind('}'));
        match (a, b) {
            (Some(a), Some(b)) if a < b => serde_json::from_str::<Value>(&reply[a..=b]).ok(),
            _ => None,
        }
    };
    match role {
        "extract" | "extract_goal" => json().is_some_and(|v| {
            v["rows"].is_array() || v["facts"].is_array() || v["subjects"].is_array()
        }),
        "advice_note" => reply
            .trim()
            .trim_start_matches("```markdown")
            .trim_start_matches("```")
            .trim()
            .starts_with("# Advice"),
        "advice_route" => !reply.trim().is_empty(),
        _ => json().is_some(),
    }
}

/// The split a call that read `group` belongs to: train (80%), dev (10%), test (10%), the same every time.
fn split(group: &str) -> &'static str {
    match Sha256::digest(group.as_bytes())[0] % 10 {
        0 => "test",
        1 => "dev",
        _ => "train",
    }
}

fn micros(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_micros())
}

/// What an export did: how many examples of each role it wrote, and how many calls it left out, and why.
#[derive(Debug, Default, serde::Serialize)]
pub struct Exported {
    pub written: BTreeMap<String, usize>,
    pub left_out: BTreeMap<String, usize>,
    pub splits: BTreeMap<String, usize>,
}

/// Writes `scope`'s logged calls to `out` as training examples (see the module). `restricted`: also those that read
/// what not every reader of the org may read.
pub async fn export(
    store: &Store,
    scope: &Scope,
    restricted: bool,
    out: &mut impl Write,
) -> Result<Exported> {
    let mut report = Exported::default();
    let calls = train_log::calls(scope).await?;
    let runs = crate::advice::run_outcomes(store, scope).await?;
    let answered = answers(store, scope).await?;
    let mut episodes: HashMap<String, bool> = HashMap::new();
    for c in calls {
        let (r, role) = (&c.record, c.record["role"].as_str().unwrap_or_default());
        let mut skip = |why: &str| *report.left_out.entry(why.to_string()).or_default() += 1;
        let reply = r["reply"].as_str().unwrap_or_default();
        let prompt = r["prompt"].as_str().unwrap_or_default();
        if prompt.is_empty() || reply.is_empty() {
            skip("empty");
            continue;
        }
        // A routing call kept with its lines by key: its prompt isn't what was sent.
        if r["prompt_kept"].as_str().is_some_and(|k| k != "whole") {
            skip("prompt kept by line keys");
            continue;
        }
        if r["usable"] != true
            || !matches!(
                r["finish"].as_str(),
                Some("stop" | "end_turn" | "stop_sequence")
            )
        {
            skip("reply not whole or not usable");
            continue;
        }
        if c.verdicts.iter().any(|v| !v["error"].is_null()) {
            skip("its work errored");
            continue;
        }
        if !reply_ok(role, reply) {
            skip("reply not what the role gives");
            continue;
        }
        let refs = &r["refs"];
        let acl: Vec<String> = refs["acl"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(String::from))
            .collect();
        if let Some(e) = refs["episode_id"].as_str() {
            if !episodes.contains_key(e) {
                let alive = episode_alive(store, scope, e, &acl).await.unwrap_or(false);
                episodes.insert(e.to_string(), alive);
            }
            if !episodes[e] {
                skip("what it read is gone");
                continue;
            }
        }
        // What the prompt showed of other episodes, readable by the readers of the episode it was made for.
        let mut gone = false;
        for e in refs["also_read"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str())
        {
            if !episodes.contains_key(e) {
                let alive = episode_alive(store, scope, e, &acl).await.unwrap_or(false);
                episodes.insert(e.to_string(), alive);
            }
            gone |= !episodes[e];
        }
        if gone {
            skip("what it read is gone");
            continue;
        }
        let ids: Vec<&str> = refs["run_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(std::iter::once(&refs["answer_id"]))
            .filter_map(|v| v.as_str())
            .collect();
        if ids.iter().any(|i| !answered.contains(*i)) {
            skip("what it read is gone");
            continue;
        }
        // Read nothing the memory can forget: kept only for so long.
        if ids.is_empty()
            && refs["episode_id"].is_null()
            && !r["ts"]
                .as_str()
                .is_some_and(|t| recent(&t[..10.min(t.len())]))
        {
            skip("kept too long");
            continue;
        }
        // Open to every reader as the memory has it now, not as it was when the call was made: an ACL narrowed since
        // makes the episode no one's but its readers'. Every episode the prompt showed counts, not only the main one.
        let mut shown: Vec<&str> = refs["episode_id"].as_str().into_iter().collect();
        shown.extend(
            refs["also_read"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str()),
        );
        let mut open = !shown.is_empty() && refs["episode_id"].is_string();
        for e in shown {
            let key = format!("open:{e}");
            if !episodes.contains_key(&key) {
                let ok = episode_alive(store, scope, e, &["*".to_string()])
                    .await
                    .unwrap_or(false);
                episodes.insert(key.clone(), ok);
            }
            open &= episodes[&key];
        }
        if !restricted && !open {
            skip("read what not every reader may");
            continue;
        }
        let group = refs["episode_id"]
            .as_str()
            .or(refs["situation"].as_str())
            .or(refs["answer_id"].as_str())
            .or(r["id"].as_str())
            .unwrap_or_default();
        let outcome = if role == "advice_note" {
            let key = refs["situation"].as_str().unwrap_or_default();
            let at = r["ts"].as_str().and_then(micros).unwrap_or(i64::MAX);
            let after: Vec<f64> = runs
                .iter()
                .filter(|o| o.at > at)
                .filter(|o| {
                    o.advice
                        .as_deref()
                        .is_some_and(|a| a.split("; ").any(|k| k.trim() == key))
                })
                .map(|o| o.reward)
                .collect();
            let n = after.len();
            json!({"runs_served_after": n,
                   "mean_reward": (n > 0).then(|| after.iter().sum::<f64>() / n as f64),
                   "runs_right": after.iter().filter(|r| **r > 0.0).count()})
        } else {
            Value::Null
        };
        let s = split(group);
        let line = json!({
            "messages": [{"role": "user", "content": prompt}, {"role": "assistant", "content": reply}],
            "meta": {"id": r["id"], "role": role, "scope": r["scope"], "ts": r["ts"], "teacher": r["model"],
                     "refs": refs, "verdicts": c.verdicts, "outcome": outcome, "split": s,
                     "tokens_in": r["tokens_in"], "tokens_out": r["tokens_out"]},
        });
        writeln!(out, "{line}")?;
        *report.written.entry(role.to_string()).or_default() += 1;
        *report.splits.entry(s.to_string()).or_default() += 1;
    }
    Ok(report)
}
