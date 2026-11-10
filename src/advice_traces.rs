//! Advice traces: the conversations in which an agent asked for advice, kept to train an advice model on what the
//! conversation held at each ask and what came of it.
//!
//! The plugin's Stop hook (`memstore_client::hook`) finds, in a Claude Code transcript (the session's and each of
//! its subagents'), every ask for advice: a call to Claude Code's advisor (`server_tool_use` named `advisor`), to
//! memstore's own `get_advice` (an MCP `tool_use` named `…__get_advice`), or advice memstore's hooks handed the agent
//! (`hook_advice`). Each Stop that finds one not sent yet sends a part of the session's trace, what its transcripts
//! gained since the last part: `POST /advice-traces {session_id, files: [{file, lines, calls}]}`. The lines are the
//! transcript's user and assistant lines with tool calls and results (each capped, secret-looking words redacted on
//! the machine and again here) and the hooks' advice; of any other line only its place in the conversation. Thinking
//! is left out, and an advisor's encrypted reply too (only its status is kept). `{check: true}` asks only whether the
//! scope keeps them.
//!
//! Each part is an episode of its author's own (source `<signature>advice-trace:<session>`), readable by them alone,
//! left out of passage search and never extracted; nothing is sent or kept twice (a part sent again is the same
//! episode), and the export merges a session's parts into its one trace ([`merged`]). One person sends at most
//! `AMPUP_MEMORY_ADVICE_TRACES_PER_DAY` (10,000) parts a day; past that the server answers 429 and the client sends again
//! later. Forgetting them, a retention policy, offboarding their author and deleting the org remove them as any of
//! their episodes. Only the org's admins (and the operator) read them all: `/admin {op: "advice_traces"}` answers one
//! training example per call, the conversation up to and including the call (its `parentUuid` chain), the advice (an
//! advisor's plaintext reply, or `get_advice`'s answer, when there is one) and the agent's next turn.
//!
//! Off unless the operator turns it on for the org (`Settings::advice_traces`); where the org's setting is unset,
//! the server's `AMPUP_MEMORY_ADVICE_TRACES` decides (off unless on), which is for a server of one org (a local one):
//! on a server of many it would keep every org's that never set it. A directory that can't be read keeps nothing.
//! An org whose `pii` setting is on has personal identifiers replaced in what is kept, as in all it stores.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::people::directory::Directory;
use crate::{Episode, Principal, Scope, Store};

/// What a source holds after its signature: `advice-trace:<session>`, every part of the session's trace
/// (`:<file>` after it from clients before parts).
const SOURCE: &str = "advice-trace:";
/// Most transcripts (the session's and its subagents') one part holds.
const MAX_FILES: usize = 200;
/// Most lines one part holds.
const MAX_LINES: usize = 50_000;
/// Most calls one part records.
const MAX_CALLS: usize = 2_000;
/// Longest text of one block kept, in characters (the hook caps a tool result well below this).
const MAX_TEXT: usize = 32_000;
/// Most lines all of a trace's calls' conversations hold together: an export copies each call's conversation.
const MAX_PREFIX_LINES: usize = 400_000;
/// Most bytes all of a trace's calls' conversations hold together, for the same reason: one trace is one page at
/// least.
const MAX_PREFIX_BYTES: usize = 64 << 20;
/// Most bytes of one trace's parts an export reads: its oldest parts up to this, the rest left out.
const MAX_TRACE_BYTES: usize = 64 << 20;
/// Most bytes of one example's conversation: past it, its oldest lines are left out (`prefix_cut`).
const MAX_EXAMPLE_BYTES: usize = 8 << 20;
/// Most bytes of examples one export page answers (at least one example, which [`MAX_EXAMPLE_BYTES`] bounds).
const PAGE_BYTES: usize = 32 << 20;
/// Most steps of the agent's next turn an example carries.
const NEXT_STEPS: usize = 30;
/// The kinds of call recorded.
pub const KINDS: [&str; 3] = ["advisor", "get_advice", "hook_advice"];

pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::fmt::Debug for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

/// A person past the day's most traces ([`max_per_day`]): kept again tomorrow, so the client sends it again.
pub struct Limited(pub String);
impl std::fmt::Display for Limited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::fmt::Debug for Limited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Limited {}

fn refuse(m: impl Into<String>) -> anyhow::Error {
    Refused(m.into()).into()
}

