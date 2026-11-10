//! `memstore support-bundle`: a diagnostic bundle the customer makes and chooses to send, never one we pull.
//!
//! It reads the engine the way a support session's diagnostics do, through the same three endpoints
//! (`/healthz`, `/telemetry`, `/metrics`) and the same views (counts, closed sets, metric families with their
//! labels dropped, config variable names with values only for the non-secret set), so the bundle holds nothing a
//! support session couldn't read, and no memory: never `/query`, `/search`, `/episode` or `/facts`.
//!
//! Logs are the one thing a session can't read, and the bundle gives a summary rather than the lines: counts by
//! severity in five-minute buckets, and the error and warning lines alone, each redacted ([`redact`]) and capped.
//! Everything is written to a directory on this machine with a `manifest.json` saying what is inside and what was
//! redacted; nothing is sent anywhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::agent::support::metric_families;
use crate::telemetry::{Envelope, Inventory, Payload, State};

/// Where the engine's logs come from.
pub enum Logs {
    /// `gcloud logging read` on the Cloud Run service, as the person running this.
    CloudRun {
        project: String,
        region: String,
        service: String,
    },
    /// Lines from a file (`kubectl logs … > engine.log`).
    File(PathBuf),
    None,
}

/// Lines kept of the error and warning lines, and the most of each kept.
const MAX_LINES: usize = 500;
const MAX_LINE: usize = 400;

/// Blanks what a log line may carry that the bundle must not: email addresses, bearer tokens and keys (any run
/// of 32 or more key-looking characters), and the query part of URLs. Returns the line and how many spans it
/// blanked.
pub fn redact(line: &str) -> (String, usize) {
    let mut out = String::with_capacity(line.len());
    let mut n = 0;
    let mut words = line.split(' ').peekable();
    let mut bearer = false;
    while let Some(w) = words.next() {
        let mut w = w.to_string();
        if bearer {
            w = "<secret>".into();
            bearer = false;
            n += 1;
        } else if w.eq_ignore_ascii_case("bearer") {
            bearer = true;
        } else if is_email(&w) {
            w = "<email>".into();
            n += 1;
        } else if let Some((k, v)) = w.split_once('=')
            && key_like(v)
        {
            w = format!("{k}=<secret>");
            n += 1;
        } else if key_like(w.trim_matches(|c: char| !c.is_ascii_alphanumeric())) {
            w = "<secret>".into();
            n += 1;
        } else if let Some(i) = w.find('?')
            && w.contains("://")
        {
            w.truncate(i);
            w.push_str("?<redacted>");
            n += 1;
        }
        out.push_str(&w);
        if words.peek().is_some() {
            out.push(' ');
        }
    }
    (out, n)
}

