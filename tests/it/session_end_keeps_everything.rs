//! A session's end is never refused for what it carries: an outcome past the entry's 2,000 characters ends the
//! session, the entry keeps its start and the session's episode keeps it whole.

use std::time::{Duration, Instant};

use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};

/// An MCP tool call: its text and whether it failed.
async fn tool(port: u16, name: &str, args: Value) -> (String, bool) {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": args}});
    let v: Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mcp"))
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
async fn a_whole_session_is_remembered() {
    let (_dir, store, port) = serve().await;
    // A hook's session body as large as it sends (1,500,000 characters).
    let body = format!(
        "Claude Code session s in repo.\n\n{} SESSION-END",
        "x".repeat(1_499_000)
    );
    let res = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/remember"))
        .json(&json!({"source": "claude-code:repo:s", "body": body, "mine": true}))
        .send()
        .await
        .unwrap();
    let status = res.status();
    let answer: Value = res.json().await.unwrap_or_default();
    assert!(status.is_success(), "{status}: {answer}");
    let id = answer["episode_id"].as_str().unwrap().to_string();
    let kept = store
        .episode_bodies(&Scope::Org, vec![id.clone()])
        .await
        .unwrap()
        .remove(&id)
        .expect("the session's episode");
    assert_eq!(kept.len(), body.len(), "the session wasn't kept whole");
}

/// A server on a local catalog of its own, as its owner: the catalog's directory, its store and its port.
async fn serve() -> (tempfile::TempDir, Store, u16) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec!["team-a".to_string()],
        Options {
            catalog: Some(catalog),
            writes_here: true,
            ..Default::default()
        },
    ));
    let up = Instant::now();
    while reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .is_err()
    {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (dir, store, port)
}

#[tokio::test]
async fn a_long_outcome_ends_the_session_and_is_kept_whole() {
    let (_dir, store, port) = serve().await;
    // A harness's session, opened by its first step.
    let sid = "long-outcome-session-01";
    let (said, failed) = tool(
        port,
        "get_advice",
        json!({"session_id": sid, "question": "write the design doc", "moment": "done"}),
    )
    .await;
    assert!(!failed, "{said}");
    let outcome = format!("{} the end of the doc", "section text ".repeat(400));
    let (said, failed) = tool(
        port,
        "end_session",
        json!({"session_id": sid, "transcript": "User: write the design doc\nClaude: written",
               "outcome": outcome}),
    )
    .await;
    assert!(!failed, "the end was refused: {said}");
    memstore::sessions::flush().await.unwrap();
    let logged = memstore::qa_log::list(&store, &Scope::Org, None)
        .await
        .unwrap();
    let entry = logged
        .iter()
        .find(|l| l.answer_id.starts_with(sid))
        .expect("the session's entry");
    assert!(entry.session.as_ref().unwrap().ended_at.is_some());
    assert_eq!(
        entry.answer.as_deref().map(|a| a.chars().count()),
        Some(2_000)
    );
    let episode = entry
        .episodes
        .first()
        .cloned()
        .expect("the session's episode");
    let body = store
        .episode_bodies(&Scope::Org, vec![episode.clone()])
        .await
        .unwrap()
        .remove(&episode)
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("the end of the doc"),
        "the episode lost the outcome's end"
    );
}

#[tokio::test]
async fn an_agent_asking_in_an_ended_session_hears_it_has_ended() {
    let (_dir, _store, port) = serve().await;
    let sid = "ended-session-01";
    let (said, failed) = tool(
        port,
        "end_session",
        json!({"session_id": sid, "transcript": "User: ship it\nClaude: shipped"}),
    )
    .await;
    assert!(!failed, "{said}");
    // The agent asks on in it: it is told the session has ended, not that its tool failed.
    let (said, failed) = tool(
        port,
        "get_advice",
        json!({"session_id": sid, "question": "what next"}),
    )
    .await;
    assert!(
        failed && said.contains(sid) && said.contains("has ended"),
        "{said}"
    );
    // Its rows written while its catalog is still here: the flush is every server's in this process.
    memstore::sessions::flush().await.unwrap();
}