/// Whether `source` is an advice trace's (`<signature>:advice-trace:<session>:<file>`).
pub fn is_trace(source: &str) -> bool {
    source
        .strip_prefix(crate::REMEMBER_SOURCE)
        .is_some_and(|rest| rest.contains(&format!(":{SOURCE}")))
}

/// Whether `scope` keeps advice traces: its team's or org's setting, else the server's `AMPUP_MEMORY_ADVICE_TRACES`.
pub fn keeps(dir: &Directory, scope: &Scope) -> bool {
    match dir.keeps_advice_traces(scope) {
        Some(on) => on,
        None => std::env::var("AMPUP_MEMORY_ADVICE_TRACES").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            )
        }),
    }
}

/// A session's or a file's id: letters, digits, `-`, `_` and `.`, not starting with `.`.
fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Every string in `v`, redacted and capped, as the hook should already have, and with `pii` its personal
/// identifiers replaced; thinking and an advisor's encrypted reply left out, whatever the hook sent.
fn scrubbed(v: &Value, pii: bool) -> Value {
    match v {
        Value::String(s) => {
            let s: String = s.chars().take(MAX_TEXT).collect();
            let s = crate::hook::redact(&s);
            Value::String(if pii { crate::pii::redact(&s).0 } else { s })
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .filter(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
                .map(|b| scrubbed(b, pii))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                // An advisor's encrypted reply is no use to anyone and stays unread: never kept.
                .filter(|(k, _)| k.as_str() != "encrypted_content" && k.as_str() != "signature")
                .map(|(k, v)| (k.clone(), scrubbed(v, pii)))
                .collect(),
        ),
        v => v.clone(),
    }
}

/// `POST /advice-traces`: keeps the caller's transcript `args` as their own episode, signed `signed` and readable
/// by `acl`. Answers what was kept; `{kept: false, off: true}` where the scope doesn't keep them.
pub async fn keep(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    acl: &[String],
    signed: &str,
    args: &Value,
) -> Result<Value> {
    // Unread, nothing is kept: the org may have turned them off.
    let folded = store.directory().await?;
    let dir = &folded.dir;
    if !keeps(dir, scope) {
        return Ok(json!({"kept": false, "off": true}));
    }
    if args["check"].as_bool() == Some(true) {
        return Ok(json!({"kept": false, "off": false}));
    }
    let session = args["session_id"].as_str().unwrap_or_default();
    if !id_ok(session) {
        return Err(refuse(
            "session_id is an id (letters, digits, '-', '_' and '.')",
        ));
    }
    // A part of the session's one trace: what its transcripts gained since the last part (a client from before
    // sent one transcript whole, as one file, each time).
    let legacy;
    let files: &[Value] = match args["files"].as_array() {
        Some(f) => f,
        None => {
            legacy =
                [json!({"file": args["file"], "lines": args["lines"], "calls": args["calls"]})];
            &legacy
        }
    };
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(refuse(format!("files is a list of 1 to {MAX_FILES}")));
    }
    let (mut total_lines, mut total_calls, mut copied, mut copied_bytes) = (0, 0, 0usize, 0usize);
    let mut names = std::collections::HashSet::new();
    for f in files {
        let file = f["file"].as_str().unwrap_or_default();
        if !id_ok(file) || !names.insert(file) {
            return Err(refuse(
                "each file is named by a distinct id (letters, digits, '-', '_' and '.')",
            ));
        }
        let (Some(lines), Some(calls)) = (f["lines"].as_array(), f["calls"].as_array()) else {
            return Err(refuse(format!("{file}: lines and calls are lists")));
        };
        if lines.is_empty() && calls.is_empty() {
            return Err(refuse(format!("{file}: lines or calls hold one at least")));
        }
        total_lines += lines.len();
        total_calls += calls.len();
        if total_lines > MAX_LINES || total_calls > MAX_CALLS {
            return Err(refuse(format!(
                "a session's transcripts hold at most {MAX_LINES} lines and {MAX_CALLS} calls together"
            )));
        }
        for (i, c) in calls.iter().enumerate() {
            let kind = c["kind"].as_str().unwrap_or_default();
            if !KINDS.contains(&kind) {
                return Err(refuse(format!(
                    "{file}: calls[{i}]: kind is one of {}",
                    KINDS.join(", ")
                )));
            }
            if c["call_id"].as_str().is_none_or(str::is_empty) {
                return Err(refuse(format!("{file}: calls[{i}]: call_id is required")));
            }
            // Its line may be in an earlier part (an ask answered only since).
            if c["line_uuid"].as_str().is_none_or(str::is_empty) {
                return Err(refuse(format!("{file}: calls[{i}]: line_uuid is required")));
            }
        }
        // Each call's conversation is copied into its example: bounded here, so no export page grows past reason.
        let by_uuid: HashMap<&str, &Value> = lines
            .iter()
            .filter_map(|l| Some((l["uuid"].as_str()?, l)))
            .collect();
        let size: HashMap<&str, usize> = lines
            .iter()
            .filter_map(|l| Some((l["uuid"].as_str()?, l.to_string().len())))
            .collect();
        for c in calls {
            let Some(l) = c["line_uuid"].as_str().and_then(|u| by_uuid.get(u)) else {
                continue;
            };
            for l in chain(&by_uuid, l) {
                copied += 1;
                copied_bytes += l["uuid"]
                    .as_str()
                    .and_then(|u| size.get(u))
                    .copied()
                    .unwrap_or(0);
            }
        }
    }
    if total_calls == 0 {
        return Err(refuse("a part holds one call at least"));
    }
    if copied > MAX_PREFIX_LINES || copied_bytes > MAX_PREFIX_BYTES {
        return Err(refuse(format!(
            "the calls' conversations hold {copied} lines and {copied_bytes} bytes together, at most \
             {MAX_PREFIX_LINES} and {MAX_PREFIX_BYTES}"
        )));
    }
    let source = format!("{signed}{SOURCE}{session}");
    let today = sent_today(store, scope, &format!("{signed}{SOURCE}")).await?;
    if today >= max_per_day() {
        return Err(Limited(format!(
            "{} has sent {today} advice traces in {scope} today, the most a day: it can't be kept right now",
            who.user()
        ))
        .into());
    }
    let pii = dir.redacts_pii(scope);
    let files: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "file": f["file"],
                "calls": scrubbed(&f["calls"], pii),
                "lines": scrubbed(&f["lines"], pii),
            })
        })
        .collect();
    let body = json!({
        "kind": "advice_trace",
        "harness": "claude_code",
        "session_id": session,
        "author": who.user(),
        "files": files,
    });
    let text = serde_json::to_vec(&body)?;
    let acl: Vec<&str> = acl.iter().map(String::as_str).collect();
    // Every part of the session under one source; one sent again (a retry) is the same episode, kept once.
    let ep = Episode::sourced(&source, &text, &acl).without_text_index();
    store
        .commit(scope, who.user(), std::slice::from_ref(&ep), &[])
        .await?;
    Ok(json!({"kept": true, "episode_id": ep.episode_id, "calls": total_calls}))
}