fn is_email(w: &str) -> bool {
    let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let Some((local, domain)) = w.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

/// 32 or more characters of the kinds keys, tokens and digests are made of, with at least one digit.
fn key_like(w: &str) -> bool {
    let w = w.trim_start_matches("sha256:");
    w.len() >= 32
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && w.bytes().any(|b| b.is_ascii_digit())
}

/// One log entry as either source gives it.
struct Entry {
    at: Option<DateTime<Utc>>,
    severity: String,
    text: String,
}

/// Makes the bundle and returns its directory.
pub async fn make(
    engine: &str,
    admin_key_file: Option<&Path>,
    out: Option<&Path>,
    logs: Logs,
    since_minutes: u32,
) -> Result<PathBuf> {
    let at = Utc::now();
    let dir = out.map(Path::to_path_buf).unwrap_or_else(|| {
        PathBuf::from(format!(
            "ampup-memory-bundle-{}",
            at.format("%Y%m%dT%H%M%SZ")
        ))
    });
    anyhow::ensure!(
        !dir.exists(),
        "{} exists already; name another with --out",
        dir.display()
    );
    std::fs::create_dir_all(&dir)?;
    let key = match admin_key_file {
        Some(f) => Some(
            std::fs::read_to_string(f)
                .with_context(|| format!("the admin key {}", f.display()))?
                .trim()
                .to_string(),
        ),
        None => None,
    };
    let base = engine.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut manifest = json!({
        "memstore_support_bundle": 1,
        "made_at": at.to_rfc3339(),
        "engine": host_of(base),
        "files": {},
        "redacted_spans": 0,
        "contains": "what a support session's diagnostics read (counts, closed sets, metric families without labels, config variable names) and a redacted log summary; no memory, no names of orgs, teams, users or scopes",
    });
    let mut files = serde_json::Map::new();
    let write = |files: &mut serde_json::Map<String, Value>, name: &str, v: &Value| -> Result<()> {
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(v)?)?;
        files.insert(name.into(), json!("ok"));
        Ok(())
    };

    // health: /healthz needs no key.
    let health = match http.get(format!("{base}/healthz")).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            json!({"status": status, "ok": v["ok"] == true})
        }
        Err(e) => json!({"unreachable": e.to_string()}),
    };
    eprintln!("health: {}", summary(&health));
    write(&mut files, "health.json", &json!({"healthz": health}))?;

    // telemetry, metrics, index_status, config: with the admin key, read through the contract's types.
    match &key {
        Some(k) => {
            match telemetry(&http, base, k).await {
                Ok((s, i)) => {
                    write(
                        &mut files,
                        "telemetry.json",
                        &json!({"state": s, "inventory": i}),
                    )?;
                    write(
                        &mut files,
                        "index_status.json",
                        &json!({
                            "index_lag_seconds": s.index_lag_seconds,
                            "index_failures": s.index_failures,
                            "scopes": i.scopes,
                            "episodes": i.episodes,
                            "facts": i.facts,
                        }),
                    )?;
                    write(
                        &mut files,
                        "config.json",
                        &json!({
                            "engine_version": s.engine_version,
                            "platform": s.platform,
                            "directory_event_version": s.directory_event_version,
                            "note": "the container's variables are read by the agent's config diagnostic, not from here",
                        }),
                    )?;
                    eprintln!(
                        "telemetry: health {:?}, version {:?}, index lag {:?} s",
                        s.health, s.engine_version, s.index_lag_seconds
                    );
                }
                Err(e) => {
                    eprintln!("telemetry: {e:#}");
                    files.insert("telemetry.json".into(), json!(format!("not read: {e:#}")));
                }
            }
            match get(&http, base, "/metrics", Some(k)).await {
                Ok((200, body)) => {
                    let fams = metric_families(&String::from_utf8_lossy(&body));
                    eprintln!("metrics: {} families", fams.len());
                    write(&mut files, "metrics.json", &json!({"families": fams}))?;
                }
                Ok((s, _)) => {
                    eprintln!("metrics: answered {s}");
                    files.insert("metrics.json".into(), json!(format!("not read: {s}")));
                }
                Err(e) => {
                    files.insert("metrics.json".into(), json!(format!("not read: {e:#}")));
                }
            }
        }
        None => {
            eprintln!("no --admin-key-file: telemetry, metrics, index status and config left out");
            for f in [
                "telemetry.json",
                "metrics.json",
                "index_status.json",
                "config.json",
            ] {
                files.insert(f.into(), json!("not read: no admin key"));
            }
        }
    }

    // logs
    let entries = match &logs {
        Logs::CloudRun {
            project,
            region,
            service,
        } => cloud_run_logs(project, region, service, at, since_minutes)?,
        Logs::File(f) => file_logs(f)?,
        Logs::None => {
            eprintln!("no --project or --logs-file: logs left out");
            vec![]
        }
    };
    let mut redacted = 0usize;
    if !matches!(logs, Logs::None) {
        let mut by_severity: BTreeMap<String, usize> = BTreeMap::new();
        let mut buckets: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut kept: Vec<String> = vec![];
        let mut dropped = 0usize;
        for e in &entries {
            *by_severity.entry(e.severity.clone()).or_default() += 1;
            if let Some(t) = e.at {
                let bucket = t
                    .format("%Y-%m-%dT%H:%M")
                    .to_string()
                    .chars()
                    .take(15)
                    .collect::<String>()
                    + "0";
                *buckets
                    .entry(bucket)
                    .or_default()
                    .entry(e.severity.clone())
                    .or_default() += 1;
            }
            if matches!(
                e.severity.as_str(),
                "ERROR" | "WARNING" | "CRITICAL" | "ALERT" | "EMERGENCY"
            ) {
                if kept.len() >= MAX_LINES {
                    dropped += 1;
                    continue;
                }
                let text: String = e.text.chars().take(MAX_LINE).collect();
                let (line, n) = redact(&text);
                redacted += n;
                kept.push(format!(
                    "{} {} {}",
                    e.at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                    e.severity,
                    line
                ));
            }
        }
        write(
            &mut files,
            "logs_summary.json",
            &json!({
                "entries": entries.len(),
                "since_minutes": since_minutes,
                "by_severity": by_severity,
                "by_ten_minutes": buckets,
                "error_lines_kept": kept.len(),
                "error_lines_dropped": dropped,
            }),
        )?;
        std::fs::write(dir.join("logs_errors.txt"), kept.join("\n") + "\n")?;
        files.insert("logs_errors.txt".into(), json!("ok"));
        eprintln!(
            "logs: {} entries, {} error or warning lines kept, {} spans redacted",
            entries.len(),
            kept.len(),
            redacted
        );
    }

    manifest["files"] = Value::Object(files);
    manifest["redacted_spans"] = json!(redacted);
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    eprintln!(
        "written to {}; read it, then send it with the ticket if you choose: tar czf {}.tar.gz {}",
        dir.display(),
        dir.display(),
        dir.display()
    );
    Ok(dir)
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

