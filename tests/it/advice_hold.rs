//! An admin holds a team note back from everyone, and a harness's `Tool loaded.` is never a session's task: a real
//! `memstore serve` over HTTP, the routing model a stand-in.
//!
//! A member who isn't an admin is refused a hold, and the note stays served; a key the map has no note for is
//! refused; the admin's hold serves the note to no one, shows in the trail, and lasts through a learn; released,
//! it is served again. A session an old client opened and ended with `Tool loaded.` as its task takes the
//! transcript's first ask instead.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

use crate::advice_versions::{doc, model};
use crate::isolation::{
    ADMIN_KEY, CLIENT_ID, admin, id_token, mcp_call, post, start_idp, tool_text,
};

/// What `/advice` serves `bearer` for the note's question: its situation, or null.
async fn served(s: &str, bearer: &str) -> Value {
    let (status, body) = post(
        &format!("{s}/advice"),
        bearer,
        &json!({"question": "rank accounts by churn"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["situation"].clone()
}

/// Waits until `/advice` serves `bearer` `want`.
async fn until_served(s: &str, bearer: &str, want: Value) {
    let up = Instant::now();
    loop {
        let got = served(s, bearer).await;
        if got == want {
            return;
        }
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "wanted {want}, got {got}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn an_admin_holds_a_note_and_a_harness_notice_is_never_a_task() {
    if !crate::own_process(
        "advice_hold::an_admin_holds_a_note_and_a_harness_notice_is_never_a_task",
    ) {
        return;
    }
    let seen = Arc::new(Mutex::new(vec![]));
    let url = model(seen.clone()).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("MEMSTORE_DECIDE", "off");
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_SESSION_FLUSH_SECS", "1");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_ADVICE_ROUTE", "openrouter");
        std::env::set_var("MEMSTORE_ADVICE_ROUTE_URL", &url);
        std::env::set_var("MEMSTORE_ADVICE_ROUTE_KEY", "stand-in");
    }
    let issuer = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
    let auth = TokenAuth::new(Some(&keys.to_string()), None)
        .unwrap()
        .allow_local_issuers()
        .with_oidc(
            OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                .await
                .unwrap(),
        );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            listener: Some(listener),
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while reqwest::Client::new().get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    admin(
        s,
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
    )
    .await;
    admin(
        s,
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}]}),
    )
    .await;
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"advice_evidence": 0, "advice_learn_people": 2}}),
    )
    .await;
    let (alice, bob) = (
        id_token(&issuer, "alice@acme.com"),
        id_token(&issuer, "bob@acme.com"),
    );
    let acme = Scope::org("acme").unwrap();
    store.ensure_scope(&acme).await.unwrap();

    // A note from 4 rewarded runs, served to bob.
    let runs: Vec<Value> = (0..4)
        .map(|n| {
            json!({"task": format!("rank accounts by churn, week {n}"), "trace": "queried the table",
                   "outcome": "done", "reward": 1, "reason": "checked"})
        })
        .collect();
    let ids = crate::advice_versions::two_people(&store, &acme, &runs).await;
    let assign: serde_json::Map<String, Value> = ids
        .iter()
        .map(|i| (i.clone(), json!(["work", "do-it"])))
        .collect();
    let map = json!({"categories": {"work": {"description": "d", "situations": {
            "do-it": {"when": "rank accounts by churn", "skills": []}}}},
        "assign": assign, "notes": {"work/do-it": "h"}, "acl": ["*"], "linked": "",
        "written": {"work/do-it": {"runs": 4, "wrong": 0, "context": "", "format": "kinds-2-cited-backed"}}});
    store
        .commit(
            &acme,
            "test",
            &[
                doc("advice/map", &map.to_string()),
                doc(
                    "advice/work/do-it",
                    "# Advice: do it\n\n## When\nrank accounts by churn\n\n## Steps\n1. [how-to] query the table\n",
                ),
            ],
            &[],
        )
        .await
        .unwrap();
    until_served(s, &bob, json!("work/do-it")).await;

    let hold = |keys: Value, hold: bool| json!({"op": "hold_advice", "org": "acme", "keys": keys, "hold": hold});
    let admin_url = format!("{s}/admin?org=acme");
    // A member who isn't an admin is refused, and the note stays served.
    let (status, body) = post(&admin_url, &bob, &hold(json!(["work/do-it"]), true)).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(served(s, &bob).await, "work/do-it");
    // A key the map has no note for is refused.
    let (status, body) = post(&admin_url, &alice, &hold(json!(["work/nope"]), true)).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("no note work/nope"), "{body}");

    // The admin's hold: served to no one, shown in the trail, and kept through a learn.
    let (status, body) = post(&admin_url, &alice, &hold(json!(["work/do-it"]), true)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["held"],
        json!(["work/do-it"])
    );
    until_served(s, &bob, Value::Null).await;
    let (status, body) = post(
        &admin_url,
        &alice,
        &json!({"op": "learn_advice", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let up = Instant::now();
    loop {
        let (_, body) = post(
            &admin_url,
            &alice,
            &json!({"op": "advice_trail", "org": "acme"}),
        )
        .await;
        let v: Value = serde_json::from_str(&body).unwrap();
        let n = v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["key"] == "work/do-it")
            .cloned()
            .unwrap_or_default();
        // The learn has run once the note's items carry ids; the hold is still there.
        if n["items"].as_array().is_some_and(|i| !i.is_empty()) {
            assert_eq!(n["held"], true, "{n}");
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{n}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(served(s, &bob).await, Value::Null);

    // Released: served again.
    let (status, body) = post(&admin_url, &alice, &hold(json!(["work/do-it"]), false)).await;
    assert_eq!(status, 200, "{body}");
    until_served(s, &bob, json!("work/do-it")).await;

    // A session an old client opened and ended with `Tool loaded.` as its task takes the transcript's first ask.
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "start_session",
        json!({"context": "Tool loaded."}),
    )
    .await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    let id = text
        .split("session_id: ")
        .nth(1)
        .and_then(|t| t.split_whitespace().next())
        .map(String::from)
        .or_else(|| {
            serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["session_id"].as_str().map(String::from))
        })
        .unwrap_or_else(|| panic!("no session id: {text}"));
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "end_session",
        json!({"session_id": id, "task": "Tool loaded.", "outcome": "done",
               "transcript": "User: Tool loaded.\nUser: fix the billing export\nClaude: fixed it"}),
    )
    .await;
    assert!(!tool_text(&raw).1, "{raw}");
    let up = Instant::now();
    loop {
        let (_, body) = post(
            &admin_url,
            ADMIN_KEY,
            &json!({"op": "activity", "org": "acme", "user": "bob@acme.com"}),
        )
        .await;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let q = v["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["id"] == id.as_str())
            .map(|e| e["question"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        if !q.is_empty() {
            assert_eq!(q, "fix the billing export");
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{body}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
