//! Cells: orgs split across deployments sharing one catalog, so one org's warm-up, indexes and background
//! work never take memory or CPU from another's. Each cell's servers keep a writer lease of their own
//! (`_writer-<cell>`), warm and do background work only for the orgs mapped to their cell, and the default
//! cell's servers forward a request that selects another cell's org to that cell.
//!
//! - `AMPUP_MEMORY_CELL`: this server's cell (default `default`).
//! - `AMPUP_MEMORY_CELL_MAP`: JSON, org to cell (`{"acme": "acme"}`); an org not named is the default cell's.
//! - `AMPUP_MEMORY_CELL_URLS`: JSON, cell to the URL the default cell forwards to (`{"acme": "http://memstore-acme"}`).
//! - `AMPUP_MEMORY_CELL_READ_TIMEOUT_SECS`: how long a console read sent to another cell may take before it is
//!   answered 504 here and the forward dropped (default 45, under the console proxy's own 50 s).
//! - `AMPUP_MEMORY_CELL_MAX_READS`: console reads in flight to any one cell at once; past it, 503 (default 64).
//!
//! Only the console's reads ([`console_read`]) have a deadline or a cap. A write or an agent's call (`/mcp`,
//! `/remember`, a session's start or end) is waited on for as long as it takes: its client, told it timed out,
//! would send it again while the first one still runs.
//!
//! A user's own scope's background work and the catalog-wide work (the ledger's compaction) are the default
//! cell's. Only the default cell forwards, so a request never goes round in a loop. Another cell's org is never
//! served or loaded here: its store refuses it (`memstore_core::cell`), and where its cell can't be reached
//! the request is answered 503.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::Scope;
pub use memstore_core::cell::{DEFAULT, is_default, me, of_org};

/// Cell to the URL the default cell forwards to.
static URLS: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| match std::env::var("AMPUP_MEMORY_CELL_URLS") {
        Ok(v) if !v.trim().is_empty() => serde_json::from_str(&v)
            .unwrap_or_else(|e| panic!("AMPUP_MEMORY_CELL_URLS is a JSON object of strings: {e}")),
        _ => HashMap::new(),
    });

/// Read the settings now, so a malformed one stops the start rather than the first request.
pub fn check() {
    let map = memstore_core::cell::map();
    LazyLock::force(&URLS);
    if me() != DEFAULT || !map.is_empty() {
        eprintln!("cell {}: orgs mapped to cells {map:?}", me());
    }
}

/// This server's cell does `scope`'s warm-up and background work (a person's own scope's: the default cell).
pub fn mine(scope: &Scope) -> bool {
    scope.org_name().map_or(DEFAULT, of_org) == me()
}

/// Whether `org` is another cell's, here on the default cell: never served or loaded here, only sent to that
/// cell ([`forward`], [`forward_read`]) or refused ([`unavailable`]).
pub fn elsewhere(org: &str) -> bool {
    is_default() && of_org(org) != DEFAULT
}

/// Where the default cell sends a request for another cell's `org`: None when that cell has no URL.
fn url_of(org: &str) -> Option<&'static str> {
    URLS.get(of_org(org)).map(|u| u.trim_end_matches('/'))
}

/// The answer for another cell's `org` when that cell can't take the request (no ready server, or no URL):
/// asked again later, never served here, so its memory is never loaded beside the default cell's.
pub fn unavailable(org: &str) -> Response<Full<Bytes>> {
    later(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("{org}'s servers aren't taking requests just now: try again"),
    )
}

/// The writer lease's directory name: one lease per cell.
pub fn lease_dir() -> String {
    if is_default() {
        "_writer".into()
    } else {
        format!("_writer-{}", me())
    }
}

use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

