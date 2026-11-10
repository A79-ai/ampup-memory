//! Error leads (`Settings::error_leads`): a real `memstore serve` over HTTP, org acme and its team eng, eng holding
//! Sentry issues in the host application's feed format and merged PRs in the eng corpus's. The routing model is a local stand-in
//! that counts its calls: neither scope has a skill or a note, so advice routes nothing, and the leads add no call.
//!
//! Unset, a question about an error is answered exactly as before. Set on team eng, `POST /advice` and MCP
//! `get_advice` add the similar past errors and the PRs that fixed them: the question's own issue named by its short
//! ID (or pasted as a traceback, matched by its frames to a fix of an issue no episode holds any more), never itself.
//! Through `reads`, acme's readers get eng's leads as each may read them; a team's setting wins over its org's.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::isolation::{
    ADMIN_KEY, CLIENT_ID, admin, id_token, mcp_call, post, start_idp, tool_text,
};

/// A routing model (OpenAI-compatible) that answers `none` and counts its calls.
async fn router(calls: Arc<AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                let _ = sock.read(&mut chunk).await;
                let reply = json!({"id": "x", "model": "stand-in", "choices": [{"index": 0,
                    "finish_reason": "stop", "message": {"role": "assistant", "content": "none"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
                .to_string();
                let out = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    port
}

/// A Sentry issue as the host application's feed writes it (`memstore_sentry.texts`, in the eng corpus's format).
fn issue(
    short: &str,
    id: u64,
    title: &str,
    culprit: &str,
    frames: &[&str],
    fixes: &[&str],
    acl: &[&str],
) -> Episode {
    let link = format!("https://acme.sentry.io/issues/{id}/");
    let mut parts = vec![format!(
        "{short} · {link}\nCulprit: {culprit}\nFirst seen in production 2026-08-01 (prod)\nEvents: 10+, users: 1+ (production, last 90 days)"
    )];
    if !frames.is_empty() {
        parts.push(format!(
            "## Stack trace\nIn-app frames of the latest event, most recent call last:\n{}",
            frames.join("\n")
        ));
    }
    if !fixes.is_empty() {
        parts.push("## Fixed by".into());
        parts.extend(fixes.iter().map(|f| f.to_string()));
    }
    let project = short.rsplit_once('-').unwrap().0.to_lowercase();
    let body = format!(
        "# {title} (sentry {project}, error, resolved)\n\n{}",
        parts.join("\n\n")
    );
    let mut e = Episode::new(&format!("sentry:{link}"), body.as_bytes(), acl);
    e.occurred_at = 1_754_000_000_000_000;
    e
}

/// A merged PR as the eng corpus holds it (`memstore_eng.pr_episode`).
fn pr(n: u64, title: &str, body: &str, files: &[&str]) -> Episode {
    let text = format!(
        "PR #{n}: {title}\n\n{title}\n\n{body}\n\nFiles changed:\n{}",
        files.join("\n")
    );
    let mut e = Episode::new(
        &format!("github:https://github.com/acme/app/pull/{n}"),
        text.as_bytes(),
        &["*"],
    );
    e.occurred_at = 1_756_000_000_000_000;
    e
}

/// POST /advice as `bearer`: the answer.
async fn ask(server: &str, bearer: &str, question: &str) -> Value {
    let (status, body) = post(
        &format!("{server}/advice"),
        bearer,
        &json!({"question": question}),
    )
    .await;
    assert_eq!(status, 200, "{question}: {body}");
    serde_json::from_str(&body).unwrap()
}

/// The short IDs of an answer's leads, in order.
fn led_to(answer: &Value) -> Vec<String> {
    answer["leads"]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|l| l["issue"]["short_id"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Asks until `check` passes on the answer (settings and commits reach the server within a few seconds).
async fn ask_until(
    server: &str,
    bearer: &str,
    question: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let up = Instant::now();
    loop {
        let got = ask(server, bearer, question).await;
        if check(&got) {
            return got;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{question}: {got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

const ASKED: &str =
    "API-SERVICE-1Q started firing after the deploy: what is it and how do I fix it?";
const TRACEBACK: &str = "The nightly export broke:\n\nTraceback (most recent call last):\n  File \"/app/services/export/job.py\", line 20, in run\n    writer.write(rows)\n  File \"/app/services/export/writer.py\", line 7, in write\n    self.f.write(line)\nValueError: I/O operation on closed file.";

#[tokio::test]
async fn advice_leads_an_error_to_similar_past_ones_and_their_fixes_where_a_scope_sets_it() {
    if !crate::own_process(
        "error_leads::advice_leads_an_error_to_similar_past_ones_and_their_fixes_where_a_scope_sets_it",
    ) {
        return;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let model = router(calls.clone()).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("AMPUP_MEMORY_DECIDE", "off");
        std::env::set_var("AMPUP_MEMORY_ADVICE_ROUTE", "openrouter");
        std::env::set_var(
            "AMPUP_MEMORY_ADVICE_ROUTE_URL",
            format!("http://127.0.0.1:{model}/v1"),
        );
        std::env::set_var("AMPUP_MEMORY_ADVICE_ROUTE_KEY", "stand-in");
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
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
    let http = reqwest::Client::new();
    let up = Instant::now();
    while http.get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Org acme and its team eng; bob is in acme alone, carol in eng too. Nothing extracts or learns here.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "eng"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["eng"]}]}),
        json!({"op": "set_settings", "org": "acme", "settings": {"extract": false, "advice": false}}),
    ] {
        admin(s, change).await;
    }
    let key = |name: &str, team: Option<&str>| {
        let k = format!("msk_{}{name}", uuid::Uuid::new_v4().simple());
        (
            k.clone(),
            json!({"op": "create_key", "org": "acme", "name": name, "team": team, "role": "reader",
                   "key": k}),
        )
    };
    let (acme_reader, c1) = key("dash", None);
    let (eng_reader, c2) = key("bot", Some("eng"));
    for c in [c1, c2] {
        admin(s, c).await;
    }
    let (bob, carol) = (
        id_token(&issuer, "bob@acme.com"),
        id_token(&issuer, "carol@acme.com"),
    );

    // Eng's memory: a past KeyError and its fix; the same error, new; an unrelated timeout and its fix; an error
    // only carol may read, in the same frame; a PR fixing an issue no episode holds (older than Sentry keeps); and
    // a PR naming only a Linear ticket.
    let eng = Scope::team("acme", "eng").unwrap();
    store.ensure_scope(&eng).await.unwrap();
    let frames = [
        "services/api/router.py:10 in handle",
        "services/billing/invoice.py:42 in build_invoice",
    ];
    store
        .commit(
            &eng,
            "test",
            &[
                issue(
                    "API-SERVICE-1A",
                    1001,
                    "KeyError: 'org_id'",
                    "services.billing.invoice in build_invoice",
                    &frames,
                    &["### acme/app#11: Guard a missing org id in invoices (merged 2026-09-01)\nFiles changed: \
                       tests/test_invoice.py, services/billing/invoice.py\n\n##### Summary\nInvoices read org_id before \
                       the org was loaded; now they wait for it."],
                    &["*"],
                ),
                issue(
                    "API-SERVICE-1Q",
                    1003,
                    "KeyError: 'org_id'",
                    "services.billing.invoice in build_invoice",
                    &frames,
                    &[],
                    &["*"],
                ),
                issue(
                    "CELERY-WORKER-2B",
                    1002,
                    "TimeoutError: upstream timed out",
                    "workers.sync in run",
                    &["workers/sync.py:5 in run"],
                    &["### acme/app#12: Retry the sync on timeouts (merged 2026-09-05)\nFiles changed: workers/sync.py\n\nRetries."],
                    &["*"],
                ),
                issue(
                    "API-SERVICE-1C",
                    1004,
                    "ValueError: bad cursor",
                    "services.billing.invoice in page",
                    &["services/billing/invoice.py:42 in build_invoice"],
                    &["### acme/app#14: Reset the invoice cursor (merged 2026-09-10)\nFiles changed: services/billing/invoice.py\n\nCAROL-ONLY-FIX."],
                    &["user:carol@acme.com"],
                ),
                pr(
                    13,
                    "Close the export file handle",
                    "Fixes API-SERVICE-9Z (https://acme.sentry.io/issues/999/): the writer closed its file before the \
                     last batch.",
                    &["services/export/writer.py", "services/export/test_writer.py"],
                ),
                pr(
                    15,
                    "ENG-77: tidy the export job",
                    "Moves code around.",
                    &["services/export/job.py", "services/export/writer.py"],
                ),
            ],
            &[],
        )
        .await
        .unwrap();

    // Unset: an error question is answered exactly as before, over HTTP and MCP.
    let before = ask(s, &eng_reader, ASKED).await;
    assert_eq!(before, json!({"situation": null, "advice": null}));
    let (_, raw) = mcp_call(s, "", &eng_reader, "get_advice", json!({"question": ASKED})).await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    assert_eq!(text, "No advice for this kind of question.");

    // Set on team eng: the past error like the question's, with its fix; never the question's own issue.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "team": "eng", "settings": {"error_leads": true}}),
    )
    .await;
    let got = ask_until(s, &eng_reader, ASKED, |g| g.get("leads").is_some()).await;
    assert_eq!(got["situation"], Value::Null, "{got}");
    let led = led_to(&got);
    assert_eq!(led[0], "API-SERVICE-1A", "{got}");
    assert!(
        led.len() <= 3 && !led.iter().any(|l| l == "API-SERVICE-1Q"),
        "{got}"
    );
    let lead = &got["leads"][0];
    assert_eq!(lead["issue"]["link"], "https://acme.sentry.io/issues/1001/");
    assert_eq!(lead["issue"]["title"], "KeyError: 'org_id'");
    let why: Vec<&str> = lead["why"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert!(why.contains(&"same exception and culprit"), "{got}");
    assert!(why.contains(&"same error message"), "{got}");
    assert!(why.iter().any(|w| w.starts_with("shared frame")), "{got}");
    assert!(lead.get("scope").is_none(), "{got}");
    let fix = &lead["fixes"][0];
    assert_eq!(fix["pr"], "acme/app#11");
    assert_eq!(fix["link"], "https://github.com/acme/app/pull/11");
    assert_eq!(fix["title"], "Guard a missing org id in invoices");
    assert_eq!(fix["merged"], "2026-09-01");
    // The file of the question's frames first, then the rest; the description without its headings.
    assert_eq!(
        fix["files"],
        json!(["services/billing/invoice.py", "tests/test_invoice.py"])
    );
    assert_eq!(
        fix["description"],
        "Invoices read org_id before the org was loaded; now they wait for it."
    );

    // A pasted traceback, no short ID: the fix of an issue no episode holds, by the file it changed; the PR
    // naming only a Linear ticket isn't a fix.
    let got = ask_until(s, &eng_reader, TRACEBACK, |g| g.get("leads").is_some()).await;
    assert_eq!(led_to(&got)[0], "API-SERVICE-9Z", "{got}");
    let lead = &got["leads"][0];
    assert_eq!(lead["issue"]["link"], "https://acme.sentry.io/issues/999/");
    assert_eq!(lead["fixes"][0]["pr"], "acme/app#13");
    assert!(
        lead["why"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "the fix changed `services/export/writer.py`"),
        "{got}"
    );
    assert!(!got.to_string().contains("acme/app#15"), "{got}");

    // A question with no error in it is answered as before.
    let got = ask(s, &eng_reader, "How do we onboard a new customer?").await;
    assert_eq!(got, json!({"situation": null, "advice": null}));

    // MCP get_advice, from the reader key (no session): the leads after the answer.
    let (_, raw) = mcp_call(s, "", &eng_reader, "get_advice", json!({"question": ASKED})).await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    assert!(
        text.starts_with("No advice for this kind of question.\n\n## Similar past errors and how they were fixed\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "1. API-SERVICE-1A: KeyError: 'org_id' (https://acme.sentry.io/issues/1001/)"
        ),
        "{text}"
    );
    assert!(
        text.contains("Fixed by acme/app#11 \"Guard a missing org id in invoices\" (merged 2026-09-01): https://github.com/acme/app/pull/11"),
        "{text}"
    );

    // Acme reads eng and sets it for the org: its readers get eng's leads as each may read them, named by eng's
    // scope. Carol's issue (the same frame) reaches carol alone.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"reads": ["team:acme/eng"], "error_leads": true}}),
    )
    .await;
    let got = ask_until(s, &acme_reader, ASKED, |g| g.get("leads").is_some()).await;
    assert_eq!(led_to(&got)[0], "API-SERVICE-1A", "{got}");
    assert_eq!(got["leads"][0]["scope"], "team:acme/eng", "{got}");
    let got = ask(s, &bob, ASKED).await;
    assert_eq!(led_to(&got)[0], "API-SERVICE-1A", "{got}");
    assert!(!led_to(&got).iter().any(|l| l == "API-SERVICE-1C"), "{got}");
    assert!(!got.to_string().contains("CAROL-ONLY-FIX"), "{got}");
    // Carol's error shares the question's innermost frame, and its fix changed that frame's file: second.
    let got = ask(s, &carol, ASKED).await;
    assert_eq!(
        led_to(&got)[..2],
        ["API-SERVICE-1A", "API-SERVICE-1C"],
        "{got}"
    );
    assert!(got.to_string().contains("CAROL-ONLY-FIX"), "{got}");

    // Neither routing nor the leads called the model so far: there was nothing to route.
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // A person over MCP gets a session opened, the leads after its answer; that session's next step has them too.
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "get_advice",
        json!({"question": ASKED}),
    )
    .await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    assert!(
        text.contains("## Similar past errors and how they were fixed\n"),
        "{text}"
    );
    let id = text
        .rsplit_once("session_id: ")
        .and_then(|(_, r)| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("no session: {text}"));
    let (status, body) = post(
        &format!("{s}/advice"),
        &bob,
        &json!({"question": ASKED, "session_id": id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(led_to(&got)[0], "API-SERVICE-1A", "{got}");

    // A team's setting wins over its org's: off on eng, eng's readers are answered as before; acme's still get
    // their leads.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "team": "eng", "settings": {"error_leads": false}}),
    )
    .await;
    ask_until(s, &eng_reader, ASKED, |g| {
        *g == json!({"situation": null, "advice": null})
    })
    .await;
    assert_eq!(
        led_to(&ask(s, &acme_reader, ASKED).await)[0],
        "API-SERVICE-1A"
    );
}
