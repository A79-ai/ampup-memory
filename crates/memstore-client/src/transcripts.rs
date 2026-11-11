//! A session's whole transcript, sent as it grows (`memstore::transcripts` keeps it): what the session note and the
//! end's trace leave out, each tool call's input and its result, so a later pass can check what was noted against
//! what actually ran. Sent where the scope keeps them (asked once, then remembered for [`OFF_FOR`] where it doesn't),
//! never with `AMPUP_MEMORY_CAPTURE=off`, for Claude Code, Codex and Gemini CLI, whose transcripts carry the tool calls.
//!
//! Before a compaction, at a Stop (at most every [`super::KEEP_EVERY`]) and at the session's end, the hook sends what
//! the transcript gained since the last part the memory took (a marker keeps how far that was; without one, the
//! memory says where its last part starts), in parts of at most [`PART_MAX`]: a part the memory didn't take is sent
//! again, from the same entry, by the next hook, and replaces any earlier send of it. Each part is running text, one
//! turn to a paragraph: each prompt, each answer, each tool
//! call with its input and each tool result, each input or result whole up to [`BLOCK_MAX`] (past that its first and
//! last [`BLOCK_END`]), a turn longer than [`UNIT_MAX`] going on in the next paragraph, an entry longer than a part
//! cut to fit one ([`fitted`]). Never thinking, nor a server tool's encrypted content; secret-looking words
//! redacted. Nor what memstore itself holds ([`left_out`]): its tools' answers and what its hooks added are each a
//! reference to the episodes and facts they named, so no copy of them outlives the original or its access rules. A
//! session keeps at most [`SESSION_MAX`]; past that the transcript says it stops there, and the rest isn't sent.
//!
//! Where the memory keeps none for this caller (the scope's setting off; a key, a reader or a session's branch where
//! it is on; a server from before transcripts), the hook marks it and doesn't ask again for [`OFF_FOR`], saying so
//! once. At the session's end, what the memory couldn't take now waits in the outbox, every part of it.

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
/// Most bytes of one entry's turns: a part's, less room for its heading and its end.
const ENTRY_MAX: usize = PART_MAX - (16 << 10);
/// How long a scope that keeps none is not asked again.
const OFF_FOR: Duration = Duration::from_secs(6 * 3600);
/// Most episodes, or facts, a reference to what memstore said names.
const NAMED_MAX: usize = 40;

/// memstore's own tools, by their own names: an MCP host calls them `mcp__<server>__<tool>` (Claude Code, Codex), or
/// by the name alone (Gemini CLI).
const MEMORY_TOOLS: [&str; 20] = [
    "search_memory",
    "get_episode",
    "get_brief",
    "list_briefs",
    "start_session",
    "get_advice",
    "end_session",
    "remember_memory",
    "forget_memory",
    "query_memory",
    "ask_memory",
    "get_entity",
    "list_facts",
    "list_advice",
    "feedback_memory",
    "close_memory",
    "log_answer",
    "record_run",
    "reward_run",
    "skills",
];

/// How each part of what memstore's hooks add to a session begins (recalled facts, advice, the notes a session
/// starts with, under this name or the one before it), on a line of its own.
const INJECTED: [&str; 4] = [
    "From the team memory (",
    "Advice from the team memory (",
    "What this user told you before (",
    "What the team shares (",
];

/// The heading of the paragraph that stands for what memstore's hooks added: a tool's result, as a later pass reads
/// a transcript (`notes_dream`), never the person's words.
const HOOKS_HEAD: &str = "Result of AmpUp Memory's hooks";
const ANSWER: &str = "AmpUp Memory's answer";
const ADDED: &str = "what AmpUp Memory's hooks added";

/// Whether `name` is one of memstore's own tools ([`MEMORY_TOOLS`]), under any server's name.
fn memory_tool(name: &str) -> bool {
    MEMORY_TOOLS.iter().any(|t| {
        name == *t
            || name
                .strip_suffix(t)
                .is_some_and(|server| server.ends_with("__"))
    })
}

/// Whether a turn's heading (`Result of <tool> (call n)`, `Error from <tool> (call n)`) is a result of one of
/// memstore's tools.
fn memory_result(turn: &str) -> bool {
    turn.strip_prefix("Result of ")
        .or_else(|| turn.strip_prefix("Error from "))
        .and_then(|r| r.rsplit_once(" (call "))
        .is_some_and(|(tool, _)| memory_tool(tool))
}

