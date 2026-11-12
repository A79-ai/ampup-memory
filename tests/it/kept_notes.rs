//! A person's preferences, kept from what they type, end to end: a real `ampup-memory serve` over HTTP on a local
//! catalog, called as memstore's hooks call it (`/mcp/hook`) and as an agent does (`/mcp`), its session sweep on a
//! one-second timer, and a stand-in for the chat models (judge, keep, extraction, sort, write) and for the decisions
//! model that grades requests.
//!
//! - Each message the person types is read for preferences once, as it reaches the server: the session's opening, a
//!   hook's question, the conversation a hook's Stop sends, the transcript at its end. The keep model sees each
//!   message with the end of the reply before it, and never an agent's own words, a recorder key's session, a hook's
//!   call with capture off, or a message it read before. What it keeps is filed under `keep:<category>:<topic>`
//!   after the person's signature, for them alone; a note they have already isn't kept again; their map and notes
//!   are what the model is shown next time. No caller writes under `keep`, and facts are never extracted from a
//!   kept note nor from a session's record (`AMPUP_MEMORY_EXTRACT_SESSIONS` is off). `/browse` files the notes and sessions for their owner only.
//! - What a session's work taught is advice's: each graded request is a run of the person's own advice
//!   (`<session>#<offset>`), and its `[preference]` items are handed at a session's start with their notes.
//! - Someone forgotten while the keep model ran gets nothing written after it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::isolation::{ADMIN_KEY, CLIENT_ID, id_token, start_idp};

/// How the keep prompt starts (`keep::find`).
const KEEP_PROMPT: &str = "Below, as JSON, are messages a user typed";

/// What the stand-in models were asked: every chat prompt, and every decisions request's state.
#[derive(Clone, Default)]
struct Seen {
    prompts: Arc<Mutex<Vec<String>>>,
    decided: Arc<Mutex<Vec<Value>>>,
    /// Keep prompts that reached the model.
    keep_asks: Arc<AtomicUsize>,
}

impl Seen {
    fn prompts(&self, start: &str) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(start))
            .cloned()
            .collect()
    }
    /// Every message the keep model was shown, in order: what the user said and the reply before it.
    fn typed(&self) -> Vec<(String, String)> {
        self.prompts(KEEP_PROMPT)
            .iter()
            .flat_map(|p| {
                let said = p.rsplit("What the user said:").next().unwrap();
                let json = &said[said.find('[').unwrap()..=said.rfind(']').unwrap()];
                serde_json::from_str::<Vec<Value>>(json)
                    .unwrap_or_else(|e| panic!("the messages as JSON: {e}\n{said}"))
            })
            .map(|t| {
                (
                    t["user_said"].as_str().unwrap().to_string(),
                    t["assistant_before"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
    /// The chat prompts that are none of the stand-in's known ones: extraction's.
    fn extraction_prompts(&self) -> Vec<String> {
        const KNOWN: [&str; 4] = [
            KEEP_PROMPT,
            "You judge how advice",
            "You keep the map",
            "You write advice",
        ];
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|p| !KNOWN.iter().any(|k| p.starts_with(k)))
            .cloned()
            .collect()
    }
    /// The requests each grading call was sent.
    fn graded(&self) -> Vec<Vec<String>> {
        self.decided
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| s["requests"].as_array())
            .map(|r| {
                r.iter()
                    .map(|r| r["request"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .collect()
    }
}

/// What the keep model keeps from a message whose words mention the first.
const NOTES: [(&str, &str, &str, &str); 6] = [
    (
        "tabs",
        "Indent with tabs, never spaces, in every repo.",
        "formatting",
        "indentation",
    ),
    (
        "pnpm",
        "Always use pnpm, never npm, for scripts in this repo.",
        "tooling",
        "package-manager",
    ),
    (
        "straight to main",
        "Never push straight to main: open a pull request and wait for review.",
        "version-control",
        "pushing-to-main",
    ),
    (
        "pin exact",
        "Pin exact dependency versions, never caret ranges.",
        "tooling",
        "versions",
    ),
    (
        "yarn",
        "Always use yarn for scripts in this repo.",
        "tooling",
        "package-manager",
    ),
    (
        "bun",
        "Always use bun for scripts in this repo.",
        "tooling",
        "package-manager",
    ),
];

/// The preference the writer puts in the person's own advice.
const LEARNED: &str = "Run the type check before pushing a CI change.";

/// A request's grade by the person's next message: "No…"/"Don't…" corrected it, "Thanks…"/"Great…" accepted it,
/// anything else showed nothing.
fn grade(next: &str) -> (&'static str, f64) {
    let n = next.trim_start();
    if n.starts_with("No") || n.starts_with("Don't") {
        ("corrected", 0.95)
    } else if n.starts_with("Thanks") || n.starts_with("Great") {
        ("accepted", 0.95)
    } else {
        ("neither", 0.9)
    }
}

/// The stand-in for every model the server calls: an OpenAI-compatible chat API, and the decisions API (OpenRouter's
/// shape) at `/decisions`. While `gate` is false, a keep prompt waits for it.
async fn models(seen: Seen, gate: Option<tokio::sync::watch::Receiver<bool>>) -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let (seen, gate) = (seen.clone(), gate.clone());
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 8192];
                let (start, len, path) = loop {
                    let n = conn.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let path = head
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or_default()
                            .to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len, path);
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
                let body = match path.ends_with("/decisions") {
                    true => decide(&seen, &req),
                    false => chat(&seen, gate, &req).await,
                }
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                conn.write_all(head.as_bytes()).await.ok();
                conn.write_all(body.as_bytes()).await.ok();
                conn.shutdown().await.ok();
            });
        }
    });
    url
}

