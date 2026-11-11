//! The hooks a person waits on (a session's start, a prompt, a tool call) never wait on the memory: against a
//! memory that takes 10 s to answer, each returns at once; against one that answers, what it found reaches the
//! agent with the session's next hook.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The characters of the last `end_session` outcome any fake memory was sent.
static OUTCOME_CHARS: AtomicUsize = AtomicUsize::new(0);

/// A memory at `127.0.0.1:<port>` that answers every MCP tool call after `delay`: a session, advice for a question,
/// an inform for a tool event, and nothing for the rest.
fn fake_memory(delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                let _ = r.read_exact(&mut body);
                std::thread::sleep(delay);
                let call: Value = serde_json::from_slice(&body).unwrap_or_default();
                let args = &call["params"]["arguments"];
                // Session `gone` ended server-side: every call in it is refused.
                let gone = args["session_id"] == "gone";
                if let Some(o) = args["outcome"].as_str() {
                    OUTCOME_CHARS.store(o.chars().count(), Ordering::SeqCst);
                }
                let text = match call["params"]["name"].as_str() {
                    _ if gone => "session gone has ended".into(),
                    Some("start_session") => json!({"session_id": "s-1"}).to_string(),
                    Some("get_advice") if !args["event"].is_null() => {
                        json!({"op": "inform", "text": "Check the migration ran first."})
                            .to_string()
                    }
                    Some("get_advice") => "Situation: deploy\nAlways run the smoke test.".into(),
                    Some("search_memory") => {
                        json!([{"entity": "demo", "predicate": "allocator", "value": "jemalloc"}])
                            .to_string()
                    }
                    _ => "[]".into(),
                };
                let answer = json!({"jsonrpc": "2.0", "id": 1,
                                    "result": {"isError": gone, "content": [{"type": "text", "text": text}]}})
                .to_string();
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            });
        }
    });
    port
}

/// A memory that records each request (its path, its `x-memstore-org` and its JSON) in `seen`: while `down` it
/// answers 503 and marks the path "503 ", otherwise it answers as one that took it.
fn flaky_memory(
    down: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (down, seen) = (down.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let (mut len, mut org, mut path) = (0, String::new(), String::new());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if path.is_empty() {
                        path = line.split(' ').nth(1).unwrap_or_default().to_string();
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if let Some(v) = lower.strip_prefix("x-memstore-org:") {
                        org = v.trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                let _ = r.read_exact(&mut body);
                let mut s = stream;
                let call: Value = serde_json::from_slice(&body).unwrap_or_default();
                if down.load(Ordering::SeqCst) {
                    // Refused, and kept as such: its path marked "503 ".
                    seen.lock()
                        .unwrap()
                        .push((format!("503 {path}"), org, call));
                    let _ = write!(
                        s,
                        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    return;
                }
                seen.lock().unwrap().push((path, org, call.clone()));
                let text = match call["params"]["name"].as_str() {
                    Some("start_session") => json!({"session_id": "s-2"}).to_string(),
                    _ => "{}".into(),
                };
                let answer = json!({"jsonrpc": "2.0", "id": 1,
                                    "result": {"content": [{"type": "text", "text": text}]}})
                .to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            });
        }
    });
    port
}

/// A person's home for one test, under cargo's temp directory for these tests rather than the system's. A session's
/// start runs git where its session is, and on a machine whose system temp directory other processes fill and empty
/// by the thousand every lookup there can wait: git there took 0.2 s to 1.9 s a call against 3 ms here, which put a
/// hook that waits on nothing over its limit. That is the machine's time, not the hook's.
fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// Runs hook `event` with `input`, as the harness would: its output and how long it took from when it was started.
/// Starting a process isn't the hook's time: under load, a spawn here took 1.7 s to return before the hook ran.
fn hook(port: u16, home: &Path, event: &str, input: Value) -> (String, Duration) {
    hook_in(port, home, event, input, None)
}

