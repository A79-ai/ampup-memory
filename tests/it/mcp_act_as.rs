//! A recorder key over `/mcp`, end to end: a real `memstore serve` over HTTP on a local catalog, org `acme` with
//! two members the directory holds.
//!
//! The rule: a recorder key names the member it acts for in `x-memstore-as` and gets the session loop, advice and
//! search as them, and nothing that writes or forgets memory. Without the header it does nothing as itself; for
//! someone the directory doesn't hold it is refused; any other kind of key is refused the header.

use std::time::{Duration, Instant};

use memstore::auth::{self, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Scope, platform};
use serde_json::{Value, json};
use sha2::Digest;

const ADMIN_KEY: &str = "msk_platform_admin_for_mcp_act_as";

static CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

async fn post(url: &str, bearer: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Value) {
    let mut r = CLIENT.post(url).bearer_auth(bearer).body(body.to_string());
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or_default())
}

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

fn tool_names(answer: &Value) -> Vec<String> {
    let mut names: Vec<String> = answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorder_key_uses_mcp_as_a_member_with_the_session_tools_only() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            listener: Some(listener),
            catalog: Some(catalog.clone()),
            token_auth: Some(TokenAuth::new(Some(&keys.to_string()), None).unwrap()),
            directory_only: true,
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
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "practice.a@agents.example.com"}, {"user": "practice.b@agents.example.com"}]}),
    ] {
        let (status, body) = post(&format!("{s}/admin"), ADMIN_KEY, &[], &change).await;
        assert_eq!(status, 200, "{change}: {body}");
    }
    let key = async |name: &str, role: &str| {
        let k = format!("msk_{}{name}", uuid::Uuid::new_v4().simple());
        let change =
            json!({"op": "create_key", "org": "acme", "name": name, "role": role, "key": k});
        let (status, body) = post(&format!("{s}/admin"), ADMIN_KEY, &[], &change).await;
        assert_eq!(status, 200, "{change}: {body}");
        k
    };
    let recorder = key("recorder", "recorder").await;
    let writer = key("bot", "writer").await;
    let mcp = format!("{s}/mcp");
    let as_a = [
        ("x-memstore-org", "acme"),
        ("x-memstore-as", "practice.a@agents.example.com"),
    ];

    // As a member it holds, the recorder key is offered the session loop, advice and search, and no write.
    let (status, listed) = post(&mcp, &recorder, &as_a, &rpc("tools/list", json!({}))).await;
    assert_eq!(status, 200, "{listed}");
    let names = tool_names(&listed);
    // get_advice is listed only where the server has a model to route with, which this one hasn't.
    assert_eq!(names, ["end_session", "search_memory", "start_session"]);

    // Named by name, a tool outside the four is refused for it, whatever it asks.
    let (status, called) = post(
        &mcp,
        &recorder,
        &as_a,
        &rpc(
            "tools/call",
            json!({"name": "remember_memory", "arguments": {"text": "a preference"}}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{called}");
    assert_eq!(called["result"]["isError"], true, "{called}");

    // Without the header it does nothing as itself; for a name the directory doesn't hold, it is refused.
    let in_acme = [("x-memstore-org", "acme")];
    let (status, _) = post(&mcp, &recorder, &in_acme, &rpc("tools/list", json!({}))).await;
    assert_eq!(status, 403);
    let stranger = [
        ("x-memstore-org", "acme"),
        ("x-memstore-as", "nobody@agents.example.com"),
    ];
    let (status, _) = post(&mcp, &recorder, &stranger, &rpc("tools/list", json!({}))).await;
    assert_eq!(status, 403);

    // Any other key is refused the header, on /mcp as everywhere.
    let (status, _) = post(&mcp, &writer, &as_a, &rpc("tools/list", json!({}))).await;
    assert_eq!(status, 403);
}
