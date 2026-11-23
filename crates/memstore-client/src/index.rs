//! `index --days N` (`/ampup-memory:index`): the Claude Code sessions on this machine that started in the last N days,
//! read where Claude Code keeps them (`~/.claude/projects/<dir>/<session>.jsonl`, or under `CLAUDE_CONFIG_DIR`), sent
//! to the memory and replayed into a report: Memory Replay.
//!
//! 1. Read: each session's tool calls and what they returned, in the repository it ran in. A program's session
//!    (`claude -p`, an SDK run: an `sdk-*` entrypoint) and a sub-agent's own transcript are left out.
//! 2. Spot: a repeat is a later session of the same repository going back for what an earlier one already had: the
//!    same file read, command run, error hit, lookup (a web page, a search, another MCP server's tool), search of the
//!    code or sub-agent question. Its cost is what the tool returned again (characters ÷ 4), once; the five steps
//!    after an error rank which errors get a note but are counted only as themselves. Quick checks (`git status`, `ls`, `pwd`, ...) are no repeat.
//! 3. Remember: each session the hooks didn't already keep (their context names no `AmpUp Memory session:`) is ended
//!    into the memory as the hooks would have ended it (`end_session` under the harness's own session id: the
//!    server opens it by that name, and a session sent before is found again, never twice), so the memory extracts
//!    from it and learns advice from how it went, dated when it happened (its first entry's time, `occurred_at`). Not
//!    with `AMPUP_MEMORY_CAPTURE=off`, nor `--report-only`.
//! 4. Notes: the biggest repeats are handed to Claude through the person's own Claude Code (`claude -p`, which
//!    signs in as Claude Code does: `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`, else their login), never
//!    with hooks, plugins or tools, and nothing it runs is kept as a session. What it is given is redacted first.
//!    It has [`NOTES_WAIT`] to answer; past that it is stopped and the report says the notes timed out.
//!
//! The report is one HTML page (`index.html`, the data in it), written under the client's config directory and
//! opened in the browser.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Capped, Client, Harness, clip, entries_of, redact, result_text, segment};

/// The page; its data replaces [`SLOT`].
const PAGE: &str = include_str!("index.html");
const SLOT: &str = "/*DATA*/null";

/// What `index` does.
pub struct Options {
    /// How far back: sessions that started in the last this many days.
    pub days: u64,
    /// Send the sessions the hooks didn't keep to the memory.
    pub send: bool,
    /// At most this many notes written by Claude (0: none).
    pub notes: usize,
    /// The model `claude -p` writes them with.
    pub model: String,
    /// Open the report in the browser.
    pub open: bool,
}

/// The kinds of repeat, in the page's order, with its label.
const KINDS: [(&str, &str); 6] = [
    ("read", "Files re-read"),
    ("run", "Commands re-run"),
    ("err", "Errors hit again"),
    ("lookup", "Lookups re-done"),
    ("search", "Searches re-run"),
    ("agent", "Sub-agents re-asked"),
];

/// The fact table each kind's repeats would fill, and what it holds.
const TABLES: [(&str, &[&str], &str); 5] = [
    (
        "file",
        &["read"],
        "A source file sessions keep re-reading: what it is for, the symbols they go looking for, its traps.",
    ),
    (
        "command",
        &["run"],
        "A build, test, deploy or query command: what it checks, how long it takes, when it fails, a cheaper variant.",
    ),
    (
        "error",
        &["err"],
        "A failure, by its message: its cause and the fix that worked.",
    ),
    (
        "lookup",
        &["lookup", "agent"],
        "An expensive outside read (docs, an API, an MCP tool, a sub-agent's research) whose answer holds for a while.",
    ),
    (
        "codebase_map",
        &["search"],
        "Where something lives: the answer to a search sessions keep running again.",
    ),
];

/// Commands too cheap to count as a repeat: looking around, not learning.
const QUICK: [&str; 16] = [
    "cd", "pwd", "echo", "which", "true", "sleep", "mkdir", "touch", "rm", "mv", "cp", "date",
    "whoami", "clear", "open", "export",
];
/// git's quick subcommands.
const GIT_QUICK: [&str; 14] = [
    "status",
    "diff",
    "log",
    "branch",
    "show",
    "add",
    "commit",
    "push",
    "checkout",
    "switch",
    "pull",
    "fetch",
    "stash",
    "rev-parse",
];
/// Commands that search the code: a repeat of one is a search, not a run.
const SEARCH: [&str; 7] = ["grep", "rg", "find", "ls", "fd", "ag", "tree"];
/// Commands that read a file: a repeat of one is a file re-read, by the file.
const READS: [&str; 7] = ["cat", "sed", "head", "tail", "less", "bat", "nl"];
/// How a tool result starts where the person or the harness refused the call: no error of the work's.
const REFUSED: [&str; 6] = [
    "The user doesn't want to proceed",
    "Permission for this action was denied",
    "Permission to use",
    "This session is isolated in the worktree",
    "User rejected",
    "[Request interrupted",
];
/// A repeat's key, at most this many characters.
const KEY_MAX: usize = 200;
/// The steps after an error its cost counts.
const AFTER_ERROR: usize = 5;
/// What a note's writer sees of one occurrence's result.
const SAMPLE_MAX: usize = 1_500;
/// How long Claude may take to write the notes before it is stopped.
const NOTES_WAIT: Duration = Duration::from_secs(300);
/// Arcs on the timeline, at most.
const ARCS_MAX: usize = 3_000;