/// [`hook`], with `AMPUP_MEMORY_ORG` naming `org`.
fn hook_in(
    port: u16,
    home: &Path,
    event: &str,
    input: Value,
    org: Option<&str>,
) -> (String, Duration) {
    warm_up();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ampup-memory"));
    cmd.args(["hook", event])
        .env("AMPUP_MEMORY_URL", format!("http://127.0.0.1:{port}"))
        .env("AMPUP_MEMORY_TOKEN", "test")
        .env("AMPUP_MEMORY_CONFIG_DIR", home.join("config"))
        .env("TMPDIR", home.join("tmp"))
        // Git finds no repository above the person's home: the one these tests are built in is not theirs.
        .env("GIT_CEILING_DIRECTORIES", home.parent().unwrap())
        .env_remove("AMPUP_MEMORY_HOOK_DETACHED")
        .env_remove("MEMSTORE_HOOK_DETACHED")
        .env_remove("AMPUP_MEMORY_ORG")
        .env_remove("MEMSTORE_ORG")
        .env_remove("AMPUP_MEMORY_SCOPE")
        .env_remove("MEMSTORE_SCOPE");
    if let Some(o) = org {
        cmd.env("AMPUP_MEMORY_ORG", o);
    }
    let mut child = cmd
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        started.elapsed(),
    )
}

/// Runs the binary once before any hook is timed: the first start of a freshly linked debug binary (hundreds of
/// MB) takes seconds on macOS, which is no hook's doing.
fn warm_up() {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        let _ = Command::new(env!("CARGO_BIN_EXE_ampup-memory"))
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// The hooks' markers directory under `home`'s temp directory.
fn markers(home: &Path) -> PathBuf {
    home.join("tmp").join("memstore-hooks")
}

/// Waits (at most 20 s) until `ready` holds.
fn until(what: &str, ready: impl Fn() -> bool) {
    let t = Instant::now();
    while !ready() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "{what} never happened"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn inbox_has_mail(home: &Path, session: &str) -> bool {
    std::fs::read_dir(markers(home)).is_ok_and(|d| {
        d.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("inbox-{session}."))
        })
    })
}

#[test]
fn a_slow_memory_holds_up_no_hook() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_secs(10));
    let sid = "slow-1";
    let base = json!({"session_id": sid, "cwd": home.path(), "transcript_path": home.path().join("t.jsonl")});
    let with = |extra: Value| {
        let mut v = base.clone();
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    let (_, took) = hook(
        port,
        home.path(),
        "session-start",
        with(json!({"source": "startup"})),
    );
    assert!(took < Duration::from_secs(2), "session-start took {took:?}");
    let (_, took) = hook(
        port,
        home.path(),
        "prompt",
        with(json!({"prompt": "deploy the api"})),
    );
    assert!(took < Duration::from_secs(2), "prompt took {took:?}");
    // A session served a note: its tool calls have triggers to fire.
    std::fs::create_dir_all(markers(home.path())).unwrap();
    std::fs::write(markers(home.path()).join(format!("session-{sid}")), "s-1").unwrap();
    std::fs::write(markers(home.path()).join(format!("advised-{sid}")), "").unwrap();
    let (_, took) = hook(
        port,
        home.path(),
        "pre-tool",
        with(json!({"tool_name": "Bash", "tool_input": {"command": "ls"}})),
    );
    assert!(took < Duration::from_secs(2), "pre-tool took {took:?}");
    let (_, took) = hook(
        port,
        home.path(),
        "post-tool",
        with(
            json!({"tool_name": "Bash", "tool_input": {}, "tool_response": "error: migration missing"}),
        ),
    );
    assert!(took < Duration::from_secs(2), "post-tool took {took:?}");
}