/// Whether `url`'s cell takes connections, known for a few seconds at a time: a cell with no ready
/// server (its Service refuses connections) is answered [`unavailable`].
async fn reachable(url: &str) -> bool {
    static SEEN: LazyLock<std::sync::Mutex<HashMap<String, (Instant, bool)>>> =
        LazyLock::new(Default::default);
    if let Some((at, up)) = SEEN.lock().unwrap().get(url)
        && at.elapsed() < Duration::from_secs(5)
    {
        return *up;
    }
    let addr = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| Some(format!("{}:{}", u.host_str()?, u.port_or_known_default()?)));
    let up = match addr {
        Some(a) => matches!(
            tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(a)).await,
            Ok(Ok(_))
        ),
        None => false,
    };
    if !up {
        eprintln!("cell at {url} unreachable: its orgs' requests are answered 503");
    }
    SEEN.lock()
        .unwrap()
        .insert(url.into(), (Instant::now(), up));
    up
}

/// `req`, for another cell's `org`, answered by that cell, or [`unavailable`] when it can't be reached.
pub async fn forward(org: &str, req: Request<Incoming>, max_body: usize) -> Response<Full<Bytes>> {
    let Some(url) = url_of(org) else {
        return unavailable(org);
    };
    if !reachable(url).await {
        return unavailable(org);
    }
    let (parts, body) = req.into_parts();
    // No more than the largest body any route takes: the cell checks its own route's limit.
    let too_large = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
        .is_some_and(|n| n > max_body);
    let body = match too_large {
        true => Err(()),
        false => http_body_util::Limited::new(body, max_body)
            .collect()
            .await
            .map_err(|_| ()),
    };
    let Ok(body) = body else {
        return reply(StatusCode::PAYLOAD_TOO_LARGE, "request too large");
    };
    let body = body.to_bytes();
    if !console_read(&parts.method, parts.uri.path(), &body) {
        return send(url, &parts.method, &parts.uri, parts.headers, body, None).await;
    }
    // Held until the read is answered or given up on, so a cell that has stopped answering holds no more of
    // this server's requests than the cap.
    let Ok(_reading) = reads_to(url).try_acquire_owned() else {
        eprintln!(
            "cell at {url}: {} console reads already waiting on it: answered 503",
            *MAX_READS
        );
        return later(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{org}'s servers are busy just now: try again"),
        );
    };
    let deadline = Some(*READ_DEADLINE);
    send(
        url,
        &parts.method,
        &parts.uri,
        parts.headers,
        body,
        deadline,
    )
    .await
}

/// How long a console read sent to another cell may take (`AMPUP_MEMORY_CELL_READ_TIMEOUT_SECS`, default 45 s):
/// under the console proxy's own 50 s, so this server stops waiting on it no later than its caller does.
static READ_DEADLINE: LazyLock<Duration> = LazyLock::new(|| {
    let secs = std::env::var("AMPUP_MEMORY_CELL_READ_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(45);
    Duration::from_secs(secs)
});

/// Console reads in flight to any one cell at once (`AMPUP_MEMORY_CELL_MAX_READS`, default 64).
static MAX_READS: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("AMPUP_MEMORY_CELL_MAX_READS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(64)
});

/// The console reads in flight to the cell at `url`.
fn reads_to(url: &str) -> std::sync::Arc<tokio::sync::Semaphore> {
    static READS: LazyLock<
        std::sync::Mutex<HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>,
    > = LazyLock::new(Default::default);
    READS
        .lock()
        .unwrap()
        .entry(url.into())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(*MAX_READS)))
        .clone()
}

/// The `/admin` ops the console reads with (its review and admin pages): none of them changes anything.
const ADMIN_READS: &[&str] = &[
    "show",
    "get_schema",
    "audit",
    "activity",
    "conversation",
    "user_facts",
    "advice_trail",
    "advice_traces",
    "advice_status",
    "branch_diff",
    "entity_owners",
    "list_proposals",
    "get_proposal",
    "entity_graph",
    "list_conflicts",
    "dream_decisions",
];