/// One tool call: its repeat key (none for a call that isn't one, an edit), and what it returned, in tokens.
struct Call {
    kind: Option<&'static str>,
    key: String,
    /// What it returned, in tokens: its own output, counted once.
    tok: u64,
    /// An error's: what the steps after it that dealt with it returned. It ranks errors and tells a note's writer
    /// what fixed it, but is no part of what was paid for twice: those steps are counted as themselves.
    after: u64,
    /// Its input and the start of what it returned, for a note's writer.
    sample: String,
}

struct Session {
    id: String,
    repo: String,
    /// When it started, Unix seconds.
    start: f64,
    prompts: usize,
    calls: Vec<Call>,
    /// The hooks kept it already: their context is in its transcript.
    kept: bool,
    /// What `end_session` takes for it (its trace, task, outcome and when it happened), for one the hooks didn't
    /// keep: worked out as it is read, so no session's transcript is held past its reading.
    end: Option<Value>,
}

/// One thing repeated across sessions: per session (in start order) its tokens and first sample.
struct Repeat<'a> {
    kind: &'static str,
    key: &'a str,
    repo: &'a str,
    sessions: Vec<(usize, u64, &'a str)>,
    /// What the later sessions spent on the steps after it, for an error: how much it cost, beyond itself.
    after: u64,
}

impl Repeat<'_> {
    /// What the sessions after the first paid for it again.
    fn respent(&self) -> u64 {
        self.sessions.iter().skip(1).map(|s| s.1).sum()
    }

    /// How much it is worth a note: what was paid for it again, and for an error the steps after it too.
    fn weight(&self) -> u64 {
        self.respent() + self.after
    }
}

