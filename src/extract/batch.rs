//! OpenRouter's Batch API (`AMPUP_MEMORY_EXTRACT=openrouter-batch`, for experiments): the same requests at half the
//! price, answered within 24 hours (usually minutes) instead of seconds.
//!
//! A caller asks as it would synchronously ([`super::Extractor::complete_with`]) and waits for its own reply.
//! Everything asked of one model within a short window (`AMPUP_MEMORY_BATCH_GATHER_SECS`, default 5) goes to
//! OpenRouter as one batch, up to `AMPUP_MEMORY_BATCH_MAX_REQUESTS` requests (default 5,000) and
//! `AMPUP_MEMORY_BATCH_MAX_MB` of request bodies (default 100); a run that asks more fills several batches at once.
//! In batch mode the extractor and the compiler put every request of a run in flight together
//! ([`super::Extractor::parallel`]), so a run's requests share batches.
//!
//! The batch is polled until it ends, its results go back to their callers, and it is deleted, so the text
//! doesn't stay at OpenRouter for the 30 days it otherwise keeps a batch. As with a synchronous call, a request
//! the API refuses (a 4xx, a failed batch) costs its episode a try, and one it couldn't answer (a 5xx, an
//! expired or cancelled batch, no answer within a day) costs none.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{HTTP, Refused};

/// The longest a batch is waited on: its window is a day.
const DEADLINE: Duration = Duration::from_secs(25 * 3600);

struct Job {
    body: Value,
    reply: oneshot::Sender<Result<Value, Failure>>,
}

/// Why a request got no reply: the API refused it (costs a try) or couldn't answer it (costs none).
#[derive(Clone, Debug)]
enum Failure {
    Refused(String),
    Down(String),
}

impl Failure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Failure::Refused(m) => Refused(m).into(),
            Failure::Down(m) => anyhow!(m),
        }
    }
}

/// One queue per API, model and key, each drained by a task of its own.
static QUEUES: LazyLock<Mutex<HashMap<String, mpsc::UnboundedSender<Job>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The reply to one chat-completions `body`, through a batch at `url` (`…/api/v1`).
pub(crate) async fn ask(url: &str, key: Option<&str>, model: &str, body: Value) -> Result<Value> {
    let name = format!("{url}\n{model}\n{}", key.unwrap_or_default());
    let (tx, rx) = oneshot::channel();
    let mut job = Job { body, reply: tx };
    // A queue whose task has ended is replaced.
    for _ in 0..2 {
        let queue = QUEUES
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_insert_with(|| {
                let (qtx, qrx) = mpsc::unbounded_channel();
                tokio::spawn(drain(
                    url.to_string(),
                    key.map(String::from),
                    model.to_string(),
                    qrx,
                ));
                qtx
            })
            .clone();
        match queue.send(job) {
            Ok(()) => {
                return rx
                    .await
                    .map_err(|_| anyhow!("{url}/batches: the batch ended without an answer"))?
                    .map_err(Failure::into_error);
            }
            Err(mpsc::error::SendError(j)) => {
                job = j;
                QUEUES.lock().unwrap().remove(&name);
            }
        }
    }
    Err(anyhow!("{url}/batches: no batch queue"))
}

/// Gather what arrives within the window into batches, and send each.
async fn drain(
    url: String,
    key: Option<String>,
    model: String,
    mut rx: mpsc::UnboundedReceiver<Job>,
) {
    let gather = Duration::from_secs(setting("AMPUP_MEMORY_BATCH_GATHER_SECS", 5));
    let max_requests = setting("AMPUP_MEMORY_BATCH_MAX_REQUESTS", 5_000) as usize;
    let max_bytes = setting("AMPUP_MEMORY_BATCH_MAX_MB", 100) as usize * 1_000_000;
    while let Some(first) = rx.recv().await {
        let until = tokio::time::Instant::now() + gather;
        let mut jobs = vec![first];
        while let Ok(Some(j)) = tokio::time::timeout_at(until, rx.recv()).await {
            jobs.push(j);
        }
        // As many batches as the limits need, sent together.
        let mut batch: Vec<Job> = vec![];
        let mut bytes = 0;
        for j in jobs {
            let n = j.body.to_string().len();
            if !batch.is_empty() && (batch.len() >= max_requests || bytes + n > max_bytes) {
                tokio::spawn(send(
                    url.clone(),
                    key.clone(),
                    model.clone(),
                    std::mem::take(&mut batch),
                ));
                bytes = 0;
            }
            bytes += n;
            batch.push(j);
        }
        tokio::spawn(send(url.clone(), key.clone(), model.clone(), batch));
    }
}

