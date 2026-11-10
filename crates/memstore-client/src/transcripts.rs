//! A session's whole transcript, sent as it grows (`memstore::transcripts` keeps it): what the session note and the
//! end's trace leave out, each tool call's input and its result, so a later pass can check what was noted against
//! what actually ran. Sent where the scope keeps them (asked once, then remembered for [`OFF_FOR`] where it doesn't),
//! never with `MEMSTORE_CAPTURE=off`, for Claude Code, Codex and Gemini CLI, whose transcripts carry the tool calls.
//!
//! Before a compaction, at a Stop (at most every [`super::KEEP_EVERY`]) and at the session's end, the hook sends what
//! the transcript gained since the last part the memory took (a marker keeps how far that was; without one, the
//! memory says where its last part starts), in parts of at most [`PART_MAX`]: a part the memory didn't take is sent
//! again, from the same entry, by the next hook, and replaces any earlier send of it. Each part is running text, one turn to a paragraph: each prompt, each answer, each tool
//! call with its input and each tool result, each input or result whole up to [`BLOCK_MAX`] (past that its first and
//! last [`BLOCK_END`]), a turn longer than [`UNIT_MAX`] going on in the next paragraph. Never thinking, nor a server
//! tool's encrypted content; secret-looking words redacted. A session keeps at most [`SESSION_MAX`]; past that
//! the transcript says it stops there, and the rest isn't sent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    Client, Harness, KEEP_EVERY, injected, marker, message_of, normalized, redact, repo_name,
    session_of, transcript_entries, transcript_of, warn, worth_retrying,
};

/// A tool call's input or result kept whole up to this many bytes; past it, its first and last [`BLOCK_END`].
const BLOCK_MAX: usize = 64 * 1024;
const BLOCK_END: usize = 32 * 1024;
/// Most bytes of one paragraph: under `get_episode`'s 12 KB page, so a page never cuts a turn.
const UNIT_MAX: usize = 10_000;
/// A line longer than this goes on on the next line.
const WRAP: usize = 4_000;
/// Most bytes of one part (the server takes 4 MB).
const PART_MAX: usize = 2 << 20;
/// Most bytes of one session's transcript.
const SESSION_MAX: usize = 32 << 20;
/// How long a scope that keeps none is not asked again.
const OFF_FOR: Duration = Duration::from_secs(6 * 3600);

/// When a hook sends: at once (a compaction, the session's end), or at most every [`KEEP_EVERY`] (a Stop).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Due {
    Now,
    Ending,
    Every,
}

/// Whether a marker was written within `d`.
fn fresh(m: &Path, d: Duration) -> bool {
    std::fs::metadata(m)
        .and_then(|m| m.modified())
        .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < d)
}

/// One send at a time per session: a Stop still sending when the session ends would otherwise send the same entries
/// as the end does, and the older could land last.
struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The session's send lock: waited for up to 20 s when `wait`, taken over from a copy that held it two minutes.
async fn lock(key: &str, wait: bool) -> Option<Lock> {
    let m = marker("transcriptlock", key)?;
    let started = Instant::now();
    let take = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&m)
            .is_ok()
    };
    loop {
        if take() {
            return Some(Lock(m));
        }
        if !fresh(&m, Duration::from_secs(120)) {
            let _ = std::fs::remove_file(&m);
            return take().then(|| Lock(m.clone()));
        }
        if !wait || started.elapsed() > Duration::from_secs(20) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// At most [`BLOCK_MAX`] of `text`: past it, its first and last [`BLOCK_END`] and how much was left out between.
fn capped(text: &str) -> String {
    if text.len() <= BLOCK_MAX {
        return text.to_string();
    }
    let head = &text[..text.floor_char_boundary(BLOCK_END)];
    let tail = &text[text.ceil_char_boundary(text.len() - BLOCK_END)..];
    let gone = text.len() - head.len() - tail.len();
    format!("{head}\n[… {gone} bytes left out …]\n{tail}")
}

/// A turn as paragraphs: `head`, then `text` indented two spaces (a blank line in it is two spaces, so it never ends
/// the paragraph), a line past [`WRAP`] going on on the next, the paragraph past [`UNIT_MAX`] going on in another
/// headed `(continued)`. Secret-looking words redacted.
fn paragraphs(head: &str, text: &str, out: &mut Vec<String>) {
    let mut lines = vec![];
    for line in text.lines() {
        let mut rest = line;
        loop {
            let cut = match rest.len() <= WRAP {
                true => rest.len(),
                false => rest.floor_char_boundary(WRAP),
            };
            lines.push(format!("  {}", &rest[..cut]));
            rest = &rest[cut..];
            if rest.is_empty() {
                break;
            }
        }
    }
    let mut cur = head.to_string();
    let mut held = 0;
    for l in lines {
        if held > 0 && cur.len() + 1 + l.len() > UNIT_MAX {
            out.push(redact(&cur));
            cur = format!("{} (continued)", head.trim_end_matches(':'));
            cur.push(':');
            held = 0;
        }
        cur.push('\n');
        cur.push_str(&l);
        held += 1;
    }
    out.push(redact(&cur));
}

/// Every string in `v` but a server tool's encrypted content and signatures, as JSON.
fn readable(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(readable).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "encrypted_content" | "signature"))
                .map(|(k, v)| (k.clone(), readable(v)))
                .collect(),
        ),
        v => v.clone(),
    }
}