pub async fn run(opts: Options) -> Result<()> {
    if opts.days == 0 {
        bail!("--days is at least 1");
    }
    let now = unix_now();
    let since = now - opts.days as f64 * 86_400.0;
    let root = projects_dir().context("no home directory to find Claude Code's sessions in")?;
    eprintln!(
        "Reading Claude Code sessions from the last {} days in {}",
        opts.days,
        root.display()
    );
    let files = transcripts(&root, since);
    let mut sessions: Vec<Session> = files
        .iter()
        .filter_map(|f| read_session(f, since))
        .collect();
    sessions.sort_by(|a, b| a.start.total_cmp(&b.start));
    if sessions.is_empty() {
        println!(
            "No Claude Code sessions started in the last {} days in {}: nothing to index.",
            opts.days,
            root.display()
        );
        return Ok(());
    }
    let repeats = repeats(&sessions);
    eprintln!(
        "{} sessions, {} tool calls, {} things repeated across sessions",
        sessions.len(),
        sessions.iter().map(|s| s.calls.len()).sum::<usize>(),
        repeats.len()
    );

    let client = Client::from_env(Duration::from_secs(60));
    // What the memory holds already, before anything is sent: each connector's sources, how far back they go.
    let held = holdings(&client, since).await;
    let indexed = match opts.send {
        true if std::env::var("AMPUP_MEMORY_CAPTURE").is_ok_and(|v| v == "off") => Indexed {
            skipped: Some("AMPUP_MEMORY_CAPTURE=off".into()),
            ..Default::default()
        },
        true => send(&client, &sessions).await,
        false => Indexed {
            skipped: Some("--report-only".into()),
            ..Default::default()
        },
    };
    let held = coverage(held, &sessions, &indexed, since);

    let (items, written) = notes(&sessions, &repeats, opts.notes, &opts.model);
    let mut data = report(
        &sessions, &repeats, items, &written, &indexed, opts.days, now,
    );
    data["holds"] = held.clone();
    let path = write_page(&data)?;

    // What the command says back.
    let respent = data["respent_tokens"].as_u64().unwrap_or(0);
    let read = data["tool_tokens"].as_u64().unwrap_or(0);
    println!(
        "Indexed the last {} days: {} Claude Code sessions, {} tool calls, in {}.",
        opts.days,
        sessions.len(),
        data["calls"],
        data["repos"]
            .as_array()
            .into_iter()
            .flatten()
            .take(5)
            .filter_map(|r| r[0].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    match &indexed.skipped {
        Some(why) => println!("Not sent to the memory ({why})."),
        None => println!(
            "Sent to the memory at {}: {} sessions ({} were there already, kept by the hooks or an earlier index{}).",
            indexed.server,
            indexed.sent,
            indexed.already,
            match (indexed.failed, &indexed.error) {
                (0, _) => String::new(),
                (n, Some(e)) => format!("; {n} not sent: {e}"),
                (n, None) => format!("; {n} not sent"),
            }
        ),
    }
    println!(
        "Paid for twice: {} of the {} tokens of tool output read. Biggest: {}.",
        short(respent),
        short(read),
        data["by_kind"]
            .as_array()
            .into_iter()
            .flatten()
            .take(3)
            .map(|k| format!(
                "{} {}",
                k["label"].as_str().unwrap_or(""),
                short(k["tok"].as_u64().unwrap_or(0))
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    say_holdings(&held, opts.days);
    println!("Notes: {written}");
    println!("Report: {}", path.display());
    if opts.open {
        open(&path);
    }
    Ok(())
}

/// Where Claude Code keeps its sessions.
fn projects_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d).join("projects")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects")),
    }
}

/// Every session transcript written to since `since`: `<dir>/<session>.jsonl`, never a sub-agent's
/// (`<dir>/<session>/subagents/`).
fn transcripts(root: &Path, since: f64) -> Vec<PathBuf> {
    let mut out = vec![];
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = f.path();
            let fresh = f
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| secs(t) >= since);
            if fresh && p.extension().is_some_and(|e| e == "jsonl") && p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// A session, if it started at `since` or later, is a person's (not a program's), and asked anything.
fn read_session(path: &Path, since: f64) -> Option<Session> {
    // Read a line at a time, keeping only the conversation (its prompts, answers, tool calls and results): a long
    // session's file is mostly what else Claude Code keeps (request records, attachments, snapshots), and is never
    // held whole.
    use std::io::BufRead as _;
    let mut kept = false;
    let mut text = String::new();
    for line in std::io::BufReader::new(std::fs::File::open(path).ok()?).lines() {
        let line = line.ok()?;
        kept |= line.contains("AmpUp Memory session: ");
        if line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"") {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let entries = entries_of(&text, Harness::ClaudeCode);
    drop(text);
    let start = entries
        .iter()
        .find_map(|e| e["timestamp"].as_str().and_then(parse_time))?;
    if start < since {
        return None;
    }
    if entries.iter().any(|e| {
        e["entrypoint"]
            .as_str()
            .is_some_and(|p| p.starts_with("sdk"))
    }) {
        return None;
    }
    let id = entries
        .iter()
        .find_map(|e| e["sessionId"].as_str())
        .map(str::to_string)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    let cwd = entries
        .iter()
        .find_map(|e| e["cwd"].as_str())
        .unwrap_or_default()
        .to_string();
    let repo = super::repo_name(&json!({ "cwd": cwd }));
    let calls = calls_of(&entries, &cwd);
    let seg = segment(&entries, Harness::ClaudeCode);
    drop(entries);
    if seg.prompts.is_empty() {
        return None;
    }
    let end = (!kept).then(|| {
        let mut args = json!({"session_id": id, "harness": Harness::ClaudeCode.name(), "transcript": seg.trace,
                              "occurred_at": (start * 1e6) as i64});
        if let Some(t) = seg.prompts.first() {
            args["task"] = json!(clip(&redact(t), 1_000));
        }
        if let Some(o) = &seg.outcome {
            args["outcome"] = json!(o);
        }
        args
    });
    Some(Session {
        id,
        repo,
        start,
        prompts: seg.prompts.len(),
        calls,
        kept,
        end,
    })
}

/// The session's tool calls in order, each with its repeat key and what it returned.
fn calls_of(entries: &[Value], cwd: &str) -> Vec<Call> {
    // What each call returned, by its id: (text, failed).
    let mut results: HashMap<&str, (String, bool)> = HashMap::new();
    for e in entries.iter().filter(|e| e["type"] == "user") {
        for b in e["message"]["content"].as_array().into_iter().flatten() {
            if b["type"] == "tool_result"
                && let Some(id) = b["tool_use_id"].as_str()
            {
                results.insert(id, (result_text(&b["content"]), b["is_error"] == true));
            }
        }
    }
    let mut calls = vec![];
    for e in entries
        .iter()
        .filter(|e| e["type"] == "assistant" && e["isSidechain"] != true)
    {
        for b in e["message"]["content"].as_array().into_iter().flatten() {
            if b["type"] != "tool_use" {
                continue;
            }
            let name = b["name"].as_str().unwrap_or_default();
            let input = &b["input"];
            let (out, failed) = b["id"]
                .as_str()
                .and_then(|id| results.get(id))
                .cloned()
                .unwrap_or_default();
            let (kind, key) = match failed {
                // The person, or the harness's permissions, said no: nothing went wrong in the work.
                true if REFUSED.iter().any(|r| out.trim_start().starts_with(r)) => {
                    (None, String::new())
                }
                true => (Some("err"), signature(&out)),
                false => keyed(name, input, cwd),
            };
            let shown = super::compact(input);
            calls.push(Call {
                kind: kind.filter(|_| !key.is_empty()),
                key: clip(&key, KEY_MAX),
                tok: out.chars().count() as u64 / 4,
                after: 0,
                sample: format!("{name} {shown}\n-> {}", clip(&out, SAMPLE_MAX)),
            });
        }
    }
    // What came after an error, the steps that dealt with it: kept beside its own cost, not in it.
    for i in 0..calls.len() {
        if calls[i].kind == Some("err") {
            let after: u64 = calls[i + 1..].iter().take(AFTER_ERROR).map(|c| c.tok).sum();
            let next: Vec<String> = calls[i + 1..]
                .iter()
                .take(AFTER_ERROR)
                .map(|c| clip(c.sample.lines().next().unwrap_or_default(), 300))
                .collect();
            calls[i].after = after;
            if !next.is_empty() {
                calls[i]
                    .sample
                    .push_str(&format!("\nThe steps after it:\n{}", next.join("\n")));
            }
        }
    }
    calls
}

/// A call's kind of repeat and its key; none for one that isn't a repeat (an edit, a quick check).
fn keyed(name: &str, input: &Value, cwd: &str) -> (Option<&'static str>, String) {
    let s = |k: &str| input[k].as_str().unwrap_or_default();
    match name {
        "Read" | "NotebookRead" => (Some("read"), relative(s("file_path"), cwd)),
        "Grep" => (
            Some("search"),
            format!("grep {} {}", s("pattern"), relative(s("path"), cwd))
                .trim()
                .to_string(),
        ),
        "Glob" => (Some("search"), format!("glob {}", s("pattern"))),
        "WebFetch" => (Some("lookup"), s("url").to_string()),
        "WebSearch" => (Some("lookup"), format!("search: {}", s("query"))),
        "Task" | "Agent" => {
            let d = match s("description") {
                "" => s("prompt"),
                d => d,
            };
            (Some("agent"), clip(d, 120))
        }
        "Bash" => {
            let whole: String = s("command")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            // What it does after any `cd <dir> &&`: the command is the same from wherever it was typed.
            let mut cmd = whole.as_str();
            while let Some(rest) = cmd.strip_prefix("cd ").and_then(|r| r.split_once(" && ")) {
                cmd = rest.1;
            }
            let mut words = cmd.split(' ');
            let first = words.next().unwrap_or_default();
            let second = words.next().unwrap_or_default();
            // Its first step's file, when it only reads one (`sed -n 1,80p src/a.rs; grep ...`): a file read.
            let step = cmd.split([';', '|', '&']).next().unwrap_or_default();
            let file = step
                .split(' ')
                .skip(1)
                .filter(|w| !w.starts_with('-') && !w.contains(['>', '<', '$', '*', '\'', '"']))
                .filter(|w| w.contains('.') || w.contains('/'))
                .last();
            if cmd.is_empty()
                || QUICK.contains(&first)
                || (first == "git" && GIT_QUICK.contains(&second))
                || first.contains("ampup-memory")
            {
                (None, String::new())
            } else if let (true, Some(f)) = (READS.contains(&first), file) {
                (Some("read"), relative(f, cwd))
            } else if SEARCH.contains(&first) {
                (Some("search"), cmd.to_string())
            } else {
                (Some("run"), cmd.to_string())
            }
        }
        n if n.starts_with("mcp__") => {
            let tool = n.rsplit("__").next().unwrap_or(n);
            match super::transcripts::memory_tool(tool) {
                true => (None, String::new()),
                false => (
                    Some("lookup"),
                    format!("{tool}:{}", clip(&input.to_string(), 160)),
                ),
            }
        }
        _ => (None, String::new()),
    }
}

/// `path` from the session's directory, when it is under it.
fn relative(path: &str, cwd: &str) -> String {
    match (cwd.is_empty(), path.strip_prefix(cwd)) {
        (false, Some(rest)) => rest.trim_start_matches('/').to_string(),
        _ => path.to_string(),
    }
}

/// An error, by what it said: its first line that says what went wrong, numbers and paths taken out, so the same
/// failure reads the same in each session.
fn signature(out: &str) -> String {
    let lines: Vec<&str> = out
        .lines()
        .map(|l| l.trim())
        .map(|l| {
            l.trim_start_matches("<tool_use_error>")
                .trim_end_matches("</tool_use_error>")
                .trim()
        })
        .map(|l| l.trim_start_matches('#').trim())
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("Exit code")
                && !matches!(*l, "Error" | "Error:" | "error")
        })
        .collect();
    let line = lines
        .iter()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error")
                || l.contains("failed")
                || l.contains("not found")
                || l.contains("panicked")
        })
        .or(lines.first())
        .copied()
        .unwrap_or_default();
    let words: Vec<String> = line
        .split_whitespace()
        .map(|w| match w.contains('/') {
            true => "<path>".to_string(),
            false => w
                .chars()
                .map(|c| if c.is_ascii_digit() { 'N' } else { c })
                .collect(),
        })
        .collect();
    clip(&words.join(" "), 160)
}