/// Where in `text` what memstore's hooks added begins: the start of the first line holding one of [`INJECTED`].
fn injected_at(text: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if INJECTED.iter().any(|l| line.contains(l)) {
            return Some(at);
        }
        at += line.len();
    }
    None
}

/// The episodes (64 hex digits) and facts (UUIDs) some of memstore's text named, each once, in order.
#[derive(Default)]
struct Named {
    episodes: Vec<String>,
    facts: Vec<String>,
}

impl Named {
    fn add(&mut self, text: &str) {
        let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
        for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
            let list = if w.len() == 64 && hex(w) {
                &mut self.episodes
            } else if w.len() == 36
                && w.split('-').map(str::len).eq([8, 4, 4, 4, 12])
                && hex(&w.replace('-', ""))
            {
                &mut self.facts
            } else {
                continue;
            };
            let w = w.to_ascii_lowercase();
            if !list.contains(&w) {
                list.push(w);
            }
        }
    }

    /// An indented line standing for `what`, naming what it named.
    fn reference(&self, what: &str) -> String {
        let list = |ids: &[String]| {
            let shown = ids[..ids.len().min(NAMED_MAX)].join(", ");
            match ids.len() > NAMED_MAX {
                true => format!("{shown} and {} more", ids.len() - NAMED_MAX),
                false => shown,
            }
        };
        let mut s =
            format!("  [left out: {what}, which stays in AmpUp Memory under its own access rules");
        if !self.episodes.is_empty() {
            s.push_str(&format!("; episodes {}", list(&self.episodes)));
        }
        if !self.facts.is_empty() {
            s.push_str(&format!("; facts {}", list(&self.facts)));
        }
        s.push(']');
        s
    }
}

