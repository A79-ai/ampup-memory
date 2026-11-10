//! A Claude Code session's advice traces, sent from the Stop hook (`memstore::advice_traces` keeps them): the
//! transcript of the session and of each of its subagents that asked for advice, with a record of each ask: a call
//! to the advisor (`server_tool_use` named `advisor`), to memstore's `get_advice` (an MCP tool named
//! `…__get_advice`), or advice memstore's own hooks handed the agent (a `hook_additional_context` attachment
//! beginning [`super::ADVICE_LEAD`]). Each Stop that finds an answered call not sent yet sends a part of the
//! session's one trace: what each transcript gained since the last part (its new lines and calls); only where the org keeps them (asked once, then remembered for
//! [`OFF_FOR`] where it doesn't), never with `AMPUP_MEMORY_CAPTURE=off`.
//!
//! What leaves the machine: the user and assistant lines, their text, tool calls and tool results (each cut to
//! [`RESULT_CAP`] characters) with secret-looking words redacted, and the advice the hooks handed on; of every
//! other line (other attachments, system lines) only its place in the conversation (`uuid`, `parentUuid`), so a
//! call's conversation can be followed back through it. Never thinking, nor an advisor's encrypted reply (only
//! whether it answered).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    ADVICE_LEAD, Client, Harness, marker, redact, result_text, session_of, transcript_of, warn,
    worth_retrying,
};

/// Longest text of one tool call's input or result sent.
const RESULT_CAP: usize = 8_000;
/// Smaller caps tried when a transcript is too big to send at [`RESULT_CAP`].
const SMALLER_CAPS: [usize; 2] = [2_000, 500];
/// Largest request sent (the server takes 16 MB).
const MAX_SEND: usize = 14 << 20;
/// How long an org that keeps none is not asked again.
const OFF_FOR: Duration = Duration::from_secs(6 * 3600);
/// Most lines and calls one part holds (`memstore::advice_traces`).
const MAX_LINES: usize = 50_000;
const MAX_CALLS: usize = 2_000;
/// Longest `get_advice` answer kept with its call.
const ADVICE_CAP: usize = 20_000;

fn capped(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}\n[… cut]")
}

/// Whether a tool is memstore's `get_advice` (by any of its MCP servers' names).
fn is_get_advice(name: &str) -> bool {
    name == "get_advice" || name.ends_with("__get_advice")
}

/// The transcripts of the session: its own and its subagents' (`<session>/subagents/*.jsonl` beside it).
fn transcripts(main: &Path) -> Vec<PathBuf> {
    let mut out = vec![main.to_path_buf()];
    if let (Some(dir), Some(stem)) = (main.parent(), main.file_stem()) {
        let subs = dir.join(stem).join("subagents");
        if let Ok(rd) = std::fs::read_dir(subs) {
            let mut found: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .collect();
            found.sort();
            out.extend(found);
        }
    }
    out
}

/// One content block as sent: thinking and encrypted replies dropped, tool input and results capped, text
/// redacted.
fn block(b: &Value, cap: usize) -> Option<Value> {
    let t = b["type"].as_str()?;
    Some(match t {
        "text" => json!({"type": "text", "text": redact(b["text"].as_str().unwrap_or_default())}),
        "tool_use" | "server_tool_use" => {
            let input = b["input"].to_string();
            let input = if input.chars().count() > cap {
                json!(redact(&capped(&input, cap)))
            } else {
                serde_json::from_str::<Value>(&redact(&input)).unwrap_or(json!(null))
            };
            json!({"type": t, "id": b["id"], "name": b["name"], "input": input})
        }
        "tool_result" => json!({
            "type": "tool_result",
            "tool_use_id": b["tool_use_id"],
            "is_error": b["is_error"],
            "content": redact(&capped(&result_text(&b["content"]), cap)),
        }),
        "advisor_tool_result" => {
            let c = &b["content"];
            let mut kept = json!({"type": c["type"]});
            if c["type"] == "advisor_result" {
                kept["text"] = json!(redact(c["text"].as_str().unwrap_or_default()));
            }
            if let Some(e) = c.get("error_code") {
                kept["error_code"] = e.clone();
            }
            json!({"type": t, "tool_use_id": b["tool_use_id"], "content": kept})
        }
        "image" => json!({"type": "image"}),
        _ => return None, // thinking, redacted_thinking and whatever comes next
    })
}