#[test]
fn what_a_copy_finds_reaches_the_next_hook() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_millis(300));
    let sid = "fast-1";
    let base = json!({"session_id": sid, "cwd": home.path(), "transcript_path": home.path().join("t.jsonl")});
    let with = |extra: Value| {
        let mut v = base.clone();
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    hook(
        port,
        home.path(),
        "session-start",
        with(json!({"source": "startup"})),
    );
    // The prompt doesn't wait for the session to open, nor for the advice.
    let (said, took) = hook(
        port,
        home.path(),
        "prompt",
        with(json!({"prompt": "deploy the api"})),
    );
    assert!(took < Duration::from_secs(2), "prompt took {took:?}");
    assert!(
        !said.contains("smoke test"),
        "the prompt waited for the advice: {said}"
    );
    until("the advice reaching the inbox", || {
        inbox_has_mail(home.path(), sid)
    });
    // The next hook hands on the session and its advice, once.
    let tool = json!({"tool_name": "Bash", "tool_input": {"command": "make deploy"}});
    let (said, _) = hook(port, home.path(), "pre-tool", with(tool.clone()));
    let v: Value = serde_json::from_str(said.trim()).expect("the pre-tool hook said nothing");
    let context = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("AmpUp Memory session: s-1"), "{context}");
    assert!(context.contains("Always run the smoke test."), "{context}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    // That tool call fired the note's triggers in a copy: its inform comes with the next hook.
    until("the inform reaching the inbox", || {
        inbox_has_mail(home.path(), sid)
    });
    let (said, _) = hook(
        port,
        home.path(),
        "post-tool",
        with(json!({"tool_name": "Bash", "tool_input": {}, "tool_response": "ok"})),
    );
    assert!(said.contains("Check the migration ran first."), "{said}");
    assert!(
        !said.contains("smoke test"),
        "advice handed on twice: {said}"
    );
}

/// A hook input for session `sid` in `cwd`, with `extra` fields.
fn input(sid: &str, cwd: &Path, extra: Value) -> Value {
    let mut v = json!({"session_id": sid, "cwd": cwd, "transcript_path": cwd.join("t.jsonl")});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}

#[test]
fn a_prompt_hands_on_the_open_session_at_once() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_millis(10));
    let (sid, cwd) = ("open-1", home.path());
    hook(
        port,
        cwd,
        "session-start",
        input(sid, cwd, json!({"source": "startup"})),
    );
    until("the session opening", || {
        markers(cwd).join(format!("session-{sid}")).exists()
    });
    // Its line is on this machine: the prompt hands it on itself, before the agent's first turn.
    let (said, _) = hook(
        port,
        cwd,
        "prompt",
        input(sid, cwd, json!({"prompt": "deploy the api"})),
    );
    assert!(said.contains("AmpUp Memory session: s-1"), "{said}");
    // The prompt's copy asks for advice and posts it, with the session's line, to the inbox: the next prompt hands
    // that on. Waited for, so the next prompt always finds it and never races it.
    until("the advice reaching the inbox", || inbox_has_mail(cwd, sid));
    let (said, _) = hook(
        port,
        cwd,
        "prompt",
        input(sid, cwd, json!({"prompt": "and the worker"})),
    );
    // The advice comes, and the line doesn't again: the first prompt handed it on, and it is handed on once.
    assert!(said.contains("Situation: deploy"), "no advice: {said}");
    assert_eq!(
        said.matches("AmpUp Memory session: s-1").count(),
        0,
        "the line handed on twice: {said}"
    );
}

#[test]
fn a_session_the_memory_ended_is_dropped_and_reopened() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_millis(10));
    let (sid, cwd) = ("dead-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
    // The markers directory, made as the hooks make it; then a session the server has since ended.
    hook(port, cwd, "pre-tool", input(sid, cwd, tool.clone()));
    let session = markers(cwd).join(format!("session-{sid}"));
    std::fs::write(&session, "gone").unwrap();
    std::fs::write(markers(cwd).join(format!("advised-{sid}")), "").unwrap();
    // A tool call asks in it, is refused, and drops it.
    hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    until("the dead session dropped", || !session.exists());
    // The next prompt opens a new one.
    hook(
        port,
        cwd,
        "prompt",
        input(sid, cwd, json!({"prompt": "deploy the api"})),
    );
    until("a new session opened", || {
        std::fs::read_to_string(&session).is_ok_and(|s| s.trim() == "s-1")
    });
}

#[test]
fn a_long_last_answer_ends_within_the_servers_outcome_cap() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_millis(10));
    let (sid, cwd) = ("long-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {}});
    hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    std::fs::write(markers(cwd).join(format!("session-{sid}")), "s-1").unwrap();
    let said = |t: &str, text: &str| {
        json!({"type": t, "message": {"role": t, "content": [{"type": "text", "text": text}]}})
            .to_string()
    };
    let transcript = [
        said("user", "write it up"),
        said("assistant", &"word ".repeat(1_000)),
    ]
    .join("\n")
        + "\n";
    std::fs::write(cwd.join("t.jsonl"), transcript).unwrap();
    hook(port, cwd, "end", input(sid, cwd, json!({"reason": "exit"})));
    until("the session ended", || {
        OUTCOME_CHARS.load(Ordering::SeqCst) > 0
    });
    let n = OUTCOME_CHARS.load(Ordering::SeqCst);
    assert!((1_000..=2_000).contains(&n), "outcome of {n} characters");
}

