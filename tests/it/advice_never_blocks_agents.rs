//! An agent's own `start_session` never waits on routing: against a routing model that takes 3 s, an agent's
//! session opens within the server's wait (`MEMSTORE_ADVICE_WAIT_MS`) with its advice pending. The session's next
//! `get_advice`, whatever it asks, waits for that routing (up to `MEMSTORE_ADVICE_ASK_WAIT_MS`) and hands on what
//! it picked, rather than answering "still being looked up". A hook's call (one that fits the advice to its
//! context, `max_chars`) still waits for the advice.

use std::time::{Duration, Instant};

use memstore::server::{Options, serve_with};
use memstore::{Episode, Scope, Store, platform};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A chat model that takes `delay` to route, answering with the first line's key it was shown (`- \`key\`: …`),
/// and finds at once that the routed advice fits.
async fn slow_router(delay: Duration) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 8192];
                let body = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break body.to_string();
                    }
                };
                let asked: Value = serde_json::from_str(&body).unwrap_or_default();
                let prompt = asked["messages"].to_string();
                // Routing is what's slow; whether the routed advice fits is answered at once.
                if !prompt.contains("Is the advice for this very task") {
                    tokio::time::sleep(delay).await;
                }
                // Whether routed advice fits the request: it does.
                let key = if prompt.contains("Is the advice for this very task") {
                    "yes".to_string()
                } else {
                    prompt
                        .split("- `")
                        .nth(1)
                        .and_then(|r| r.split('`').next())
                        .unwrap_or("none")
                        .to_string()
                };
                let reply = json!({"id": "x", "model": "stand-in", "choices": [{"index": 0,
                    "finish_reason": "stop", "message": {"role": "assistant", "content": key}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
                .to_string();
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    port
}

/// An MCP tool call: its text, timed.
async fn tool(port: u16, name: &str, args: Value) -> (String, Duration) {
    let started = Instant::now();
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
    assert_ne!(v["result"]["isError"], json!(true), "{name}: {v}");
    assert!(!text.is_empty(), "{name}: {v}");
    (text, started.elapsed())
}

#[tokio::test]
async fn an_agents_session_opens_before_its_advice_is_routed() {
    if !crate::own_process(
        "advice_never_blocks_agents::an_agents_session_opens_before_its_advice_is_routed",
    ) {
        return;
    }
    let router = slow_router(Duration::from_secs(3)).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("MEMSTORE_ADVICE_ROUTE", "openrouter");
        std::env::set_var(
            "MEMSTORE_ADVICE_ROUTE_URL",
            format!("http://127.0.0.1:{router}/v1"),
        );
        std::env::set_var("MEMSTORE_ADVICE_ROUTE_KEY", "stand-in");
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_DECIDE", "off");
        std::env::set_var("MEMSTORE_ADVICE_WAIT_MS", "500");
        std::env::set_var("MEMSTORE_ADVICE_HOLDOUT", "0");
        std::env::set_var("MEMSTORE_ADVICE_TRIAL", "0");
    }
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let all = &["*"][..];
    let meta =
        json!({"slug": "ship-release", "acl": all, "sources": [], "names": [], "compiler": "test"});
    let mut skill = Episode::new(
        "doc:skills/ship-release",
        format!(
            "<!-- memstore skill {meta} -->\n# ship-release\n\nRun the smoke test before tagging.\n"
        )
        .as_bytes(),
        all,
    );
    skill.document = Some("skills/ship-release".into());
    let mut index = Episode::new(
        "doc:skills/index~all",
        b"- `ship-release`: when shipping a release (`aip/ship-release`)\n",
        all,
    );
    index.document = Some("skills/index~all".into());
    store
        .commit(&Scope::Org, "test", &[skill, index], &[])
        .await
        .unwrap();
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

    // The agent's session opens at once, its advice still being routed.
    let (said, took) = tool(
        port,
        "start_session",
        json!({"context": "ship the release"}),
    )
    .await;
    assert!(
        took < Duration::from_secs(2),
        "start_session took {took:?}: {said}"
    );
    let started: Value = serde_json::from_str(&said).unwrap();
    assert!(started["advice_pending"].is_string(), "{said}");
    let sid = started["session_id"].as_str().unwrap().to_string();
    // Its next get_advice, asked at once and whatever it asks, waits for the routing still going and hands on its
    // pick: a host without hooks (ChatGPT, Codex) gets the advice it asked for, not "still being looked up".
    let (said, took) = tool(
        port,
        "get_advice",
        json!({"session_id": sid, "question": "what next"}),
    )
    .await;
    assert!(said.contains("ship-release"), "{said}");
    assert!(said.contains("Procedure: ship-release"), "{said}");
    assert!(took < Duration::from_secs(6), "get_advice took {took:?}");

    // A hook's start (it fits the advice to its context) waits for the advice, as before.
    let (said, took) = tool(
        port,
        "start_session",
        json!({"context": "ship the release", "max_chars": 4000}),
    )
    .await;
    assert!(
        took >= Duration::from_secs(3),
        "the hook's start didn't route: {said}"
    );
    assert!(said.contains("Procedure: ship-release"), "{said}");
}