/// Whether a request is one of the console's reads: `/me`, the Explorer's `/browse` and its lists (`/lessons`
/// among them), and the `/admin` ops in [`ADMIN_READS`]. Only these are given up on ([`READ_DEADLINE`]): asked
/// again, they change nothing. Every other route, and an `/admin` op that isn't listed, is waited on.
fn console_read(method: &hyper::Method, path: &str, body: &[u8]) -> bool {
    match path {
        "/me" => true,
        "/browse" | "/facts/list" | "/advice/list" | "/lessons" => method == hyper::Method::POST,
        "/admin" => serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["op"].as_str().map(|op| ADMIN_READS.contains(&op)))
            .unwrap_or(false),
        _ => false,
    }
}

/// A request already read here (`body`) that turned out to be for `org` (and its `team`, where the scope is
/// a team's), another cell's, though it didn't name the org: answered by that cell, the org and team named for
/// it as they were resolved here; [`unavailable`] when that cell can't be reached.
pub async fn forward_read(
    (org, team): (&str, Option<&str>),
    method: &hyper::Method,
    uri: &hyper::Uri,
    mut headers: hyper::HeaderMap,
    body: Bytes,
) -> Response<Full<Bytes>> {
    let Some(url) = url_of(org) else {
        return unavailable(org);
    };
    if !reachable(url).await {
        return unavailable(org);
    }
    // Names that resolved to a scope are valid header values.
    for (h, v) in [("x-memstore-org", Some(org)), ("x-memstore-team", team)] {
        if let Some(v) = v.and_then(|v| v.parse().ok()) {
            headers.insert(h, v);
        }
    }
    // Waited on whatever it is: this path forwards writes and agents' calls too.
    send(url, method, uri, headers, body, None).await
}

const HOP: [&str; 5] = [
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

fn reply(code: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(
        serde_json::json!({ "error": msg }).to_string(),
    )));
    *r.status_mut() = code;
    r
}

/// [`reply`], with when to ask again: about when [`reachable`] looks at the cell again.
fn later(code: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut r = reply(code, msg);
    r.headers_mut().insert(
        hyper::header::RETRY_AFTER,
        hyper::header::HeaderValue::from_static("5"),
    );
    r
}

/// The request answered by the cell at `url`; given up on after `deadline`, where there is one, and answered 504:
/// the forward is dropped then, which closes its connection, so the cell can stop on it too.
async fn send(
    url: &str,
    method: &hyper::Method,
    uri: &hyper::Uri,
    mut headers: hyper::HeaderMap,
    body: Bytes,
    deadline: Option<Duration>,
) -> Response<Full<Bytes>> {
    static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    });
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    for h in HOP {
        headers.remove(h);
    }
    // Taken by then, a failure here may have reached the cell: never served a second time here.
    let gone = || {
        reply(
            StatusCode::BAD_GATEWAY,
            "this org's server didn't answer: try again",
        )
    };
    let exchange = async {
        let res = match HTTP
            .request(method.clone(), format!("{url}{path}"))
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("forwarding to the cell at {url} failed: {e}");
                return None;
            }
        };
        let (status, headers) = (res.status(), res.headers().clone());
        let body = res.bytes().await.ok()?;
        Some((status, headers, body))
    };
    let answered = match deadline {
        None => exchange.await,
        Some(d) => match tokio::time::timeout(d, exchange).await {
            Ok(a) => a,
            Err(_) => {
                eprintln!(
                    "cell at {url} didn't answer {} {} within {}s: given up, answered 504",
                    method,
                    uri.path(),
                    d.as_secs()
                );
                return later(
                    StatusCode::GATEWAY_TIMEOUT,
                    "this org's server is slow to answer just now: try again",
                );
            }
        },
    };
    let Some((status, mut headers, body)) = answered else {
        return gone();
    };
    for h in HOP {
        headers.remove(h);
    }
    let mut r = Response::new(Full::new(body));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

/// Whether this server forwards to other cells at all (the default cell, with orgs mapped to other cells).
pub fn forwards() -> bool {
    is_default() && !memstore_core::cell::map().is_empty()
}