#[test]
fn an_end_the_memory_couldnt_take_is_sent_again_where_it_was_going() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let down = Arc::new(AtomicBool::new(true));
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let port = flaky_memory(down.clone(), seen.clone());
    let (sid, cwd) = ("down-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {}});
    hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    std::fs::write(markers(cwd).join(format!("session-{sid}")), "s-1").unwrap();
    let said = |t: &str, text: &str| {
        json!({"type": t, "message": {"role": t, "content": [{"type": "text", "text": text}]}})
            .to_string()
    };
    let transcript = [
        said("user", "draft the launch plan"),
        said("assistant", "the launch plan, drafted"),
    ]
    .join("\n")
        + "\n";
    std::fs::write(cwd.join("t.jsonl"), transcript).unwrap();
    // The session ends in org acme while the memory is down: its end waits in the outbox.
    hook_in(
        port,
        cwd,
        "end",
        input(sid, cwd, json!({"reason": "exit"})),
        Some("acme"),
    );
    let waiting = home
        .path()
        .join("config/outbox")
        .join(format!("end-{sid}.json"));
    until("the end kept to send again", || waiting.exists());
    // The memory is back, and the person has moved to org globex: the next session's start sends the end
    // to acme, as it was going.
    down.store(false, Ordering::SeqCst);
    hook_in(
        port,
        cwd,
        "session-start",
        input("next-1", cwd, json!({"source": "startup"})),
        Some("globex"),
    );
    let ended = || {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(p, _, c)| !p.starts_with("503") && c["params"]["name"] == "end_session")
            .cloned()
    };
    until("the end sent again", || ended().is_some());
    let (_, org, call) = ended().unwrap();
    assert_eq!(org, "acme");
    let args = &call["params"]["arguments"];
    assert_eq!(args["session_id"], "s-1");
    assert!(
        args["transcript"]
            .as_str()
            .is_some_and(|t| t.contains("draft the launch plan")),
        "{args}"
    );
    assert!(
        args["discard"].is_null(),
        "a kept session discarded: {args}"
    );
    until("the outbox emptied", || !waiting.exists());
}

#[test]
fn a_run_that_kept_no_transcript_ends_its_session_discarded() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let port = flaky_memory(Arc::new(AtomicBool::new(false)), seen.clone());
    // A `--no-session-persistence` run (a summarizer's): its hooks name a transcript that is never written.
    let (sid, cwd) = ("no-transcript-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {}});
    hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    std::fs::write(markers(cwd).join(format!("session-{sid}")), "s-1").unwrap();
    hook(
        port,
        cwd,
        "end",
        input(sid, cwd, json!({"reason": "other"})),
    );
    let ended = || {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(_, _, c)| c["params"]["name"] == "end_session")
            .cloned()
    };
    until("the session ended", || ended().is_some());
    let (_, _, call) = ended().unwrap();
    let args = &call["params"]["arguments"];
    assert_eq!(args["session_id"], "s-1");
    assert_eq!(args["discard"], true, "{args}");
}

#[test]
fn a_prompt_the_memory_couldnt_answer_is_asked_again() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let down = Arc::new(AtomicBool::new(true));
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let port = flaky_memory(down.clone(), seen.clone());
    let (sid, cwd) = ("retry-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {}});
    hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    std::fs::write(markers(cwd).join(format!("session-{sid}")), "s-1").unwrap();
    // The first prompt's question never reaches the memory.
    hook(
        port,
        cwd,
        "prompt",
        input(sid, cwd, json!({"prompt": "deploy the api"})),
    );
    until("the first prompt's ask refused", || {
        seen.lock().unwrap().iter().any(|(p, _, c)| {
            p.starts_with("503") && c["params"]["arguments"]["question"] == "deploy the api"
        })
    });
    // Its copy lets go of the ask a moment after the refusal.
    std::thread::sleep(Duration::from_millis(500));
    // The memory is back: the next prompt asks.
    down.store(false, Ordering::SeqCst);
    hook(
        port,
        cwd,
        "prompt",
        input(sid, cwd, json!({"prompt": "and the worker"})),
    );
    until("the next prompt asked", || {
        seen.lock().unwrap().iter().any(|(p, _, c)| {
            !p.starts_with("503") && c["params"]["arguments"]["question"] == "and the worker"
        })
    });
}