/// How many parts of advice traces were written under `prefix` (one person's) in `scope` in the last day, those
/// forgotten since included: forgetting one must not free its place.
async fn sent_today(store: &Store, scope: &Scope, prefix: &str) -> Result<usize> {
    let since = crate::now_micros() - 86_400_000_000;
    let snap = store.rows(&scope.namespace(), crate::EPISODES).await?;
    let mut n = 0;
    for b in snap.rows.iter() {
        let (source, at) = (
            crate::strings(b, "source"),
            crate::timestamps(b, "recorded_at"),
        );
        for i in 0..b.num_rows() {
            if source[i].starts_with(prefix) && at[i].is_some_and(|t| t >= since) {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Most parts of advice traces one person may send in a scope in a day: `AMPUP_MEMORY_ADVICE_TRACES_PER_DAY`, default
/// 10,000. Each Stop that finds a new ask sends a part, so it counts sends, not sessions.
fn max_per_day() -> usize {
    std::env::var("AMPUP_MEMORY_ADVICE_TRACES_PER_DAY")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10_000)
}

/// The lines on the `parentUuid` chain ending at `end`, in order.
fn chain<'a>(by_uuid: &HashMap<&str, &'a Value>, end: &'a Value) -> Vec<&'a Value> {
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut cur = Some(end);
    while let Some(l) = cur {
        let id = l["uuid"].as_str().unwrap_or_default();
        if !seen.insert(id.to_string()) {
            break;
        }
        out.push(l);
        cur = l["parentUuid"]
            .as_str()
            .and_then(|p| by_uuid.get(p).copied());
    }
    out.reverse();
    out
}

/// What the agent did after `start`, until the person spoke again (or it asked again): its text and tool calls.
fn next_turn(kids: &HashMap<&str, Vec<&Value>>, start: &Value) -> Vec<Value> {
    let mut steps = vec![];
    let mut cur = start;
    while steps.len() < NEXT_STEPS {
        let Some(next) = cur["uuid"]
            .as_str()
            .and_then(|u| kids.get(u))
            .and_then(|k| k.first())
        else {
            break;
        };
        cur = next;
        let content = &cur["message"]["content"];
        if cur["type"] == "user" {
            let typed = content.is_string()
                || content
                    .as_array()
                    .is_some_and(|b| b.iter().any(|b| b["type"] == "text"));
            if typed {
                break;
            }
            continue;
        }
        for b in content.as_array().into_iter().flatten() {
            match b["type"].as_str() {
                Some("text") if b["text"].as_str().is_some_and(|t| !t.trim().is_empty()) => {
                    steps.push(json!({"said": b["text"]}))
                }
                Some("tool_use") => steps.push(json!({"tool": b["name"], "input": b["input"]})),
                Some("server_tool_use") if b["name"] == "advisor" => {
                    steps.push(json!({"advisor": b["id"]}));
                    return steps;
                }
                _ => {}
            }
        }
    }
    steps
}

/// One session's trace from its parts' bodies, oldest first: each transcript's lines once (by `uuid`, in the order
/// first sent) and its calls once (by `call_id`, the newest record of each).
pub fn merged(parts: &[Value]) -> Value {
    let mut order: Vec<String> = vec![];
    // Per transcript: its lines, their uuids, and its calls by id.
    type File = (
        Vec<Value>,
        std::collections::HashSet<String>,
        Vec<Value>,
        HashMap<String, usize>,
    );
    let mut files: HashMap<String, File> = HashMap::new();
    for p in parts {
        // A part holds `files`; a transcript sent whole by a client from before is one file.
        let one = [json!({"file": p["file"], "lines": p["lines"], "calls": p["calls"]})];
        let fs: &[Value] = p["files"].as_array().map(Vec::as_slice).unwrap_or(&one);
        for f in fs {
            let name = f["file"].as_str().unwrap_or_default().to_string();
            let (lines, seen, calls, at) = files.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                Default::default()
            });
            for l in f["lines"].as_array().into_iter().flatten() {
                let id = l["uuid"].as_str().unwrap_or_default();
                if seen.insert(id.to_string()) {
                    lines.push(l.clone());
                }
            }
            for c in f["calls"].as_array().into_iter().flatten() {
                let id = c["call_id"].as_str().unwrap_or_default().to_string();
                match at.get(&id) {
                    Some(&i) => calls[i] = c.clone(),
                    None => {
                        at.insert(id, calls.len());
                        calls.push(c.clone());
                    }
                }
            }
        }
    }
    let last = parts.last().cloned().unwrap_or_default();
    json!({
        "session_id": last["session_id"],
        "author": last["author"],
        "files": order.iter().map(|n| {
            let (lines, _, calls, _) = &files[n];
            json!({"file": n, "lines": lines, "calls": calls})
        }).collect::<Vec<_>>(),
    })
}

/// The training examples in one trace (as [`merged`] makes it): per call, its conversation up to and including the
/// call, the advice given (when readable) and the agent's next turn. They stop once they hold `budget` bytes (one at
/// least), as a page does.
pub fn examples(body: &Value, budget: usize) -> Vec<Value> {
    let mut out = vec![];
    let mut bytes = 0;
    for f in body["files"].as_array().into_iter().flatten() {
        file_examples(body, &f["file"], f, &mut out, &mut bytes, budget);
    }
    out
}

/// [`examples`] of one transcript `f` (`{lines, calls}`) of the trace `body`.
fn file_examples(
    body: &Value,
    file: &Value,
    f: &Value,
    out: &mut Vec<Value>,
    bytes: &mut usize,
    budget: usize,
) {
    let lines: Vec<&Value> = f["lines"].as_array().into_iter().flatten().collect();
    let by_uuid: HashMap<&str, &Value> = lines
        .iter()
        .filter_map(|l| Some((l["uuid"].as_str()?, *l)))
        .collect();
    let mut kids: HashMap<&str, Vec<&Value>> = HashMap::new();
    for l in &lines {
        if let Some(p) = l["parentUuid"].as_str() {
            kids.entry(p).or_default().push(l);
        }
    }
    for c in f["calls"].as_array().into_iter().flatten() {
        if *bytes >= budget && !out.is_empty() {
            return;
        }
        let Some(at) = c["line_uuid"].as_str().and_then(|u| by_uuid.get(u)) else {
            continue;
        };
        {
            // Newest first, up to MAX_EXAMPLE_BYTES, so no one example outgrows a page.
            let (mut prefix, mut size, mut cut) = (vec![], 0, false);
            for l in chain(&by_uuid, at).into_iter().rev() {
                if l["type"] != "user" && l["type"] != "assistant" {
                    continue;
                }
                let line = json!({"type": l["type"], "message": l["message"]});
                size += line.to_string().len();
                if size > MAX_EXAMPLE_BYTES && !prefix.is_empty() {
                    cut = true;
                    break;
                }
                prefix.push(line);
            }
            prefix.reverse();
            let after = c["result_uuid"]
                .as_str()
                .and_then(|u| by_uuid.get(u).copied());
            let ex = json!({
                "kind": c["kind"],
                "call_id": c["call_id"],
                "session_id": body["session_id"],
                "file": file,
                "author": body["author"],
                "timestamp": c["timestamp"],
                "model": c["model"],
                "advisor_model": c["advisor_model"],
                "status": c["status"],
                "advice": c["advice"],
                "prefix": prefix,
                "prefix_cut": cut,
                "next_turn": after.map(|a| next_turn(&kids, a)).unwrap_or_default(),
            });
            *bytes += ex.to_string().len();
            out.push(ex);
        }
    }
}

/// `/admin {op: "advice_traces"}`: the examples of `scope`'s traces, every author's, for its admins; the traces
/// (each session's parts merged, [`merged`]) from `offset` on, at most `limit` of them and about [`PAGE_BYTES`] of
/// examples (a trace's examples stop there too, so a page holds one trace's at least, never one past reason).
pub async fn export(store: &Store, scope: &Scope, offset: usize, limit: usize) -> Result<Value> {
    // Each trace (an author's session: its source, less what a client from before put after the session) with its
    // parts as (sent, episode id), and when it last grew.
    let mut traces: HashMap<String, (i64, Vec<(i64, String)>)> = HashMap::new();
    for e in store.current_episodes(scope).await?.into_values() {
        // One whose readers are gone (forgotten, its ACL emptied) is no one's to export, whatever stays of it.
        if !is_trace(&e.source) || e.acl.iter().all(|a| a == crate::FORGOTTEN) {
            continue;
        }
        let Some(at) = e.source.rfind(SOURCE).map(|i| i + SOURCE.len()) else {
            continue;
        };
        let key = match e.source[at..].find(':') {
            Some(j) => e.source[..at + j].to_string(),
            None => e.source.clone(),
        };
        let t = traces.entry(key).or_insert((i64::MIN, vec![]));
        t.0 = t.0.max(e.occurred_at);
        t.1.push((e.occurred_at, e.episode_id));
    }
    // (when it last grew, its key, its parts)
    type Trace = (i64, String, Vec<(i64, String)>);
    let mut traces: Vec<Trace> = traces.into_iter().map(|(k, (at, p))| (at, k, p)).collect();
    traces.sort();
    let total = traces.len();
    let mut out = vec![];
    let (mut read, mut bytes) = (0, 0);
    for (_, _, mut parts) in traces.into_iter().skip(offset).take(limit) {
        if bytes >= PAGE_BYTES {
            break;
        }
        read += 1;
        parts.sort();
        let ids: Vec<String> = parts.into_iter().map(|(_, id)| id).collect();
        let (mut bodies, mut raw, mut used) = (vec![], 0, vec![]);
        // A few at a time, oldest first, up to MAX_TRACE_BYTES of them: a session sent in very many parts is read
        // only that far.
        'parts: for batch in ids.chunks(4) {
            let got = store.episode_bodies(scope, batch.to_vec()).await?;
            for id in batch {
                let Some(b) = got.get(id) else { continue };
                raw += b.len();
                if raw > MAX_TRACE_BYTES && !bodies.is_empty() {
                    break 'parts;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(b) {
                    bodies.push(v);
                    used.push(id.clone());
                }
            }
        }
        let ids = used;
        for mut ex in examples(&merged(&bodies), PAGE_BYTES.saturating_sub(bytes)) {
            ex["episode_ids"] = json!(ids);
            bytes += ex.to_string().len();
            out.push(ex);
        }
    }
    let next = offset + read;
    Ok(json!({
        "examples": out,
        "traces": total,
        "next_offset": (next < total).then_some(next),
    }))
}
