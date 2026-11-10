//! Cells: the default cell's server forwards a request for another cell's org to that cell and never serves
//! it itself (503 where that cell can't be reached); its store refuses that org's memory outright; an org's
//! delete and forget are done by its own cell, and a forget in every org names an org that wasn't done; a
//! cell's servers keep a writer lease of their own.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use memstore::auth::{self, TokenAuth};
use memstore::people::directory::{Directory, Event};
use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn serve(dir: &std::path::Path, port: u16, env: &[(&str, String)]) -> Server {
    let models = format!("{}/.cache/memstore/models", std::env::var("HOME").unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ampup-memory"));
    cmd.env("HOME", dir.with_extension("home"))
        .env(
            "AMPUP_MEMORY_MODEL_DIR",
            std::env::var("AMPUP_MEMORY_MODEL_DIR").unwrap_or(models),
        )
        .args(["serve", "--local"])
        .arg(dir)
        .args(["org", &port.to_string()])
        .env_remove("AMPUP_MEMORY_CALLER_IDENTITY")
        .env_remove("MEMSTORE_CALLER_IDENTITY")
        .env_remove("AMPUP_MEMORY_SERVE_BIND")
        .env_remove("MEMSTORE_SERVE_BIND")
        .env_remove("AMPUP_MEMORY_PLATFORM")
        .env_remove("MEMSTORE_PLATFORM")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let server = Server(cmd.spawn().unwrap());
    let up = Instant::now();
    while reqwest::get(format!("http://127.0.0.1:{port}/"))
        .await
        .is_err()
    {
        assert!(
            up.elapsed() < Duration::from_secs(60),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server
}

/// Another cell's server: answers anything with `x-cell: acme`, and sends back the request it got.
async fn fake_cell() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 64 << 10];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return; // the reachability probe
                }
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = conn
                    .write_all(b"HTTP/1.1 201 Created\r\nx-cell: acme\r\ncontent-length: 4\r\nconnection: close\r\n\r\ncell")
                    .await;
            });
        }
    });
    (port, rx)
}

#[tokio::test]
async fn the_default_cell_forwards_another_cells_org_and_never_serves_it_when_that_cell_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let (cell, mut got) = fake_cell().await;
    let port = free_port();
    let _server = serve(
        dir.path(),
        port,
        &[
            (
                "AMPUP_MEMORY_CELL_MAP",
                r#"{"acme": "acme", "gone": "gone"}"#.into(),
            ),
            (
                "AMPUP_MEMORY_CELL_URLS",
                format!(
                    r#"{{"acme": "http://127.0.0.1:{cell}", "gone": "http://127.0.0.1:{}"}}"#,
                    free_port()
                ),
            ),
        ],
    )
    .await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("http://127.0.0.1:{port}/mcp?org=acme"))
        .header("authorization", "Bearer t0ken")
        .body("{\"jsonrpc\": \"2.0\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    assert_eq!(res.headers()["x-cell"], "acme");
    assert_eq!(res.text().await.unwrap(), "cell");
    let sent = got.recv().await.unwrap();
    assert!(sent.starts_with("POST /mcp?org=acme HTTP/1.1"), "{sent}");
    assert!(
        sent.to_ascii_lowercase()
            .contains("authorization: bearer t0ken"),
        "{sent}"
    );
    assert!(sent.ends_with("{\"jsonrpc\": \"2.0\"}"), "{sent}");

    // Selected by header too.
    let res = http
        .get(format!("http://127.0.0.1:{port}/v1/facts"))
        .header("x-memstore-org", "acme")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.headers().get("x-cell").map(|v| v.to_str().unwrap()),
        Some("acme")
    );

    // The default cell's own org: served here.
    let res = http
        .get(format!("http://127.0.0.1:{port}/v1/facts?org=other"))
        .send()
        .await
        .unwrap();
    assert!(res.headers().get("x-cell").is_none(), "other was forwarded");
    assert_ne!(res.status().as_u16(), 503);

    // An org whose cell can't be reached: asked again later, never served (or loaded) here.
    let res = http
        .get(format!("http://127.0.0.1:{port}/v1/facts?org=gone"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 503);
}

const ADMIN_KEY: &str = "msk_platform_admin_for_cells";