#[test]
fn a_session_is_remembered_whole_with_what_it_wrote() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let port = flaky_memory(Arc::new(AtomicBool::new(false)), seen.clone());
    let (sid, cwd) = ("whole-1", home.path());
    // A long answer (past the 60,000 characters a session body was cut to) and a document written whole.
    let answer = format!(
        "ANSWER-START {} ANSWER-MIDDLE {} ANSWER-END",
        "a ".repeat(40_000),
        "b ".repeat(40_000)
    );
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": "write the design doc"}}),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "Write", "input": {"file_path": "/repo/docs/design.md",
             "content": format!("# Design\n\nDOC-START {} DOC-END", "c ".repeat(5_000))}},
            {"type": "text", "text": answer}]}}),
    ];
    let transcript: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(cwd.join("t.jsonl"), transcript).unwrap();
    hook(port, cwd, "end", input(sid, cwd, json!({"reason": "exit"})));
    let remembered = || {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(path, _, _)| path.starts_with("/remember"))
            .map(|(_, _, b)| b["body"].as_str().unwrap_or_default().to_string())
    };
    until("the session remembered", || remembered().is_some());
    let body = remembered().unwrap();
    for part in [
        "ANSWER-START",
        "ANSWER-MIDDLE",
        "ANSWER-END",
        "wrote /repo/docs/design.md",
        "DOC-START",
        "DOC-END",
    ] {
        assert!(
            body.contains(part),
            "{part} missing from the remembered session"
        );
    }
}

#[test]
fn the_inbox_hands_on_what_the_context_takes() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let port = fake_memory(Duration::from_millis(10));
    let (sid, cwd) = ("full-1", home.path());
    let tool = json!({"tool_name": "Bash", "tool_input": {}});
    // The markers directory, made as the hooks make it.
    hook(port, cwd, "pre-tool", input(sid, cwd, tool.clone()));
    // Two answers waiting that together outgrow the 10,000 characters a hook may add.
    for (n, word) in [(1, "first"), (2, "second")] {
        let text = format!("{word} {}", "x".repeat(6_000));
        std::fs::write(markers(cwd).join(format!("inbox-{sid}.{n:024}.1")), text).unwrap();
    }
    let (said, _) = hook(port, cwd, "pre-tool", input(sid, cwd, tool.clone()));
    assert!(
        said.contains("first") && !said.contains("second"),
        "{}",
        &said[..80.min(said.len())]
    );
    let (said, _) = hook(port, cwd, "pre-tool", input(sid, cwd, tool.clone()));
    assert!(
        said.contains("second") && !said.contains("first"),
        "{}",
        &said[..80.min(said.len())]
    );
    let (said, _) = hook(port, cwd, "pre-tool", input(sid, cwd, tool));
    assert!(said.trim().is_empty(), "{said}");
}

#[test]
fn a_session_start_hands_on_the_repos_last_recall() {
    let home = temp_home();
    std::fs::create_dir_all(home.path().join("tmp")).unwrap();
    let repo = home.path().join("demo-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let vcs = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    vcs(&["init", "-q"]);
    vcs(&[
        "remote",
        "add",
        "origin",
        "git@github.com:acme/demo-repo.git",
    ]);
    let port = fake_memory(Duration::from_millis(10));
    // The repo's first session: nothing known yet, so its start says nothing of it.
    let start = json!({"source": "startup"});
    let (said, _) = hook(
        port,
        home.path(),
        "session-start",
        input("r-1", &repo, start.clone()),
    );
    assert!(!said.contains("jemalloc"), "{said}");
    until("the repo's recall kept", || {
        markers(home.path()).join("recall-demo-repo").exists()
    });
    // The next one's start hands it on at once.
    let (said, took) = hook(
        port,
        home.path(),
        "session-start",
        input("r-2", &repo, start),
    );
    assert!(said.contains("allocator: jemalloc"), "{said}");
    assert!(took < Duration::from_secs(2), "session-start took {took:?}");
}
