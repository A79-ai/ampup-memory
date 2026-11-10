//! Review checks keyed by files (`POST /advice {files}`) read only what the caller may: a real `memstore serve`
//! over HTTP, callers authenticating with API keys, the condensing model a local stand-in that records what it was
//! shown. A skill, a pull request or an issue version the caller may not read never reaches the answer or the
//! model, and a readable skill that cites a pull request the caller may not read is left out whole. A condensing
//! reply the server kept is served again only to a caller whose own readable skills make the same request.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memstore::auth::{self, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const READER_KEY: &str = "msk_reader_key_for_advice_files";
const OUTSIDER_KEY: &str = "msk_outsider_key_for_advice_files";
const TEAM_B_KEY: &str = "msk_team_b_key_for_advice_files";

/// A model endpoint that records each request body and answers one bullet.
async fn stand_in_model(seen: Arc<Mutex<Vec<String>>>) -> u16 {
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
                // Headers, then as much body as Content-Length says.
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
                seen.lock().unwrap().push(body);
                let reply = json!({"id": "x", "model": "stand-in", "choices": [{"index": 0,
                    "finish_reason": "stop", "message": {"role": "assistant",
                    "content": "- `app/a.py`: keep the retry budget (#1)"}}],
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

fn pr(n: u64, files: &[&str], acl: &[&str]) -> Episode {
    let list: String = files.iter().map(|f| format!("- {f} (+1 −1)\n")).collect();
    let body = format!(
        "# fix: change {n} (pr acme/app#{n}, merged 2026-09-0{n})\nPR {n}.\n\n## Files changed\n{list}"
    );
    let mut e = Episode::new(&format!("github:acme/app#{n}"), body.as_bytes(), acl);
    e.occurred_at = 1_788_000_000_000_000 + n as i64;
    e
}

fn doc(key: &str, body: &str, acl: &[&str]) -> Episode {
    let mut e = Episode::new("compile:skills", body.as_bytes(), acl);
    e.document = Some(key.into());
    e
}

fn skill(slug: &str, cites: &[&str], text: &str, acl: &[&str]) -> Episode {
    let meta = json!({"slug": slug, "acl": acl, "sources": [], "names": cites, "compiler": "test"});
    doc(
        &format!("skills/{slug}"),
        &format!("<!-- memstore skill {meta} -->\n# {slug}\n\n{text}\n"),
        acl,
    )
}

fn issue(version: &str, at: i64, acl: &[&str]) -> Episode {
    let mut e = Episode::new(
        "linear:acme/ENG-1",
        format!("# ENG-1 retries\n{version}").as_bytes(),
        acl,
    );
    e.occurred_at = at;
    e
}

#[tokio::test]
async fn advice_by_files_serves_only_what_the_caller_may_read() {
    if !crate::own_process("advice_files::advice_by_files_serves_only_what_the_caller_may_read") {
        return;
    }
    let seen = Arc::new(Mutex::new(vec![]));
    let model = stand_in_model(seen.clone()).await;
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe {
        std::env::set_var("MEMSTORE_ADVICE_ROUTE", "openrouter");
        std::env::set_var(
            "MEMSTORE_ADVICE_ROUTE_URL",
            format!("http://127.0.0.1:{model}/v1"),
        );
        std::env::set_var("MEMSTORE_ADVICE_ROUTE_KEY", "stand-in");
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
    }

    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let (team, all, other) = (&["group:team-a"][..], &["*"][..], &["group:team-b"][..]);
    store
        .commit(
            &Scope::Org,
            "test",
            &[
                pr(1, &["app/a.py"], team),
                pr(2, &["app/a.py", "app/b.py"], all),
                pr(3, &["app/a.py"], other),
                // The team's skill, from the team's PR: the reader's only check.
                skill("keep-retries", &["github:acme/app#1"], "TEAM-SKILL-TEXT", team),
                doc("skills/index~a", "- `keep-retries`: when retrying (`aip/keep-retries`)\n", team),
                // Everyone's skill, from a PR only team-b reads: nobody here gets it, nor its text.
                skill("from-team-b", &["github:acme/app#3"], "TEAM-B-PR-SKILL-TEXT", all),
                // team-b's skill, listed in everyone's index: listed, unreadable.
                skill("team-b-only", &["github:acme/app#2"], "TEAM-B-SKILL-TEXT", other),
                doc(
                    "skills/index~all",
                    "- `from-team-b`: when (`aip/from-team-b`)\n- `team-b-only`: when (`aip/team-b-only`)\n",
                    all,
                ),
                issue("ISSUE-FOR-EVERYONE", 1_788_000_000_000_010, all),
                issue("ISSUE-FOR-THE-TEAM", 1_788_000_000_000_020, team),
                issue("ISSUE-FOR-TEAM-B", 1_788_000_000_000_030, other),
            ],
            &[],
        )
        .await
        .unwrap();

    let key_row = |id: &str, key: &str, groups: &[&str]| {
        json!({"id": id, "sha256": auth::hex(&sha2::Sha256::digest(key.as_bytes())),
               "user": format!("svc:{id}"), "groups": groups, "admin": false})
    };
    let keys = json!([
        key_row("reader", READER_KEY, &["team-a"]),
        key_row("outsider", OUTSIDER_KEY, &[]),
        key_row("team-b", TEAM_B_KEY, &["team-b"])
    ]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            catalog: Some(catalog),
            token_auth: Some(TokenAuth::new(Some(&keys.to_string()), None).unwrap()),
            ..Default::default()
        },
    ));
    let http = reqwest::Client::new();
    let up = Instant::now();
    while http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_err()
    {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ask = |key: &'static str| {
        let http = http.clone();
        async move {
            let r = http
                .post(format!("http://127.0.0.1:{port}/advice"))
                .bearer_auth(key)
                .json(
                    &json!({"files": ["app/a.py"], "title": "retry less", "pr": 9,
                              "repo": "acme/app", "issues": ["eng-1"]}),
                )
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            let body: Value = r.json().await.unwrap();
            assert_eq!(status, 200, "{body}");
            body
        }
    };

    let got = ask(OUTSIDER_KEY).await;
    assert_eq!(got["skills"], json!([]), "{got}");
    assert_eq!(got["checks"], json!([]), "{got}");
    assert_eq!(got["issues"].as_array().unwrap().len(), 1, "{got}");
    assert!(
        got["issues"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ISSUE-FOR-EVERYONE"),
        "{got}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "the model was asked for a caller with nothing to condense"
    );

    let got = ask(READER_KEY).await;
    let keys: Vec<&str> = got["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["skill:keep-retries"], "{got}");
    assert_eq!(got["skills"][0]["sources"], json!(["acme/app#1"]), "{got}");
    assert_eq!(
        got["checks"],
        json!(["- `app/a.py`: keep the retry budget (#1)"]),
        "{got}"
    );
    let text = got["issues"][0]["text"].as_str().unwrap();
    assert!(text.contains("ISSUE-FOR-THE-TEAM"), "{got}");
    let all = got.to_string();
    for secret in [
        "ISSUE-FOR-TEAM-B",
        "TEAM-B-PR-SKILL-TEXT",
        "TEAM-B-SKILL-TEXT",
        "from-team-b",
        "team-b-only",
    ] {
        assert!(!all.contains(secret), "{secret} reached the answer: {got}");
    }
    let asked = seen.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one condensing call");
    assert!(asked[0].contains("TEAM-SKILL-TEXT"), "{}", asked[0]);
    for secret in ["TEAM-B-PR-SKILL-TEXT", "TEAM-B-SKILL-TEXT", "ISSUE-FOR"] {
        assert!(!asked[0].contains(secret), "{secret} reached the model");
    }

    // The reader's condensing reply is kept: asked again, the same answer, with no model call.
    let again = ask(READER_KEY).await;
    assert_eq!(again, got, "the kept reply answers the same request");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a kept reply asked the model"
    );
    // It never reaches a caller who can't read the reader's skill.
    let got = ask(OUTSIDER_KEY).await;
    assert_eq!(got["skills"], json!([]), "{got}");
    assert_eq!(got["checks"], json!([]), "{got}");
    assert_eq!(seen.lock().unwrap().len(), 1, "{got}");
    // Another team's same change is condensed from its own skills, in a request of its own.
    let got = ask(TEAM_B_KEY).await;
    let all = got.to_string();
    for secret in ["TEAM-SKILL-TEXT", "keep-retries", "ISSUE-FOR-THE-TEAM"] {
        assert!(!all.contains(secret), "{secret} reached team-b: {got}");
    }
    let asked = seen.lock().unwrap().clone();
    assert_eq!(
        asked.len(),
        2,
        "team-b's checks came from the reader's kept reply"
    );
    assert!(!asked[1].contains("TEAM-SKILL-TEXT"), "{}", asked[1]);
}