/// A tool result's whole text: a string, or its blocks (text as it is, an image as `[image]`, anything else as JSON).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match (b["type"].as_str(), b["text"].as_str()) {
                (_, Some(t)) => t.to_string(),
                (Some("image"), _) => "[image]".into(),
                _ => readable(b).to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => readable(other).to_string(),
    }
}

/// A call's input or arguments as text: a string as it is (Codex sends its arguments as JSON text), else JSON.
fn input_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => readable(v).to_string(),
    }
}

/// Each tool call of the transcript by its id: its number in the session (from 1) and its tool's name.
fn calls_of(entries: &[Value]) -> HashMap<String, (usize, String)> {
    let mut out: HashMap<String, (usize, String)> = HashMap::new();
    let mut add = |id: &Value, name: &Value| {
        if let Some(id) = id.as_str().filter(|i| !i.is_empty()) {
            let n = out.len() + 1;
            out.entry(id.to_string())
                .or_insert((n, name.as_str().unwrap_or("a tool").to_string()));
        }
    };
    for v in entries {
        // Claude Code's assistant blocks.
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if matches!(b["type"].as_str(), Some("tool_use" | "server_tool_use")) {
                add(&b["id"], &b["name"]);
            }
        }
        // Codex's calls.
        let p = &v["payload"];
        if matches!(
            p["type"].as_str(),
            Some("function_call" | "custom_tool_call" | "local_shell_call")
        ) {
            add(&p["call_id"], &p["name"]);
        }
        // Gemini CLI's.
        for c in v["toolCalls"].as_array().into_iter().flatten() {
            add(&c["id"], &c["name"]);
        }
    }
    out
}

