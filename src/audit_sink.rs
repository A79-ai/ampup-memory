//! Where audit events go besides the catalog, for a customer's log pipeline or SIEM: `MEMSTORE_AUDIT_SINK`.
//!
//! - `stdout`: one JSON object per line on the server's standard output, for a log shipper that already
//!   reads the pod's logs;
//! - `file:<path>`: the same lines appended to a file;
//! - `https://…`: each flush POSTed as newline-delimited JSON (`MEMSTORE_AUDIT_SINK_TOKEN`, if set, is the
//!   bearer token), retried a few times, then dropped and counted.
//!
//! A line is the event as the audit view shows it ([`crate::audit::Entry::to_json`]) plus `"source":
//! "memstore"`. Events the buffer couldn't hold arrive as `{"kind": "gap", "dropped": N}`, so a hole in the
//! log is visible in the destination. Delivery is at most once and never waits on a request: the catalog's
//! `memory_system.audit` stays the record, and the sink is a copy.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use prometheus::IntCounter;
use serde_json::json;
use tokio::sync::mpsc;

use crate::audit::{Entry, Sink};

/// Flushes the sink couldn't deliver (or had no room for): their events are in the catalog, not the sink.
pub static SINK_FAILURES: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "memstore_audit_sink_failures_total",
        "Audit flushes the configured sink (MEMSTORE_AUDIT_SINK) did not take",
    )
    .unwrap()
});

/// Flushes queued for delivery at most; past it a flush is dropped, so a stuck destination never grows memory.
const QUEUE: usize = 256;
const ATTEMPTS: u32 = 3;

enum Dest {
    Stdout,
    File(String),
    Https(String, Option<String>),
}

/// The sink `MEMSTORE_AUDIT_SINK` names, or none when it is unset. A value that is none of the forms above
/// stops the server: an audit destination that silently isn't one is worse than a server that won't start.
/// Call from inside the runtime.
pub fn from_env() -> Result<Option<Sink>> {
    let Ok(v) = std::env::var("MEMSTORE_AUDIT_SINK") else {
        return Ok(None);
    };
    let v = v.trim();
    let dest = match v {
        "" => return Ok(None),
        "stdout" => Dest::Stdout,
        _ => match (v.strip_prefix("file:"), v.starts_with("https://")) {
            (Some(path), _) if !path.is_empty() => Dest::File(path.to_string()),
            (_, true) => Dest::Https(
                v.to_string(),
                std::env::var("MEMSTORE_AUDIT_SINK_TOKEN")
                    .ok()
                    .filter(|t| !t.is_empty()),
            ),
            _ => bail!("MEMSTORE_AUDIT_SINK is stdout, file:<path> or an https:// URL, got {v:?}"),
        },
    };
    if let Dest::File(path) = &dest {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("MEMSTORE_AUDIT_SINK file {path} can't be written"))?;
    }
    let (tx, rx) = mpsc::channel::<String>(QUEUE);
    tokio::spawn(deliver(dest, rx));
    Ok(Some(std::sync::Arc::new(
        move |rows: &[Entry], dropped: u64| {
            let mut lines = String::new();
            for e in rows {
                let mut v = e.to_json();
                v["source"] = json!("memstore");
                lines.push_str(&v.to_string());
                lines.push('\n');
            }
            if dropped > 0 {
                lines.push_str(
                    &json!({"source": "memstore", "kind": "gap", "dropped": dropped}).to_string(),
                );
                lines.push('\n');
            }
            if !lines.is_empty() && tx.try_send(lines).is_err() {
                SINK_FAILURES.inc();
                eprintln!("audit sink: queue full, a flush was not sent (it is in the catalog)");
            }
        },
    )))
}

async fn deliver(dest: Dest, mut rx: mpsc::Receiver<String>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    while let Some(lines) = rx.recv().await {
        match &dest {
            Dest::Stdout => print!("{lines}"),
            Dest::File(path) => {
                use std::io::Write;
                let wrote = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut f| f.write_all(lines.as_bytes()));
                if let Err(e) = wrote {
                    SINK_FAILURES.inc();
                    eprintln!("audit sink: {path} not written: {e}");
                }
            }
            Dest::Https(url, token) => {
                let mut sent = false;
                for attempt in 0..ATTEMPTS {
                    let mut req = client
                        .post(url)
                        .header("content-type", "application/x-ndjson")
                        .body(lines.clone());
                    if let Some(t) = token {
                        req = req.bearer_auth(t);
                    }
                    match req.send().await.and_then(|r| r.error_for_status()) {
                        Ok(_) => {
                            sent = true;
                            break;
                        }
                        Err(e) => {
                            eprintln!(
                                "audit sink: attempt {} failed: {}",
                                attempt + 1,
                                e.without_url()
                            );
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        }
                    }
                }
                if !sent {
                    SINK_FAILURES.inc();
                }
            }
        }
    }
}