/// What was repeated across sessions of the same repository, the costliest first.
fn repeats(sessions: &[Session]) -> Vec<Repeat<'_>> {
    // By (repo, kind, key): each session that went for it, its tokens and its first sample.
    type Seen<'a> = (Vec<(usize, u64, &'a str)>, u64);
    let mut by: BTreeMap<(&str, &str, &str), Seen> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
        for c in &s.calls {
            let Some(kind) = c.kind else { continue };
            let seen = by
                .entry((s.repo.as_str(), kind, c.key.as_str()))
                .or_default();
            match seen.0.last_mut() {
                Some(last) if last.0 == i => last.1 += c.tok,
                _ => seen.0.push((i, c.tok, c.sample.as_str())),
            }
            // The steps after an error, in the sessions after the first that hit it.
            if seen.0.first().is_some_and(|f| f.0 != i) {
                seen.1 += c.after;
            }
        }
    }
    let mut out: Vec<Repeat> = by
        .into_iter()
        .filter(|(_, v)| v.0.len() > 1)
        .map(|((repo, kind, key), (sessions, after))| Repeat {
            kind,
            key,
            repo,
            sessions,
            after,
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.weight()));
    out
}

/// What sending the sessions did.
#[derive(Default)]
struct Indexed {
    server: String,
    sent: usize,
    already: usize,
    failed: usize,
    error: Option<String>,
    /// Why nothing was sent.
    skipped: Option<String>,
}

/// Ends each session the hooks didn't keep into the memory, as the hooks would have at its end.
async fn send(client: &Client, sessions: &[Session]) -> Indexed {
    let mut done = Indexed {
        server: client.url.clone(),
        ..Default::default()
    };
    if client.unsigned_remote() {
        done.skipped = Some(format!(
            "nobody is signed in to {}: `ampup-memory join` first",
            client.url
        ));
        return done;
    }
    // The session this runs in is the hooks' to end.
    let here = std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_default();
    let todo: Vec<&Session> = sessions
        .iter()
        .filter(|s| !s.kept && s.id != here)
        .collect();
    done.already = sessions.len() - todo.len();
    for (n, s) in todo.iter().enumerate() {
        let Some(args) = s.end.clone() else { continue };
        eprint!("\rSending sessions to the memory: {}/{}", n + 1, todo.len());
        match client.tool("end_session", args).await {
            Ok(_) => done.sent += 1,
            // Sent by an earlier index: the server found it by its id, ended.
            Err(e) if super::session_gone(&e) || format!("{e:#}").contains("has ended") => {
                done.already += 1
            }
            Err(e) => {
                done.failed += 1;
                let capped = e.downcast_ref::<Capped>().is_some();
                done.error = Some(format!("{e:#}"));
                // At the day's most, or unreachable: the rest would fare the same.
                if capped || super::worth_retrying(&e) {
                    done.failed += todo.len() - n - 1;
                    break;
                }
            }
        }
    }
    eprintln!();
    done
}