/// The advice memstore's hooks handed on in attachment line `v`, if it is one.
fn hook_advice(v: &Value) -> Option<String> {
    let a = &v["attachment"];
    if v["type"] != "attachment" || a["type"] != "hook_additional_context" {
        return None;
    }
    let text = match &a["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    text.contains(ADVICE_LEAD).then_some(text)
}

/// A transcript's lines as sent: every line in the conversation, the user and assistant lines and the hooks'
/// advice whole (as [`block`] keeps them), any other only by its place.
fn lines(entries: &[Value], cap: usize) -> Vec<Value> {
    entries
        .iter()
        .filter(|v| v["uuid"].is_string())
        .map(|v| {
            if let Some(advice) = hook_advice(v) {
                return json!({
                    "type": "attachment", "uuid": v["uuid"], "parentUuid": v["parentUuid"],
                    "isSidechain": v["isSidechain"], "timestamp": v["timestamp"],
                    "attachment": {"type": "hook_additional_context", "hookEvent": v["attachment"]["hookEvent"],
                                   "content": redact(&capped(&advice, ADVICE_CAP))},
                });
            }
            if v["type"] != "user" && v["type"] != "assistant" {
                return json!({"type": v["type"], "uuid": v["uuid"], "parentUuid": v["parentUuid"]});
            }
            let m = &v["message"];
            let content = match &m["content"] {
                Value::String(s) => json!(redact(s)),
                Value::Array(bs) => {
                    Value::Array(bs.iter().filter_map(|b| block(b, cap)).collect())
                }
                _ => json!(null),
            };
            json!({
                "type": v["type"], "uuid": v["uuid"], "parentUuid": v["parentUuid"],
                "isSidechain": v["isSidechain"], "isMeta": v["isMeta"], "timestamp": v["timestamp"],
                "message": {"role": m["role"], "model": m["model"], "content": content},
            })
        })
        .collect()
}

/// Each answered call in `entries`: its record, with the line it is on and the line its answer is on.
fn calls(entries: &[Value]) -> Vec<Value> {
    // Each answer by the id of the call it answers: (its line's uuid, the block).
    let mut answers: std::collections::HashMap<&str, (&Value, &Value)> = Default::default();
    for v in entries {
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if matches!(
                b["type"].as_str(),
                Some("advisor_tool_result" | "tool_result")
            ) && let Some(id) = b["tool_use_id"].as_str()
            {
                answers.insert(id, (&v["uuid"], b));
            }
        }
    }
    let mut out = vec![];
    for v in entries {
        if let Some(advice) = hook_advice(v) {
            out.push(json!({
                "kind": "hook_advice", "call_id": v["uuid"], "line_uuid": v["uuid"], "result_uuid": v["uuid"],
                "timestamp": v["timestamp"], "hook_event": v["attachment"]["hookEvent"], "status": "ok",
                "is_sidechain": v["isSidechain"], "advice": redact(&capped(&advice, ADVICE_CAP)),
            }));
        }
    }
    for v in entries.iter().filter(|v| v["type"] == "assistant") {
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            let name = b["name"].as_str().unwrap_or_default();
            let kind = match b["type"].as_str() {
                Some("server_tool_use") if name == "advisor" => "advisor",
                Some("tool_use") if is_get_advice(name) => "get_advice",
                _ => continue,
            };
            let Some(id) = b["id"].as_str() else { continue };
            let Some((at, answer)) = answers.get(id) else {
                continue; // not answered yet: the next Stop sends it
            };
            let mut c = json!({
                "kind": kind, "call_id": id, "line_uuid": v["uuid"], "result_uuid": at,
                "timestamp": v["timestamp"], "model": v["message"]["model"],
                "advisor_model": v["advisorModel"], "is_sidechain": v["isSidechain"],
            });
            if kind == "advisor" {
                let t = answer["content"]["type"].as_str().unwrap_or_default();
                c["status"] = json!(match t {
                    "advisor_result" => "ok",
                    "advisor_redacted_result" => "encrypted",
                    _ => "error",
                });
                if t == "advisor_result" {
                    c["advice"] = json!(redact(
                        answer["content"]["text"].as_str().unwrap_or_default()
                    ));
                }
            } else {
                c["input"] = json!(redact(&b["input"].to_string()));
                c["status"] = json!(if answer["is_error"] == true {
                    "error"
                } else {
                    "ok"
                });
                c["advice"] = json!(redact(&capped(
                    &result_text(&answer["content"]),
                    ADVICE_CAP
                )));
            }
            out.push(c);
        }
    }
    out
}

/// Whether a marker was written within `d`.
fn fresh(m: &Path, d: Duration) -> bool {
    std::fs::metadata(m)
        .and_then(|m| m.modified())
        .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < d)
}

/// One transcript's share of a part: its name, how many lines it has, its lines and calls not sent yet.
type Part = (String, usize, Vec<Value>, Vec<Value>);

