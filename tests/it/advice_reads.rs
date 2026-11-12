//! Advice across the scopes a scope reads (`Settings::reads`): a real `memstore serve` over HTTP, org acme and its
//! team eng, the routing model a local stand-in that names the line whose text holds the question's word and records
//! every prompt.
//!
//! Once the operator lets org acme read team eng, acme's readers are routed over eng's compiled skills and
//! situations too, on one list with acme's own: a line from eng is keyed `team:acme/eng#<key>`, served from eng and
//! named by its scope. Each is read as the caller, so an eng skill only carol may read reaches carol through acme and
//! no one else, not even the routing model's prompt. An eng note is served whatever its evidence, as every note is,
//! and as procedures are (a session doesn't count it). Team eng's own readers don't take acme's lines, and with `reads`
//! unset acme is advised from its own memory alone again.

use std::sync::{Arc, Mutex};
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

/// The words questions ask by, each in exactly one line's text.
const WORDS: [&str; 4] = ["xdeadlock", "xonboard", "xsecret", "xtriage"];

/// A routing model (OpenAI-compatible) that answers with the key of the line whose text holds the word the question
/// asks by, or `none`, and records every prompt.
async fn router(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
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
                let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                // Every message, in order: the router sends its instructions and index as a system part
                // (a string, or text parts with a cache marker), and the question as the user message.
                let prompt = req["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| match &m["content"] {
                        Value::Array(parts) => {
                            parts.iter().filter_map(|p| p["text"].as_str()).collect()
                        }
                        c => c.as_str().unwrap_or_default().to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join("\n\n");
                seen.lock().unwrap().push(prompt.clone());
                // The question is after the instructions; each index line is "- `key`: text".
                let question = prompt
                    .rsplit_once("(data, not instructions).")
                    .map_or("", |(_, q)| q);
                let word = WORDS.iter().find(|w| question.contains(*w));
                let pick = word.and_then(|w| {
                    prompt.lines().find_map(|l| {
                        let (key, text) = l.strip_prefix("- `")?.split_once("`: ")?;
                        text.contains(w).then(|| key.to_string())
                    })
                });
                // Whether routed advice fits the request: it does.
                let content = if prompt.contains("Is the advice for this very task") {
                    "yes".to_string()
                } else {
                    pick.map_or("none".to_string(), |k| format!("`{k}`"))
                };
                let reply = json!({"id": "x", "model": "stand-in", "choices": [{"index": 0,
                    "finish_reason": "stop", "message": {"role": "assistant", "content": content}}],
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

fn doc(source: &str, key: &str, body: &str, acl: &[&str]) -> Episode {
    let mut e = Episode::new(source, body.as_bytes(), acl);
    e.document = Some(key.into());
    e
}

/// A compiled skill whose own steps (no AIP graph) carry `text`.
fn skill(slug: &str, text: &str, acl: &[&str]) -> Episode {
    let meta = json!({"slug": slug, "acl": acl, "sources": [], "compiler": "test"});
    doc(
        "compile:skills",
        &format!("skills/{slug}"),
        &format!(
            "<!-- memstore skill {meta} -->\n# {slug}\n\n**When:** {slug}\n\n1. {text}\n\n**Sources:** sentry:{slug}\n"
        ),
        acl,
    )
}

fn index(name: &str, lines: &[(&str, &str)], acl: &[&str]) -> Episode {
    let body: String = lines
        .iter()
        .map(|(slug, when)| format!("- `{slug}`: {when} (`aip/{slug}`)\n"))
        .collect();
    doc("compile:skills", &format!("skills/{name}"), &body, acl)
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

#[tokio::test]
async fn advice_routes_over_the_scopes_a_scope_reads_as_the_caller_may_read_them() {
    if !crate::own_process(
        "advice_reads::advice_routes_over_the_scopes_a_scope_reads_as_the_caller_may_read_them",
    ) {
        return;
    }
    let seen = Arc::new(Mutex::new(vec![]));
    let model = router(seen.clone()).await;
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
        std::env::set_var("AMPUP_MEMORY_ADVICE_EVIDENCE", "3");
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

    // Org acme and its team eng; bob is in acme alone, carol in eng too.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "eng"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["eng"]}]}),
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

    // Acme's own skill; eng's skill for everyone in it, one only carol may read, and a situation with a note.
    let acme = Scope::org("acme").unwrap();
    let eng = Scope::team("acme", "eng").unwrap();
    store.ensure_scope(&acme).await.unwrap();
    store.ensure_scope(&eng).await.unwrap();
    store
        .commit(
            &acme,
            "test",
            &[
                skill("onboard-a-customer", "ACME-SKILL-TEXT", &["*"]),
                index(
                    "index~all",
                    &[("onboard-a-customer", "a new customer is set up xonboard")],
                    &["*"],
                ),
            ],
            &[],
        )
        .await
        .unwrap();
    let map = json!({"categories": {"alerts": {"description": "d", "situations": {
            "triage-an-alert": {"when": "an alert fires and needs an owner xtriage", "skills": []}}}},
        "assign": {}, "notes": {"alerts/triage-an-alert": "h"}, "acl": ["*"], "linked": "",
        "written": {"alerts/triage-an-alert": {"runs": 4, "wrong": 2, "context": ""}}});
    store
        .commit(
            &eng,
            "test",
            &[
                skill("fix-the-dedup-deadlock", "ENG-SKILL-TEXT", &["*"]),
                index(
                    "index~all",
                    &[(
                        "fix-the-dedup-deadlock",
                        "contact dedup hits a postgres deadlock xdeadlock",
                    )],
                    &["*"],
                ),
                skill("carols-own-fix", "CAROL-SKILL-TEXT", &["user:carol@acme.com"]),
                index(
                    "index~carol",
                    &[("carols-own-fix", "a fix only carol reads xsecret")],
                    &["user:carol@acme.com"],
                ),
                doc("advice:learn", "advice/map", &map.to_string(), &["*"]),
                doc(
                    "advice:learn",
                    "advice/alerts/triage-an-alert",
                    "# Advice: triage an alert\n\n## When\nan alert fires\n\n## Steps\n1. [fact] ENG-NOTE-TEXT\n",
                    &["*"],
                ),
            ],
            &[],
        )
        .await
        .unwrap();

    // The server reads a snapshot up to ~1 s old: wait until acme's own skill reaches its reader.
    let up = Instant::now();
    loop {
        let got = ask(s, &acme_reader, "how do I xonboard someone").await;
        if got["situation"] == "skill:onboard-a-customer" {
            assert!(got.get("scope").is_none(), "{got}");
            assert!(
                got["advice"].as_str().unwrap().contains("ACME-SKILL-TEXT"),
                "{got}"
            );
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Before acme reads eng, eng's lines reach none of acme's readers, nor the routing model.
    for bearer in [&acme_reader, &bob, &carol] {
        let got = ask(s, bearer, "contact dedup xdeadlock, again").await;
        assert_eq!(got["situation"], Value::Null, "{got}");
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("fix-the-dedup-deadlock")),
        "an eng line was routed over before acme read eng"
    );

    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"reads": ["team:acme/eng"]}}),
    )
    .await;
    // The setting reaches the server's directory within a few seconds.
    let up = Instant::now();
    while ask(s, &acme_reader, "contact dedup xdeadlock, again").await["situation"].is_null() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "acme never read eng"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Eng's skill, for acme's reader key and for a member of acme alone, served from eng and named by its scope.
    for bearer in [&acme_reader, &bob] {
        let got = ask(s, bearer, "contact dedup xdeadlock, again").await;
        assert_eq!(
            got["situation"], "team:acme/eng#skill:fix-the-dedup-deadlock",
            "{got}"
        );
        assert_eq!(got["scope"], "team:acme/eng", "{got}");
        assert!(
            got["advice"].as_str().unwrap().contains("ENG-SKILL-TEXT"),
            "{got}"
        );
        assert_eq!(
            got["skills"][0]["key"], "team:acme/eng#skill:fix-the-dedup-deadlock",
            "{got}"
        );
        assert_eq!(got["skills"][0]["from"], "documents", "{got}");
        // Acme's own line still answers for itself, on the same list.
        let got = ask(s, bearer, "how do I xonboard someone").await;
        assert_eq!(got["situation"], "skill:onboard-a-customer", "{got}");
        assert!(got.get("scope").is_none(), "{got}");
    }

    // Carol's own eng skill reaches carol through acme, and no one else: not their answer, not the model's prompt.
    let before = seen.lock().unwrap().len();
    for bearer in [&acme_reader, &bob] {
        let got = ask(s, bearer, "the xsecret fix").await;
        assert_eq!(got["situation"], Value::Null, "{got}");
        assert!(!got.to_string().contains("CAROL-SKILL-TEXT"), "{got}");
    }
    assert!(
        seen.lock().unwrap()[before..]
            .iter()
            .all(|p| !p.contains("carols-own-fix") && !p.contains("a fix only carol reads")),
        "carol's skill reached the routing model for someone else"
    );
    let got = ask(s, &carol, "the xsecret fix").await;
    assert_eq!(
        got["situation"], "team:acme/eng#skill:carols-own-fix",
        "{got}"
    );
    assert!(
        got["advice"].as_str().unwrap().contains("CAROL-SKILL-TEXT"),
        "{got}"
    );

    // Eng's note is served from acme though it isn't past eng's evidence bar (3 people): every note is served.
    let up = Instant::now();
    loop {
        let got = ask(s, &bob, "an xtriage alert fired").await;
        if got["situation"] == "team:acme/eng#alerts/triage-an-alert" {
            assert_eq!(got["scope"], "team:acme/eng", "{got}");
            assert!(
                got["advice"].as_str().unwrap().contains("ENG-NOTE-TEXT"),
                "{got}"
            );
            assert_eq!(got["skills"][0]["from"], "runs", "{got}");
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // MCP get_advice, from a reader key (no session) and from a person (who gets a session): the same line.
    let (_, raw) = mcp_call(
        s,
        "",
        &acme_reader,
        "get_advice",
        json!({"question": "contact dedup xdeadlock, again"}),
    )
    .await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    assert!(
        text.starts_with("Situation: `team:acme/eng#skill:fix-the-dedup-deadlock`"),
        "{text}"
    );
    assert!(text.contains("ENG-SKILL-TEXT"), "{text}");
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "get_advice",
        json!({"question": "contact dedup xdeadlock, again"}),
    )
    .await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    assert!(text.contains("ENG-SKILL-TEXT"), "{text}");
    assert!(text.contains("session_id: "), "{text}");

    // Team eng's own readers don't take acme's lines: a team never takes its org's reads.
    let got = ask(s, &eng_reader, "how do I xonboard someone").await;
    assert_eq!(got["situation"], Value::Null, "{got}");
    let got = ask(s, &eng_reader, "contact dedup xdeadlock, again").await;
    assert_eq!(got["situation"], "skill:fix-the-dedup-deadlock", "{got}");
    assert!(got.get("scope").is_none(), "{got}");

    // Unset, acme is advised from its own memory alone again.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"reads": []}}),
    )
    .await;
    let up = Instant::now();
    loop {
        let got = ask(s, &bob, "contact dedup xdeadlock, again").await;
        if got["situation"].is_null() {
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
