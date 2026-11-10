//! Cells: orgs split across deployments sharing one catalog, so one org's warm-up, indexes and background
//! work never take memory or CPU from another's. Each cell's servers keep a writer lease of their own
//! (`_writer-<cell>`), warm and do background work only for the orgs mapped to their cell, and the default
//! cell's servers forward a request that selects another cell's org to that cell.
//!
//! - `MEMSTORE_CELL`: this server's cell (default `default`).
//! - `MEMSTORE_CELL_MAP`: JSON, org to cell (`{"acme": "acme"}`); an org not named is the default cell's.
//! - `MEMSTORE_CELL_URLS`: JSON, cell to the URL the default cell forwards to (`{"acme": "http://memstore-acme"}`).
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
    LazyLock::new(|| match std::env::var("MEMSTORE_CELL_URLS") {
        Ok(v) if !v.trim().is_empty() => serde_json::from_str(&v)
            .unwrap_or_else(|e| panic!("MEMSTORE_CELL_URLS is a JSON object of strings: {e}")),
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
    reply(
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
    send(
        url,
        &parts.method,
        &parts.uri,
        parts.headers,
        body.to_bytes(),
    )
    .await
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
    send(url, method, uri, headers, body).await
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

/// The request answered by the cell at `url`.
async fn send(
    url: &str,
    method: &hyper::Method,
    uri: &hyper::Uri,
    mut headers: hyper::HeaderMap,
    body: Bytes,
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
            return gone();
        }
    };
    let (status, mut headers) = (res.status(), res.headers().clone());
    let Ok(body) = res.bytes().await else {
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
