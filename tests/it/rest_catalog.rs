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
//! - The catalog can rewrite a table (`Store::check_rewrites`), which forget and org delete need.
//!
//! They need the fixture in `tests/fixtures/rest-catalog` (the Iceberg REST fixture and MinIO):
//!
//! ```text
//! docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait
//! source tests/fixtures/rest-catalog/env.sh
//! cargo test --test it rest_catalog::
//! ```
//!
//! Without `MEMSTORE_TEST_REST_URI` each test prints that it was skipped and passes without running. Every
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
    /// More catalog props, `key=value;...` (`MEMSTORE_TEST_REST_PROPS`: a catalog's credential and scope).
    extra: String,
}

impl Rest {
    /// The fixture, or None (and the test says it was skipped) without `MEMSTORE_TEST_REST_URI`. Also None
    /// once the test passed in a process of its own (`crate::own_process`): this sets the index's S3 settings
    /// in the environment, which no other test may see.
    fn from_env(test: &str) -> Option<Rest> {
        let Ok(uri) = std::env::var("MEMSTORE_TEST_REST_URI") else {
            eprintln!(
                "SKIPPED {test}: MEMSTORE_TEST_REST_URI is not set. Start tests/fixtures/rest-catalog \
                 (docker compose) and `source tests/fixtures/rest-catalog/env.sh` to run it."
            );
            return None;
        };
        if !crate::own_process(&format!("rest_catalog::{test}")) {
            return None;
        }
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let bucket = var("MEMSTORE_TEST_S3_BUCKET", "memstore");
        let rest = Rest {
            uri,
            warehouse: var("MEMSTORE_TEST_REST_WAREHOUSE", "s3://memstore/warehouse"),
            s3_endpoint: var("MEMSTORE_TEST_S3_ENDPOINT", "http://localhost:19000"),
            index: format!("s3://{bucket}/index/{}", run_id()),
            bucket,
            key: var("MEMSTORE_TEST_S3_KEY", "admin"),
            secret: var("MEMSTORE_TEST_S3_SECRET", "password"),
            extra: var("MEMSTORE_TEST_REST_PROPS", ""),
        };
        // The search index (Lance) reads its S3 settings from the environment, in this process and in the
        // servers it starts. Set once, before any store of this process opens an index.
        static ENV: std::sync::Once = std::sync::Once::new();
        ENV.call_once(|| unsafe {
            std::env::set_var("MEMSTORE_INDEX_URI", &rest.index);
            std::env::set_var("AWS_ACCESS_KEY_ID", &rest.key);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", &rest.secret);
            std::env::set_var("AWS_REGION", "us-east-1");
            std::env::set_var("AWS_ENDPOINT", &rest.s3_endpoint);
            std::env::set_var("AWS_ALLOW_HTTP", "true");
        });
        Some(rest)
    }

    /// Catalog and storage props besides the address, as `MEMSTORE_REST_PROPS` takes them.
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

    /// Every object under `prefixes` (bucket keys) whose bytes hold `tag`.
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
                let bytes = op.read(e.path()).await.unwrap().to_vec();
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
    let port = free_port();
    let log = dir.join(format!("serve-{port}.log"));
    let out = std::fs::File::create(&log).unwrap();
    let props: Vec<String> = rest
        .storage_props()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let child = Command::new(env!("CARGO_BIN_EXE_memstore"))
        .args(["serve", "org", &port.to_string()])
        .env("MEMSTORE_PLATFORM", "rest")
        .env("MEMSTORE_REST_URI", uri)
        .env("MEMSTORE_WAREHOUSE", &rest.warehouse)
        .env("MEMSTORE_REST_PROPS", props.join(";"))
        .env("MEMSTORE_CALLER_IDENTITY", "token")
        .env("MEMSTORE_API_KEYS", keys_file(dir))
        .env("MEMSTORE_OIDC_ISSUER", issuer)
        .env("MEMSTORE_OIDC_AUDIENCE", CLIENT_ID)
        .env("MEMSTORE_ALLOW_LOCAL_ISSUERS", "1")
        .env("MEMSTORE_DIRECTORY_SNAPSHOT_EVERY", "7")
        .env("MEMSTORE_AUDIT_FLUSH_SECS", "1")
        .env("MEMSTORE_CONFIG_DIR", dir)
        .env_remove("MEMSTORE_DISABLED_FILE")
        .env_remove("MEMSTORE_TEAM_CSV")
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

    // Acme's admin forgets Carol through one process.
    let (status, body) = admin_as(
        a,
        &alice,
        &json!({"op": "forget_user", "user": "carol@acme.com", "org": acme}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["forgotten"][&acme]["episodes"], 2, "{body}");
    assert_eq!(body["forgotten"][&acme]["facts"], 2, "{body}");
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