/// The connectors a source's first segment names: those that write sources (the hosted connectors, the sources a local
/// server reads, the coding agents' hooks). The memory's own sources (what people remembered, advice, the engine's
/// passes and documents) are no connector's, and aren't listed.
const CONNECTORS: [&str; 30] = [
    "slack",
    "github",
    "git",
    "linear",
    "sentry",
    "ampup",
    "gong",
    "zoom",
    "recall",
    "meeting",
    "hubspot",
    "salesforce",
    "dynamics_crm",
    "crm",
    "gmail",
    "outlook",
    "email",
    "granola",
    "fireflies",
    "fathom",
    "doc",
    "claude-code",
    "codex",
    "gemini",
    "cursor",
    "copilot",
    "droid",
    "qwen",
    "cline",
    "opencode",
];

/// What the memory holds that the caller may read (`/browse {op: "sources", since}`): each source with its episodes,
/// those in the window, and when its oldest and newest happened. An error says why it couldn't be read.
async fn holdings(client: &Client, since: f64) -> Result<Vec<Value>> {
    if client.unsigned_remote() {
        bail!("nobody is signed in to {}", client.url);
    }
    let since_us = (since * 1e6) as i64;
    let v = client
        .post(
            "/browse",
            &json!({"op": "sources", "since": since_us, "top": 0}),
        )
        .await?;
    v["sources"]
        .as_array()
        .cloned()
        .context("the memory answered no sources (a server from before `/browse`'s sources?)")
}

/// Each connector the memory holds, its sources summed, and whether it covers the window: `covered` (it goes back
/// past the window's start), `partial` (it starts inside the window: the days before it are missing), `stale`
/// (nothing in the window) or, for Claude Code, what this index read on this machine and sent.
fn coverage(
    held: Result<Vec<Value>>,
    sessions: &[Session],
    indexed: &Indexed,
    since: f64,
) -> Value {
    let us = |v: &Value| v.as_i64().map(|t| t as f64 / 1e6);
    let sources = match held {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("{e:#}"), "connectors": []}),
    };
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for s in sources {
        let kind = s["source"]
            .as_str()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();
        if CONNECTORS.contains(&kind.as_str()) {
            by.entry(kind).or_default().push(s);
        }
    }
    let mut connectors: Vec<Value> = by
        .into_iter()
        .map(|(kind, mut list)| {
            list.sort_by_key(|s| std::cmp::Reverse(s["episodes"].as_u64()));
            let episodes: u64 = list.iter().filter_map(|s| s["episodes"].as_u64()).sum();
            let window: u64 = list.iter().filter_map(|s| s["since_episodes"].as_u64()).sum();
            let first = list.iter().filter_map(|s| us(&s["first_occurred"])).reduce(f64::min);
            let last = list.iter().filter_map(|s| us(&s["last_occurred"])).reduce(f64::max);
            // A server from before `first_occurred` says only when the newest happened: whether it is recent.
            let dated = list.iter().any(|s| s.get("first_occurred").is_some());
            let status = match (first, last) {
                (_, Some(l)) if l < since => "stale",
                _ if !dated => "recent",
                (Some(f), _) if f > since => "partial",
                _ => "covered",
            };
            json!({
                "kind": kind,
                "label": label(&kind),
                "episodes": episodes,
                "in_window": if dated { json!(window) } else { Value::Null },
                "first": first.map(day_label),
                "last": last.map(day_label),
                "missing_days": first.filter(|f| *f > since).map(|f| ((f - since) / 86_400.0).floor() as u64),
                "status": status,
                "sources": list.iter().take(8).map(|s| json!({
                    "source": s["source"], "episodes": s["episodes"], "in_window": s["since_episodes"],
                    "first": us(&s["first_occurred"]).map(day_label), "last": us(&s["last_occurred"]).map(day_label),
                })).collect::<Vec<_>>(),
                "more_sources": list.len().saturating_sub(8),
            })
        })
        .collect();
    connectors.sort_by_key(|c| std::cmp::Reverse(c["in_window"].as_u64()));
    // Claude Code, from this machine: what was in the window, what the memory had, what this sent.
    let on_disk = sessions.len();
    let row = connectors.iter().position(|c| c["kind"] == "claude-code");
    let local = json!({
        "on_disk": on_disk,
        // Kept by the hooks (their context is in the transcript), whether or not this sent the rest.
        "already": match indexed.skipped {
            Some(_) => sessions.iter().filter(|s| s.kept).count(),
            None => indexed.already,
        },
        "sent": indexed.sent,
        "failed": indexed.failed,
        "skipped": indexed.skipped,
    });
    match row {
        Some(i) => connectors[i]["local"] = local,
        None => connectors.insert(
            0,
            json!({"kind": "claude-code", "label": label("claude-code"), "episodes": 0, "in_window": 0,
                   "status": "partial", "sources": [], "more_sources": 0, "local": local}),
        ),
    }
    if let Some(c) = connectors.iter_mut().find(|c| c["kind"] == "claude-code") {
        // Every session of the window is there once this sent the rest.
        if indexed.skipped.is_none() && indexed.failed == 0 {
            c["status"] = json!(if indexed.sent > 0 {
                "filled"
            } else {
                "covered"
            });
        }
    }
    json!({"connectors": connectors})
}

