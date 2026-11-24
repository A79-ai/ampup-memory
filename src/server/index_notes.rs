//! `POST /index/notes`: the notes `ampup-memory-client index` asks for its Memory Replay, written on this server's
//! own model account, so a person needs no model of their own to get them.
//!
//! The caller sends only the repeats (`items`: what its sessions went back for, each with its id and first sample)
//! and a model by name; the prompt is this server's ([`crate::hook::index::notes_prompt`], the same text `claude -p`
//! is given on the caller's machine), so no caller runs a prompt of its own on the server's account. At most
//! [`MAX_ITEMS`] repeats and [`MAX_BODY`] characters a call, and [`max_per_day`] calls a person a day (in this server's memory: each replica keeps its own count); metered as an
//! ask, and stopped where asking is (`crate::limits`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::Principal;
use crate::extract::Extractor;

/// The models a call may ask for, by name, and the OpenRouter model each is.
const MODELS: [(&str, &str); 2] = [
    ("haiku", "anthropic/claude-haiku-5.5"),
    ("sonnet", "anthropic/claude-sonnet-5.5"),
];

/// Each repeat's fields the prompt names, and the most characters a text one keeps.
const FIELDS: [(&str, usize); 8] = [
    ("id", 0),
    ("kind", 20),
    ("repo", 200),
    ("key", 500),
    ("sessions", 0),
    ("tokens_spent_again", 0),
    ("steps_after_it", 0),
    ("first_time", 4_000),
];

/// The most characters each field of a note answered keeps.
const NOTE_FIELDS: [(&str, usize); 6] = [
    ("type", 10),
    ("title", 200),
    ("text", 1_000),
    ("would_have_helped", 400),
    ("saves_tokens_estimate", 0),
    ("confidence", 10),
];

/// Repeats one call may ask notes for.
pub(super) const MAX_ITEMS: usize = 40;
/// The repeats' JSON, at most this many characters.
const MAX_BODY: usize = 200_000;

/// What a call asks: the repeats and the model.
pub(super) struct Ask {
    items: Vec<Value>,
    model: &'static str,
}

impl Ask {
    pub(super) fn parse(v: &Value) -> Result<Ask, String> {
        let items = v["items"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("items is a list of the repeats to write notes for")?;
        if items.len() > MAX_ITEMS {
            return Err(format!("at most {MAX_ITEMS} items a call"));
        }
        if v["items"].to_string().len() > MAX_BODY {
            return Err(format!("items are at most {MAX_BODY} characters"));
        }
        // Only the fields the prompt names, as numbers or short texts: nothing else a caller adds reaches the model.
        let items = items
            .iter()
            .enumerate()
            .map(|(n, i)| {
                let mut o = json!({"id": n});
                for (k, max) in FIELDS.iter().skip(1) {
                    match &i[*k] {
                        Value::String(t) if *max > 0 => {
                            o[*k] = json!(t.chars().take(*max).collect::<String>())
                        }
                        Value::Number(x) if *max == 0 => o[*k] = json!(x),
                        _ => {}
                    }
                }
                o
            })
            .collect();
        let name = v["model"].as_str().unwrap_or("sonnet");
        let model = MODELS
            .iter()
            .find(|(n, id)| *n == name || *id == name)
            .map(|(_, id)| *id)
            .ok_or_else(|| format!("model is haiku or sonnet, not {name}"))?;
        Ok(Ask { items, model })
    }
}

/// Calls a person may make a day: `AMPUP_MEMORY_INDEX_NOTES_PER_DAY`, default 20.
fn max_per_day() -> usize {
    std::env::var("AMPUP_MEMORY_INDEX_NOTES_PER_DAY")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(20)
}

/// Each person's calls in the last day (by person, in any scope → their times), in this server's memory.
static CALLS: LazyLock<Mutex<HashMap<String, Vec<std::time::Instant>>>> =
    LazyLock::new(Default::default);

/// Takes one of `who`'s calls of the day (in any scope), or says why there is none left.
pub(super) fn take(who: &Principal) -> Result<(), String> {
    let day = std::time::Duration::from_secs(86_400);
    let mut calls = CALLS.lock().unwrap();
    let mine = calls.entry(who.user().to_string()).or_default();
    mine.retain(|t| t.elapsed() < day);
    let max = max_per_day();
    if mine.len() >= max {
        return Err(format!(
            "{} has had index notes written {max} times today, the most a day: run index again tomorrow, or \
             write them through your own Claude Code (--notes-with claude)",
            who.user()
        ));
    }
    mine.push(std::time::Instant::now());
    Ok(())
}

/// Writes the notes and answers `{notes, model, tokens: {in, out}, cost_usd}`.
pub(super) async fn run(ask: Ask) -> Result<Value> {
    let Some(cx) = Extractor::from_env()? else {
        return Err(super::ToolError("no model is configured on this server".into()).into());
    };
    let cx = cx.with_model(ask.model);
    let prompt = format!(
        "{}\n\nAnswer with one JSON object and nothing else, of this JSON schema: {}",
        crate::hook::index::notes_prompt(&ask.items)?,
        crate::hook::index::notes_schema()
    );
    let body = json!({"messages": [{"role": "user", "content": prompt}], "max_tokens": 6000,
                      "usage": {"include": true}});
    let v = cx.chat(body).await?;
    let reply = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    // Only notes for repeats asked, each with only a note's fields, cut to size: the answer is notes, nothing else.
    let notes: Vec<Value> = json_object(reply)
        .and_then(|o| o["notes"].as_array().cloned())
        .ok_or_else(|| anyhow::anyhow!("the model answered no notes"))?
        .iter()
        .filter_map(|n| {
            let id = n["id"]
                .as_u64()
                .filter(|i| (*i as usize) < ask.items.len())?;
            let mut o = json!({"id": id});
            for (k, max) in NOTE_FIELDS {
                match &n[k] {
                    Value::String(t) if max > 0 => {
                        o[k] = json!(t.chars().take(max).collect::<String>())
                    }
                    Value::Number(x) if max == 0 => o[k] = json!(x),
                    _ => {}
                }
            }
            Some(o)
        })
        .collect();
    Ok(json!({
        "notes": notes,
        "model": ask.model,
        "tokens": {"in": v["usage"]["prompt_tokens"], "out": v["usage"]["completion_tokens"]},
        "cost_usd": v["usage"]["cost"],
    }))
}

/// The JSON object in a model's reply, which may wrap it in prose or a code fence.
fn json_object(reply: &str) -> Option<Value> {
    let (a, b) = (reply.find('{')?, reply.rfind('}')?);
    serde_json::from_str(reply.get(a..=b)?).ok()
}
