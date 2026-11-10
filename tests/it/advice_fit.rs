//! Advice is served only for the very task asked, on a real `memstore serve` over HTTP with no decisions model, so
//! the chat router routes (a stand-in).
//!
//! A question of a kind of task the map knows but has no advice for is routed to it and served nothing, not a
//! neighbouring kind's note. A note the chat router picks is served only once the chat router, reading it whole,
//! finds it is for the very task asked: one it finds is for another task is served to no one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

use crate::advice_versions::{doc, model};
use crate::isolation::{ADMIN_KEY, CLIENT_ID, admin, id_token, post, start_idp};

/// What `/advice` answers `bearer` for `question`.
async fn ask(s: &str, bearer: &str, question: &str) -> Value {
    let (status, body) = post(
        &format!("{s}/advice"),
        bearer,
        &json!({"question": question}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn advice_is_served_only_for_the_very_task_asked() {
    if !crate::own_process("advice_fit::advice_is_served_only_for_the_very_task_asked") {
        return;
    }
    let seen = Arc::new(Mutex::new(vec![]));
    let url = model(seen.clone()).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("MEMSTORE_DECIDE", "off");
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
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
        json!({"op": "set_settings", "org": "acme", "settings": {"advice_evidence": 0}}),
    )
    .await;
    let bob = id_token(&issuer, "bob@acme.com");
    let acme = Scope::org("acme").unwrap();
    store.ensure_scope(&acme).await.unwrap();

    // Two kinds of task: ranking accounts, with a note, and deploying the service, with none.
    let map = json!({"categories": {"work": {"description": "d", "situations": {
            "do-it": {"when": "rank accounts by churn", "skills": []},
            "deploy": {"when": "deploy the billing service", "skills": []}}}},
        "assign": {}, "notes": {"work/do-it": "h"}, "acl": ["*"], "linked": "",
        "written": {"work/do-it": {"runs": 4, "wrong": 0, "context": "", "format": "kinds-2-cited-backed"}}});
    store
        .commit(
            &acme,
            "test",
            &[
                doc("advice/map", &map.to_string()),
                doc(
                    "advice/work/do-it",
                    "# Advice: rank\n\n## When\nrank accounts by churn\n\n## Steps\n1. [how-to] RANK-NOTE query the table\n",
                ),
            ],
            &[],
        )
        .await
        .unwrap();
    let up = Instant::now();
    loop {
        let got = ask(s, &bob, "rank accounts by churn").await;
        if got["situation"] == "work/do-it" {
            assert!(
                got["advice"].as_str().unwrap().contains("RANK-NOTE"),
                "{got}"
            );
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The note was served once the chat router found it fits.
    let fit_asked = |seen: &Arc<Mutex<Vec<String>>>| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("An AI agent was given the request below"))
            .count()
    };
    assert!(
        fit_asked(&seen) >= 1,
        "the routed note was served unchecked"
    );

    // A known kind of task with no advice: routed to it (its line is offered), and served nothing.
    let got = ask(s, &bob, "deploy the billing service to staging").await;
    assert_eq!(got["situation"], Value::Null, "{got}");
    assert!(!got.to_string().contains("RANK-NOTE"), "{got}");
    let routed = seen
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|p| p.contains("Which one line is the kind of task"))
        .cloned()
        .unwrap();
    assert!(
        routed.contains("`work/deploy`"),
        "a kind of task with no advice was left off the router's list: {routed}"
    );

    // The note the chat router picks, found by it not to be for the very task asked: served to no one.
    let before = fit_asked(&seen);
    let got = ask(s, &bob, "rank accounts by churn WRONG").await;
    assert_eq!(got["situation"], Value::Null, "{got}");
    assert!(!got.to_string().contains("RANK-NOTE"), "{got}");
    assert_eq!(fit_asked(&seen), before + 1, "the fit wasn't asked");
}