/// Entry `v`'s turns as paragraphs, `who` the agent's name.
fn turns(
    v: &Value,
    harness: Harness,
    who: &str,
    calls: &HashMap<String, (usize, String)>,
) -> Vec<String> {
    let mut out = vec![];
    let call = |id: &Value| {
        calls
            .get(id.as_str().unwrap_or_default())
            .cloned()
            .unwrap_or((0, "a tool".into()))
    };
    let result = |out: &mut Vec<String>, id: &Value, failed: bool, text: &str| {
        let (n, name) = call(id);
        let head = match failed {
            true => format!("Error from {name} (call {n}):"),
            false => format!("Result of {name} (call {n}):"),
        };
        paragraphs(&head, &capped(text), out);
    };
    let called = |out: &mut Vec<String>, id: &Value, input: &str| {
        let (n, name) = call(id);
        paragraphs(
            &format!("{who} called {name} (call {n}):"),
            &capped(input),
            out,
        );
    };
    if harness == Harness::ClaudeCode {
        if [&v["isMeta"], &v["isSidechain"], &v["isCompactSummary"]]
            .iter()
            .any(|f| f.as_bool() == Some(true))
        {
            return out;
        }
        let content = &v["message"]["content"];
        match (v["type"].as_str(), content) {
            (Some("user"), Value::String(s)) if !injected(s.trim()) => {
                paragraphs("User:", s.trim(), &mut out)
            }
            (Some("user"), Value::Array(blocks)) => {
                let typed: Vec<&str> = blocks
                    .iter()
                    .filter_map(|b| match b["type"].as_str() {
                        Some("text") => b["text"].as_str(),
                        Some("image") => Some("[image]"),
                        _ => None,
                    })
                    .collect();
                let text = typed.join("\n");
                if !injected(text.trim()) {
                    paragraphs("User:", text.trim(), &mut out);
                }
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    result(
                        &mut out,
                        &b["tool_use_id"],
                        b["is_error"].as_bool() == Some(true),
                        &result_text(&b["content"]),
                    );
                }
            }
            (Some("assistant"), Value::Array(blocks)) => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or_default().trim();
                            if !t.is_empty() {
                                paragraphs(&format!("{who}:"), t, &mut out);
                            }
                        }
                        Some("tool_use" | "server_tool_use") => {
                            called(&mut out, &b["id"], &input_text(&b["input"]))
                        }
                        // A server tool's answer (a web search's, the advisor's), what of it is readable.
                        Some(t) if t.ends_with("_tool_result") => {
                            let c = &b["content"];
                            let text = c["text"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| result_text(&readable(c)));
                            result(&mut out, &b["tool_use_id"], false, &text);
                        }
                        // Thinking, and whatever comes next.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        return out;
    }
    // Codex: its calls and their outputs.
    let p = &v["payload"];
    match p["type"].as_str() {
        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
            let input = match p["type"].as_str() {
                Some("function_call") => input_text(&p["arguments"]),
                Some("custom_tool_call") => input_text(&p["input"]),
                _ => input_text(&p["action"]),
            };
            called(&mut out, &p["call_id"], &input);
            return out;
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            // Often `{"output": "...", "metadata": {...}}` as text: its output read out, its metadata after.
            let raw = input_text(&p["output"]);
            let text = match serde_json::from_str::<Value>(&raw) {
                Ok(o) if o["output"].is_string() => {
                    let mut t = o["output"].as_str().unwrap_or_default().to_string();
                    if !o["metadata"].is_null() {
                        t.push_str(&format!("\n[metadata: {}]", o["metadata"]));
                    }
                    t
                }
                _ => raw,
            };
            result(&mut out, &p["call_id"], false, &text);
            return out;
        }
        Some("reasoning") => return out,
        _ => {}
    }
    // What was said (Codex's messages, Gemini CLI's), then the calls Gemini CLI made in that turn with their results.
    if let Some((role, text)) = message_of(&normalized(v.clone())) {
        let text = text.trim();
        match role {
            "user" if !injected(text) => paragraphs("User:", text, &mut out),
            "assistant" | "model" if !text.is_empty() => {
                paragraphs(&format!("{who}:"), text, &mut out)
            }
            _ => {}
        }
    }
    for c in v["toolCalls"].as_array().into_iter().flatten() {
        called(&mut out, &c["id"], &input_text(&c["args"]));
        let mut said = vec![];
        for part in c["result"].as_array().into_iter().flatten() {
            let r = &part["functionResponse"]["response"];
            match (
                part["text"].as_str(),
                r["output"].as_str(),
                r["error"].as_str(),
            ) {
                (Some(t), _, _) | (_, Some(t), _) | (_, _, Some(t)) => said.push(t.to_string()),
                _ if !r.is_null() => said.push(readable(r).to_string()),
                _ => {}
            }
        }
        if said.is_empty()
            && let Some(d) = c["resultDisplay"].as_str()
        {
            said.push(d.to_string());
        }
        let failed = c["status"].as_str() == Some("error");
        if !said.is_empty() || failed {
            result(&mut out, &c["id"], failed, &said.join("\n"));
        }
    }
    out
}

/// Whether the scope keeps them, asked before sending megabytes it would refuse: where it doesn't (a server from before
/// answers neither), `off` is marked.
async fn check(client: &Client, off: Option<&Path>, ask: &Value) -> Result<Value> {
    let v = client.post("/transcripts", ask).await?;
    if v["off"] != false
        && let Some(m) = off
    {
        let _ = std::fs::write(m, "");
    }
    Ok(v)
}

