//! The offboarding and isolation flows on a real Iceberg REST catalog over object storage, with two
//! `memstore serve` OS processes (the built binary) sharing it, driven over HTTP. Every other test runs on the
//! in-memory catalog with its servers in the test's own process; these exercise what only a REST catalog
//! does: rewrites committed through the REST protocol, purges done by the catalog server, and the files left in
//! an object store, listed there.
//!
//! - Offboarding: a person forgotten through one process is gone from every read path of both, and their
//!   tags are in no object under the org's tables or search index; an org deleted through the other leaves
//!   no object of its tables or index, no table or namespace in the catalog, and no row in the audit or key
//!   usage tables; the other org keeps everything.
//! - Isolation: every caller reads only the scope they are entitled to, through either process.
//! - The catalog can rewrite a table (`Store::check_rewrites`), which forget and org delete need; one throttled
//!   while they check (BigLake's 429) has them answered busy (503, the catalog's `Retry-After`), nothing changed.
//! - Home's answers shared between servers (`AMPUP_MEMORY_SHARED_ANSWERS=object`): an episode forgotten through
//!   one process (sharing or not), or aged out under a retention policy, is in no Home either process or a new
//!   one answers, and in no shared answer left in the bucket.
//!
//! - Home worked out at start for who read it lately: a server process started after another wrote its readers
//!   down answers each one's first Home at once, as they stand when it starts, and to them alone; one who left
//!   reads nothing, one whose standing changed after it started gets theirs as they stand now.
//!
//! They need the fixture in `tests/fixtures/rest-catalog` (the Iceberg REST fixture and MinIO):
//!
//! ```text
//! docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait
//! source tests/fixtures/rest-catalog/env.sh
//! cargo test --test it rest_catalog::
//! ```
//!
//! Without `AMPUP_MEMORY_TEST_REST_URI` each test prints that it was skipped and passes without running. Every
//! test makes orgs and tags of its own, so tests (and runs) share one fixture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use http_body_util::Full;
use hyper::body::Bytes;
use memstore::{AclTarget, Episode, Fact, Scope, Store, auth, platform};
use serde_json::{Value, json};
use sha2::Digest;

const CLIENT_ID: &str = "memstore-cli";
const ADMIN_KEY: &str = "msk_platform_admin_for_rest_catalog";

static CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// The fixture, as the environment names it.
struct Rest {
    uri: String,
    warehouse: String,
    s3_endpoint: String,
    bucket: String,
    key: String,
    secret: String,
    /// Where this run's search indexes go (`s3://<bucket>/index/<run>`).
    index: String,
    /// More catalog props, `key=value;...` (`AMPUP_MEMORY_TEST_REST_PROPS`: a catalog's credential and scope).
    extra: String,
}

impl Rest {
    /// The fixture, or None (and the test says it was skipped) without `AMPUP_MEMORY_TEST_REST_URI`. Also None
    /// once the test passed in a process of its own (`crate::own_process`): this sets the index's S3 settings
    /// in the environment, which no other test may see.
    fn from_env(test: &str) -> Option<Rest> {
        let Ok(uri) = std::env::var("AMPUP_MEMORY_TEST_REST_URI") else {
            eprintln!(
                "SKIPPED {test}: AMPUP_MEMORY_TEST_REST_URI is not set. Start tests/fixtures/rest-catalog \
                 (docker compose) and `source tests/fixtures/rest-catalog/env.sh` to run it."
            );
            return None;
        };
        if !crate::own_process(&format!("rest_catalog::{test}")) {
            return None;
        }
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let bucket = var("AMPUP_MEMORY_TEST_S3_BUCKET", "memstore");
        let rest = Rest {
            uri,
            warehouse: var(
                "AMPUP_MEMORY_TEST_REST_WAREHOUSE",
                "s3://memstore/warehouse",
            ),
            s3_endpoint: var("AMPUP_MEMORY_TEST_S3_ENDPOINT", "http://localhost:19000"),
            index: format!("s3://{bucket}/index/{}", run_id()),
            bucket,
            key: var("AMPUP_MEMORY_TEST_S3_KEY", "admin"),
            secret: var("AMPUP_MEMORY_TEST_S3_SECRET", "password"),
            extra: var("AMPUP_MEMORY_TEST_REST_PROPS", ""),
        };
        // The search index (Lance) reads its S3 settings from the environment, in this process and in the
        // servers it starts. Set once, before any store of this process opens an index.
        static ENV: std::sync::Once = std::sync::Once::new();
        ENV.call_once(|| unsafe {
            std::env::set_var("AMPUP_MEMORY_INDEX_URI", &rest.index);
            std::env::set_var("AWS_ACCESS_KEY_ID", &rest.key);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", &rest.secret);
            std::env::set_var("AWS_REGION", "us-east-1");
            std::env::set_var("AWS_ENDPOINT", &rest.s3_endpoint);
            std::env::set_var("AWS_ALLOW_HTTP", "true");
        });
        Some(rest)
    }

    /// Catalog and storage props besides the address, as `AMPUP_MEMORY_REST_PROPS` takes them.
    fn storage_props(&self) -> Vec<(String, String)> {
        let mut props: Vec<(String, String)> = [
            ("s3.endpoint", self.s3_endpoint.clone()),
            ("s3.access-key-id", self.key.clone()),
            ("s3.secret-access-key", self.secret.clone()),
            ("s3.region", "us-east-1".into()),
            ("s3.path-style-access", "true".into()),
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into();
        for kv in self.extra.split(';') {
            if let Some((k, v)) = kv.split_once('=') {
                props.push((k.trim().into(), v.trim().into()));
            }
        }
        props
    }

    async fn catalog(&self) -> Arc<dyn iceberg::Catalog> {
        let mut props: HashMap<String, String> = self.storage_props().into_iter().collect();
        props.insert("uri".into(), self.uri.clone());
        props.insert("warehouse".into(), self.warehouse.clone());
        platform::rest("rest", props).await.unwrap()
    }

    fn s3(&self) -> opendal::Operator {
        let b = opendal::services::S3::default()
            .bucket(&self.bucket)
            .endpoint(&self.s3_endpoint)
            .region("us-east-1")
            .access_key_id(&self.key)
            .secret_access_key(&self.secret);
        opendal::Operator::new(b).unwrap().finish()
    }

    /// The bucket key a `s3://<bucket>/...` location names.
    fn key_of(&self, location: &str) -> String {
        location
            .strip_prefix(&format!("s3://{}/", self.bucket))
            .unwrap_or_else(|| panic!("{location} is not in bucket {}", self.bucket))
            .trim_end_matches('/')
            .to_string()
    }

    /// Every object under `prefixes` (bucket keys) whose bytes hold `tag` (a data file's rows uncompressed:
    /// [`crate::readable`]).
    async fn objects_with(&self, prefixes: &[String], tag: &str) -> Vec<String> {
        let op = self.s3();
        let mut out = vec![];
        for p in prefixes {
            let mut entries = op
                .lister_with(&format!("{p}/"))
                .recursive(true)
                .await
                .unwrap();
            while let Some(e) = entries.try_next().await.unwrap() {
                if e.metadata().is_dir() {
                    continue;
                }
                let bytes = crate::readable(op.read(e.path()).await.unwrap().to_vec(), e.path());
                if bytes.windows(tag.len()).any(|w| w == tag.as_bytes()) {
                    out.push(e.path().to_string());
                }
            }
        }
        out
    }

    /// How many objects are under `prefixes`.
    async fn objects(&self, prefixes: &[String]) -> usize {
        let op = self.s3();
        let mut n = 0;
        for p in prefixes {
            let mut entries = op
                .lister_with(&format!("{p}/"))
                .recursive(true)
                .await
                .unwrap();
            while let Some(e) = entries.try_next().await.unwrap() {
                n += usize::from(!e.metadata().is_dir());
            }
        }
        n
    }
}

/// One id per process (each test here has its own): its search indexes, orgs and tags carry it.
fn run_id() -> &'static str {
    static ID: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    &ID
}