/// Sends a part of the session's trace: what its transcripts gained since the last part, once they hold an answered
/// call not sent yet.
pub(super) async fn send(client: &Client, input: &Value, harness: Harness) -> Result<()> {
    if harness != Harness::ClaudeCode
        || std::env::var("AMPUP_MEMORY_CAPTURE").unwrap_or_default() == "off"
    {
        return Ok(());
    }
    let Some(main) = transcript_of(input).map(PathBuf::from) else {
        return Ok(());
    };
    // Per server and org: one org that keeps none says nothing of another.
    let place: String = format!(
        "{}-{}",
        client.url,
        client.org.as_deref().unwrap_or_default()
    )
    .chars()
    .filter(char::is_ascii_alphanumeric)
    .collect();
    let off = marker("advice-traces-off", &place);
    if off.as_deref().is_some_and(|m| fresh(m, OFF_FOR)) {
        return Ok(());
    }
    let session = session_of(input);
    // Every transcript of the session holding an ask, the session's own first: sent together as its one trace.
    let mut found_files = vec![];
    for path in transcripts(&main) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Most transcripts call neither: no parse.
        if !text.contains("\"name\":\"advisor\"")
            && !text.contains("get_advice\"")
            && !text.contains(ADVICE_LEAD)
        {
            continue;
        }
        let entries: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let found = calls(&entries);
        if found.is_empty() {
            continue;
        }
        let file = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        found_files.push((file, entries, found));
    }
    // What earlier parts sent, per transcript: how many of its lines, and which calls.
    let sent = marker("advice-traces", &format!("{session}-{place}"));
    let mut had: Value = sent
        .as_ref()
        .and_then(|m| std::fs::read_to_string(m).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let id = |c: &Value| c["call_id"].as_str().unwrap_or_default().to_string();
    // This part: per transcript, the lines and calls not sent yet.
    let mut parts: Vec<Part> = found_files
        .into_iter()
        .map(|(file, entries, found)| {
            let from = had[&file]["lines"].as_u64().unwrap_or(0) as usize;
            let from = from.min(entries.len());
            let old: Vec<&str> = had[&file]["calls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let new: Vec<Value> = found
                .into_iter()
                .filter(|c| !old.contains(&id(c).as_str()))
                .collect();
            let n = entries.len();
            (file, n, entries.into_iter().skip(from).collect(), new)
        })
        .collect();
    if parts.iter().all(|(_, _, _, c)| c.is_empty()) {
        return Ok(());
    }
    let mark = |had: &mut Value, parts: &[Part]| {
        for (file, n, _, calls) in parts {
            let f = &mut had[file.as_str()];
            f["lines"] = json!(n);
            let mut ids: Vec<Value> = f["calls"].as_array().cloned().unwrap_or_default();
            ids.extend(calls.iter().map(|c| json!(id(c))));
            f["calls"] = json!(ids);
        }
        if let Some(m) = &sent {
            let _ = std::fs::write(m, had.to_string());
        }
    };
    // Within the server's bounds: the session's own transcript first, then its subagents' while they fit; what
    // doesn't fit is never sent.
    let (mut lines_n, mut calls_n) = (0, 0);
    let mut over = vec![];
    for p in std::mem::take(&mut parts) {
        if lines_n + p.2.len() <= MAX_LINES && calls_n + p.3.len() <= MAX_CALLS {
            (lines_n, calls_n) = (lines_n + p.2.len(), calls_n + p.3.len());
            parts.push(p);
        } else {
            over.push(p);
        }
    }
    // Whether the org keeps them, before sending megabytes it would refuse.
    let v = client
        .post("/advice-traces", &json!({"check": true}))
        .await?;
    if v["off"] == true {
        if let Some(m) = &off {
            let _ = std::fs::write(m, "");
        }
        return Ok(());
    }
    // Smaller tool results first; still too big, the subagents' transcripts from the last, never the session's own.
    let mut body = None;
    while body.is_none() && !parts.is_empty() {
        for cap in std::iter::once(RESULT_CAP).chain(SMALLER_CAPS) {
            let files: Vec<Value> = parts
                .iter()
                .filter(|(_, _, l, c)| !l.is_empty() || !c.is_empty())
                .map(|(file, _, entries, calls)| {
                    json!({"file": file, "lines": lines(entries, cap), "calls": calls})
                })
                .collect();
            let b = json!({"session_id": session, "harness": "claude_code", "files": files});
            if b.to_string().len() <= MAX_SEND {
                body = Some(b);
                break;
            }
        }
        if body.is_none() {
            if parts.len() == 1 {
                break;
            }
            over.extend(parts.pop());
        }
    }
    if !over.is_empty() {
        warn(format!(
            "advice trace {session}: {} of its transcripts are too big to send",
            over.len()
        ));
        mark(&mut had, &over);
    }
    let Some(body) = body else {
        mark(&mut had, &parts);
        return Ok(());
    };
    if body["files"].as_array().is_none_or(|f| {
        f.iter()
            .all(|f| f["calls"].as_array().is_none_or(Vec::is_empty))
    }) {
        // Only the new calls that didn't fit: nothing left to send.
        mark(&mut had, &parts);
        return Ok(());
    }
    let v = match client.post("/advice-traces", &body).await {
        Ok(v) => v,
        // Unreached, or past the day's most: the next Stop sends it again. Refused: it would be refused again, so
        // it isn't.
        Err(e) => {
            if !worth_retrying(&e) {
                mark(&mut had, &parts);
            }
            warn(format!("advice trace {session} wasn't sent: {e:#}"));
            return Ok(());
        }
    };
    if v["off"] == true {
        if let Some(m) = &off {
            let _ = std::fs::write(m, "");
        }
        return Ok(());
    }
    if v["kept"] == true {
        mark(&mut had, &parts);
    }
    Ok(())
}