/// A connector's name as people know it.
fn label(kind: &str) -> String {
    match kind {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        "cursor" => "Cursor",
        "copilot" => "Copilot",
        "slack" => "Slack",
        "github" => "GitHub",
        "git" => "git history",
        "linear" => "Linear",
        "sentry" => "Sentry",
        "hubspot" => "HubSpot",
        "salesforce" => "Salesforce",
        "dynamics_crm" => "Dynamics CRM",
        "gong" => "Gong",
        "zoom" => "Zoom",
        "granola" => "Granola",
        "gmail" => "Gmail",
        "outlook" => "Outlook",
        "meeting" => "Meetings",
        "crm" => "CRM",
        "email" => "Email",
        "recall" => "Recall",
        "fireflies" => "Fireflies",
        "fathom" => "Fathom",
        "doc" => "Documents",
        "ampup" => "AmpUp",
        k => k,
    }
    .to_string()
}

/// What the memory holds, for the command to say.
fn say_holdings(held: &Value, days: u64) {
    if let Some(e) = held["error"].as_str() {
        println!("What the memory holds: couldn't be read ({e}).");
        return;
    }
    println!("What the memory holds for the last {days} days, by connector:");
    for c in held["connectors"].as_array().into_iter().flatten() {
        let name = c["label"].as_str().unwrap_or_default();
        let line = match (c["kind"].as_str(), c["status"].as_str()) {
            (Some("claude-code"), _) => {
                let l = &c["local"];
                match l["skipped"].as_str() {
                    Some(why) => format!(
                        "{} sessions on this machine, {} of them captured already by the hooks as they ended (or an earlier index); the rest not sent ({why})",
                        l["on_disk"], l["already"]
                    ),
                    None => format!(
                        "{} sessions on this machine: {} captured already by the hooks as they ended (or an earlier index), {} sent now{}",
                        l["on_disk"],
                        l["already"],
                        l["sent"],
                        match l["failed"].as_u64() {
                            Some(0) | None => String::new(),
                            Some(n) => format!(", {n} not sent"),
                        }
                    ),
                }
            }
            (_, Some("recent")) => format!(
                "{} episodes, the newest {} (how far back isn't known: the server is older than this client)",
                c["episodes"],
                c["last"].as_str().unwrap_or("?")
            ),
            (_, Some("stale")) => format!(
                "nothing in the window ({} episodes, the newest {}): it may have stopped",
                c["episodes"],
                c["last"].as_str().unwrap_or("?")
            ),
            (_, Some("partial")) => format!(
                "missing early days: {} in the window, but it starts {}, so the {} days before that need a backfill",
                c["in_window"],
                c["first"].as_str().unwrap_or("?"),
                c["missing_days"]
            ),
            _ => format!(
                "{} in the window, back to {}: covered",
                c["in_window"],
                c["first"].as_str().unwrap_or("?")
            ),
        };
        println!("  {name}: {line}");
    }
}

/// Notes for the costliest repeats, written by Claude through the person's Claude Code; where it can't, each says
/// what was repeated, and the second value says why it wasn't written.
fn notes(
    sessions: &[Session],
    repeats: &[Repeat],
    max: usize,
    model: &str,
) -> (Vec<Value>, String) {
    let top: Vec<&Repeat> = repeats
        .iter()
        .filter(|r| r.weight() > 0)
        // With none to write, the page still shows what was repeated most.
        .take(if max == 0 { 12 } else { max })
        .collect();
    let mut items: Vec<Value> = top
        .iter()
        .map(|r| {
            let days: Vec<String> = r.sessions.iter().map(|s| day_label(sessions[s.0].start)).collect();
            let label = KINDS.iter().find(|k| k.0 == r.kind).map_or(r.kind, |k| k.1);
            json!({
                "type": if r.kind == "err" { "advice" } else { "fact" },
                "table": TABLES.iter().find(|t| t.1.contains(&r.kind)).map(|t| t.0),
                "title": format!("{label}: {}", clip(r.key, 80)),
                "text": format!("{} sessions in {} went back for this.", r.sessions.len(), r.repo),
                "would_have_helped": format!("The {} later sessions, which read it again.", r.sessions.len() - 1),
                "saves_tokens_estimate": r.weight(),
                "repo": r.repo,
                "kind": r.kind,
                "key": r.key,
                "sessions": r.sessions.len(),
                "respent_tok": r.respent(),
                "status": if r.sessions.len() >= 3 { "ready" } else { "candidate" },
                "first_day": days[0],
                "later_days": days[1..],
            })
        })
        .collect();
    if max == 0 {
        return (items, "not written by Claude (--notes 0).".into());
    }
    if top.is_empty() {
        return (items, "nothing repeated, so none to write.".into());
    }
    eprintln!(
        "Writing {} notes with Claude ({model}) through Claude Code",
        top.len()
    );
    let asked: Vec<Value> = top
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({"id": i, "kind": r.kind, "repo": r.repo, "key": redact(r.key), "sessions": r.sessions.len(),
                   "tokens_spent_again": r.respent(), "steps_after_it": r.after, "first_time": redact(r.sessions[0].2)})
        })
        .collect();
    match write_notes(&asked, model) {
        Ok(written) => {
            let mut n = 0;
            for w in written {
                let Some(item) = w["id"].as_u64().and_then(|i| items.get_mut(i as usize)) else {
                    continue;
                };
                for k in ["type", "title", "text", "would_have_helped", "confidence"] {
                    if let Some(v) = w[k].as_str().filter(|v| !v.trim().is_empty()) {
                        item[k] = json!(v);
                    }
                }
                if let Some(t) = w["saves_tokens_estimate"].as_u64() {
                    item["saves_tokens_estimate"] = json!(t);
                }
                n += 1;
            }
            (
                items,
                format!("{n} written by Claude ({model}) through your Claude Code sign-in."),
            )
        }
        Err(e) => {
            eprintln!("Claude Code didn't write the notes: {e:#}");
            (
                items,
                format!(
                    "not written: Claude Code couldn't ({}). Sign in with `claude`, or make a token with `claude \
                     setup-token` and set CLAUDE_CODE_OAUTH_TOKEN to it, then run this again.",
                    clip(&format!("{e:#}"), 200)
                ),
            )
        }
    }
}

