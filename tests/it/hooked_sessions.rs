//! A client whose hooks open and close each session (the Claude Code and Cursor plugins) is never offered
//! start_session or end_session, nor told to call them, end to end: the plugin's own MCP entry (`.mcp.json`'s
//! command environment) running the real `ampup-memory mcp` bridge against a real `ampup-memory serve --local`.
//!
//! An agent told to close its own session spent its last turn on it, and its answer to the user was lost. A client
//! without hooks still gets both calls, and the hooks' own session calls (`/mcp/hook`) still open and close one.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::local_serve::{free_port, home_of, serve_local};

/// The environment a plugin's MCP entry gives its command.
fn plugin_env(file: &str) -> Vec<(String, String)> {
    let path = format!("{}/plugins/ampup-memory/{file}", env!("CARGO_MANIFEST_DIR"));
    let entry: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = &entry["mcpServers"]["ampup-memory"];
    assert_eq!(
        entry["args"],
        json!(["mcp"]),
        "{path} runs the stdio bridge"
    );
    entry["env"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

/// What the bridge answers `initialize` and `tools/list` with: the instructions and the tools' names.
fn connect(dir: &std::path::Path, port: u16, env: &[(String, String)]) -> (String, Vec<String>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ampup-memory"));
    // Only what the plugin's entry sets: none of this machine's own memory settings (its org, scope or token).
    for (k, _) in std::env::vars()
        .filter(|(k, _)| k.starts_with("AMPUP_MEMORY_") || k.starts_with("MEMSTORE_"))
    {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .arg("mcp")
        .env("HOME", home_of(dir))
        .env("AMPUP_MEMORY_URL", format!("http://127.0.0.1:{port}"))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let msgs = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
               "capabilities": {}, "clientInfo": {"name": "hooked_sessions", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ];
    let mut stdin = child.stdin.take().unwrap();
    for m in msgs {
        writeln!(stdin, "{m}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let answers: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let by_id = |id: i64| {
        answers
            .iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("no answer {id}: {answers:?}"))
    };
    let instructions = by_id(1)["result"]["instructions"]
        .as_str()
        .unwrap_or_else(|| panic!("initialize: {}", by_id(1)))
        .to_string();
    let tools = by_id(2)["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list: {}", by_id(2)))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    (instructions, tools)
}

async fn hook_call(port: u16, name: &str, arguments: Value) -> Value {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": arguments}});
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mcp/hook"))
        .header("x-memstore-hooks", "sessions")
        .json(&call)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooked_client_is_never_asked_to_open_or_close_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let port = free_port();
    let _server = serve_local(&store, port, &[]).await;

    for file in [".mcp.json", "cursor/mcp.json"] {
        let env = plugin_env(file);
        let (instructions, tools) = connect(&store, port, &env);
        for call in ["start_session", "end_session"] {
            assert!(
                !tools.iter().any(|t| t == call),
                "{file} is offered {call}: {tools:?}"
            );
            assert!(
                !instructions.contains(call),
                "{file} is told to call {call}: {instructions}"
            );
        }
        assert!(
            tools.iter().any(|t| t == "remember_memory"),
            "{file}: {tools:?}"
        );
        assert!(
            instructions.contains("your last message is always your answer"),
            "{instructions}"
        );
    }

    // Without hooks (Claude Desktop, ChatGPT, an IDE without memstore's hooks), the agent runs the session itself.
    let (instructions, tools) = connect(&store, port, &[]);
    for call in ["start_session", "end_session"] {
        assert!(
            tools.iter().any(|t| t == call),
            "a hookless client isn't offered {call}: {tools:?}"
        );
        assert!(
            instructions.contains(call),
            "a hookless client isn't told to call {call}"
        );
    }

    // The hooks' own calls still open and close the session.
    let started = hook_call(
        port,
        "start_session",
        json!({"context": "why is checkout slow?"}),
    )
    .await;
    let text = started["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(started["result"]["isError"], json!(true), "{started}");
    let session = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find(|w| uuid::Uuid::parse_str(w).is_ok())
        .unwrap_or_else(|| panic!("no session id in {text}"));
    let ended = hook_call(port, "end_session", json!({"session_id": session,
        "transcript": "User: why is checkout slow?\nAssistant: the order-db index was dropped", "outcome": "answered"})).await;
    assert_ne!(ended["result"]["isError"], json!(true), "{ended}");
    assert!(ended.get("error").is_none(), "{ended}");
}
