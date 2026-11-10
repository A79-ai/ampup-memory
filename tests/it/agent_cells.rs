//! The deployment agent gives a cell's Deployments (`AMPUP_MEMORY_AGENT_ALSO_DEPLOYMENTS`) the same version
//! and flags as the engine's own.

use std::collections::BTreeMap;

use memstore::agent::{Kubernetes, Target};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A Kubernetes API answering GETs of a Deployment with `deployments[name]` (`{}` otherwise), and every
/// PATCH with `{}`; it sends back each request it got.
async fn fake_api(
    deployments: serde_json::Value,
) -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (tx, deployments) = (tx.clone(), deployments.clone());
            tokio::spawn(async move {
                let mut buf = vec![0; 64 << 10];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_string();
                let body = match line.strip_prefix("GET ") {
                    Some(rest) => {
                        let name = rest.split(' ').next().unwrap().rsplit('/').next().unwrap();
                        deployments[name].to_string()
                    }
                    None => "{}".into(),
                };
                let _ = tx.send(req);
                let _ = conn
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (port, rx)
}

fn k8s(port: u16) -> Kubernetes {
    Kubernetes::at(
        &format!("http://127.0.0.1:{port}"),
        None,
        None,
        "acme".into(),
        "memory".into(),
        "memstore".into(),
    )
    .unwrap()
    .with_also(vec!["memory-cell-b".into()])
    .unwrap()
}

fn deployment(image: &str, env: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"spec": {"template": {"spec": {"containers": [
        {"name": "memstore", "image": image, "env": env}
    ]}}}})
}

#[tokio::test]
async fn a_version_change_goes_to_every_cells_deployment() {
    let (port, mut rx) = fake_api(serde_json::json!({})).await;
    let k = k8s(port);
    let env = BTreeMap::from([("MEMSTORE_FLAG_X".to_string(), Some("1".to_string()))]);
    k.set("repo/memstore@sha256:abc", &env).await.unwrap();
    let mut got = vec![];
    while let Ok(r) = rx.try_recv() {
        got.push(r);
    }
    let lines: Vec<&str> = got.iter().map(|r| r.lines().next().unwrap()).collect();
    assert_eq!(
        lines,
        [
            "PATCH /apis/apps/v1/namespaces/acme/deployments/memory HTTP/1.1",
            "PATCH /apis/apps/v1/namespaces/acme/deployments/memory-cell-b HTTP/1.1",
        ]
    );
    for r in &got {
        assert!(
            r.contains("repo/memstore@sha256:abc") && r.contains("MEMSTORE_FLAG_X"),
            "{r}"
        );
    }
    assert!(
        Kubernetes::at(
            "http://127.0.0.1:1",
            None,
            None,
            "acme".into(),
            "m".into(),
            "m".into()
        )
        .unwrap()
        .with_also(vec!["Not_A/Name".into()])
        .is_err()
    );
}

#[tokio::test]
async fn a_cell_left_on_another_version_is_brought_to_the_engines() {
    let engine = deployment(
        "repo/memstore@sha256:new",
        serde_json::json!([{"name": "MEMSTORE_FLAG_X", "value": "1"}, {"name": "MEMSTORE_CELL_MAP", "value": "{}"}]),
    );
    let behind = deployment(
        "repo/memstore@sha256:old",
        serde_json::json!([{"name": "MEMSTORE_FLAG_Y", "value": "2"}, {"name": "MEMSTORE_CELL", "value": "cell-b"}]),
    );
    let (port, mut rx) =
        fake_api(serde_json::json!({"memory": engine, "memory-cell-b": behind})).await;
    k8s(port).sync_also().await.unwrap();
    let patches: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|r| r.starts_with("PATCH "))
        .collect();
    assert_eq!(patches.len(), 1, "{patches:?}");
    let p = &patches[0];
    assert!(
        p.starts_with("PATCH /apis/apps/v1/namespaces/acme/deployments/memory-cell-b "),
        "{p}"
    );
    let body: serde_json::Value =
        serde_json::from_str(p.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let c = &body["spec"]["template"]["spec"]["containers"][0];
    assert_eq!(c["image"], "repo/memstore@sha256:new");
    let env = c["env"].as_array().unwrap();
    assert!(
        env.contains(&serde_json::json!({"name": "MEMSTORE_FLAG_X", "value": "1"})),
        "{env:?}"
    );
    assert!(
        env.contains(&serde_json::json!({"name": "MEMSTORE_FLAG_Y", "$patch": "delete"})),
        "{env:?}"
    );
    // The cell's own settings are none of the agent's business.
    assert!(!p.contains("MEMSTORE_CELL"), "{p}");

    // Already alike: nothing is patched.
    let alike = deployment(
        "repo/memstore@sha256:new",
        serde_json::json!([{"name": "MEMSTORE_FLAG_X", "value": "1"}]),
    );
    let (port, mut rx) =
        fake_api(serde_json::json!({"memory": alike, "memory-cell-b": alike})).await;
    k8s(port).sync_also().await.unwrap();
    assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|r| r.starts_with("PATCH ")));
}