fn summary(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

async fn get(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    key: Option<&str>,
) -> Result<(u16, Vec<u8>)> {
    let mut req = http.get(format!("{base}{path}"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let r = req.send().await.with_context(|| format!("GET {path}"))?;
    let status = r.status().as_u16();
    Ok((status, r.bytes().await?.to_vec()))
}

/// The engine's state and inventory as its exporter would send them, through the contract's own types.
async fn telemetry(http: &reqwest::Client, base: &str, key: &str) -> Result<(State, Inventory)> {
    let (status, body) = get(http, base, "/telemetry", Some(key)).await?;
    anyhow::ensure!(
        status == 200,
        "/telemetry answered {status} (is the admin key the operator's?)"
    );
    let v: Value = serde_json::from_slice(&body).context("/telemetry: not JSON")?;
    let (mut state, mut inventory) = (None, None);
    for e in v["would_send"]
        .as_array()
        .context("/telemetry: no would_send")?
    {
        let e: Envelope =
            serde_json::from_value(e.clone()).context("/telemetry: outside the contract")?;
        match e.payload {
            Payload::State(s) => state = Some(s),
            Payload::Inventory(i) => inventory = Some(i),
            Payload::Alerts(_) | Payload::Usage(_) | Payload::Advice(_) => {}
        }
    }
    Ok((
        state.context("/telemetry: no state")?,
        inventory.context("/telemetry: no inventory")?,
    ))
}

/// `gcloud logging read` on the service, newest first, up to 5,000 entries; printed before it runs.
fn cloud_run_logs(
    project: &str,
    region: &str,
    service: &str,
    at: DateTime<Utc>,
    since_minutes: u32,
) -> Result<Vec<Entry>> {
    let since = (at - chrono::Duration::minutes(since_minutes as i64)).to_rfc3339();
    let filter = format!(
        "resource.type=\"cloud_run_revision\" AND resource.labels.service_name=\"{service}\" AND resource.labels.location=\"{region}\" AND timestamp>=\"{since}\""
    );
    let args = [
        "logging",
        "read",
        &filter,
        "--project",
        project,
        "--limit",
        "5000",
        "--order=desc",
        "--format=json(timestamp,severity,textPayload,jsonPayload.message)",
    ];
    eprintln!(
        "  $ gcloud {}",
        args.iter()
            .map(|a| if a.contains(' ') {
                format!("'{a}'")
            } else {
                a.to_string()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    let out = std::process::Command::new("gcloud")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("gcloud (is it installed and on PATH?)")?;
    anyhow::ensure!(
        out.status.success(),
        "gcloud logging read: {}",
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
    );
    let v: Value = serde_json::from_slice(&out.stdout).context("gcloud: not JSON")?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .map(|e| Entry {
                    at: e["timestamp"]
                        .as_str()
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&Utc)),
                    severity: e["severity"].as_str().unwrap_or("DEFAULT").to_string(),
                    text: e["textPayload"]
                        .as_str()
                        .or(e["jsonPayload"]["message"].as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Lines from a file. The severity is the first of ERROR, WARN, INFO, DEBUG found in the line's first 60
/// characters (what tracing and the engine's own `eprintln!` lines carry), else DEFAULT; a leading RFC 3339
/// time is the entry's time.
fn file_logs(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let head: String = l.chars().take(60).collect();
            let severity = ["ERROR", "WARN", "INFO", "DEBUG"]
                .into_iter()
                .find(|s| head.contains(s))
                .map(|s| if s == "WARN" { "WARNING" } else { s })
                .unwrap_or("DEFAULT")
                .to_string();
            let at = l
                .split_whitespace()
                .next()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            Entry {
                at,
                severity,
                text: l.to_string(),
            }
        })
        .collect())
}