/// `paragraphs` (an entry's turns, or a whole part's) with what memstore itself holds left out, so no copy of it
/// outlives the original's forget, retention or offboarding, or its reader's access to it: each result of one of its
/// tools ([`MEMORY_TOOLS`]; the call and its input stay) becomes a reference to the episodes and facts it named, and
/// in a turn said to be the person's (`User:`), what its hooks added, from the first line [`INJECTED`] finds to the
/// turn's end, becomes a paragraph of its own ([`HOOKS_HEAD`]) with the same kind of reference. What the person typed
/// and what other tools returned stay as they are.
fn left_out(paragraphs: Vec<String>) -> Vec<String> {
    // The reference being gathered: its heading, the turn whose `(continued)` paragraphs go into it, what it stands
    // for and what that named.
    type Open = Option<(String, String, &'static str, Named)>;
    fn close(open: &mut Open, out: &mut Vec<String>) {
        if let Some((head, _, what, named)) = open.take() {
            out.push(format!("{head}:\n{}", named.reference(what)));
        }
    }
    let mut out = vec![];
    let mut open: Open = None;
    for p in paragraphs {
        let (line, body) = p.split_once('\n').unwrap_or((p.as_str(), ""));
        let continued = line.ends_with(" (continued):");
        let turn = line
            .strip_suffix(':')
            .map(|h| h.strip_suffix(" (continued)").unwrap_or(h));
        if let (true, Some(t), Some((_, of, _, named))) = (continued, turn, open.as_mut())
            && t == of
        {
            named.add(body);
            continue;
        }
        close(&mut open, &mut out);
        let added_at = match turn {
            Some("User") => injected_at(body),
            _ => None,
        };
        match (turn, added_at) {
            (Some(t), _) if memory_result(t) => {
                let mut named = Named::default();
                named.add(body);
                open = Some((t.to_string(), t.to_string(), ANSWER, named));
            }
            (_, Some(at)) => {
                let (typed, added) = body.split_at(at);
                if !typed.trim().is_empty() {
                    out.push(format!("{line}\n{}", typed.trim_end()));
                }
                let mut named = Named::default();
                named.add(added);
                open = Some((HOOKS_HEAD.to_string(), "User".into(), ADDED, named));
            }
            _ => out.push(p),
        }
    }
    close(&mut open, &mut out);
    out
}

/// A part's `text` as a hook from before [`left_out`] sent it, with what memstore holds left out: the server keeps
/// every part this way, so an older hook's copies are references too.
pub fn without_memory(text: &str) -> String {
    left_out(text.split("\n\n").map(str::to_string).collect()).join("\n\n")
}

/// An entry's paragraphs within `max` bytes (each counted with the blank line after it): whole where they fit, else
/// those that fit and the next cut where it reaches `max`, with a line saying how much of the entry was left out.
/// So an entry longer than a part (a prompt pasted whole, a turn of many results) is kept cut, not refused.
fn fitted(paragraphs: Vec<String>, max: usize) -> Vec<String> {
    let total: usize = paragraphs.iter().map(|p| p.len() + 2).sum();
    if total <= max {
        return paragraphs;
    }
    let room = max.saturating_sub(256);
    let mut out = vec![];
    let mut used = 0;
    for p in paragraphs {
        if used + p.len() + 2 <= room {
            used += p.len() + 2;
            out.push(p);
            continue;
        }
        let cut = &p[..p.floor_char_boundary(room.saturating_sub(used + 2))];
        let (kept, head) = match cut.rfind('\n') {
            Some(i) => (&cut[..i], true),
            None => ("", false),
        };
        let gone = total - used - kept.len();
        let said =
            format!("[… {gone} bytes of this entry left out: it is longer than a part holds …]");
        out.push(match head {
            true => format!("{kept}\n  {said}"),
            false => said,
        });
        break;
    }
    out
}

/// Whether the memory answered that it keeps none for this caller here: refused (403: a key, a reader, a session's
/// branch, where the scope keeps them), or no route for them (404 or 405: a server from before transcripts).
fn not_here(e: &anyhow::Error) -> bool {
    let e = format!("{e:#}");
    [": 403", ": 404", ": 405"].iter().any(|c| e.contains(c))
}

/// Marks that the memory keeps none for this caller here, so it isn't asked again for [`OFF_FOR`]: `refused`, the
/// error that said so, else the scope keeps none. A refusal of a caller where the scope keeps them is said once,
/// when first marked, never at every hook.
fn mark_off(off: Option<&Path>, refused: Option<&anyhow::Error>) {
    let Some(m) = off else {
        return;
    };
    let why = refused.map_or(String::new(), |e| {
        match format!("{e:#}").contains(": 403") {
            true => "refused".into(),
            false => "no route".into(),
        }
    });
    if why == "refused"
        && std::fs::read_to_string(m).ok().as_deref() != Some("refused")
        && let Some(e) = refused
    {
        warn(format!(
            "transcripts are on here but aren't kept for this sign-in, so none is sent (asked again in {} hours): {e:#}",
            OFF_FOR.as_secs() / 3600
        ));
    }
    let _ = std::fs::write(m, why);
}

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
        // What a hook added to the session: memstore's hooks', a reference to what it named ([`left_out`]); any other
        // hook's isn't part of the conversation.
        if v["type"] == "attachment" {
            let a = &v["attachment"];
            let text = match &a["content"] {
                Value::Array(xs) => xs
                    .iter()
                    .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join("\n"),
                c => c.as_str().map_or_else(|| c.to_string(), str::to_string),
            };
            if a["type"] == "hook_additional_context"
                && let Some(at) = injected_at(&text)
            {
                let mut named = Named::default();
                named.add(&text[at..]);
                let event = a["hookEvent"].as_str().unwrap_or("a hook");
                out.push(format!(
                    "{HOOKS_HEAD} ({event}):\n{}",
                    named.reference(ADDED)
                ));
            }
            return out;
        }
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

/// Whether the scope keeps them, asked before sending megabytes it would refuse: where it keeps none for this caller
/// (off, a refusal, or no route: [`not_here`]), `off` is marked ([`mark_off`]) and the answer is `off`.
async fn check(client: &Client, off: Option<&Path>, ask: &Value) -> Result<Value> {
    match client.post("/transcripts", ask).await {
        Ok(v) => {
            if v["off"] != false {
                mark_off(off, None);
            }
            Ok(v)
        }
        Err(e) if not_here(&e) => {
            mark_off(off, Some(&e));
            Ok(json!({"kept": false, "off": true}))
        }
        Err(e) => Err(e),
    }
}

/// Sends what the session's transcript gained since the last part the memory took, in parts.
pub(super) async fn send(client: &Client, input: &Value, harness: Harness, due: Due) -> Result<()> {
    if !matches!(
        harness,
        Harness::ClaudeCode | Harness::Codex | Harness::Gemini
    ) || std::env::var("AMPUP_MEMORY_CAPTURE").unwrap_or_default() == "off"
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
        .map(|v| fitted(left_out(turns(v, harness, who, &calls)), ENTRY_MAX))
        .collect();
    if rendered.iter().all(Vec::is_empty) {
        // Nothing to send (lines that aren't turns): past them.
        sent["entries"] = json!(n);
        let _ = std::fs::write(&sent_marker, sent.to_string());
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
    // The parts, each at most PART_MAX: what the session gained from `from` on, as far as SESSION_MAX lets it.
    struct Part {
        start: usize,
        end: usize,
        text: String,
        full: bool,
    }
    let mut parts: Vec<Part> = vec![];
    let mut bytes = sent["bytes"].as_u64().unwrap_or(0) as usize;
    let mut at = 0;
    while at < rendered.len() {
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
        if units.is_empty() && !full {
            continue;
        }
        let head = format!(
            "{} session {session} in {repo}: its transcript from entry {}. Each tool call's input and result is \
             whole up to {} KB (past that, its first and last {} KB); thinking is left out, and so is what AmpUp \
             Memory's tools returned and its hooks added (each a reference to what it named); secret-looking words \
             are redacted.",
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
            text.push_str(&format!(
                "\n\nThe transcript stops here: it reached {} MB, the most a session keeps.",
                SESSION_MAX >> 20
            ));
        }
        bytes += text.len();
        parts.push(Part {
            start,
            end: at,
            text,
            full,
        });
        if full {
            break;
        }
    }
    let body = |p: &Part| {
        json!({"harness": harness.name(), "repo": repo, "session_id": session, "from": from + p.start,
               "to": from + p.end, "text": p.text})
    };
    // Unreached at the session's end: every part not yet kept waits in the outbox, each under its own name, for the
    // next hook that sends what waits there (the next hook sends it again otherwise).
    let unreached = |e: anyhow::Error, rest: &[Part]| {
        if due == Due::Ending {
            for p in rest {
                let name = format!("{session}-{:08}", from + p.start);
                client.queue("transcript", &name, "/transcripts", body(p), None);
            }
        }
        e.context("the session's transcript waits to be sent")
    };
    if !checked {
        match check(client, off.as_deref(), &json!({"check": true})).await {
            Ok(v) if v["off"] != false => return Ok(()),
            Ok(_) => {}
            Err(e) if worth_retrying(&e) => return Err(unreached(e, &parts)),
            Err(e) => return Err(e),
        }
    }
    for (i, p) in parts.iter().enumerate() {
        match client.post("/transcripts", &body(p)).await {
            Ok(v) if v["kept"] == true => {
                sent["entries"] = json!(from + p.end);
                sent["bytes"] = json!(sent["bytes"].as_u64().unwrap_or(0) as usize + p.text.len());
                sent["parts"] = json!(sent["parts"].as_u64().unwrap_or(0) + 1);
                if p.full {
                    sent["full"] = json!(true);
                }
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
            // A part the person forgot isn't kept again: past it.
            Ok(v) if v["forgotten"] == true => {
                sent["entries"] = json!(from + p.end);
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
            Ok(v) => {
                if v["off"] == true {
                    mark_off(off.as_deref(), None);
                }
                return Ok(());
            }
            Err(e) if worth_retrying(&e) => return Err(unreached(e, &parts[i..])),
            // Not kept for this caller here (turned off, or a server from before): sent again once it is.
            Err(e) if not_here(&e) => {
                mark_off(off.as_deref(), Some(&e));
                return Ok(());
            }
            // Refused: it would be refused again, so it isn't sent again.
            Err(e) => {
                warn(format!(
                    "transcript {session}: entries {} to {} weren't kept: {e:#}",
                    from + p.start,
                    from + p.end
                ));
                sent["entries"] = json!(from + p.end);
                let _ = std::fs::write(&sent_marker, sent.to_string());
            }
        }
    }
    Ok(())
}