/// The notes, from `claude -p`: no tools, hooks, plugins or MCP servers, and no session kept.
fn write_notes(asked: &[Value], model: &str) -> Result<Vec<Value>> {
    let schema = json!({
        "type": "object",
        "required": ["notes"],
        "properties": {"notes": {"type": "array", "items": {
            "type": "object",
            "required": ["id", "type", "title", "text", "would_have_helped", "saves_tokens_estimate", "confidence"],
            "properties": {
                "id": {"type": "integer"},
                "type": {"type": "string", "enum": ["advice", "skill", "note", "fact"]},
                "title": {"type": "string"},
                "text": {"type": "string"},
                "would_have_helped": {"type": "string"},
                "saves_tokens_estimate": {"type": "integer"},
                "confidence": {"type": "string", "enum": ["high", "medium", "low"]}
            }
        }}}
    });
    let prompt = format!(
        "These are things a person's coding agent sessions went back for again: a later session of the same \
         repository re-read the same file, re-ran the same command, hit the same error, re-did the same lookup or \
         search, or asked a sub-agent the same thing, after an earlier session already had the answer. Each comes with \
         what the first session saw (its tool call and the start of what it returned; for an error, the steps after it).\n\n\
         For each, write the short note a memory should have kept from the first session so the later ones needn't \
         have paid again: the answer itself where the sample shows it (what the file is for and where to look in it, \
         what the command checks and what its passing output says, the error's cause and the fix that worked, what \
         the lookup said), not advice to remember. `type`: advice (how to do a kind of task, or avoid a trap), skill \
         (a procedure of several steps), fact (one thing true of the code or a tool), note (anything else). `title`: \
         one line. `text`: at most 3 sentences, concrete, naming the file, command or error. `would_have_helped`: one \
         sentence on what the later sessions did that the note would have spared them. `saves_tokens_estimate`: \
         tokens it would have saved them, at most tokens_spent_again plus steps_after_it (for an error, what the later sessions spent getting past it). `confidence`: how sure the sample makes you. \
         Write nothing you can't see in the sample; never repeat a secret. Answer with one note per id.\n\n{}",
        serde_json::to_string_pretty(asked)?
    );
    let mut child = std::process::Command::new("claude")
        .args([
            "-p",
            "--model",
            model,
            "--output-format",
            "json",
            "--tools",
            "",
            "--no-session-persistence",
        ])
        .args([
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--json-schema",
        ])
        .arg(schema.to_string())
        // Its hooks never run (no settings, no plugins), and it says nothing of its own to the memory either way.
        .env("AMPUP_MEMORY_CAPTURE", "off")
        .env_remove("CLAUDECODE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("can't run `claude` (is Claude Code installed and on PATH?)")?;
    // Its output read as it comes, so a full pipe never stalls it, while this waits at most NOTES_WAIT: a `claude`
    // stuck on a sign-in prompt or a stalled network is stopped, and the report is still written, without notes.
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = vec![];
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().context("claude's stdin")?;
        stdin.write_all(prompt.as_bytes())?;
    }
    let deadline = std::time::Instant::now() + NOTES_WAIT;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {} minutes", NOTES_WAIT.as_secs() / 60);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let (out, err) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    let v: Value = serde_json::from_slice(&out).with_context(|| {
        format!(
            "claude answered no JSON (exit {status}): {}",
            clip(String::from_utf8_lossy(&err).trim(), 300)
        )
    })?;
    if v["is_error"] == true || !status.success() {
        bail!(
            "{}",
            clip(v["result"].as_str().unwrap_or("claude failed"), 300)
        );
    }
    let notes = v["structured_output"]["notes"]
        .as_array()
        .cloned()
        .or_else(|| {
            v["result"]
                .as_str()
                .and_then(|r| serde_json::from_str::<Value>(r).ok())
                .and_then(|r| r["notes"].as_array().cloned())
        })
        .context("claude's answer has no notes")?;
    Ok(notes)
}