/// `/admin` as the operator: the status, the `x-cell` header (set by the other cell) and the answer.
async fn admin(s: &str, body: Value) -> (u16, Option<String>, Value) {
    let r = reqwest::Client::new()
        .post(format!("{s}/admin"))
        .bearer_auth(ADMIN_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    let cell = r
        .headers()
        .get("x-cell")
        .map(|v| v.to_str().unwrap().to_string());
    let status = r.status().as_u16();
    (status, cell, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn another_cells_org_is_never_opened_here_and_its_delete_and_forget_are_its_cells() {
    // The cell map is read once per process (`memstore_core::cell`): this test's alone.
    if !crate::own_process(
        "cells::another_cells_org_is_never_opened_here_and_its_delete_and_forget_are_its_cells",
    ) {
        return;
    }
    let (cell, mut got) = fake_cell().await;
    let urls = format!(
        r#"{{"acme": "http://127.0.0.1:{cell}", "gone": "http://127.0.0.1:{}"}}"#,
        free_port()
    );
    unsafe {
        std::env::set_var(
            "AMPUP_MEMORY_CELL_MAP",
            r#"{"acme": "acme", "gone": "gone"}"#,
        );
        std::env::set_var("AMPUP_MEMORY_CELL_URLS", urls);
    }
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    // acme and gone are in the directory, as their own cells made them (the directory is every cell's), and
    // dave belongs to both.
    let ev = |op: &str, org: &str| Event::new("svc:test", op, org);
    store
        .append_directory(&[
            ev("org.create", "acme"),
            ev("member.add", "acme").user("dave@x.com"),
            ev("org.create", "gone"),
            ev("member.add", "gone").user("dave@x.com"),
        ])
        .await
        .unwrap();

    // This cell's store never opens another cell's org, and never finds it empty either (a forget or a
    // purge would then delete nothing and say it was done).
    let e = store.org_namespaces("acme").await.unwrap_err();
    assert!(memstore::cell::is_other_cell(&e), "{e:#}");
    let e = store
        .ensure_scope(&Scope::org("acme").unwrap())
        .await
        .unwrap_err();
    assert!(memstore::cell::is_other_cell(&e), "{e:#}");
    // Nor its search index: every index open (a read, or a write's) refuses it.
    let e = memstore::search::coverage(&format!(
        "{}/index/memory_org_acme/v10",
        dir.path().display()
    ))
    .await
    .unwrap_err();
    assert!(memstore::cell::is_other_cell(&e), "{e:#}");

    let keys = json!([{"id": "platform", "user": "svc:platform", "admin": true,
                       "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes()))}]);
    let port = free_port();
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
    let s = format!("http://127.0.0.1:{port}");
    let up = Instant::now();
    while reqwest::get(&s).await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // An org's delete is its own cell's, sent there with the org named.
    let (status, from, _) = admin(
        &s,
        json!({"op": "delete_org", "org": "acme", "confirm": "acme"}),
    )
    .await;
    assert_eq!((status, from.as_deref()), (201, Some("acme")));
    let sent = got.recv().await.unwrap();
    assert!(sent.starts_with("POST /admin HTTP/1.1"), "{sent}");
    assert!(
        sent.to_ascii_lowercase().contains("x-memstore-org: acme"),
        "{sent}"
    );
    assert!(sent.contains(r#""op":"delete_org""#), "{sent}");

    // Its cell down: refused, never done here (the org is still there).
    let (status, ..) = admin(
        &s,
        json!({"op": "delete_org", "org": "gone", "confirm": "gone"}),
    )
    .await;
    assert_eq!(status, 503);
    assert!(
        Directory::fresh(&store)
            .await
            .unwrap()
            .orgs
            .contains_key("gone")
    );

    // A forget in every org: acme's part sent to acme's cell, the default org's done here, and gone (its cell
    // down) named as not done, never skipped.
    let (status, _, body) = admin(&s, json!({"op": "forget_user", "user": "dave@x.com"})).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["failed"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["failed"][0]["org"], "gone", "{body}");
    assert!(body["forgotten"].get("default").is_some(), "{body}");
    let sent = got.recv().await.unwrap();
    assert!(
        sent.contains(r#""op":"forget_user""#) && sent.contains(r#""org":"acme""#),
        "{sent}"
    );
}

#[tokio::test]
async fn a_cells_servers_keep_a_writer_lease_of_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let _server = serve(
        dir.path(),
        free_port(),
        &[("AMPUP_MEMORY_CELL", "acme".into())],
    )
    .await;
    let found = |name: &str| {
        walkdir(dir.path())
            .iter()
            .any(|p| p.file_name().is_some_and(|f| f == name))
    };
    let up = Instant::now();
    while !found("_writer-acme") {
        assert!(
            up.elapsed() < Duration::from_secs(60),
            "no _writer-acme lease"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!found("_writer"), "the default cell's lease was taken");
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        }
        out.push(p);
    }
    out
}