/// Sends what the session's transcript gained since the last part the memory took, in parts.
pub(super) async fn send(client: &Client, input: &Value, harness: Harness, due: Due) -> Result<()> {
    if !matches!(
        harness,
        Harness::ClaudeCode | Harness::Codex | Harness::Gemini
    ) || std::env::var("MEMSTORE_CAPTURE").unwrap_or_default() == "off"
    {
        return Ok(());
    }
    let session = session_of(input);
    if transcript_of(input).is_none_or(|p| !Path::new(p).exists()) || session.is_empty() {
        return Ok(());
    }
    // Per server and org: one that keeps none says nothing of another.
    let place: String = format!(
        "{}-{}",
        client.url,
        client.org.as_deref().unwrap_or_default()
    )
    .chars()
    .filter(char::is_ascii_alphanumeric)
    .collect();
    let off = marker("transcriptoff", &place);
    if off.as_deref().is_some_and(|m| fresh(m, OFF_FOR)) {
        return Ok(());
    }
    let key = format!("{session}-{place}");
    let Some(sent_marker) = marker("transcript", &key) else {
        return Ok(());
    };
    if due == Due::Every && fresh(&sent_marker, KEEP_EVERY) {
        return Ok(());
    }
    let Some(_lock) = lock(&key, due != Due::Every).await else {
        return Ok(());
    };
    // How far the memory took it: entries, bytes, parts, and whether it reached SESSION_MAX.
    let mut sent: Value = std::fs::read_to_string(&sent_marker)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if sent["full"] == true {
        return Ok(());
    }
    let entries = transcript_entries(input, harness);
    // A Codex session another agent started (its approval reviewer, a spawned thread): nobody typed it.
    if entries
        .iter()
        .any(|v| v["type"] == "session_meta" && v["payload"]["source"].get("subagent").is_some())
    {
        return Ok(());
    }
    let n = entries.len();
    let mut checked = false;
    if sent.get("entries").is_none() {
        // No marker (a first send, or one pruned after a week or cleaned away): the memory says how far its parts of
        // the session go, and the send starts again at its last part, replacing it. Where the person forgot a part
        // after it (or one aged out), none of what the transcript holds now is sent: only what it gains from here.
        let ask = json!({"check": true, "harness": harness.name(), "session_id": session});
        let v = check(client, off.as_deref(), &ask).await?;
        if v["off"] != false {
            return Ok(());
        }
        checked = true;
        if let Some(f) = v["from"].as_u64() {
            sent["entries"] = json!(f);
            sent["bytes"] = v["bytes"].clone();
        } else if v["forgotten"] == true {
            sent["entries"] = json!(n);
            sent["bytes"] = v["bytes"].clone();
            let _ = std::fs::write(&sent_marker, sent.to_string());
            return Ok(());
        }
    }
    // A transcript shorter than what was sent was written anew: sent again from its start, each part replacing the
    // one sent from the same entry.
    let mut from = (sent["entries"].as_u64().unwrap_or(0) as usize).min(n);
    if (sent["entries"].as_u64().unwrap_or(0) as usize) > n {
        from = 0;
    }
    if from >= n {
        return Ok(());
    }
    let who = match harness {
        Harness::ClaudeCode => "Claude",
        h => h.label(),
    };
    let calls = calls_of(&entries);
    let rendered: Vec<Vec<String>> = entries[from..]
        .iter()
        .map(|v| turns(v, harness, who, &calls))
        .collect();
    if rendered.iter().all(Vec::is_empty) {
        // Nothing to send (lines that aren't turns): past them.
        sent["entries"] = json!(n);
        let _ = std::fs::write(&sent_marker, sent.to_string());
        return Ok(());
    }
    if !checked && check(client, off.as_deref(), &json!({"check": true})).await?["off"] != false {
        return Ok(());
    }
    let repo: String = repo_name(input)
        .chars()
        .map(|c| match c {
            ':' | '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let mut at = 0;
    while at < rendered.len() {
        let mut bytes = sent["bytes"].as_u64().unwrap_or(0) as usize;
        let start = at;
        let mut units: Vec<&String> = vec![];
        let mut size = 0;
        let mut full = false;
        while at < rendered.len() {
            let s: usize = rendered[at].iter().map(|p| p.len() + 2).sum();
            if bytes + size + s > SESSION_MAX {
                full = true;
                break;
            }
            if !units.is_empty() && size + s > PART_MAX {
                break;
            }
            units.extend(&rendered[at]);
            size += s;
            at += 1;
        }
        let stop = format!(
            "The transcript stops here: it reached {} MB, the most a session keeps.",
            SESSION_MAX >> 20
        );
        let head = format!(
            "{} session {session} in {repo}: its transcript from entry {}. Each tool call's input and result is \
             whole up to {} KB (past that, its first and last {} KB); thinking is left out and secret-looking \
             words are redacted.",
            harness.label(),
            from + start,
            BLOCK_MAX >> 10,
            BLOCK_END >> 10
        );
        let mut text = std::iter::once(&head)
            .chain(units.iter().copied())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        if full {
            text.push_str(&format!("\n\n{stop}"));
        }
        if units.is_empty() && !full {
            continue;
        }
        let body = json!({"harness": harness.name(), "repo": repo, "session_id": session,
                          "from": from + start, "to": from + at, "text": text});
        match client.post("/transcripts", &body).await {
            Ok(v) if v["kept"] == true => {
                bytes += text.len();
                sent["entries"] = json!(from + at);
                sent["bytes"] = json!(bytes);
                sent["parts"] = json!(sent["parts"].as_u64().unwrap_or(0) + 1);
                if full {
                    sent["full"] = json!(true);
                }
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
            // A part the person forgot isn't kept again: past it.
            Ok(v) if v["forgotten"] == true => {
                sent["entries"] = json!(from + at);
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
            Ok(v) => {
                if v["off"] == true
                    && let Some(m) = &off
                {
                    let _ = std::fs::write(m, "");
                }
                return Ok(());
            }
            // Unreached: the next hook sends it again, or, at the session's end, the outbox does.
            Err(e) if worth_retrying(&e) => {
                if due == Due::Ending {
                    client.queue("transcript", session, "/transcripts", body, None);
                }
                return Err(e.context("the session's transcript waits to be sent"));
            }
            // Refused: it would be refused again, so it isn't sent again.
            Err(e) => {
                warn(format!(
                    "transcript {session}: entries {} to {} weren't kept: {e:#}",
                    from + start,
                    from + at
                ));
                sent["entries"] = json!(from + at);
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
        }
        if full {
            break;
        }
    }
    Ok(())
}