/// The page's data.
fn report(
    sessions: &[Session],
    repeats: &[Repeat],
    items: Vec<Value>,
    written: &str,
    indexed: &Indexed,
    days: u64,
    now: f64,
) -> Value {
    let calls: usize = sessions.iter().map(|s| s.calls.len()).sum();
    let tool_tokens: u64 = sessions.iter().flat_map(|s| &s.calls).map(|c| c.tok).sum();
    let mut by_kind: Vec<Value> = KINDS
        .iter()
        .map(|(k, label)| {
            let tok: u64 = repeats
                .iter()
                .filter(|r| r.kind == *k)
                .map(Repeat::respent)
                .sum();
            json!({"kind": k, "label": label, "tok": tok})
        })
        .filter(|k| k["tok"].as_u64() > Some(0))
        .collect();
    by_kind.sort_by_key(|k| std::cmp::Reverse(k["tok"].as_u64()));
    let respent: u64 = repeats.iter().map(Repeat::respent).sum();
    let mut by_day: BTreeMap<String, u64> = BTreeMap::new();
    let mut arcs = vec![];
    for r in repeats {
        let first = r.sessions[0].0;
        for &(i, tok, _) in &r.sessions[1..] {
            *by_day.entry(day_key(sessions[i].start)).or_default() += tok;
            if arcs.len() < ARCS_MAX {
                arcs.push(json!({"from": short_id(&sessions[first].id), "to": short_id(&sessions[i].id), "kind": r.kind, "tok": tok}));
            }
        }
    }
    let mut top = serde_json::Map::new();
    for (k, _) in KINDS {
        let t: Vec<Value> = repeats
            .iter()
            .filter(|r| r.kind == k)
            .take(8)
            .map(|r| json!({"key": redact(r.key), "sessions": r.sessions.len(), "tok": r.respent(), "repo": r.repo}))
            .collect();
        if !t.is_empty() {
            top.insert(k.into(), json!(t));
        }
    }
    let fact_tables: Vec<Value> = TABLES
        .iter()
        .map(|(table, kinds, description)| {
            let mine: Vec<&Repeat> = repeats.iter().filter(|r| kinds.contains(&r.kind)).collect();
            json!({
                "table": table,
                "description": description,
                "keys": mine.len(),
                "tokens_absorbed": mine.iter().map(|r| r.respent()).sum::<u64>(),
                "repeats": mine.iter().map(|r| r.sessions.len() - 1).sum::<usize>(),
                "examples": mine.iter().take(6).map(|r| redact(r.key)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut repos: HashMap<&str, usize> = HashMap::new();
    for s in sessions {
        *repos.entry(s.repo.as_str()).or_default() += 1;
    }
    let mut repos: Vec<(&str, usize)> = repos.into_iter().collect();
    repos.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let items: Vec<Value> = items
        .into_iter()
        .map(|mut i| {
            for k in ["title", "text", "would_have_helped", "key"] {
                if let Some(t) = i[k].as_str() {
                    i[k] = json!(redact(t));
                }
            }
            i
        })
        .collect();
    json!({
        "generated": time_label(now),
        "days": days,
        "sessions": sessions.len(),
        "by_harness": {"claude-code": sessions.len()},
        "repos": repos.iter().take(6).map(|(r, n)| json!([r, n])).collect::<Vec<_>>(),
        "prompts": sessions.iter().map(|s| s.prompts).sum::<usize>(),
        "calls": calls,
        "tool_tokens": tool_tokens,
        "respent_tokens": respent,
        "by_kind": by_kind,
        "by_day": by_day,
        "timeline": sessions.iter().map(|s| json!({"id": short_id(&s.id), "t": s.start, "h": "claude-code", "repo": s.repo, "calls": s.calls.len()})).collect::<Vec<_>>(),
        "arcs": arcs,
        "items": items,
        "notes_written": written,
        "fact_tables": fact_tables,
        "top": top,
        "indexed": {
            "server": indexed.server,
            "sent": indexed.sent,
            "already": indexed.already,
            "failed": indexed.failed,
            "skipped": indexed.skipped,
            "repeated_things": repeats.len(),
            "distinct": sessions.iter().map(|s| s.repo.as_str()).collect::<HashSet<_>>().len(),
        },
    })
}

/// Writes the page under the client's config directory, readable by its owner alone.
fn write_page(data: &Value) -> Result<PathBuf> {
    let dir = crate::client::config_dir().join("index");
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "memory-replay-{}.html",
        time_label(unix_now()).replace([':', ' '], "-")
    );
    let path = dir.join(name);
    // `</` can't close the script it sits in.
    let json = data.to_string().replace("</", "<\\/");
    let page = PAGE.replacen(SLOT, &json, 1);
    super::write_private(&dir, &path, &page)?;
    Ok(path)
}

/// Opens the page in the browser, if this machine has one to open it with.
fn open(path: &Path) {
    let opener = match cfg!(target_os = "macos") {
        true => "open",
        false => "xdg-open",
    };
    let _ = std::process::Command::new(opener)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn short(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", (n as f64 / 1e3).round()),
        n => n.to_string(),
    }
}

fn unix_now() -> f64 {
    secs(SystemTime::now())
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// An RFC 3339 time (`2026-11-12T09:30:00.123Z`, or with an offset) as Unix seconds.
fn parse_time(s: &str) -> Option<f64> {
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, sec) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        frac = format!("0.{digits}").parse().unwrap_or(0.0);
        rest = &r[digits.len()..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() >= 6 => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec - offset) as f64 + frac)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of Unix day `z`: (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Nov 12` (UTC).
fn day_label(t: f64) -> String {
    let (_, m, d) = civil_from_days((t as i64).div_euclid(86_400));
    format!("{} {d}", MONTHS[(m - 1) as usize])
}

/// `11-12` (UTC), for the cost by day.
fn day_key(t: f64) -> String {
    let (_, m, d) = civil_from_days((t as i64).div_euclid(86_400));
    format!("{m:02}-{d:02}")
}

/// `2026-11-23 14:05` (UTC).
fn time_label(t: f64) -> String {
    let t = t as i64;
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!("{y}-{m:02}-{d:02} {:02}:{:02}", s / 3600, s % 3600 / 60)
}