/// One batch: submit it, wait for it to end, hand each caller its reply, delete it.
async fn send(url: String, key: Option<String>, model: String, jobs: Vec<Job>) {
    let (bodies, replies): (Vec<Value>, Vec<_>) =
        jobs.into_iter().map(|j| (j.body, j.reply)).unzip();
    let n = bodies.len();
    match run(&url, key.as_deref(), &model, bodies).await {
        Ok((id, mut results)) => {
            for (i, reply) in replies.into_iter().enumerate() {
                let r = results.remove(&format!("r{i}")).unwrap_or_else(|| {
                    Err(Failure::Down(format!(
                        "{url}/batches/{id}: no result for request {i}"
                    )))
                });
                let _ = reply.send(r);
            }
            // Once read, the batch's text needn't stay at OpenRouter.
            let _ = authed(HTTP.delete(format!("{url}/batches/{id}")), key.as_deref())
                .send()
                .await;
            eprintln!("batch {id}: {n} requests to {model} answered");
        }
        Err(f) => {
            eprintln!("batch of {n} requests to {model} failed: {f:?}");
            for reply in replies {
                let _ = reply.send(Err(f.clone()));
            }
        }
    }
}

fn authed(req: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    let req = req
        .header("X-Title", "memstore")
        .header("HTTP-Referer", "https://github.com/A79-ai/ampup-memory");
    match key {
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

/// A 5xx, 408 or 429 is the API down, as is a 401 to 404 (our key, account, access or model name: the
/// synchronous path's rule); any other non-success is a refusal.
fn classify(status: u16, msg: String) -> Failure {
    if status >= 500 || matches!(status, 401 | 402 | 403 | 404 | 408 | 429) {
        Failure::Down(msg)
    } else {
        Failure::Refused(msg)
    }
}

type Results = HashMap<String, Result<Value, Failure>>;

async fn run(
    url: &str,
    key: Option<&str>,
    model: &str,
    bodies: Vec<Value>,
) -> Result<(String, Results), Failure> {
    let requests: Vec<Value> = bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| json!({"custom_id": format!("r{i}"), "body": body}))
        .collect();
    // `endpoint` and `model` must come before `requests`, which serde_json's sorted keys give.
    let submit = json!({"endpoint": "/v1/chat/completions", "model": model, "requests": requests});
    let res = authed(HTTP.post(format!("{url}/batches")), key)
        .header("content-type", "application/json")
        .body(submit.to_string())
        .send()
        .await
        .map_err(|e| Failure::Down(format!("POST {url}/batches: {e}")))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = format!(
            "{url}/batches: {status}: {}",
            text.chars().take(300).collect::<String>()
        );
        return Err(classify(status.as_u16(), msg));
    }
    let id = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["id"].as_str().map(String::from))
        .ok_or_else(|| Failure::Refused(format!("{url}/batches: no batch id in the reply")))?;
    // Poll until it ends: every 10 s at first, then less often, at most once a minute.
    let started = tokio::time::Instant::now();
    let mut every = Duration::from_secs(10);
    loop {
        tokio::time::sleep(every).await;
        every = (every * 3 / 2).min(Duration::from_secs(60));
        if started.elapsed() > DEADLINE {
            return Err(Failure::Down(format!(
                "{url}/batches/{id}: not done within a day"
            )));
        }
        let Ok(res) = authed(HTTP.get(format!("{url}/batches/{id}")), key)
            .send()
            .await
        else {
            continue;
        };
        let Ok(v) = res.json::<Value>().await else {
            continue;
        };
        match v["status"].as_str().unwrap_or_default() {
            "completed" => return Ok((id.clone(), results(&id, &v))),
            "failed" => {
                return Err(Failure::Refused(format!(
                    "{url}/batches/{id} failed: {}",
                    v["error"].to_string().chars().take(300).collect::<String>()
                )));
            }
            s @ ("expired" | "cancelled") => {
                return Err(Failure::Down(format!("{url}/batches/{id}: {s}")));
            }
            _ => {}
        }
    }
}

/// Each request's reply body, or why it has none, by its `custom_id`.
fn results(id: &str, v: &Value) -> Results {
    let mut out = HashMap::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let Some(cid) = r["custom_id"].as_str() else {
            continue;
        };
        let res = &r["response"];
        let got = if res.is_object() {
            let status = res["status_code"].as_u64().unwrap_or(200) as u16;
            if (200..300).contains(&status) {
                Ok(res["body"].clone())
            } else {
                let body = res["body"].to_string();
                // As a synchronous call says it (`400 Bad Request`), so a refusal reads the same either way.
                let said = reqwest::StatusCode::from_u16(status)
                    .map_or_else(|_| status.to_string(), |s| s.to_string());
                Err(classify(
                    status,
                    format!(
                        "batch {id} {cid}: {said}: {}",
                        body.chars().take(300).collect::<String>()
                    ),
                ))
            }
        } else {
            let e = r["error"].to_string();
            Err(Failure::Refused(format!(
                "batch {id} {cid}: {}",
                e.chars().take(300).collect::<String>()
            )))
        };
        out.insert(cid.to_string(), got);
    }
    out
}
