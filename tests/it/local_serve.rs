//! `memstore serve --local`: one binary on a laptop, no cloud and no Docker. Memories survive a restart, and a
//! server that answers everyone without sign-in refuses to listen beyond this machine.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use memstore::{Episode, Fact, Scope};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The real binary, serving `dir` on `port`.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A home directory of its own, so nothing the server writes can land in the real `~/.cache`; the
/// embedding model is still read from the real cache rather than downloaded again.
fn home_of(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_extension("home")
}

async fn serve_local(dir: &std::path::Path, port: u16, env: &[(&str, &str)]) -> Server {
    let models = format!("{}/.cache/memstore/models", std::env::var("HOME").unwrap());
    let child = Command::new(env!("CARGO_BIN_EXE_ampup-memory"))
        .env("HOME", home_of(dir))
        .env(
            "AMPUP_MEMORY_MODEL_DIR",
            std::env::var("AMPUP_MEMORY_MODEL_DIR").unwrap_or(models),
        )
        .args(["serve", "--local"])
        .arg(dir)
        .args(["org", &port.to_string()])
        .env_remove("AMPUP_MEMORY_CALLER_IDENTITY")
        .env_remove("MEMSTORE_CALLER_IDENTITY")
        .env_remove("AMPUP_MEMORY_SERVE_BIND")
        .env_remove("MEMSTORE_SERVE_BIND")
        .env_remove("AMPUP_MEMORY_PLATFORM")
        .env_remove("MEMSTORE_PLATFORM")
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let server = Server(child);
    let http = reqwest::Client::new();
    let up = Instant::now();
    while http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_err()
    {
        assert!(
            up.elapsed() < Duration::from_secs(60),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server
}

/// What `search_memory` answers over MCP, as an agent (Claude Code) would call it.
async fn search(port: u16, query: &str) -> String {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": "search_memory", "arguments": {"query": query}}});
    let res = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&call)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let v: Value = res.json().await.unwrap();
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Write one fact, cited by one episode shared with the local caller, straight into the directory (no
/// server holds it), the way an import would.
async fn remember(dir: &std::path::Path, entity: &str, value: &str) {
    let store = crate::local_store(dir).await;
    store.ensure_scope(&Scope::Org).await.unwrap();
    let ep = Episode::new(
        "note:owner",
        format!("{entity} is owned by {value}").as_bytes(),
        &["group:local"],
    );
    let fact = Fact::new(entity, "owner", value).from_episodes(&[&ep]);
    store
        .commit(&Scope::Org, "test", &[ep], &[fact])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_local_server_refuses_to_listen_beyond_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ampup-memory"))
        .args(["serve", "--local"])
        .arg(dir.path())
        .args(["org", &free_port().to_string()])
        .env("AMPUP_MEMORY_SERVE_BIND", "0.0.0.0")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a no-sign-in server bound to every interface"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("127.0.0.1 only"), "{err}");
}

#[tokio::test]
async fn a_web_page_that_rebinds_its_domain_to_this_machine_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    remember(dir.path(), "billing export", "priya").await;
    let port = free_port();
    let _server = serve_local(dir.path(), port, &[]).await;
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": "search_memory", "arguments": {"query": "billing export"}}});
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/mcp");
    // DNS rebinding: the browser connects to 127.0.0.1 but still names the attacker's domain.
    for (why, host, origin) in [
        ("a foreign Host", format!("evil.example:{port}"), None),
        (
            "a foreign Origin",
            format!("127.0.0.1:{port}"),
            Some("http://evil.example".to_string()),
        ),
    ] {
        let mut req = http.post(&url).header("host", host).json(&call);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let res = req.send().await.unwrap();
        assert_eq!(res.status().as_u16(), 403, "{why}");
        let body = res.text().await.unwrap();
        assert!(!body.contains("priya"), "{why}: {body}");
    }
    // The same call addressed to this machine is answered.
    assert!(search(port, "billing export owner").await.contains("priya"));
}

/// An MCP tool call: its text and whether it failed.
async fn tool(port: u16, name: &str, args: Value) -> (String, bool) {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": args}});
    let v: Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&call)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (text, v["result"]["isError"] == json!(true))
}

#[tokio::test]
async fn a_session_whose_run_kept_no_transcript_is_not_one_of_the_days_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let _server = serve_local(dir.path(), port, &[("AMPUP_MEMORY_SESSIONS_PER_DAY", "2")]).await;
    // A harness's session, opened by its end (as by its first step).
    let end = |sid: &'static str, discard: bool| async move {
        tool(
            port,
            "end_session",
            json!({"session_id": sid, "task": "summarize this", "discard": discard,
                   "transcript": "User: summarize this\nClaude: summarized"}),
        )
        .await
    };
    // Two summarizer runs (`--no-session-persistence`): their end hooks discard their sessions.
    for sid in ["no-transcript-run-1", "no-transcript-run-2"] {
        let (said, failed) = end(sid, true).await;
        assert!(!failed, "{said}");
    }
    // The day's two sessions are still to come; a third is past the cap.
    for sid in ["kept-session-1", "kept-session-2"] {
        let (said, failed) = end(sid, false).await;
        assert!(
            !failed,
            "a discarded session counted toward the cap: {said}"
        );
    }
    let (said, failed) = end("kept-session-3", false).await;
    assert!(failed && said.contains("the most a day"), "{said}");
}