/// The decisions model: grades each request by its next message ([`grade`]); answers no other question, so its
/// callers fall back as they do without one.
fn decide(seen: &Seen, req: &Value) -> Value {
    let state = &req["state"];
    seen.decided.lock().unwrap().push(state.clone());
    let mut answers = serde_json::Map::new();
    if let Some(requests) = state["requests"].as_array() {
        for name in req["questions"]
            .as_object()
            .into_iter()
            .flat_map(|q| q.keys())
        {
            let Some(n) = name.strip_prefix('r').and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            let (choice, confidence) =
                grade(requests[n - 1]["next message"].as_str().unwrap_or_default());
            answers.insert(
                name.clone(),
                json!({"type": "choice", "choice": choice, "confidence": confidence}),
            );
        }
    }
    json!({"answers": answers, "usage": {"cost": 0}})
}

/// The chat models: the judge says every session succeeded; the keep model keeps [`NOTES`] by what the user said;
/// the sorter files every task under one situation; the writer writes one note with a preference in it.
async fn chat(seen: &Seen, gate: Option<tokio::sync::watch::Receiver<bool>>, req: &Value) -> Value {
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
    seen.prompts.lock().unwrap().push(prompt.clone());
    let reply = if prompt.starts_with("You judge how advice") {
        json!({"outcome": "succeeded", "reason": "the user got what they asked", "notes": {}})
            .to_string()
    } else if prompt.starts_with(KEEP_PROMPT) {
        seen.keep_asks.fetch_add(1, Ordering::SeqCst);
        if let Some(mut g) = gate {
            let _ = g.wait_for(|open| *open).await;
        }
        // Only what the user said: never the reply before it.
        let said = prompt
            .rsplit("What the user said:")
            .next()
            .unwrap_or_default();
        let json = &said[said.find('[').unwrap_or(0)..=said.rfind(']').unwrap_or(0)];
        let users: Vec<String> = serde_json::from_str::<Vec<Value>>(json)
            .unwrap_or_default()
            .iter()
            .map(|t| t["user_said"].as_str().unwrap_or_default().to_string())
            .collect();
        let keep: Vec<Value> = NOTES
            .iter()
            .filter(|(w, ..)| users.iter().any(|u| u.contains(w)))
            .map(|(_, text, category, topic)| json!({"text": text, "category": category, "topic": topic}))
            .collect();
        json!({"keep": keep}).to_string()
    } else if prompt.starts_with("You keep the map") {
        let tasks: Vec<&str> = prompt
            .lines()
            .filter_map(|l| l.split_once(": ").map(|(h, _)| h))
            .filter(|h| {
                h.len() > 1 && h.starts_with('T') && h[1..].bytes().all(|b| b.is_ascii_digit())
            })
            .collect();
        json!({"map": {"work": {"description": "d", "situations": {"ci": {"when": "change the CI workflow"}}}},
               "assign": tasks.iter().map(|t| (t.to_string(), json!(["work", "ci"]))).collect::<serde_json::Map<_, _>>()})
        .to_string()
    } else if prompt.starts_with("You write advice") {
        // Every run shown cited: each lesson rests on all of them.
        let runs = (1..=40)
            .map(|n| format!("R{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "# Advice\n\n## When\nchange the CI workflow\n\n## Steps\n1. [how-to] lint with pnpm (runs: {runs})\n\
             2. [preference] {LEARNED} (runs: {runs})\n\n## Pitfalls\npushing before the type check\n"
        )
    } else {
        "{}".to_string()
    };
    json!({"choices": [{"message": {"content": reply}, "finish_reason": "stop"}],
           "usage": {"prompt_tokens": prompt.len() / 4, "completion_tokens": reply.len() / 4}})
}

/// A server on a local catalog with org `acme` (alice its admin, carol a member, advice on, each person's own advice
/// learned after one graded session), the sweep every second, and its models the stand-in at `models`.
struct Rig {
    _dir: tempfile::TempDir,
    store: Store,
    s: String,
    issuer: String,
    recorder: String,
    writer: String,
}

impl Rig {
    async fn start(models: &str) -> Rig {
        // SAFETY: each test here runs alone in its process (`own_process`).
        unsafe {
            for (k, v) in [
                ("AMPUP_MEMORY_MODEL_CACHE", "off"),
                ("AMPUP_MEMORY_EXTRACT", "openai"),
                ("AMPUP_MEMORY_EXTRACT_URL", models),
                ("AMPUP_MEMORY_EXTRACT_MODEL", "stand-in"),
                // Every episode is offered to extraction, so the one it is never offered is a choice.
                ("AMPUP_MEMORY_EXTRACT_GOAL_BLIND", "1"),
                ("AMPUP_MEMORY_ADVICE_ROUTE", "openrouter"),
                ("AMPUP_MEMORY_ADVICE_ROUTE_URL", models),
                ("AMPUP_MEMORY_ADVICE_ROUTE_KEY", "stand-in"),
                ("AMPUP_MEMORY_DECIDE", "openrouter"),
                ("AMPUP_MEMORY_DECIDE_URL", &format!("{models}/decisions")),
                ("AMPUP_MEMORY_DECIDE_KEY", "stand-in"),
                ("AMPUP_MEMORY_SESSION_SWEEP_SECS", "1"),
                ("AMPUP_MEMORY_SESSION_JUDGE_WAIT_SECS", "0"),
                ("AMPUP_MEMORY_SESSION_FLUSH_SECS", "1"),
                // The team's timer doesn't learn during a test.
                ("AMPUP_MEMORY_ADVICE_SECS", "3600"),
            ] {
                std::env::set_var(k, v);
            }
        }
        let issuer = start_idp().await;
        let dir = tempfile::tempdir().unwrap();
        let catalog = platform::local(&format!("file://{}", dir.path().display()))
            .await
            .unwrap();
        let store = Store::new(catalog.clone())
            .with_index_base(dir.path().join("index").display().to_string());
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
        let up = Instant::now();
        while reqwest::get(&s).await.is_err() {
            assert!(
                up.elapsed() < Duration::from_secs(30),
                "server never came up"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut rig = Rig {
            _dir: dir,
            store,
            s,
            issuer,
            recorder: String::new(),
            writer: String::new(),
        };
        for change in [
            json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
            json!({"op": "invite", "org": "acme", "members": [{"user": "carol@acme.com"}]}),
            json!({"op": "set_settings", "org": "acme", "settings": {"advice": true,
                   "advice_evidence": 0, "advice_learn_people": 1, "advice_personal_runs": 1}}),
        ] {
            rig.admin(change).await;
        }
        for (name, role) in [("rec", "recorder"), ("bot", "writer")] {
            let key = format!("msk_{}{name}", uuid::Uuid::new_v4().simple());
            rig.admin(
                json!({"op": "create_key", "org": "acme", "name": name, "role": role, "key": key}),
            )
            .await;
            match role {
                "recorder" => rig.recorder = key,
                _ => rig.writer = key,
            }
        }
        rig.store.ensure_scope(&acme()).await.unwrap();
        rig
    }

    async fn admin(&self, change: Value) -> Value {
        let (status, body) = post(&format!("{}/admin", self.s), ADMIN_KEY, &[], &change).await;
        assert_eq!(status, 200, "{change}: {body}");
        body
    }

    fn token(&self, user: &str) -> String {
        id_token(&self.issuer, user)
    }

    /// An MCP tool's text, and whether it is an error, called with `bearer` in org `acme` at `path` (`/mcp` as an
    /// agent calls it, `/mcp/hook` as the hooks do), as `user` when given (a recorder key's `x-memstore-as`).
    async fn tool_at(
        &self,
        path: &str,
        bearer: &str,
        user: Option<&str>,
        name: &str,
        args: Value,
    ) -> (String, bool) {
        let mut headers = vec![("x-memstore-org", "acme")];
        if let Some(u) = user {
            headers.push(("x-memstore-as", u));
        }
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": name, "arguments": args}});
        let (status, body) = post(&format!("{}{path}", self.s), bearer, &headers, &call).await;
        assert_eq!(status, 200, "{name}: {body}");
        let r = &body["result"];
        let text = match body["error"]["message"].as_str() {
            Some(e) => e.to_string(),
            None => r["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        (text, r["isError"] == true || body.get("error").is_some())
    }

    async fn tool(
        &self,
        bearer: &str,
        user: Option<&str>,
        name: &str,
        args: Value,
    ) -> (String, bool) {
        self.tool_at("/mcp", bearer, user, name, args).await
    }

    async fn hook(&self, bearer: &str, name: &str, args: Value) -> (String, bool) {
        let (text, failed) = self.tool_at("/mcp/hook", bearer, None, name, args).await;
        assert!(!failed, "{name}: {text}");
        (text, failed)
    }

    /// A session as memstore's hooks run it for `bearer`: opened, its first prompt asked, the conversation sent at
    /// each of `stops` (a prefix of `transcript`'s turns), and ended with the whole of `transcript`. Its id.
    async fn hooked(&self, bearer: &str, transcript: &str, stops: &[usize]) -> String {
        let (text, _) = self
            .hook(
                bearer,
                "start_session",
                json!({"harness": "claude-code", "max_chars": 4000}),
            )
            .await;
        let id = session_id(&text);
        let turns: Vec<&str> = transcript.split_inclusive('\n').collect();
        let first = turns[0].strip_prefix("User: ").unwrap().trim();
        self.hook(
            bearer,
            "get_advice",
            json!({"session_id": id, "harness": "claude-code", "question": first,
                                               "max_chars": 4000, "remembered": true}),
        )
        .await;
        for &n in stops {
            let so_far: String = turns[..n].concat();
            self.hook(bearer, "get_advice", json!({"session_id": id, "harness": "claude-code", "question": "-",
                                                   "event": {"kind": "stop"}, "conversation_so_far": so_far}))
                .await;
        }
        self.hook(
            bearer,
            "end_session",
            json!({"session_id": id, "harness": "claude-code",
                                                "transcript": transcript, "outcome": "done"}),
        )
        .await;
        id
    }

    async fn episodes(&self) -> Vec<(String, Episode)> {
        self.store
            .current_episodes(&acme())
            .await
            .unwrap()
            .into_iter()
            .collect()
    }

    /// Carol's kept notes now, by source: each one's source, ACL and text.
    async fn kept(&self) -> Vec<(String, Vec<String>, String)> {
        let kept: Vec<(String, Episode)> = self
            .episodes()
            .await
            .into_iter()
            .filter(|(_, e)| e.source.contains(":carol@acme.com:keep"))
            .collect();
        let bodies = self
            .store
            .episode_bodies(&acme(), kept.iter().map(|(id, _)| id.clone()).collect())
            .await
            .unwrap();
        let mut out: Vec<_> = kept
            .into_iter()
            .map(|(id, e)| {
                let b = bodies
                    .get(&id)
                    .map(|b| String::from_utf8_lossy(b).into_owned());
                (e.source, e.acl, b.unwrap_or_default())
            })
            .collect();
        out.sort();
        out
    }

    async fn browse(&self, bearer: &str, path: Value) -> Value {
        let (status, body) = post(
            &format!("{}/browse", self.s),
            bearer,
            &[("x-memstore-org", "acme")],
            &json!({"op": "ls", "path": path}),
        )
        .await;
        assert_eq!(status, 200, "{path}: {body}");
        body
    }
}

fn acme() -> Scope {
    Scope::org("acme").unwrap()
}

async fn post(url: &str, bearer: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Value) {
    let mut r = reqwest::Client::new()
        .post(url)
        .bearer_auth(bearer)
        .body(body.to_string());
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(json!({"raw": text})),
    )
}

fn session_id(text: &str) -> String {
    text.split("session_id: ")
        .nth(1)
        .and_then(|t| t.split_whitespace().next())
        .map(String::from)
        .or_else(|| {
            serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|v| v["session_id"].as_str().map(String::from))
        })
        .unwrap_or_else(|| panic!("no session id: {text}"))
}

/// Waits up to a minute for `done`.
async fn until<T>(what: &str, mut done: impl AsyncFnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = done().await {
            return v;
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "{what}: not within a minute"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Carol's first session: five requests. Her first message holds a preference with the task; she corrects the
/// first request, interrupts the second, accepts the third, and the fourth's next message shows nothing.
const TRANSCRIPT: &str = "User: Add a lint step to CI for the web app. I always want tabs, never spaces, in every repo.\n\
Claude: I added `npm run lint` to the CI workflow.\n\
User: No, this repo uses pnpm, never npm. Always use pnpm for every script here. Now also run the type check.\n\
Claude: Switched the lint step to `pnpm lint` and added `pnpm typecheck`. Pushing to main now.\n\
User: [Request interrupted by user] Don't push to main. We never push straight to main here: always open a pull request and wait for review.\n\
Claude: Understood. I opened a pull request with both steps instead.\n\
User: Thanks, that looks right. Separately, what time is the standup tomorrow?\n\
Claude: The standup is at 10am.\n\
User: Also add a changelog entry for the lint step, under Unreleased.\n\
Claude: Added the changelog entry.\n";

#[tokio::test(flavor = "multi_thread")]
async fn preferences_are_kept_from_what_people_type() {
    if !crate::own_process("kept_notes::preferences_are_kept_from_what_people_type") {
        return;
    }
    let seen = Seen::default();
    let rig = Rig::start(&models(seen.clone(), None).await).await;
    let carol = rig.token("carol@acme.com");
    let as_carol = Some("carol@acme.com");

    // A recorder key may not write to a session carol opened herself; its own session for her, where "she" says to
    // use yarn, keeps nothing as hers.
    let (text, _) = rig
        .tool(
            &carol,
            None,
            "start_session",
            json!({"context": "Tidy the changelog.", "harness": "claude-code"}),
        )
        .await;
    let hers = session_id(&text);
    for (name, args) in [
        (
            "end_session",
            json!({"session_id": hers, "outcome": "done",
            "transcript": "User: Tidy the changelog.\nClaude: Done.\nUser: No, always use bun here.\n"}),
        ),
        (
            "get_advice",
            json!({"session_id": hers, "moment": "next", "question": "next step",
            "conversation_so_far": "User: always use bun"}),
        ),
    ] {
        let (text, failed) = rig.tool(&rig.recorder, as_carol, name, args).await;
        assert!(
            failed && text.contains("writes only to the sessions it opened"),
            "{name}: {text}"
        );
    }
    let (text, failed) = rig
        .tool(&rig.recorder, as_carol, "start_session",
              json!({"context": "Set up the docs build. Always use yarn here.", "harness": "claude-code"}))
        .await;
    assert!(!failed, "{text}");
    let recorded = session_id(&text);
    rig.tool(
        &rig.recorder,
        as_carol,
        "end_session",
        json!({"session_id": recorded, "outcome": "done",
        "transcript": "User: Set up the docs build.\nClaude: I added an npm script.\n\
                       User: No, always use yarn here, never npm.\nClaude: Switched to yarn.\n"}),
    )
    .await;

    // An agent's own question and account of a session are its words, not hers.
    let (text, _) = rig
        .tool(
            &carol,
            None,
            "get_advice",
            json!({"session_id": hers, "moment": "next",
              "question": "The user wants bun for scripts; how do I set that up?"}),
        )
        .await;
    assert!(!text.is_empty());
    rig.tool(&carol, None, "end_session", json!({"session_id": hers, "outcome": "done",
        "transcript": "The user asked to tidy the changelog and said to use bun for scripts. I tidied it."}))
        .await;

    // With capture off, a hook's call keeps nothing.
    let (text, _) = rig
        .hook(
            &carol,
            "start_session",
            json!({"harness": "claude-code", "max_chars": 4000}),
        )
        .await;
    let off = session_id(&text);
    rig.hook(
        &carol,
        "get_advice",
        json!({"session_id": off, "harness": "claude-code", "max_chars": 4000,
        "question": "Always use bun for scripts in this repo.", "keep": false}),
    )
    .await;

    // Carol's own session, as the hooks run it: her first prompt, two Stops, then its end.
    let first = rig.hooked(&carol, TRANSCRIPT, &[4, 6]).await;
    let kept = until("carol's notes kept", async || {
        let k = rig.kept().await;
        (k.len() >= 3).then_some(k)
    })
    .await;

    // Every message she typed in it was read once, with the reply before it; nothing else was. Read in the
    // background, the end's messages may still be with the model when the Stops' notes land: waited for, then a
    // moment more, so a message read twice would show.
    let hers = || {
        let mut typed = seen.typed();
        typed.retain(|(said, _)| !said.starts_with("Tidy the changelog"));
        typed
    };
    until("all five of her messages read", async || {
        (hers().len() >= 5).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let typed = hers();
    let users: Vec<&str> = typed.iter().map(|(u, _)| u.as_str()).collect();
    assert_eq!(users.len(), 5, "each of her five messages once: {users:#?}");
    for (i, start) in [
        "Add a lint step",
        "No, this repo uses pnpm",
        "Don't push to main",
        "Thanks, that looks right",
        "Also add a changelog entry",
    ]
    .iter()
    .enumerate()
    {
        assert!(users[i].starts_with(start), "message {i}: {users:#?}");
    }
    assert_eq!(typed[0].1, "", "her first message has no reply before it");
    assert!(
        typed[1].1.starts_with("I added `npm run lint`"),
        "{:?}",
        typed[1]
    );
    assert!(typed[2].1.contains("Pushing to main now"), "{:?}", typed[2]);
    for (u, _) in &typed {
        assert!(
            !u.contains("yarn") && !u.contains("bun"),
            "the keep model read {u:?}"
        );
    }

    // Filed in her map, for her alone, in the model's words.
    let signed = "mcp:remember_memory:carol@acme.com:";
    let sources: Vec<&str> = kept.iter().map(|(s, ..)| s.as_str()).collect();
    assert_eq!(
        sources,
        [
            format!("{signed}keep:formatting:indentation"),
            format!("{signed}keep:tooling:package-manager"),
            format!("{signed}keep:version-control:pushing-to-main"),
        ],
        "{kept:#?}"
    );
    for (_, acl, text) in &kept {
        assert_eq!(acl, &["user:carol@acme.com".to_string()]);
        assert!(!text.contains("yarn") && !text.contains("bun"), "{text}");
    }
    // Her session's record is a session record.
    assert!(
        rig.episodes()
            .await
            .iter()
            .any(|(_, e)| e.source == format!("{signed}memstore:session:{first}")),
        "no session record for {first}"
    );

    // No caller writes under `keep`.
    let (text, failed) = rig
        .tool(
            &rig.writer,
            None,
            "remember_memory",
            json!({"source": "keep:deploys:main", "body": "push to main"}),
        )
        .await;
    assert!(
        failed && text.contains("AmpUp Memory files what it keeps"),
        "{text}"
    );

    // A second session: pnpm again (not kept twice) and a new preference, filed in her map.
    rig.hooked(
        &carol,
        "User: Bump the web app's dependencies.\nClaude: I ran `npm update` and committed the lockfile.\n\
         User: No, use pnpm here, not npm. Also pin exact versions from now on, never caret ranges.\n\
         Claude: Redone with `pnpm update` and exact versions pinned.\n",
        &[],
    )
    .await;
    let kept = until("the second session's note", async || {
        let k = rig.kept().await;
        (k.len() > 3).then_some(k)
    })
    .await;
    let last = seen.prompts(KEEP_PROMPT).pop().unwrap();
    assert!(
        last.contains("- tooling / package-manager"),
        "her map shown: {last}"
    );
    assert!(
        last.contains("- Always use pnpm, never npm"),
        "her notes shown: {last}"
    );
    assert_eq!(
        kept.iter()
            .filter(|(s, ..)| s.ends_with("keep:tooling:package-manager"))
            .count(),
        1,
        "the repeat was kept again: {kept:#?}"
    );
    assert!(
        kept.iter()
            .any(|(s, ..)| s.ends_with("keep:tooling:versions")),
        "{kept:#?}"
    );

    // Facts: a note she remembers is read; her sessions' records and her kept notes never are.
    rig.tool(
        &carol,
        None,
        "remember_memory",
        json!({"source": "orrery", "body": "The Orrery service is owned by the Lattice team."}),
    )
    .await;
    until("her remembered note extracted", async || {
        seen.extraction_prompts()
            .iter()
            .any(|p| p.contains("Orrery service"))
            .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for p in seen.extraction_prompts() {
        for said in ["Add a lint step", "Bump the web app"] {
            assert!(
                !p.contains(said),
                "a session's record was read for facts: {said}"
            );
        }
        for (.., note) in &kept {
            assert!(
                !p.contains(note.as_str()),
                "a kept note was read for facts: {note}"
            );
        }
    }

    // Her Explorer files her notes by category and topic, titled, and her sessions by day; alice sees none of it.
    let notes = rig.browse(&carol, json!(["notes"])).await;
    let names: Vec<&str> = notes["children"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        names.contains(&"tooling") && names.contains(&"version-control"),
        "{notes}"
    );
    let leaf = rig
        .browse(&carol, json!(["notes", "tooling", "versions"]))
        .await;
    assert_eq!(
        leaf["children"][0]["title"], "Pin exact dependency versions, never caret ranges.",
        "{leaf}"
    );
    let ended = rig.browse(&carol, json!(["sessions", "ended"])).await;
    assert!(
        ended["children"].as_array().is_some_and(|d| !d.is_empty()),
        "{ended}"
    );
    let alice = rig.token("alice@acme.com");
    for path in [json!(["notes"]), json!(["sessions"])] {
        let theirs = rig.browse(&alice, path.clone()).await;
        assert!(
            theirs["children"].as_array().is_none_or(|c| c.is_empty()),
            "alice reads carol's {path}: {theirs}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn advice_learns_from_each_graded_request_and_hands_its_preferences() {
    if !crate::own_process(
        "kept_notes::advice_learns_from_each_graded_request_and_hands_its_preferences",
    ) {
        return;
    }
    let seen = Seen::default();
    let rig = Rig::start(&models(seen.clone(), None).await).await;
    let carol = rig.token("carol@acme.com");
    let id = rig.hooked(&carol, TRANSCRIPT, &[]).await;

    // Graded request by request (the last has no next message), then learned as her own advice.
    let space = memstore::advice::Space::mine("carol@acme.com");
    let map = until(
        "her own advice learned from the graded requests",
        async || {
            let pinned = rig.store.pin(&acme(), memstore::MAIN).await.unwrap();
            let map = pinned
                .engine_document(&rig.store, &space.map())
                .await
                .unwrap()?;
            let map = serde_json::from_slice::<Value>(&map).ok()?;
            map["assign"]
                .as_object()?
                .keys()
                .any(|k| k.starts_with(&format!("{id}#")))
                .then_some(map)
        },
    )
    .await;
    let graded = seen.graded();
    let hers = graded
        .iter()
        .find(|r| r[0].starts_with("Add a lint step"))
        .expect("graded");
    assert_eq!(hers.len(), 4, "{hers:?}");

    // Each graded request is a run of its own; the one whose next message showed nothing isn't, nor is the last.
    let runs = map["assign"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&format!("{id}#")))
        .count();
    assert_eq!(runs, 3, "{map:#}");
    let sorted = seen.prompts("You keep the map").pop().unwrap();
    let tasks: Vec<&str> = sorted.lines().filter(|l| l.starts_with('T')).collect();
    // A request that steers work is sorted with the work it steers.
    assert!(
        tasks
            .iter()
            .any(|t| t.contains("Add a lint step") && t.contains("Then: No, this repo uses pnpm")),
        "{tasks:#?}"
    );
    assert!(
        !tasks
            .iter()
            .any(|t| t.contains("Then: Also add a changelog entry")),
        "{tasks:#?}"
    );

    // Her next session's start hands the preference her own advice learned, with her notes.
    until("her own advice written", async || {
        let pinned = rig.store.pin(&acme(), memstore::MAIN).await.unwrap();
        pinned
            .engine_document(&rig.store, &space.note("work/ci"))
            .await
            .unwrap()
            .map(|_| ())
    })
    .await;
    let (text, failed) = rig
        .tool(&carol, None, "start_session",
              json!({"context": "Fix the type check in the CI change before pushing.", "harness": "claude-code"}))
        .await;
    assert!(!failed, "{text}");
    assert!(
        text.contains(LEARNED),
        "her own advice's preference not handed: {text}"
    );
    // Alice's start hands none of it.
    let (text, _) = rig
        .tool(&rig.token("alice@acme.com"), None, "start_session",
              json!({"context": "Fix the type check in the CI change before pushing.", "harness": "claude-code"}))
        .await;
    assert!(
        !text.contains(LEARNED),
        "carol's advice handed to alice: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_kept_for_someone_forgotten_while_the_model_ran() {
    if !crate::own_process("kept_notes::nothing_is_kept_for_someone_forgotten_while_the_model_ran")
    {
        return;
    }
    let seen = Seen::default();
    let (open, gate) = tokio::sync::watch::channel(false);
    let rig = Rig::start(&models(seen.clone(), Some(gate)).await).await;
    let carol = rig.token("carol@acme.com");
    rig.hooked(&carol, TRANSCRIPT, &[]).await;
    until("the keep model asked", async || {
        (seen.keep_asks.load(Ordering::SeqCst) > 0).then_some(())
    })
    .await;

    // Forgotten while the model runs, then the model answers.
    rig.admin(json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}))
        .await;
    open.send(true).unwrap();
    // The model's answer is in hand at once: anything it would write is written within a sweep or two.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let left: Vec<String> = rig
        .episodes()
        .await
        .into_iter()
        .map(|(_, e)| e.source)
        .filter(|s| s.contains("carol@acme.com"))
        .collect();
    assert!(
        left.is_empty(),
        "written for carol after she was forgotten: {left:?}"
    );
}