/// A name of this run's own: `<what><run>`.
fn fresh(what: &str) -> String {
    format!("{what}{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A `memstore serve` process; killed when dropped, its log printed if the test failed.
struct Server {
    url: String,
    child: Child,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(40).collect();
            eprintln!(
                "--- {} ({}), last lines:\n{}",
                self.url,
                self.log.display(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
    }
}

/// The deployment's keys file: the operator's admin key.
fn keys_file(dir: &Path) -> PathBuf {
    let keys = json!([{"id": "platform", "user": "svc:platform", "admin": true,
                       "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes()))}]);
    let path = dir.join("keys.json");
    std::fs::write(&path, keys.to_string()).unwrap();
    path
}

/// Start `memstore serve org <port>` on the REST catalog at `uri` (the fixture, or a stand-in in front of
/// it), taking the operator's key and people's ID tokens from `issuer`.
async fn serve(rest: &Rest, uri: &str, dir: &Path, issuer: &str) -> Server {
    serve_env(rest, uri, dir, issuer, &[]).await
}

/// [`serve`], with `env` set too.
async fn serve_env(
    rest: &Rest,
    uri: &str,
    dir: &Path,
    issuer: &str,
    env: &[(&str, &str)],
) -> Server {
    let port = free_port();
    let log = dir.join(format!("serve-{port}.log"));
    let out = std::fs::File::create(&log).unwrap();
    let props: Vec<String> = rest
        .storage_props()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let child = Command::new(env!("CARGO_BIN_EXE_ampup-memory"))
        .args(["serve", "org", &port.to_string()])
        .env("AMPUP_MEMORY_PLATFORM", "rest")
        .env("AMPUP_MEMORY_REST_URI", uri)
        .env("AMPUP_MEMORY_WAREHOUSE", &rest.warehouse)
        .env("AMPUP_MEMORY_REST_PROPS", props.join(";"))
        .env("AMPUP_MEMORY_CALLER_IDENTITY", "token")
        .env("AMPUP_MEMORY_API_KEYS", keys_file(dir))
        .env("AMPUP_MEMORY_OIDC_ISSUER", issuer)
        .env("AMPUP_MEMORY_OIDC_AUDIENCE", CLIENT_ID)
        .env("AMPUP_MEMORY_ALLOW_LOCAL_ISSUERS", "1")
        .env("AMPUP_MEMORY_DIRECTORY_SNAPSHOT_EVERY", "7")
        .env("AMPUP_MEMORY_AUDIT_FLUSH_SECS", "1")
        .env("AMPUP_MEMORY_CONFIG_DIR", dir)
        .env_remove("AMPUP_MEMORY_DISABLED_FILE")
        .env_remove("MEMSTORE_DISABLED_FILE")
        .env_remove("AMPUP_MEMORY_TEAM_CSV")
        .env_remove("MEMSTORE_TEAM_CSV")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let mut server = Server {
        url: format!("http://127.0.0.1:{port}"),
        child,
        log,
    };
    let up = Instant::now();
    while reqwest::get(&server.url).await.is_err() {
        if let Some(status) = server.child.try_wait().unwrap() {
            panic!(
                "memstore serve exited ({status}): {}",
                std::fs::read_to_string(&server.log).unwrap_or_default()
            );
        }
        assert!(
            up.elapsed() < Duration::from_secs(60),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server
}

/// A person's ID token from the stand-in identity provider.
fn id_token(issuer: &str, user: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("k1".into());
    jsonwebtoken::encode(
        &header,
        &json!({"iss": issuer, "aud": CLIENT_ID, "sub": format!("sub-{user}"), "email": user,
                "email_verified": true, "iat": now, "exp": now + 3600}),
        &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!("../fixtures/idp_key.pem"))
            .unwrap(),
    )
    .unwrap()
}

/// The identity provider's public side, in this process: OpenID discovery and its signing key. The servers,
/// other processes, fetch it over HTTP.
async fn start_idp() -> String {
    let port = free_port();
    let issuer = format!("http://127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let iss = issuer.clone();
    tokio::spawn(async move {
        loop {
            let (conn, _) = listener.accept().await.unwrap();
            let iss = iss.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let iss = iss.clone();
                        async move {
                            let body = match req.uri().path() {
                                "/.well-known/openid-configuration" => json!({
                                    "issuer": iss, "authorization_endpoint": format!("{iss}/authorize"),
                                    "token_endpoint": format!("{iss}/token"), "jwks_uri": format!("{iss}/jwks"),
                                }),
                                "/jwks" => {
                                    json!({"keys": [{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256",
                                    "n": include_str!("../fixtures/idp_key.n").trim(), "e": "AQAB"}]})
                                }
                                _ => json!({"error": "unsupported"}),
                            };
                            Ok::<_, hyper::Error>(
                                hyper::Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(body.to_string())))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(conn), svc)
                    .await;
            });
        }
    });
    issuer
}

/// A stand-in in front of the REST catalog at `upstream` that answers every request about a rewrite probe table
/// as BigLake answers a spent quota (429 RESOURCE_EXHAUSTED, `Retry-After: 7`) and passes the rest through: a
/// catalog that can rewrite tables but is throttled while a forget or a delete checks that it can.
async fn throttling(upstream: &str) -> String {
    let port = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let upstream = upstream.trim_end_matches('/').to_string();
    tokio::spawn(async move {
        loop {
            let (conn, _) = listener.accept().await.unwrap();
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let upstream = upstream.clone();
                        async move {
                            use http_body_util::BodyExt;
                            let path = req.uri().path_and_query().unwrap().to_string();
                            if path.contains("rewrite_probe_") {
                                let quota = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
                                    "message": "Quota exceeded for quota metric 'Iceberg REST catalog read requests'"}});
                                return Ok::<_, hyper::Error>(
                                    hyper::Response::builder()
                                        .status(429)
                                        .header("content-type", "application/json")
                                        .header("retry-after", "7")
                                        .body(Full::new(Bytes::from(quota.to_string())))
                                        .unwrap(),
                                );
                            }
                            let (parts, body) = req.into_parts();
                            let body = body.collect().await?.to_bytes();
                            let mut out = CLIENT
                                .request(parts.method.clone(), format!("{upstream}{path}"))
                                .body(body);
                            for (k, v) in &parts.headers {
                                if k != hyper::header::HOST && k != hyper::header::CONTENT_LENGTH {
                                    out = out.header(k, v);
                                }
                            }
                            let r = out.send().await.unwrap();
                            let mut back = hyper::Response::builder().status(r.status().as_u16());
                            for (k, v) in r.headers() {
                                if k != hyper::header::TRANSFER_ENCODING
                                    && k != hyper::header::CONTENT_LENGTH
                                {
                                    back = back.header(k, v);
                                }
                            }
                            Ok(back.body(Full::new(r.bytes().await.unwrap())).unwrap())
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(conn), svc)
                    .await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A POST with `bearer`: the HTTP status and the raw answer.
async fn post(url: &str, bearer: &str, body: &Value) -> (u16, String) {
    let r = CLIENT
        .post(url)
        .bearer_auth(bearer)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

/// `/admin` as `bearer`: the status and the answer.
async fn admin_as(s: &str, bearer: &str, req: &Value) -> (u16, Value) {
    let (status, body) = post(&format!("{s}/admin"), bearer, req).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// `/admin` as the operator; must succeed.
async fn admin(s: &str, req: Value) -> Value {
    let (status, body) = admin_as(s, ADMIN_KEY, &req).await;
    assert_eq!(status, 200, "{req}: {body}");
    body
}

fn new_key(tag: &str) -> String {
    format!("msk_{}{tag}", uuid::Uuid::new_v4().simple())
}

fn create_key(org: &str, name: &str, key: &str) -> Value {
    json!({"op": "create_key", "org": org, "name": name, "team": null, "role": "reader", "key": key})
}

/// `remember_memory` over MCP as `bearer` into the selection `sel`: the new episode's id.
async fn remember(s: &str, sel: &str, bearer: &str, text: &str) -> String {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "remember_memory", "arguments": {"source": "notes", "body": text, "for": "team"}}});
    let (_, raw) = post(&format!("{s}/mcp{sel}"), bearer, &call).await;
    let v: Value = serde_json::from_str(&raw).unwrap();
    let t = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(t.contains("remembered in"), "{raw}");
    t.split("episode ")
        .nth(1)
        .and_then(|t| t.split(',').next())
        .unwrap()
        .to_string()
}

/// Everything `bearer` gets back from every read path of selection `sel` on `s`: search, fact and passage
/// queries, the episodes `ids` and MCP search, as one text. None when the selection is refused.
async fn reads(s: &str, sel: &str, bearer: &str, ids: &[&str]) -> Option<String> {
    let mut out = String::new();
    let mut paths = vec![
        ("/search", json!({"data": [[0, "owner note deal", 50, -1]]})),
        (
            "/query",
            json!({"data": [[0, {"find": "facts", "time": "all", "limit": 500}]]}),
        ),
        (
            "/query",
            json!({"data": [[0, {"find": "passages", "text": "note owner", "limit": 100}]]}),
        ),
        // The named metrics' answers, kept between reads (and past new writes) in each process, and the
        // facts they cite: a forget through one process must reach the other's kept answers too.
        (
            "/query",
            json!({"data": [[0, {"find": "metrics", "include": ["evidence"], "limit": 500}]]}),
        ),
    ];
    for id in ids {
        paths.push(("/episode", json!({"data": [[0, id]]})));
    }
    for (path, body) in paths {
        let (status, text) = post(&format!("{s}{path}{sel}"), bearer, &body).await;
        if status == 401 || status == 403 {
            return None;
        }
        // A failed read is part of what was read: `until` keeps trying, and says so if it never passes.
        if status != 200 {
            out.push_str(&format!("[read failed: {path}{sel} HTTP {status} {text}]"));
            continue;
        }
        out.push_str(&text);
    }
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "search_memory", "arguments": {"query": "owner note deal"}}});
    let (status, text) = post(&format!("{s}/mcp{sel}"), bearer, &call).await;
    if status == 401 || status == 403 {
        return None;
    }
    out.push_str(&text);
    Some(out)
}

/// Wait until `check` holds for what `bearer` reads through `s` (within the servers' staleness window).
async fn until(
    s: &str,
    sel: &str,
    bearer: &str,
    ids: &[&str],
    what: &str,
    check: impl Fn(&Option<String>) -> bool,
) {
    let t = Instant::now();
    loop {
        let got = reads(s, sel, bearer, ids).await;
        let failed = got.as_deref().is_some_and(|r| r.contains("[read failed: "));
        if check(&got) && !failed {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "{what} on {s}: {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// A memory written straight to the catalog (as ingest would): an episode holding `tag` and a fact whose
/// entity and value carry it. Returns the episode id.
async fn seed(store: &Store, scope: &Scope, tag: &str) -> String {
    store.ensure_scope(scope).await.unwrap();
    let ep = Episode::new("note:owner", format!("{tag} owner note").as_bytes(), &["*"]);
    let id = ep.episode_id.clone();
    let f = Fact::new(&format!("deal:{tag}"), "owner", tag).from_episodes(&[&ep]);
    store.commit(scope, "test", &[ep], &[f]).await.unwrap();
    id
}

/// A fact extracted from episodes already committed: entity and value carry `tag`.
async fn extract(store: &Store, scope: &Scope, tag: &str, from: &[&str]) {
    let mut f = Fact::new(&format!("deal:{tag}"), "owner", tag);
    f.source_episodes = from.iter().map(|s| s.to_string()).collect();
    store.commit(scope, "extractor", &[], &[f]).await.unwrap();
}

/// Bring the scopes' fact and passage indexes up to date (in the object store, under this run's prefix).
async fn index(store: &Store, scopes: &[Scope]) {
    for s in scopes {
        memstore::search::update(store, s).await.unwrap();
        memstore::search::passages::update(store, s).await.unwrap();
    }
}

/// Where an org's memory is in the bucket: its tables' locations (as the catalog has them) and its search
/// indexes.
async fn org_prefixes(
    rest: &Rest,
    catalog: &Arc<dyn iceberg::Catalog>,
    store: &Store,
    org: &str,
) -> Vec<String> {
    let mut out = vec![];
    for ns in store.org_namespaces(org).await.unwrap() {
        let nsi = iceberg::NamespaceIdent::new(ns.clone());
        for t in catalog.list_tables(&nsi).await.unwrap() {
            let table = catalog.load_table(&t).await.unwrap();
            out.push(rest.key_of(table.metadata().location()));
        }
        out.push(rest.key_of(&format!("{}/{ns}", rest.index)));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn forget_and_org_delete_remove_the_bytes_from_object_storage() {
    let Some(rest) = Rest::from_env("forget_and_org_delete_remove_the_bytes_from_object_storage")
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    let catalog = rest.catalog().await;
    let store = Store::new(catalog.clone());
    let (sa, sb) = (
        serve(&rest, &rest.uri, dir.path(), &issuer).await,
        serve(&rest, &rest.uri, dir.path(), &issuer).await,
    );
    let (a, b) = (sa.url.as_str(), sb.url.as_str());

    // Whether this catalog can rewrite a table in place (one commit replacing its files, the old snapshots expired
    // and their files deleted) and drop one with purge. The fixture can.
    let can_rewrite = store.check_rewrites().await;
    if let Err(e) = &can_rewrite {
        eprintln!("this catalog can't rewrite tables: {e:#}");
    }

    // Two orgs; acme with a team. Alice runs acme, Gary globex; Bob and Carol are acme's members.
    let (acme, globex) = (fresh("acme"), fresh("globex"));
    for change in [
        json!({"op": "create_org", "org": acme, "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": acme, "team": "a1"}),
        json!({"op": "invite", "org": acme, "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["a1"]}]}),
        json!({"op": "create_org", "org": globex, "admin": "gary@globex.com"}),
        json!({"op": "invite", "org": globex, "members": [{"user": "erin@globex.com"}]}),
    ] {
        admin(a, change).await;
    }
    let a_key = new_key("acmereader");
    admin(b, create_key(&acme, "dash", &a_key)).await;
    let a_key_sha = auth::hex(&sha2::Sha256::digest(a_key.as_bytes()));
    let [alice, bob, carol, erin] = [
        "alice@acme.com",
        "bob@acme.com",
        "carol@acme.com",
        "erin@globex.com",
    ]
    .map(|u| id_token(&issuer, u));

    // Memory: ingested notes per scope, notes people wrote through either process, facts drawn from them.
    let run = run_id();
    let tag = |t: &str| format!("xq{run}{t}");
    let acme_scope = Scope::org(&acme).unwrap();
    let a1 = Scope::team(&acme, "a1").unwrap();
    let globex_scope = Scope::org(&globex).unwrap();
    // A SQL metric over the deals' owners on each: its answer is kept by each server process.
    for s in [&acme_scope, &a1, &globex_scope] {
        store.ensure_scope(s).await.unwrap();
        store
            .set_schema(s, "main", "test", &crate::isolation::owners_schema(), false)
            .await
            .unwrap();
    }
    let org_note = seed(&store, &acme_scope, &tag("orgalpha")).await;
    let team_note = seed(&store, &a1, &tag("teamalpha")).await;
    let g_note = seed(&store, &globex_scope, &tag("orgbeta")).await;
    let sel_acme = format!("?org={acme}");
    let sel_a1 = format!("?org={acme}&team=a1");
    let sel_globex = format!("?org={globex}");
    let carol_team = remember(
        a,
        &sel_a1,
        &carol,
        &format!("{} owner note", tag("carolteam")),
    )
    .await;
    let carol_org = remember(
        b,
        &sel_acme,
        &carol,
        &format!("{} owner note", tag("carolorg")),
    )
    .await;
    let bob_org = remember(
        a,
        &sel_acme,
        &bob,
        &format!("{} owner note", tag("bobkeeps")),
    )
    .await;
    let erin_org = remember(
        b,
        &sel_globex,
        &erin,
        &format!("{} owner note", tag("erinbeta")),
    )
    .await;
    extract(&store, &a1, &tag("carolfact"), &[&carol_team]).await;
    extract(&store, &acme_scope, &tag("bobfact"), &[&bob_org]).await;
    extract(
        &store,
        &acme_scope,
        &tag("mixedfact"),
        &[&carol_org, &bob_org],
    )
    .await;
    index(
        &store,
        &[acme_scope.clone(), a1.clone(), globex_scope.clone()],
    )
    .await;
    let carol_tags = ["carolteam", "carolorg", "carolfact", "mixedfact"].map(tag);
    let acme_tags = ["orgalpha", "teamalpha", "bobkeeps", "bobfact"].map(tag);
    let globex_tags = ["orgbeta", "erinbeta"].map(tag);
    let acme_at = org_prefixes(&rest, &catalog, &store, &acme).await;
    let globex_at = org_prefixes(&rest, &catalog, &store, &globex).await;
    // The listing sees what is stored (else every "in no object" below would hold trivially).
    for t in carol_tags.iter().chain(&acme_tags) {
        assert!(
            !rest.objects_with(&acme_at, t).await.is_empty(),
            "{t} not in the bucket"
        );
    }
    for t in &globex_tags {
        assert!(
            !rest.objects_with(&globex_at, t).await.is_empty(),
            "{t} not in the bucket"
        );
    }
    let acme_ids = [org_note.as_str(), bob_org.as_str(), carol_org.as_str()];
    let a1_ids = [team_note.as_str(), carol_team.as_str()];
    let g_ids = [g_note.as_str(), erin_org.as_str()];
    for s in [a, b] {
        until(s, &sel_acme, &bob, &acme_ids, "bob reads acme", |r| {
            r.as_ref()
                .is_some_and(|r| r.contains(&tag("carolorg")) && r.contains(&tag("bobkeeps")))
        })
        .await;
        until(s, &sel_a1, &carol, &a1_ids, "carol reads a1", |r| {
            r.as_ref().is_some_and(|r| r.contains(&tag("carolteam")))
        })
        .await;
        // Acme's key reads acme through both, so its use is in the audit and key usage tables.
        until(s, "", &a_key, &acme_ids, "acme's key reads acme", |r| {
            r.as_ref().is_some_and(|r| r.contains(&tag("orgalpha")))
        })
        .await;
    }

    // A record's newer version replaces the old (`replaces_source`): the answers kept may still hold the old
    // version's text, so deleting that version later reaches them in both processes.
    let old = seed(&store, &acme_scope, &tag("replacedold")).await;
    for s in [a, b] {
        until(s, &sel_acme, &bob, &[], "bob reads the old version", |r| {
            r.as_ref().is_some_and(|r| r.contains(&tag("replacedold")))
        })
        .await;
    }
    let new = seed(&store, &acme_scope, &tag("replacednew")).await;
    store
        .supersede(
            &acme_scope,
            "test",
            &[AclTarget::SourceExcept {
                source: "note:owner".into(),
                keep: vec![new, org_note.clone()],
            }],
        )
        .await
        .unwrap();
    for s in [a, b] {
        until(s, &sel_acme, &bob, &[], "bob reads the new version", |r| {
            r.as_ref().is_some_and(|r| r.contains(&tag("replacednew")))
        })
        .await;
    }
    let (status, body) = post(
        &format!("{a}/retract{sel_acme}"),
        ADMIN_KEY,
        &json!({"episode_id": old}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for s in [a, b] {
        until(
            s,
            &sel_acme,
            &bob,
            &[],
            "the replaced version deleted",
            |r| r.as_ref().is_some_and(|r| !r.contains(&tag("replacedold"))),
        )
        .await;
    }

    // Refused: a plain member forgetting anyone, globex's admin forgetting an acme member. Nothing goes.
    let gary = id_token(&issuer, "gary@globex.com");
    for (who, req) in [
        (
            &bob,
            json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
        ),
        (
            &gary,
            json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
        ),
        (
            &alice,
            json!({"op": "delete_org", "org": acme, "confirm": acme}),
        ),
    ] {
        let (status, body) = admin_as(b, who, &req).await;
        assert_eq!(status, 403, "{req} should be refused: {body}");
    }
    for t in &carol_tags {
        assert!(
            !rest.objects_with(&acme_at, t).await.is_empty(),
            "{t} gone after refusals"
        );
    }

    // On a catalog that can't rewrite tables, the forget and the delete are refused before anything
    // changes, saying what the catalog can't do: Carol is still in acme, her memory still stored.
    if can_rewrite.is_err() {
        for (who, req) in [
            (
                &alice,
                json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
            ),
            (
                &ADMIN_KEY.to_string(),
                json!({"op": "delete_org", "org": acme, "confirm": acme}),
            ),
        ] {
            let (status, body) = admin_as(a, who, &req).await;
            let error = body["error"].as_str().unwrap_or_default();
            assert!(
                status == 403
                    && error.contains("this catalog can't")
                    && error.contains("nothing was changed"),
                "{req}: {status} {body}"
            );
        }
        for s in [a, b] {
            until(s, &sel_a1, &carol, &a1_ids, "carol still reads a1", |r| {
                r.as_ref().is_some_and(|r| r.contains(&tag("carolteam")))
            })
            .await;
        }
        for t in &carol_tags {
            assert!(!rest.objects_with(&acme_at, t).await.is_empty(), "{t} gone");
        }
        let system = catalog
            .list_tables(&iceberg::NamespaceIdent::new("memory_system".into()))
            .await
            .unwrap();
        assert!(
            system
                .iter()
                .all(|t| !t.name().starts_with("rewrite_probe_")),
            "the probe left {system:?}"
        );
        return;
    }

    // A catalog that can rewrite tables but is throttled while the forget or the delete checks it can: not
    // refused as a catalog that can't, but answered busy (503) with the catalog's own wait, nothing changed.
    {
        let throttled = serve(&rest, &throttling(&rest.uri).await, dir.path(), &issuer).await;
        for (who, req) in [
            (
                &alice,
                json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
            ),
            (
                &ADMIN_KEY.to_string(),
                json!({"op": "delete_org", "org": acme, "confirm": acme}),
            ),
        ] {
            let r = CLIENT
                .post(format!("{}/admin", throttled.url))
                .bearer_auth(who)
                .body(req.to_string())
                .send()
                .await
                .unwrap();
            let (status, wait) = (r.status().as_u16(), r.headers().get("retry-after").cloned());
            let body: Value = r.json().await.unwrap_or_default();
            let error = body["error"].as_str().unwrap_or_default();
            assert!(
                status == 503
                    && wait.as_ref().is_some_and(|w| w == "7")
                    && error.contains("busy")
                    && error.contains("nothing was changed")
                    && !error.contains("can't"),
                "{req}: {status} {wait:?} {body}"
            );
        }
        for s in [a, b] {
            until(s, &sel_a1, &carol, &a1_ids, "carol still reads a1", |r| {
                r.as_ref().is_some_and(|r| r.contains(&tag("carolteam")))
            })
            .await;
        }
        for t in &carol_tags {
            assert!(
                !rest.objects_with(&acme_at, t).await.is_empty(),
                "{t} gone though the catalog was busy"
            );
        }
    }

    // Acme's admin forgets Carol through one process.
    let (status, body) = admin_as(
        a,
        &alice,
        &json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["forgotten"][&acme]["episodes"], 2, "{body}");
    // Her two notes' own facts (each remembered note is a fact citing it), carolfact and mixedfact.
    assert_eq!(body["forgotten"][&acme]["facts"], 4, "{body}");
    // Gone from every read path of both processes; the rest of acme's memory stays.
    for s in [a, b] {
        until(s, &sel_acme, &bob, &acme_ids, "acme after forget", |r| {
            r.as_ref().is_some_and(|r| {
                carol_tags.iter().all(|t| !r.contains(t))
                    && r.contains(&tag("orgalpha"))
                    && r.contains(&tag("bobkeeps"))
            })
        })
        .await;
        until(s, &sel_a1, &carol, &a1_ids, "carol refused", |r| {
            r.is_none()
        })
        .await;
    }
    // And from every object under acme's tables and indexes: data files, old snapshots, bodies, indexes.
    for t in &carol_tags {
        let left = rest.objects_with(&acme_at, t).await;
        assert!(left.is_empty(), "{t} still stored: {left:?}");
    }
    for t in &acme_tags {
        assert!(
            !rest.objects_with(&acme_at, t).await.is_empty(),
            "{t} lost forgetting carol"
        );
    }
    for t in &globex_tags {
        assert!(
            !rest.objects_with(&globex_at, t).await.is_empty(),
            "{t} lost forgetting carol"
        );
    }
    // No table was left renamed aside.
    for ns in store.org_namespaces(&acme).await.unwrap() {
        let tables = catalog
            .list_tables(&iceberg::NamespaceIdent::new(ns.clone()))
            .await
            .unwrap();
        assert!(
            tables.iter().all(|t| !t.name().contains("__old_")),
            "{ns}: {tables:?}"
        );
    }

    // Before the delete, acme's rows are in the audit and key usage tables (else "gone" proves nothing).
    let t = Instant::now();
    loop {
        let fresh_store = Store::new(catalog.clone());
        let used = fresh_store
            .key_usage()
            .await
            .unwrap()
            .contains_key(&a_key_sha);
        let audited = fresh_store
            .audit_rows()
            .await
            .unwrap()
            .iter()
            .any(|e| e.org.as_deref() == Some(acme.as_str()));
        if used && audited {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "acme's key use never written"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // The operator deletes acme through the other process.
    admin(b, json!({"op": "delete_org", "org": acme, "confirm": acme})).await;
    // Its key and people are refused by both processes.
    for s in [a, b] {
        for (who, sel) in [
            (&a_key, ""),
            (&a_key, sel_acme.as_str()),
            (&bob, sel_acme.as_str()),
        ] {
            until(s, sel, who, &acme_ids, "refused after delete", |r| {
                r.is_none()
            })
            .await;
        }
    }
    // Nothing of it is left in the bucket, its namespaces are gone from the catalog.
    let left = rest.objects(&acme_at).await;
    assert_eq!(
        left, 0,
        "{left} objects left under acme's tables and indexes"
    );
    assert!(store.org_namespaces(&acme).await.unwrap().is_empty());
    // Its rows are gone from the audit and key usage tables, and stay gone once both processes have
    // written what they hold (a process's buffered rows of a deleted org must not bring them back).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let fresh_store = Store::new(catalog.clone());
    assert!(
        !fresh_store
            .key_usage()
            .await
            .unwrap()
            .contains_key(&a_key_sha),
        "acme's key usage kept"
    );
    let rows = fresh_store.audit_rows().await.unwrap();
    let kept: Vec<_> = rows
        .iter()
        .filter(|e| e.org.as_deref() == Some(acme.as_str()))
        .collect();
    assert!(kept.is_empty(), "acme's audit rows kept: {kept:?}");
    assert!(
        rows.iter().any(|e| e.org.is_none()
            && e.detail
                .as_deref()
                .is_some_and(|d| d.starts_with(&format!("delete org {acme}")))),
        "no audit row for the delete"
    );
    // Globex keeps everything, in the bucket and through both processes.
    for t in &globex_tags {
        assert!(
            !rest.objects_with(&globex_at, t).await.is_empty(),
            "{t} lost deleting acme"
        );
    }
    for s in [a, b] {
        until(s, &sel_globex, &erin, &g_ids, "globex after delete", |r| {
            r.as_ref()
                .is_some_and(|r| globex_tags.iter().all(|t| r.contains(t)))
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn isolation_holds_across_two_server_processes_on_a_rest_catalog() {
    let Some(rest) =
        Rest::from_env("isolation_holds_across_two_server_processes_on_a_rest_catalog")
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    let store = Store::new(rest.catalog().await);
    let (sa, sb) = (
        serve(&rest, &rest.uri, dir.path(), &issuer).await,
        serve(&rest, &rest.uri, dir.path(), &issuer).await,
    );
    let (a, b) = (sa.url.as_str(), sb.url.as_str());
    let (o1, o2) = (fresh("iso"), fresh("iso"));
    for change in [
        json!({"op": "create_org", "org": o1, "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": o1, "team": "a1"}),
        json!({"op": "invite", "org": o1, "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["a1"]}]}),
        json!({"op": "create_org", "org": o2, "admin": "gary@globex.com"}),
        json!({"op": "invite", "org": o2, "members": [{"user": "erin@globex.com"}]}),
    ] {
        admin(a, change).await;
    }
    let (o1_key, a1_key) = (new_key("orgreader"), new_key("teamreader"));
    admin(b, create_key(&o1, "dash", &o1_key)).await;
    admin(
        b,
        json!({"op": "create_key", "org": o1, "name": "ci", "team": "a1", "role": "reader", "key": a1_key}),
    )
    .await;

    // Each scope's memory carries a tag of its own: the served scope, o1, its team a1, and o2.
    let run = run_id();
    let served = format!("xi{run}served{}", &o1[3..]);
    let (t1, ta1, t2) = (
        format!("xi{o1}org"),
        format!("xi{o1}team"),
        format!("xi{o2}org"),
    );
    let mut ids = vec![];
    for (scope, tag) in [
        (Scope::Org, &served),
        (Scope::org(&o1).unwrap(), &t1),
        (Scope::team(&o1, "a1").unwrap(), &ta1),
        (Scope::org(&o2).unwrap(), &t2),
    ] {
        ids.push(seed(&store, &scope, tag).await);
    }
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let all = [&served, &t1, &ta1, &t2];
    let tok = |u: &str| id_token(&issuer, u);
    let (alice, bob, carol, erin) = (
        tok("alice@acme.com"),
        tok("bob@acme.com"),
        tok("carol@acme.com"),
        tok("erin@globex.com"),
    );
    let (sel1, sel_a1, sel2) = (
        format!("?org={o1}"),
        format!("?org={o1}&team=a1"),
        format!("?org={o2}"),
    );
    // Who reads what, through which selection: the one tag they are entitled to, or refused (None).
    let cases: Vec<(&str, &str, &str, Option<&String>)> = vec![
        ("bob in o1", &bob, &sel1, Some(&t1)),
        ("alice in o1", &alice, &sel1, Some(&t1)),
        ("carol in a1", &carol, &sel_a1, Some(&ta1)),
        ("carol in o1", &carol, &sel1, Some(&t1)),
        ("alice in a1 (not a member)", &alice, &sel_a1, None),
        ("bob in a1", &bob, &sel_a1, None),
        ("erin in o2", &erin, &sel2, Some(&t2)),
        ("erin in o1", &erin, &sel1, None),
        ("bob in o2", &bob, &sel2, None),
        ("o1's key", &o1_key, "", Some(&t1)),
        ("o1's key selecting o2", &o1_key, &sel2, None),
        ("a1's key", &a1_key, "", Some(&ta1)),
        ("the operator, no selection", ADMIN_KEY, "", Some(&served)),
    ];
    for s in [a, b] {
        for (who, bearer, sel, own) in &cases {
            let t = Instant::now();
            loop {
                let got = reads(s, sel, bearer, &ids).await;
                let failed = got.as_deref().is_some_and(|r| r.contains("[read failed: "));
                match (own, &got) {
                    (None, None) => break,
                    (None, Some(r)) => {
                        let seen: Vec<_> = all.iter().filter(|t| r.contains(t.as_str())).collect();
                        assert!(seen.is_empty(), "{who} on {s} read {seen:?}");
                        // Not refused, but nothing of anyone's: also fine.
                        if !failed {
                            break;
                        }
                    }
                    (Some(own), Some(r)) => {
                        let foreign: Vec<_> = all
                            .iter()
                            .filter(|t| **t != *own && r.contains(t.as_str()))
                            .collect();
                        assert!(foreign.is_empty(), "{who} on {s} read {foreign:?}");
                        if r.contains(own.as_str()) && !failed {
                            break;
                        }
                    }
                    (Some(_), None) => {}
                }
                assert!(
                    t.elapsed() < Duration::from_secs(30),
                    "{who} on {s} never read their own memory: {got:?}"
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }
}

/// What `bearer`'s Home through `s` (selection `sel`) answers, every part of it, as text.
async fn home(s: &str, sel: &str, bearer: &str) -> String {
    let parts = [
        "counts",
        "week",
        "lessons",
        "branches",
        "activity",
        "proposals",
        "conflicts",
        "dream",
        "schema",
    ];
    let (status, text) = post(
        &format!("{s}/browse{sel}"),
        bearer,
        &json!({"op": "home", "parts": parts}),
    )
    .await;
    assert_eq!(status, 200, "home through {s}: {text}");
    text
}

/// Home's count of the sources its reader reads (`counts.sources`), None while it is being counted.
fn sources_count(home: &str) -> Option<u64> {
    serde_json::from_str::<Value>(home).ok()?["counts"]["sources"].as_u64()
}

/// Waits, at most `within`, until `check` holds of what `get` answers: what it last answered, and whether it did.
async fn wait_until<T, F: std::future::Future<Output = T>>(
    within: Duration,
    mut get: impl FnMut() -> F,
    check: impl Fn(&T) -> bool,
) -> (T, bool) {
    let t = Instant::now();
    loop {
        let got = get().await;
        if check(&got) || t.elapsed() >= within {
            let ok = check(&got);
            return (got, ok);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_forgotten_or_aged_out_episode_leaves_no_shared_home_answer() {
    let Some(rest) = Rest::from_env("a_forgotten_or_aged_out_episode_leaves_no_shared_home_answer")
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    // This process's store marks its commits as the servers do (`memstore_core`'s `adds_only_tag` runs only where
    // answers are shared): a commit it makes below is unmarked because of what it writes, not because it never
    // checks.
    unsafe { std::env::set_var("AMPUP_MEMORY_SHARED_ANSWERS", "object") };
    let store = Store::new(rest.catalog().await);
    // Every server shares Home's answers in the bucket, beside the catalog's system tables, and reads what another
    // server wrote at most 2 s late, and commits its sessions' rows every second.
    let sharing = [
        ("AMPUP_MEMORY_SHARED_ANSWERS", "object"),
        ("AMPUP_MEMORY_HARD_STALENESS_MS", "2000"),
        ("AMPUP_MEMORY_SESSION_FLUSH_SECS", "1"),
    ];
    let sa = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
    let a = sa.url.as_str();
    let answers = [rest.key_of(&format!("{}/memory_system/_answers", rest.warehouse))];
    let org = fresh("shared");
    admin(
        a,
        json!({"op": "create_org", "org": org, "admin": "alice@acme.com"}),
    )
    .await;
    // A reader of their own for each case: a Home kept for a reader is answered past later additions until it is
    // worked out again, so a reader who opened Home before an episode was written may not see it for a while. Each
    // case's episode is the only one its reader reads (those before it are gone by then): Home counts 1 source
    // while it is there, and never 1 once it is gone.
    let reader = |n: &str| {
        let (org, issuer) = (org.clone(), issuer.clone());
        let user = format!("{n}@acme.com");
        async move {
            admin(
                a,
                json!({"op": "invite", "org": org, "members": [{"user": user}]}),
            )
            .await;
            id_token(&issuer, &user)
        }
    };
    let scope = Scope::org(&org).unwrap();
    store.ensure_scope(&scope).await.unwrap();
    let sel = format!("?org={org}");
    let run = run_id();
    // An episode shared with the whole org from a source of its own: Home's sources name it by the tag.
    let write = |tag: String, occurred_at: Option<i64>| {
        let (store, scope, org) = (store.clone(), scope.clone(), org.clone());
        async move {
            let all = format!("group:{org}.all");
            let mut ep = Episode::new(
                &format!("{tag}:note"),
                format!("{tag} note").as_bytes(),
                &[&all],
            );
            if let Some(at) = occurred_at {
                ep.occurred_at = at;
            }
            store.commit(&scope, "test", &[ep], &[]).await.unwrap();
        }
    };
    // The reader's first Home through `s`, once `s` reads the episode, shows `tag`, and an answer in the bucket
    // holds it.
    let shown_and_shared = |s: String, tag: String, bob: String| {
        let (sel, answers) = (sel.clone(), answers.clone());
        let rest = &rest;
        async move {
            let ls = json!({"op": "ls", "path": ["sources"]});
            let url = format!("{s}/browse{sel}");
            let (_, ok) = wait_until(
                Duration::from_secs(30),
                || post(&url, &bob, &ls),
                |(_, t)| t.contains(&tag),
            )
            .await;
            assert!(ok, "{s} never read {tag}");
            let (_, ok) = wait_until(
                Duration::from_secs(60),
                || home(&s, &sel, &bob),
                |h| h.contains(&tag),
            )
            .await;
            assert!(ok, "{tag} never shown on {s}'s Home");
            let (h, ok) = wait_until(
                Duration::from_secs(60),
                || home(&s, &sel, &bob),
                |h| sources_count(h) == Some(1),
            )
            .await;
            assert!(ok, "{s}'s Home never counted {tag}'s source: {h}");
            let (_, ok) = wait_until(
                Duration::from_secs(30),
                || rest.objects_with(&answers, &tag),
                |o| !o.is_empty(),
            )
            .await;
            assert!(ok, "no shared answer of {s}'s holds {tag}");
        }
    };

    // An episode only `acl` reads, ingested through server `s`: a commit marked as only adding.
    let ingest = |s: String, source: String, acl: String| {
        let sel = sel.clone();
        async move {
            let (status, body) = post(
                &format!("{s}/ingest{sel}"),
                ADMIN_KEY,
                &json!({"episodes": [{
                    "body": format!("{source} note"),
                    "source": source,
                    "occurred_at": chrono::Utc::now().timestamp_micros(),
                    "acl": [acl],
                }]}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
    };
    // A positive control: with only an answer logged and an episode its reader can't read ingested since its
    // answers were put, a new server's first Home is answered from them, whole. Without it, every check below could
    // pass with nothing ever read from the store.
    let tag = format!("xq{run}control");
    let bob = reader("control").await;
    write(tag.clone(), None).await;
    shown_and_shared(a.to_string(), tag.clone(), bob.clone()).await;
    // Every part's answer put: A's first Home worked each out, and puts land in the background.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (status, body) = post(
        &format!("{a}/log_answer{sel}"),
        &bob,
        &json!({"question": format!("what is {tag}?"), "cited": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    ingest(
        a.to_string(),
        format!("note:{tag}after"),
        "user:alice@acme.com".into(),
    )
    .await;
    let sc = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
    let h = home(&sc.url, &sel, &bob).await;
    let shared = serde_json::from_str::<Value>(&h)
        .ok()
        .map(|v| v["shared"] == true);
    assert!(
        shared == Some(true) && h.contains(&tag),
        "a new server's first Home wasn't the stored answers, with only an answer logged and an episode ingested since: {h}"
    );
    drop(sc);
    // Gone before the cases below, each of whose readers must read its own episode alone.
    let (status, body) = post(
        &format!("{a}/retract{sel}"),
        ADMIN_KEY,
        &json!({"source": format!("{tag}:note")}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Forgotten (retracted) through server B, once with B sharing nothing (A finds the loss on its next build and
    // purges) and once with B sharing too (B purges as it forgets); then hidden from its reader by a narrower ACL,
    // through B sharing; by a merge alone (a branch wrote the episode again under a narrower ACL before the answers
    // were put, so the merge moves only the ledger); and by the same episode written again under a narrower ACL on
    // main (an append, not marked as only adding).
    for (variant, b_shares) in [
        ("off", false),
        ("on", true),
        ("acl", true),
        ("merge", true),
        ("reingest", true),
    ] {
        let b_env = [
            (
                "AMPUP_MEMORY_SHARED_ANSWERS",
                if b_shares { "object" } else { "off" },
            ),
            ("AMPUP_MEMORY_HARD_STALENESS_MS", "2000"),
        ];
        let sb = serve_env(&rest, &rest.uri, dir.path(), &issuer, &b_env).await;
        let tag = format!("xq{run}forget{variant}");
        let bob = reader(&format!("forget{variant}")).await;
        write(tag.clone(), None).await;
        let branch = format!("ws{run}");
        if variant == "merge" {
            store.create_branch(&scope, &branch, "test").await.unwrap();
            let narrower = Episode::new(
                &format!("{tag}:note"),
                format!("{tag} note").as_bytes(),
                &["user:alice@acme.com"],
            );
            store
                .commit_on(&scope, &branch, "test", &[narrower], &[])
                .await
                .unwrap();
        }
        shown_and_shared(a.to_string(), tag.clone(), bob.clone()).await;
        // The answers that hold it, as they were before it was lost: put back once purged, they must not be
        // answered.
        let op = rest.s3();
        let mut held = vec![];
        for key in rest.objects_with(&answers, &tag).await {
            let bytes = op.read(&key).await.unwrap().to_vec();
            held.push((key, bytes));
        }
        assert!(!held.is_empty(), "B {variant}: no shared answer held {tag}");
        let (status, body) = match variant {
            "merge" => {
                store.merge(&scope, &branch, "test").await.unwrap();
                (200, String::new())
            }
            "reingest" => {
                let narrower = Episode::new(
                    &format!("{tag}:note"),
                    format!("{tag} note").as_bytes(),
                    &["user:alice@acme.com"],
                );
                store
                    .commit(&scope, "test", &[narrower], &[])
                    .await
                    .unwrap();
                (200, String::new())
            }
            "acl" => {
                post(
                    &format!("{}/acl{sel}", sb.url),
                    ADMIN_KEY,
                    &json!({"source": format!("{tag}:note"), "acl": ["user:alice@acme.com"]}),
                )
                .await
            }
            _ => {
                post(
                    &format!("{}/retract{sel}", sb.url),
                    ADMIN_KEY,
                    &json!({"source": format!("{tag}:note")}),
                )
                .await
            }
        };
        assert_eq!(status, 200, "{body}");
        // A stops showing it within its staleness window, and then never again.
        let (h, ok) = wait_until(
            Duration::from_secs(30),
            || home(a, &sel, &bob),
            |h| !h.contains(&tag),
        )
        .await;
        assert!(
            ok,
            "B {variant}: A still shows {tag} after B forgot it: {h}"
        );
        // Nor counts it: not on the Home that stopped showing it, nor on any after.
        assert_ne!(
            sources_count(&h),
            Some(1),
            "B {variant}: A counts {tag}'s source after B forgot it: {h}"
        );
        // No shared answer holds it once A's purge (B's too, when it shares) is done.
        let (left, ok) = wait_until(
            Duration::from_secs(30),
            || rest.objects_with(&answers, &tag),
            |o| o.is_empty(),
        )
        .await;
        assert!(
            ok,
            "B {variant}: shared answers outlived the forget of {tag}: {left:?}"
        );
        // The answers from before the loss put back, as a put that lost its race with the purge would leave them,
        // and since, an answer logged and an episode its reader can't read ingested: commits marked as only adding,
        // so only the loss's own unmarked write refuses them.
        for (key, bytes) in &held {
            op.write(key, bytes.clone()).await.unwrap();
        }
        let (status, body) = post(
            &format!("{a}/log_answer{sel}"),
            &bob,
            &json!({"question": format!("what is {tag}?"), "cited": []}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        ingest(
            a.to_string(),
            format!("note:xq{run}after{variant}"),
            "user:alice@acme.com".into(),
        )
        .await;
        // A new server's first Home (from the shared answers, or worked out) doesn't show it, nor A's next ones.
        let sc = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
        for s in [sc.url.as_str(), a, sc.url.as_str(), a] {
            let h = home(s, &sel, &bob).await;
            assert!(
                !h.contains(&tag) && sources_count(&h) != Some(1),
                "B {variant}: {s} showed or counted {tag} after it was forgotten: {h}"
            );
        }
        for (key, _) in &held {
            op.delete(key).await.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let left = rest.objects_with(&answers, &tag).await;
        assert!(
            left.is_empty(),
            "B {variant}: a shared answer holding {tag} was put after its forget: {left:?}"
        );
    }

    // A session ended discarded: its person's activity no longer counts it, though nothing is forgotten and nothing
    // purged. Admin's Home stored while it counted must not be answered after (a session's rows are never marked as
    // only adding). In an org of its own, where the session is the only activity.
    let org2 = fresh("discard");
    admin(
        a,
        json!({"op": "create_org", "org": org2, "admin": "alice@acme.com"}),
    )
    .await;
    admin(
        a,
        json!({"op": "invite", "org": org2, "members": [{"user": "dan@acme.com"}]}),
    )
    .await;
    let scope2 = Scope::org(&org2).unwrap();
    store.ensure_scope(&scope2).await.unwrap();
    let sel2 = format!("?org={org2}");
    let (alice, dan) = (
        id_token(&issuer, "alice@acme.com"),
        id_token(&issuer, "dan@acme.com"),
    );
    let sessions_of = |h: &str| {
        serde_json::from_str::<Value>(h)
            .ok()
            .and_then(|v| v["activity"]["sessions"].as_u64())
    };
    let (status, body) = post(
        &format!("{a}/sessions{sel2}"),
        &dan,
        &json!({"task": format!("xq{run}discard what does the renewal need?")}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let session = serde_json::from_str::<Value>(&body).unwrap()["session_id"].clone();
    let (status, body) = post(
        &format!("{a}/advice{sel2}"),
        &dan,
        &json!({"session_id": session, "event": {"kind": "stop"},
                "conversation_so_far": format!("user: what does the renewal need?\nassistant: xq{run}discard")}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (h, ok) = wait_until(
        Duration::from_secs(90),
        || home(a, &sel2, &alice),
        |h| sessions_of(h) == Some(1),
    )
    .await;
    assert!(ok, "A's Home never counted dan's session: {h}");
    // The answers as stored once the session's rows settle, and their control: a new server answers admin's Home
    // from them, counting the session. Tried again while the session's rows still move (each refuses the answers
    // stored before it), and A puts the same answer again at most every 10 s.
    let op = rest.s3();
    let mut held = vec![];
    let mut h = String::new();
    let until = Instant::now() + Duration::from_secs(90);
    while Instant::now() < until {
        tokio::time::sleep(Duration::from_secs(3)).await;
        home(a, &sel2, &alice).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        held.clear();
        for key in rest.objects_with(&answers, &scope2.namespace()).await {
            held.push((key.clone(), op.read(&key).await.unwrap().to_vec()));
        }
        let sc = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
        h = home(&sc.url, &sel2, &alice).await;
        if serde_json::from_str::<Value>(&h)
            .ok()
            .map(|v| v["shared"] == true)
            == Some(true)
            && sessions_of(&h) == Some(1)
        {
            break;
        }
        h.clear();
    }
    assert!(
        !h.is_empty() && !held.is_empty(),
        "a new server's first Home was never the stored answers counting dan's session"
    );
    let (status, body) = post(
        &format!("{a}/sessions/end{sel2}"),
        &dan,
        &json!({"session_id": session, "discard": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (h, ok) = wait_until(
        Duration::from_secs(90),
        || home(a, &sel2, &alice),
        |h| sessions_of(h).unwrap_or(0) == 0,
    )
    .await;
    assert!(ok, "A's Home still counts dan's discarded session: {h}");
    // The answers stored while it counted, as they were: a new server must not answer them, nor A after.
    for (key, bytes) in &held {
        op.write(key, bytes.clone()).await.unwrap();
    }
    let sc = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
    for s in [sc.url.as_str(), a] {
        let h = home(s, &sel2, &alice).await;
        assert!(
            sessions_of(&h).unwrap_or(0) == 0,
            "{s} counted dan's session after it was discarded: {h}"
        );
    }
    drop(sc);

    // Aged out: a policy keeps the tag's source a day, and its episode turns a day old shortly. No table is written
    // when it does, so the answers' key doesn't move: only their expiry keeps it from being answered.
    let tag = format!("xq{run}ages");
    let bob = reader("ages").await;
    admin(
        a,
        json!({"op": "retention", "org": org, "rules": [format!("{tag}:=1d")]}),
    )
    .await;
    let day = 86_400_000_000_i64;
    let ages_out = chrono::Utc::now().timestamp_micros() + 40_000_000;
    write(tag.clone(), Some(ages_out - day)).await;
    shown_and_shared(a.to_string(), tag.clone(), bob.clone()).await;
    let left = ages_out - chrono::Utc::now().timestamp_micros();
    assert!(
        left > 0,
        "seeding took past the episode's ageing out: make it age out later"
    );
    tokio::time::sleep(Duration::from_micros(left as u64 + 200_000)).await;
    // Answered at once (A's own kept answer, or the shared answers) and by a new server alike: not shown.
    let sc = serve_env(&rest, &rest.uri, dir.path(), &issuer, &sharing).await;
    for s in [a, sc.url.as_str(), a, sc.url.as_str()] {
        let h = home(s, &sel, &bob).await;
        assert!(
            !h.contains(&tag) && sources_count(&h) != Some(1),
            "{s} showed or counted {tag} after it aged out: {h}"
        );
    }
}

/// A Home worked out at start for who read it lately (`memstore::recent_readers`) is answered only to its own reader.
/// Bob (a member, who alone reads one episode), Alice (the org's admin), Dan, Fay and Gina (in a group the closures
/// give her) open Home through server A, which writes them down. Before server B starts, Dan leaves the org, Fay is
/// disabled and Gina is taken out of her group. B works out Bob's, Alice's and Gina's Home before it is ready, as each
/// reads now, and neither Dan's nor Fay's, who leave the file: Bob's first Home is answered at once and holds his
/// episode; Gina's is answered at once without her old group's episode; Dan and Fay are refused. Alice, demoted after
/// B worked hers out as an admin, and Bob, made an admin after it worked his out as a member, are each answered their
/// Home as they stand now on their first ask. Erin, who never opened Home, gets hers worked out for her. Bob's and
/// Alice's other Studio reads (their sources listing, Bob's Skills list, Alice's Traces, an admin's read through
/// `/admin`) are made again at start as each of them, and answered from what was kept (the server's count of kept
/// answers served moves for them, and not for Erin's): each first listing on B holds only what its reader reads; the
/// skills' summaries read at start list Bob's skill to him alone; Alice, once demoted, reads Traces no more. Hank, an
/// admin who opens Traces on A and is demoted before B starts, has his Home worked out as the member he is, and his
/// Traces not made at start: his first Traces on B are refused, and answer nothing kept. Sam and Sue (a member) open Home through A as super
/// admins, reading all of the org: B, which lists Sam alone as one, works Sam's out as one reading all, and Sue's as
/// the member she signs in as there, without Bob's episode.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_home_worked_out_at_start_is_answered_only_to_its_own_reader() {
    let Some(rest) =
        Rest::from_env("a_home_worked_out_at_start_is_answered_only_to_its_own_reader")
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    // The platform's tenant, which signs super admins in.
    let tenant = start_idp().await;
    let catalog = rest.catalog().await;
    let store = Store::new(catalog.clone());
    // Readers are written down every second, and a server reads what this test commits at most 2 s late. A lists Sam
    // and Sue as super admins, B Sam alone.
    let env_with = |super_admins: &'static str| {
        [
            ("AMPUP_MEMORY_HOME_READERS_SECS", "1"),
            ("AMPUP_MEMORY_HARD_STALENESS_MS", "2000"),
            ("AMPUP_MEMORY_AUTH0_ISSUER", tenant.as_str()),
            ("AMPUP_MEMORY_AUTH0_CLIENT_IDS", CLIENT_ID),
            ("AMPUP_MEMORY_SUPER_ADMINS", super_admins),
        ]
    };
    let env = env_with("sam@acme.com,sue@acme.com");
    let sa = serve_env(&rest, &rest.uri, dir.path(), &issuer, &env).await;
    let a = sa.url.as_str();
    let org = fresh("primed");
    // Disabled below, for good in a catalog later runs share: a user of this run's alone.
    let fay_id = format!("fay.{org}@acme.com");
    admin(
        a,
        json!({"op": "create_org", "org": org, "admin": "alice@acme.com"}),
    )
    .await;
    admin(
        a,
        json!({"op": "invite", "org": org, "members": [{"user": "bob@acme.com"}, {"user": "dan@acme.com"}, {"user": "erin@acme.com"}, {"user": fay_id}, {"user": "gina@acme.com"}, {"user": "sue@acme.com"}]}),
    )
    .await;
    // Hank, a second admin, demoted before B starts.
    admin(
        a,
        json!({"op": "add_member", "org": org, "user": "hank@acme.com", "role": "admin"}),
    )
    .await;
    let [alice, bob, dan, erin, fay, gina, hank] = [
        "alice@acme.com",
        "bob@acme.com",
        "dan@acme.com",
        "erin@acme.com",
        fay_id.as_str(),
        "gina@acme.com",
        "hank@acme.com",
    ]
    .map(|u| id_token(&issuer, u));
    let [sam, sue] = ["sam", "sue"].map(|u| {
        crate::isolation::tenant_token(
            &tenant,
            &format!("{u}@acme.com"),
            &format!("google-oauth2|{u}"),
        )
    });
    let scope = Scope::org(&org).unwrap();
    store.ensure_scope(&scope).await.unwrap();
    let sel = format!("?org={org}");
    let run = run_id();
    let (shared, secret, clubbed) = (
        format!("xq{run}primedall"),
        format!("xq{run}primedbob"),
        format!("xq{run}primedclub"),
    );
    let all = format!("group:{org}.all");
    // A group the closures give Gina, as a channel's members are given its history.
    let club = format!("{org}.club");
    let closure = |s: String, body: Value| async move {
        let (status, text) = post(&format!("{s}/closure"), ADMIN_KEY, &body).await;
        assert_eq!(status, 200, "{body}: {text}");
    };
    closure(
        a.to_string(),
        json!({"user": "gina@acme.com", "add": [club.clone()]}),
    )
    .await;
    // And a skill for everyone and one for Bob alone, whose summaries B reads at start for the Skills list.
    let skill = |name: &str, acl: &str| {
        let mut e = Episode::new(
            &format!("github:acme/app#skill/.claude/skills/{name}/SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} steps.\n---\n\n# {name}\n")
                .as_bytes(),
            &[acl],
        );
        e.document = Some(format!(
            "repo-skills/acme/app/.claude/skills/{name}/SKILL.md"
        ));
        e
    };
    let episodes = [
        Episode::new(&format!("{shared}:note"), b"a note for everyone", &[&all]),
        Episode::new(
            &format!("{secret}:note"),
            b"a note for bob alone",
            &["user:bob@acme.com"],
        ),
        Episode::new(
            &format!("{clubbed}:note"),
            b"a note for the club",
            &[&format!("group:{club}")],
        ),
        skill(&format!("{shared}skill"), &all),
        skill(&format!("{secret}skill"), "user:bob@acme.com"),
    ];
    store.commit(&scope, "test", &episodes, &[]).await.unwrap();
    let first_home = |s: String, bearer: String| {
        let sel = sel.clone();
        async move {
            let (status, text) =
                post(&format!("{s}/browse{sel}"), &bearer, &json!({"op": "home"})).await;
            (
                status,
                serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            )
        }
    };
    // A reads both episodes before anyone opens Home (a Home kept for a reader is answered past later additions).
    for (bearer, tag) in [(&bob, &secret), (&alice, &shared)] {
        let url = format!("{a}/browse{sel}");
        let ls = json!({"op": "ls", "path": ["sources"]});
        let (_, ok) = wait_until(
            Duration::from_secs(30),
            || post(&url, bearer, &ls),
            |(_, t)| t.contains(tag.as_str()),
        )
        .await;
        assert!(ok, "A never read {tag}");
    }
    // Each reads Home through A until it shows what they read: Bob his own episode, Gina her group's, the others the
    // shared one.
    for (who, bearer, tag) in [
        ("bob", &bob, &secret),
        ("alice", &alice, &shared),
        ("dan", &dan, &shared),
        ("fay", &fay, &shared),
        ("gina", &gina, &clubbed),
        // Super admins read all of the org, Bob's episode too.
        ("sam", &sam, &secret),
        ("sue", &sue, &secret),
    ] {
        let (h, ok) = wait_until(
            Duration::from_secs(60),
            || home(a, &sel, bearer),
            |h| h.contains(tag.as_str()),
        )
        .await;
        assert!(ok, "{who}'s Home on A never showed {tag}: {h}");
    }
    let (_, h) = first_home(a.to_string(), alice.clone()).await;
    assert!(
        !h.to_string().contains(&secret),
        "alice's Home on A holds bob's episode: {h}"
    );
    assert!(
        !h["dream"].is_null(),
        "alice's Home on A has no admin's part: {h}"
    );
    // Bob opens the Skills list on A: his own, with his skill.
    let (status, l) = post(
        &format!("{a}/skills/review{sel}"),
        &bob,
        &json!({"op": "list"}),
    )
    .await;
    assert!(
        status == 200 && l.contains(&format!("{secret}skill")),
        "bob's Skills list on A: {l}"
    );
    // Alice, the org's admin, opens Traces on A: the org's activity as the list reads it, through `/admin`.
    let traces = json!({"op": "activity", "org": org, "light": true, "limit": 50});
    let (status, body) = admin_as(a, &alice, &traces).await;
    assert_eq!(status, 200, "alice's Traces on A: {body}");
    // And Hank, as an admin too.
    let (status, body) = admin_as(a, &hank, &traces).await;
    assert_eq!(status, 200, "hank's Traces on A: {body}");
    // And the folder of his source: a read that names memory, never written down.
    let (status, l) = post(
        &format!("{a}/browse{sel}"),
        &bob,
        &json!({"op": "ls", "path": ["sources", secret]}),
    )
    .await;
    assert_eq!(status, 200, "{l}");
    // A writes them down beside the org's facts table.
    let facts = catalog
        .load_table(&iceberg::TableIdent::new(
            iceberg::NamespaceIdent::new(scope.namespace()),
            "facts".into(),
        ))
        .await
        .unwrap();
    let prefix = [rest.key_of(facts.metadata().location())];
    for user in [
        "alice@acme.com",
        "bob@acme.com",
        "dan@acme.com",
        fay_id.as_str(),
        "gina@acme.com",
        "sam@acme.com",
        "sue@acme.com",
        "hank@acme.com",
    ] {
        let (_, ok) = wait_until(
            Duration::from_secs(30),
            || rest.objects_with(&prefix, user),
            |o| o.iter().any(|k| k.ends_with("readers/home.json")),
        )
        .await;
        assert!(ok, "{user} never written down as a recent Home reader");
    }
    // The file as A left it, all eight in it (Hank with his Traces), with Bob's two reads (his sources and his Skills
    // list) and Alice's two (her sources and her Traces): A writes a reader again once they read something new.
    let reads_of = |l: &[memstore::recent_readers::RecentReader], user: &str| {
        l.iter()
            .find(|r| r.user == user)
            .map_or(0, |r| r.reads.len())
    };
    let (as_a_left, ok) = wait_until(
        Duration::from_secs(30),
        || async { store.recent_readers(&scope).await.unwrap() },
        |l| {
            l.len() == 8
                && reads_of(l, "bob@acme.com") == 2
                && reads_of(l, "alice@acme.com") == 2
                && reads_of(l, "hank@acme.com") == 1
        },
    )
    .await;
    assert!(ok, "A never wrote all eight and their reads: {as_a_left:?}");
    // What they read is written in fixed shapes alone: nothing in the file names Bob's source.
    let named: Vec<String> = rest
        .objects_with(&prefix, &secret)
        .await
        .into_iter()
        .filter(|k| k.ends_with("readers/home.json"))
        .collect();
    assert!(
        named.is_empty(),
        "the readers file names a source: {named:?}"
    );
    drop(sa);
    // Before B starts: Dan left, Fay is disabled, Gina is out of her group and Hank is an admin no more.
    // Changed through a server that works no Home out at start: B alone reads the readers as A wrote them.
    let sb_admin = serve_env(
        &rest,
        &rest.uri,
        dir.path(),
        &issuer,
        &[("AMPUP_MEMORY_PRIME_AT_ONCE", "0")],
    )
    .await;
    admin(
        &sb_admin.url,
        json!({"op": "remove_member", "org": org, "user": "dan@acme.com"}),
    )
    .await;
    closure(
        sb_admin.url.clone(),
        json!({"user": fay_id, "disabled": true}),
    )
    .await;
    closure(
        sb_admin.url.clone(),
        json!({"user": "gina@acme.com", "remove": [club.clone()]}),
    )
    .await;
    admin(
        &sb_admin.url,
        json!({"op": "add_member", "org": org, "user": "hank@acme.com", "role": "member"}),
    )
    .await;
    drop(sb_admin);
    // As A left it, whatever another test's server (each warms every org in the catalog) took out since.
    store
        .put_recent_readers(&scope, as_a_left.clone())
        .await
        .unwrap();

    let sb = serve_env(
        &rest,
        &rest.uri,
        dir.path(),
        &issuer,
        &env_with("sam@acme.com"),
    )
    .await;
    let b = sb.url.clone();
    let t = Instant::now();
    while reqwest::get(format!("{b}/ready"))
        .await
        .map_or(true, |r| r.status() != 200)
    {
        assert!(t.elapsed() < Duration::from_secs(120), "B never ready");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // B worked Bob's, Alice's, Gina's, Hank's, Sam's and Sue's Home out before it was ready, and neither Dan's nor Fay's (a positive control:
    // without it, the checks below could pass with nothing worked out at start). Dan and Fay, still written down, are
    // left out and taken out of the file: by B, or first by another test's server warming every org in the catalog.
    let log = std::fs::read_to_string(&sb.log).unwrap_or_default();
    let line = log
        .lines()
        .find(|l| l.contains(&format!("recent readers of org:{org} ")))
        .unwrap_or_default();
    let (gone, found) = line
        .split(" written down no longer read it so")
        .next()
        .and_then(|l| l.rsplit(", ").next())
        .and_then(|l| l.split_once(" of "))
        .and_then(|(g, f)| Some((g.parse::<usize>().ok()?, f.parse::<usize>().ok()?)))
        .unwrap_or((usize::MAX, 0));
    assert!(
        line.contains("primed the Home of 6 of 6 ") && gone <= 2 && found == gone + 6,
        "B didn't work out Alice's, Bob's, Gina's, Hank's, Sam's and Sue's Home of org:{org} alone at start: {log}"
    );
    // Dan and Fay are out of the file now B started; those who still read the org are in it.
    let now: Vec<String> = store
        .recent_readers(&scope)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.user)
        .collect();
    assert!(
        !now.iter()
            .any(|u| u == "dan@acme.com" || u == fay_id.as_str())
            && [
                "alice@acme.com",
                "bob@acme.com",
                "gina@acme.com",
                "sam@acme.com",
                "sue@acme.com",
                "hank@acme.com",
            ]
            .iter()
            .all(|u| now.iter().any(|n| n == u)),
        "the readers file once B started: {now:?}"
    );
    // B made Bob's and Alice's `ls sources` and Bob's Skills list from A again, each as its own reader: each first
    // listing on B is its reader's alone, and answered from what B kept for its reader (the count of kept answers
    // served moves), which Erin's, never made at start, isn't. Hank's Traces, an admin's read he may make no more, are
    // the one not made.
    assert!(
        line.contains("4 of their 5 other Studio reads"),
        "B didn't make Alice's and Bob's reads at start, or made Hank's Traces: {line}"
    );
    let served = || {
        let b = b.clone();
        async move {
            let text = CLIENT
                .get(format!("{b}/metrics"))
                .bearer_auth(ADMIN_KEY)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            text.lines()
                .find_map(|l| l.strip_prefix("memstore_kept_answers_served_total "))
                .and_then(|n| n.trim().parse::<u64>().ok())
                .expect("no count of kept answers served")
        }
    };
    for (who, bearer, sees_secret) in [
        ("bob", &bob, true),
        ("alice", &alice, false),
        ("erin", &erin, false),
    ] {
        let before = served().await;
        let (status, l) = post(
            &format!("{b}/browse{sel}"),
            bearer,
            &json!({"op": "ls", "path": ["sources"]}),
        )
        .await;
        let kept = served().await - before;
        assert_eq!(status, 200, "{l}");
        assert!(
            l.contains(shared.as_str()) || who == "erin",
            "{who}'s sources on B don't list the shared note: {l}"
        );
        assert_eq!(
            l.contains(secret.as_str()),
            sees_secret,
            "{who}'s first sources listing on B: {l}"
        );
        assert_eq!(
            kept > 0,
            who != "erin",
            "{who}'s first sources listing on B answered {kept} kept answers"
        );
    }
    // B read both skills' summaries at start, as one reading all: each is listed only to who reads its skill.
    assert!(
        log.contains(&format!("read the summaries of 2 skills of org:{org} ")),
        "B didn't read the skills' summaries at start: {log}"
    );
    for (who, bearer, sees_secret) in [
        ("bob", &bob, true),
        ("alice", &alice, false),
        ("erin", &erin, false),
    ] {
        let before = served().await;
        let (status, l) = post(
            &format!("{b}/skills/review{sel}"),
            bearer,
            &json!({"op": "list"}),
        )
        .await;
        let kept = served().await - before;
        assert_eq!(status, 200, "{l}");
        assert!(
            l.contains(&format!("{shared}skill"))
                && l.contains(&format!("{secret}skill")) == sees_secret,
            "{who}'s Skills list on B: {l}"
        );
        // Bob's, listed at start as he lists it, is answered from what was kept for him.
        assert!(
            who != "bob" || kept > 0,
            "bob's first Skills list on B wasn't the one made at start"
        );
    }
    // Bob's first Home on B is the one worked out for him, with his own episode.
    let (status, h) = first_home(b.clone(), bob.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_object() && h.to_string().contains(&secret),
        "bob's first Home on B wasn't his own, worked out at start: {h}"
    );
    assert!(
        h["dream"].is_null(),
        "bob, a member, read an admin's part: {h}"
    );
    // Gina's is hers as she reads now, out of her group: at once, without the group's episode.
    let (status, h) = first_home(b.clone(), gina.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_object(),
        "gina's first Home on B wasn't worked out at start for her groups now: {h}"
    );
    assert!(
        !h.to_string().contains(&clubbed) && !h.to_string().contains(&secret),
        "gina's Home on B holds what she no longer reads: {h}"
    );
    // Sam, still a super admin on B, reads all of the org: his first Home is the one worked out for him so, with Bob's
    // episode.
    let (status, h) = first_home(b.clone(), sam.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_object() && h.to_string().contains(&secret),
        "sam's first Home on B wasn't worked out at start as a super admin's: {h}"
    );
    // Sue, a super admin on A and not on B, is the member she signs in as on B: her first Home is the one worked out
    // for her as a member, without Bob's episode.
    let (status, h) = first_home(b.clone(), sue.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        !h.to_string().contains(&secret),
        "sue, no longer a super admin, was answered a Home reading all of the org: {h}"
    );
    assert!(
        h["kept"].is_object(),
        "sue's first Home on B wasn't worked out at start as the member she is there: {h}"
    );
    // Alice's Traces, made again at start as she, an admin, read them: her first on B is answered from what was kept.
    let before = served().await;
    let (status, body) = admin_as(&b, &alice, &traces).await;
    assert_eq!(status, 200, "alice's Traces on B: {body}");
    assert!(
        served().await > before,
        "alice's first Traces on B weren't the ones made at start"
    );
    // Hank's, demoted before B started, are refused, with nothing kept served.
    let before = served().await;
    let (status, body) = admin_as(&b, &hank, &traces).await;
    assert!(
        status != 200 && body.get("entries").is_none(),
        "hank, not an admin, read the org's activity on B: {status} {body}"
    );
    assert_eq!(
        served().await,
        before,
        "hank's first Traces on B were answered from what was kept"
    );
    // Alice, demoted after B worked hers out as an admin: her first Home is a member's, worked out for her now, and
    // she reads the org's activity no more.
    admin(
        &b,
        json!({"op": "add_member", "org": org, "user": "alice@acme.com", "role": "member"}),
    )
    .await;
    for (who, bearer) in [("alice", &alice), ("bob", &bob)] {
        let (status, body) = admin_as(&b, bearer, &traces).await;
        assert!(
            status != 200 && body.get("entries").is_none(),
            "{who}, not an admin, read the org's activity on B: {status} {body}"
        );
    }
    let pending = |h: &Value, part: &str| {
        h["pending"]
            .as_array()
            .is_some_and(|p| p.iter().any(|x| x == part))
    };
    let (status, h) = first_home(b.clone(), alice.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_null() && h["dream"].is_null(),
        "alice, no longer an admin, was answered the Home B worked out for her as one: {h}"
    );
    // Asked again for every part (a first ask that ran out of time leaves them all pending): none is an admin's.
    let again: Value = serde_json::from_str(&home(&b, &sel, &alice).await).unwrap();
    assert!(
        again["dream"].is_null(),
        "alice, no longer an admin, read an admin's part on B: {again}"
    );
    assert!(
        !h.to_string().contains(&secret),
        "alice's Home on B holds bob's episode: {h}"
    );
    // Erin, a member who never opened Home, had nothing worked out: hers is worked out for her, with what she reads.
    let (status, h) = first_home(b.clone(), erin.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_null(),
        "erin's first Home was one worked out for someone: {h}"
    );
    let (h, ok) = wait_until(
        Duration::from_secs(60),
        || home(&b, &sel, &erin),
        |h| h.contains(shared.as_str()),
    )
    .await;
    assert!(ok && !h.contains(&secret), "erin's Home on B: {h}");
    // Dan, who left, and Fay, disabled, read nothing.
    for (who, bearer) in [("dan", &dan), ("fay", &fay)] {
        let (status, h) = first_home(b.clone(), bearer.clone()).await;
        assert!(
            status != 200 && !h.to_string().contains(&shared),
            "{who} read Home: {status} {h}"
        );
    }
    // Bob made an admin now: his first Home is his as an admin, never the one worked out while he was a member.
    admin(
        &b,
        json!({"op": "add_member", "org": org, "user": "bob@acme.com", "role": "admin"}),
    )
    .await;
    let (status, h) = first_home(b.clone(), bob.clone()).await;
    assert_eq!(status, 200, "{h}");
    assert!(
        h["kept"].is_null() && (!h["dream"].is_null() || pending(&h, "dream")),
        "bob, an admin now, was answered his Home as a member: {h}"
    );
}
