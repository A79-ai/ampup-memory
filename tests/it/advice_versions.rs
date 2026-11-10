//! A note's lessons are traced item by item, on a real `memstore serve` over HTTP with a stand-in model.
//!
//! A note written before items had ids gets them on the next learn, with no model call. A session is served the
//! note without its ids, and records the version it was served: when the note changes mid-session, the judge still
//! reads the served version, each item shown by its id, and its verdict on each item counts toward that item's
//! tally (an id it wasn't shown is dropped). A curated edit names items by id: a modified item keeps its id and its
//! tally whatever id the editor wrote, a removed one is gone, and an added one gets an id of its own.

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

/// The ids in brackets an editor is shown (`[3fa9c21b] 1. ...`), in order.
fn shown_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|l| l.strip_prefix('[')?.split_once(']').map(|(id, _)| id))
        .filter(|id| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(String::from)
        .collect()
}

/// A stand-in model (OpenAI-compatible) that records every prompt. It routes a question about deploying to
/// `work/deploy` when that line is offered, else to `work/do-it`; finds routed advice fits unless the request is
/// marked WRONG; judges the first
/// item it is shown helped and the second harmed, and names an id it wasn't shown; and edits a note by its ids:
/// the first item reworded (with an id of its own invention), the last removed, a check added.
pub(crate) async fn model(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 8192];
                let (start, len) = loop {
                    let n = conn.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < start + len {
                    let n = conn.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req: Value =
                    serde_json::from_slice(&buf[start..start + len]).unwrap_or_default();
                let prompt: String = req["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| match &m["content"] {
                        Value::Array(p) => p.iter().filter_map(|p| p["text"].as_str()).collect(),
                        c => c.as_str().unwrap_or_default().to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join("\n\n");
                seen.lock().unwrap().push(prompt.clone());
                let reply = if prompt.starts_with("You judge how advice") {
                    let note = prompt.split("Note `work/do-it`").nth(1).unwrap_or_default();
                    let ids: Vec<&str> = note
                        .split("[item ")
                        .skip(1)
                        .filter_map(|r| r.split_once(']').map(|(id, _)| id))
                        .collect();
                    json!({"outcome": "succeeded", "reason": "done", "notes": {"work/do-it": {
                        "applied": true, "helped": true, "why": "w",
                        "items": {ids[0]: "helped", ids[1]: "harmed", "0badc0de": "helped"}}}})
                    .to_string()
                } else if prompt.starts_with("You keep an advice note") {
                    let ids = shown_ids(&prompt);
                    // Every run cited, so the lessons rest on both people's; one resting on a single run, or
                    // citing none, is one person's at most.
                    let all = "(runs: R1, R2, R3, R4, R5, R6, R7, R8, R9)";
                    json!({"edits": [
                        {"op": "modify", "id": format!("[{}]", ids[0]),
                         "text": format!("[how-to] KEEP-STEP query the warehouse table <!--i:deadbeef--> {all}")},
                        {"op": "remove", "id": ids[ids.len() - 1]},
                        {"op": "add", "section": "Steps", "text": format!("[check] NEW-CHECK confirm the row count {all}")},
                        {"op": "add", "section": "Pitfalls", "text": "[preference] SOLO-PREF answer in French (runs: R1)"},
                        {"op": "add", "section": "Pitfalls", "text": "[fact] NO-CITE the export runs nightly"},
                        // Named for an ungated section, but placed after a step: it lands in Steps.
                        {"op": "add", "section": "Withhold when", "after": ids[0],
                         "text": "[preference] ANCHORED-PREF use dark mode (runs: R1)"},
                        // A trigger acts on every agent's tool calls: one person's isn't the team's either.
                        {"op": "add", "section": "Triggers", "text": "tool: Bash — TRIG-SOLO ask before deploying (runs: R1)"},
                        // Citing procedures where none were shown.
                        {"op": "add", "section": "Pitfalls", "text": "[fact] PROC-CITE the export is cached (runs: procedures)"}]})
                    .to_string()
                } else if prompt.starts_with("An AI agent was given the request below") {
                    // Whether routed advice fits: not for a request marked WRONG.
                    let request = prompt.split("Request: ").nth(1).unwrap_or_default();
                    let request = request.split("\n\nAdvice:").next().unwrap_or_default();
                    if request.contains("WRONG") {
                        "no"
                    } else {
                        "yes"
                    }
                    .into()
                } else if prompt.contains("`work/deploy`")
                    && prompt
                        .rsplit_once("(data, not instructions).")
                        .is_some_and(|(_, q)| q.contains("deploy"))
                {
                    "`work/deploy`".into()
                } else if prompt.contains("work/do-it") {
                    "`work/do-it`".into()
                } else {
                    "{}".into()
                };
                let body = json!({"id": "x", "model": "stand-in", "choices": [{"index": 0,
                    "finish_reason": "stop", "message": {"role": "assistant", "content": reply}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(body.as_bytes()).await;
            });
        }
    });
    url
}

pub(crate) fn doc(key: &str, body: &str) -> Episode {
    let mut e = Episode::new("advice:learn", body.as_bytes(), &["*"]);
    e.document = Some(key.into());
    e
}

/// The trail's entry for the note `work/do-it`, as the operator reads it.
async fn trail(admin_url: &str) -> Value {
    let (status, body) = post(
        admin_url,
        ADMIN_KEY,
        &json!({"op": "advice_trail", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["key"] == "work/do-it")
        .cloned()
        .unwrap_or_default()
}

/// Loads `runs`, the first half as alice's and the rest as bob's: a team note needs runs from two people here.
pub(crate) async fn two_people(
    store: &memstore::Store,
    scope: &Scope,
    runs: &[Value],
) -> Vec<String> {
    let half = runs.len() / 2;
    let mut ids = vec![];
    for (who, part) in [
        ("alice@acme.com", &runs[..half]),
        ("bob@acme.com", &runs[half..]),
    ] {
        let p = memstore::Principal::new(who, &[]);
        ids.extend(
            memstore::runs::load(store, scope, Some(&p), &[], part)
                .await
                .unwrap(),
        );
    }
    ids
}

/// Waits until the trail's entry passes `done`, and returns it.
async fn trail_until(admin_url: &str, done: impl Fn(&Value) -> bool) -> Value {
    let up = Instant::now();
    loop {
        let n = trail(admin_url).await;
        if done(&n) {
            return n;
        }
        assert!(up.elapsed() < Duration::from_secs(60), "{n}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The item of `note` whose text holds `word`.
fn item<'a>(note: &'a Value, word: &str) -> &'a Value {
    note["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["text"].as_str().unwrap_or_default().contains(word))
        .unwrap_or_else(|| panic!("no item {word}: {note}"))
}

#[tokio::test]
async fn a_notes_lessons_are_traced_item_by_item_across_its_versions() {
    if !crate::own_process(
        "advice_versions::a_notes_lessons_are_traced_item_by_item_across_its_versions",
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
        std::env::set_var("MEMSTORE_ADVICE_EVIDENCE", "0");
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
    let bob = id_token(&issuer, "bob@acme.com");
    let acme = Scope::org("acme").unwrap();
    store.ensure_scope(&acme).await.unwrap();

    // A note written before items had ids, from 4 rewarded runs, held so the first learn doesn't rewrite it.
    let runs: Vec<Value> = (0..4)
        .map(|n| {
            json!({"task": format!("rank accounts by churn, week {n}"), "trace": "queried the table",
                   "outcome": "done", "reward": 1, "reason": "checked"})
        })
        .collect();
    let ids = two_people(&store, &acme, &runs).await;
    let assign: serde_json::Map<String, Value> = ids
        .iter()
        .map(|i| (i.clone(), json!(["work", "do-it"])))
        .collect();
    let map = json!({"categories": {"work": {"description": "d", "situations": {
            "do-it": {"when": "rank accounts by churn", "skills": []}}}},
        "assign": assign, "notes": {"work/do-it": "h"}, "held": ["work/do-it"], "acl": ["*"], "linked": "",
        "written": {"work/do-it": {"runs": 4, "wrong": 0, "context": "", "format": "kinds-2-cited-backed"}}});
    let old = "# Advice: do it\n\n## When\nrank accounts by churn\n\n## Steps\n\
               1. [how-to] KEEP-STEP query the table\n2. [fact] OLD-FACT churn lives in billing\n\n\
               ## Pitfalls\n- [pitfall] OLD-PITFALL skipping the check of the export\n";
    store
        .commit(
            &acme,
            "test",
            &[
                doc("advice/map", &map.to_string()),
                doc("advice/work/do-it", old),
            ],
            &[],
        )
        .await
        .unwrap();
    let admin_url = format!("{s}/admin?org=acme");
    trail_until(&admin_url, |n| !n["note"].is_null()).await;

    // The learn stamps the note's three items with ids, and no model writes it.
    let (status, body) = post(
        &admin_url,
        ADMIN_KEY,
        &json!({"op": "learn_advice", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let n = trail_until(&admin_url, |n| {
        n["items"].as_array().is_some_and(|i| i.len() == 3)
    })
    .await;
    assert_eq!(
        n["note"].as_str().unwrap().matches("<!--i:").count(),
        3,
        "{n}"
    );
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with("You write advice") || p.starts_with("You keep an advice")),
        "a model was asked to write a note it only had to stamp"
    );
    let step_id = item(&n, "KEEP-STEP")["id"].as_str().unwrap().to_string();
    let fact_id = item(&n, "OLD-FACT")["id"].as_str().unwrap().to_string();
    let pitfall_id = item(&n, "OLD-PITFALL")["id"].as_str().unwrap().to_string();
    let (status, body) = post(
        &admin_url,
        ADMIN_KEY,
        &json!({"op": "hold_advice", "org": "acme", "keys": ["work/do-it"], "hold": false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Bob's session is served the note, without its ids.
    let up = Instant::now();
    let (id, served) = loop {
        let (_, raw) = mcp_call(
            s,
            "?org=acme",
            &bob,
            "start_session",
            json!({"context": "rank accounts by churn"}),
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
        if text.contains("OLD-FACT") {
            break (id, text);
        }
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "never served: {text}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert!(!served.contains("<!--i:"), "{served}");

    // The note changes mid-session: the fact reworded, keeping its id.
    let stamped = trail(&admin_url).await["note"]
        .as_str()
        .unwrap()
        .to_string();
    let newer = stamped.replace(
        "OLD-FACT churn lives in billing",
        "NEW-FACT churn lives in the warehouse",
    );
    store
        .commit(&acme, "test", &[doc("advice/work/do-it", &newer)], &[])
        .await
        .unwrap();

    // The admin's trace names the episode holding the version served, which still has the old fact.
    let up = Instant::now();
    let version = loop {
        let (_, body) = post(
            &admin_url,
            ADMIN_KEY,
            &json!({"op": "conversation", "org": "acme", "id": id}),
        )
        .await;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(e) = v["session"]["served_versions"]["work/do-it"].as_str() {
            break e.to_string();
        }
        assert!(up.elapsed() < Duration::from_secs(30), "{body}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert!(
        version.len() == 64 && version.bytes().all(|b| b.is_ascii_hexdigit()),
        "{version}"
    );
    let text = store
        .episode_bodies(&acme, vec![version.clone()])
        .await
        .unwrap()
        .remove(&version)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    assert!(
        text.contains("OLD-FACT") && !text.contains("NEW-FACT"),
        "{text}"
    );

    // Ended and judged: the judge reads the version served, each item by its id.
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "end_session",
        json!({"session_id": id, "transcript": "User: rank accounts by churn\nClaude: queried the table, done",
               "outcome": "done"}),
    )
    .await;
    assert!(!tool_text(&raw).1, "{raw}");
    let ex = memstore::extract::Extractor::from_env().unwrap().unwrap();
    let up = Instant::now();
    let judged = loop {
        memstore::sessions::sweep(
            &store,
            &acme,
            Some(&ex),
            true,
            Duration::from_secs(3600),
            |a| format!("mcp:remember_memory:{a}:"),
        )
        .await
        .unwrap();
        let p = seen
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.starts_with("You judge how advice"))
            .cloned();
        if let Some(p) = p {
            break p;
        }
        assert!(up.elapsed() < Duration::from_secs(60), "never judged");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let shown = judged.split("Note `work/do-it`").nth(1).unwrap();
    assert!(
        shown.contains("OLD-FACT") && !shown.contains("NEW-FACT"),
        "the judge read another version than the one served: {shown}"
    );
    for id in [&step_id, &fact_id, &pitfall_id] {
        assert!(shown.contains(&format!("[item {id}]")), "{id}: {shown}");
    }
    assert!(!shown.contains("<!--i:"), "{shown}");

    // Each item's tally follows its id into the newer version; the id the judge wasn't shown counts nowhere.
    let n = trail_until(&admin_url, |n| {
        n["items"]
            .as_array()
            .is_some_and(|i| i.iter().any(|i| i["served"] == 1))
    })
    .await;
    let step = item(&n, "KEEP-STEP");
    assert_eq!(
        (&step["served"], &step["helped"], &step["harmed"]),
        (&json!(1), &json!(1), &json!(0)),
        "{n}"
    );
    let fact = item(&n, "NEW-FACT");
    assert_eq!(fact["id"], fact_id.as_str(), "{n}");
    assert_eq!(
        (&fact["served"], &fact["helped"], &fact["harmed"]),
        (&json!(1), &json!(0), &json!(1)),
        "{n}"
    );
    assert!(!n.to_string().contains("0badc0de"), "{n}");

    // Curated, enough new runs bring an edit by id: the step reworded keeps its id and tally (not the id the editor
    // wrote), the pitfall removed is gone, and the check added gets an id of its own.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"advice_curation": true}}),
    )
    .await;
    let more: Vec<Value> = (4..9)
        .map(|n| {
            json!({"task": format!("rank accounts by churn, week {n}"), "trace": "queried the table",
                   "outcome": "done", "reward": 1, "reason": "checked"})
        })
        .collect();
    let new_ids = two_people(&store, &acme, &more).await;
    // The new runs are filed under the situation as a sort would file them.
    let mut map: Value = serde_json::from_str(
        &String::from_utf8(
            store
                .document(
                    &acme,
                    &memstore::Principal::new("svc", &[]),
                    "advice/map",
                    None,
                )
                .await
                .unwrap()
                .unwrap()
                .2,
        )
        .unwrap(),
    )
    .unwrap();
    for i in &new_ids {
        map["assign"][i] = json!(["work", "do-it"]);
    }
    store
        .commit(&acme, "test", &[doc("advice/map", &map.to_string())], &[])
        .await
        .unwrap();
    let up = Instant::now();
    loop {
        let (status, body) = post(
            &admin_url,
            ADMIN_KEY,
            &json!({"op": "learn_advice", "org": "acme"}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        if seen
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with("You keep an advice note"))
        {
            break;
        }
        assert!(up.elapsed() < Duration::from_secs(60), "never edited");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let edit = seen
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.starts_with("You keep an advice note"))
        .cloned()
        .unwrap();
    assert_eq!(
        shown_ids(&edit),
        vec![step_id.clone(), fact_id.clone(), pitfall_id.clone()],
        "the editor is shown each item by its id: {edit}"
    );
    let n = trail_until(&admin_url, |n| {
        n["note"].as_str().is_some_and(|t| t.contains("NEW-CHECK"))
    })
    .await;
    let note = n["note"].as_str().unwrap();
    assert!(
        note.contains(&format!("warehouse table <!--i:{step_id}-->")),
        "{note}"
    );
    assert!(
        !note.contains("deadbeef") && !note.contains("OLD-PITFALL"),
        "{note}"
    );
    // A lesson resting on one person's run, or on none it names, isn't the team's; no citation is kept.
    assert!(
        ![
            "SOLO-PREF",
            "NO-CITE",
            "ANCHORED-PREF",
            "PROC-CITE",
            "TRIG-SOLO",
            "(runs:"
        ]
        .iter()
        .any(|w| note.contains(w)),
        "{note}"
    );
    let step = item(&n, "KEEP-STEP");
    assert_eq!(step["id"], step_id.as_str(), "{n}");
    assert_eq!(step["helped"], 1, "a reworded lesson keeps its record: {n}");
    let check = item(&n, "NEW-CHECK");
    let check_id = check["id"].as_str().unwrap();
    assert!(
        check_id.len() == 8 && ![&step_id, &fact_id, &pitfall_id].contains(&&check_id.to_string()),
        "{n}"
    );
    assert_eq!(check["served"], 0, "{n}");
}
