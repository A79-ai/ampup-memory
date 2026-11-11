//! Offboarding, end to end, on a real `memstore serve` over HTTP and a local catalog: two orgs, `acme`
//! (team `a1`) and `globex`, filled with memories that each carry a tag no other memory has. Then:
//!
//! - `memstore org export acme` (the operator, through the CLI) and the `/admin` export as acme's admin:
//!   acme's facts, entities and episodes, nothing of globex's, no key or key hash; the admin's holds what acme
//!   shares with all its members, as their reads do, and not what is a single other member's;
//! - callers who must be refused are: globex's admin forgetting an acme member, a plain member forgetting
//!   anyone or exporting, acme's admin deleting acme, acme's admin exporting globex, a service key exporting;
//! - acme's admin demoted in the middle of forgetting carol (after carol is out of acme) deletes nothing of hers
//!   and leaves no tombstone; made an admin again, she runs the forget again and it finishes:
//! - acme's admin forgets carol: her episodes are gone from search, queries and episode reads, through pins taken
//!   before the forget too (a forget wins over a pin), and from every file in the warehouse and the search index;
//!   everyone else's memory stays; a tombstone is in acme's audit and carol no longer belongs to acme;
//! - an org deleted only in the directory (a delete that stopped halfway) can't be created again until
//!   `org delete` is run again, which finishes it;
//! - `memstore org delete acme --confirm acme` (the operator): acme's key and people are refused, its
//!   tags are in no file under the warehouse or the index, its rows are gone from the audit and key usage
//!   tables, and stay gone when a server that heard of the delete late flushes what it held for acme (or for
//!   acme created again since), and globex is untouched; all along the server's maintenance window compacts the ledger, and
//!   a compaction after the delete leaves no ledger file naming acme's scopes;
//! - on a catalog that can't rename a table, put a second table at one location, or delete a purged table's
//!   files (as some REST catalogs can't), a forget and a delete are refused before anything changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::Bytes;
use iceberg::table::Table;
use iceberg::{Catalog, NamespaceIdent, TableCommit, TableCreation, TableIdent};
use memstore::audit::{Entry, Kind};
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::people::directory::{Directory, Event};
use memstore::server::{Options, serve_with};
use memstore::{AclTarget, Episode, Fact, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const CLIENT_ID: &str = "memstore-cli";
const ADMIN_KEY: &str = "msk_platform_admin_for_offboarding";
const ALICE_FILE_KEY: &str = "msk_plain_file_key_in_alices_name";

/// The server's catalog, which, once armed, demotes alice from acme's admins right after the directory commit
/// that takes carol out of acme: an admin demoted between a forget's removal and the deletion that follows it.
struct DemoteMidForget {
    inner: Arc<dyn Catalog>,
    /// The demotion's writer, straight on `inner`.
    store: Store,
    armed: Arc<AtomicBool>,
}

impl std::fmt::Debug for DemoteMidForget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DemoteMidForget")
    }
}

#[async_trait::async_trait]
impl Catalog for DemoteMidForget {
    async fn list_namespaces(
        &self,
        parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        self.inner.list_namespaces(parent).await
    }
    async fn create_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<iceberg::Namespace> {
        self.inner.create_namespace(ns, props).await
    }
    async fn get_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<iceberg::Namespace> {
        self.inner.get_namespace(ns).await
    }
    async fn namespace_exists(&self, ns: &NamespaceIdent) -> iceberg::Result<bool> {
        self.inner.namespace_exists(ns).await
    }
    async fn update_namespace(
        &self,
        ns: &NamespaceIdent,
        props: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        self.inner.update_namespace(ns, props).await
    }
    async fn drop_namespace(&self, ns: &NamespaceIdent) -> iceberg::Result<()> {
        self.inner.drop_namespace(ns).await
    }
    async fn list_tables(&self, ns: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        self.inner.list_tables(ns).await
    }
    async fn create_table(
        &self,
        ns: &NamespaceIdent,
        creation: TableCreation,
    ) -> iceberg::Result<Table> {
        self.inner.create_table(ns, creation).await
    }
    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<Table> {
        self.inner.load_table(table).await
    }
    async fn drop_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.drop_table(table).await
    }
    async fn purge_table(&self, table: &TableIdent) -> iceberg::Result<()> {
        self.inner.purge_table(table).await
    }
    async fn table_exists(&self, table: &TableIdent) -> iceberg::Result<bool> {
        self.inner.table_exists(table).await
    }
    async fn rename_table(&self, src: &TableIdent, dest: &TableIdent) -> iceberg::Result<()> {
        self.inner.rename_table(src, dest).await
    }
    async fn register_table(&self, table: &TableIdent, location: String) -> iceberg::Result<Table> {
        self.inner.register_table(table, location).await
    }
    async fn update_table(&self, commit: TableCommit) -> iceberg::Result<Table> {
        let id = commit.identifier();
        let directory =
            id.namespace().to_url_string() == "memory_system" && id.name() == "directory";
        let table = self.inner.update_table(commit).await?;
        if directory && self.armed.load(Ordering::SeqCst) {
            let dir = Directory::fresh(&self.store).await.unwrap();
            let carol_out = dir
                .orgs
                .get("acme")
                .is_some_and(|o| !o.members.contains_key("carol@acme.com"));
            if carol_out && self.armed.swap(false, Ordering::SeqCst) {
                let mut demote = Event::new("svc:platform", "member.add", "acme");
                demote.user = Some("alice@acme.com".into());
                demote.role = Some("member".into());
                self.store.append_directory(&[demote]).await.unwrap();
            }
        }
        Ok(table)
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

/// The identity provider's public side: OpenID discovery and its signing key.
async fn start_idp() -> String {
    // Bound here, not picked and then re-bound: a test running alongside could take a port in between.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let issuer = format!("http://127.0.0.1:{port}");
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

static CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

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

/// `/admin` as `bearer`.
async fn admin_as(s: &str, bearer: &str, req: Value) -> (u16, String) {
    post(&format!("{s}/admin"), bearer, &req).await
}

/// `/admin` as the operator; must succeed.
async fn admin(s: &str, req: Value) -> Value {
    let (status, body) = admin_as(s, ADMIN_KEY, req.clone()).await;
    assert_eq!(status, 200, "{req}: {body}");
    serde_json::from_str(&body).unwrap()
}

/// The `memstore` binary with its own config dir.
fn memstore(home: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ampup-memory"))
        .args(args)
        .env_remove("AMPUP_MEMORY_CALLER_IDENTITY")
        .env_remove("MEMSTORE_CALLER_IDENTITY")
        .env("AMPUP_MEMORY_CONFIG_DIR", home)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
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

/// Everything `bearer` gets back from every read path of selection `sel`: search, facts and passage
/// queries, and the episodes `ids`, as one text. `None` when the selection is refused.
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
        // The named metrics' answers (kept between reads) and the facts they cite.
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
        assert_eq!(status, 200, "{path}{sel}: {text}");
        out.push_str(&text);
    }
    let call = |name: &str, args: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let mut tools = vec![call("search_memory", json!({"query": "owner note deal"}))];
    for id in ids {
        tools.push(call("get_episode", json!({"episode_id": id})));
    }
    for c in tools {
        let (status, text) = post(&format!("{s}/mcp{sel}"), bearer, &c).await;
        if status == 401 || status == 403 {
            return None;
        }
        out.push_str(&text);
    }
    Some(out)
}

/// Waits until `check` holds for what `bearer` reads (within the servers' staleness window).
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
        if check(&got) {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{what}: {got:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Every file under `root` whose bytes hold `tag`.
fn files_with(root: &Path, tag: &str) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p)
                && bytes.windows(tag.len()).any(|w| w == tag.as_bytes())
            {
                out.push(p);
            }
        }
    }
    out
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

/// A fact extracted from episodes already committed (as extraction would): entity and value carry `tag`.
async fn extract(store: &Store, scope: &Scope, tag: &str, from: &[&str]) {
    let mut f = Fact::new(&format!("deal:{tag}"), "owner", tag);
    f.source_episodes = from.iter().map(|s| s.to_string()).collect();
    store.commit(scope, "extractor", &[], &[f]).await.unwrap();
}

/// Bring every scope's fact and passage index up to date, so the index holds what the tables hold.
async fn index(store: &Store, scopes: &[Scope]) {
    for s in scopes {
        memstore::search::update(store, s).await.unwrap();
        memstore::search::passages::update(store, s).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_org_is_exported_a_person_forgotten_and_an_org_deleted_for_good() {
    // Its index location is set in the environment (below), which no other test may see.
    if !crate::own_process(
        "offboarding::an_org_is_exported_a_person_forgotten_and_an_org_deleted_for_good",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let warehouse = dir.path().join("warehouse");
    let index_dir = dir.path().join("index");
    std::fs::create_dir_all(&warehouse).unwrap();
    // The index lives under this test's directory, so its files are checked too. Set before any store.
    unsafe { std::env::set_var("AMPUP_MEMORY_INDEX_URI", &index_dir) };
    // Every audit event is also appended to this file as it is flushed (the SIEM sink).
    let sink_file = dir.path().join("audit-sink.ndjson");
    unsafe {
        std::env::set_var(
            "AMPUP_MEMORY_AUDIT_SINK",
            format!("file:{}", sink_file.display()),
        )
    };
    // The server's maintenance window, held open, compacts the ledger whenever it holds more than two data files,
    // every second: the forget and the delete below race those compactions, as they would a production server's
    // in the night.
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_SECS", "1") };
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_WINDOW", "always") };
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_PACE", "0") };
    unsafe { std::env::set_var("AMPUP_MEMORY_LEDGER_COMPACT_FILES", "2") };
    let roots = [warehouse.as_path(), index_dir.as_path()];
    let in_files =
        |tag: &str| -> Vec<PathBuf> { roots.iter().flat_map(|r| files_with(r, tag)).collect() };
    // The ledger's files that hold `s` (a scope's namespace: every ledger row names its scope).
    let ledger_with = |s: &str| -> Vec<PathBuf> {
        files_with(&warehouse, s)
            .into_iter()
            .filter(|p| p.components().any(|c| c.as_os_str() == "ledger"))
            .collect()
    };

    let issuer = start_idp().await;
    let catalog = platform::local(&format!("file://{}", warehouse.display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    // The server reads and writes through a catalog that demotes alice in the middle of a forget, once armed.
    let demote_mid_forget = Arc::new(AtomicBool::new(false));
    let served: Arc<dyn Catalog> = Arc::new(DemoteMidForget {
        inner: catalog.clone(),
        store: Store::new(catalog.clone()),
        armed: demote_mid_forget.clone(),
    });
    // The operator's admin key, and a plain file key the keys file gives alice's name.
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true},
                      {"id": "alicefile", "sha256": auth::hex(&sha2::Sha256::digest(ALICE_FILE_KEY.as_bytes())),
                       "user": "alice@acme.com"}]);
    let token_auth = TokenAuth::new(Some(&keys.to_string()), None)
        .unwrap()
        .allow_local_issuers()
        .with_oidc(
            OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                .await
                .unwrap(),
        );
    let port = free_port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            catalog: Some(served),
            token_auth: Some(token_auth),
            audit_flush: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while reqwest::get(s).await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Two orgs; acme with a team. Alice and Gary are their admins; Bob and Carol plain members.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "a1"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["a1"]}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
        json!({"op": "invite", "org": "globex", "members": [{"user": "erin@globex.com"}]}),
    ] {
        admin(s, change).await;
    }
    let a_key = format!("msk_{}acmereader", uuid::Uuid::new_v4().simple());
    let b_key = format!("msk_{}globexreader", uuid::Uuid::new_v4().simple());
    admin(
        s,
        json!({"op": "create_key", "org": "acme", "name": "dash", "role": "reader", "key": a_key}),
    )
    .await;
    admin(
        s,
        json!({"op": "create_key", "org": "globex", "name": "dash", "role": "reader", "key": b_key}),
    )
    .await;
    let a_key_sha = auth::hex(&sha2::Sha256::digest(a_key.as_bytes()));
    let [alice, bob, carol, gary, erin] = [
        "alice@acme.com",
        "bob@acme.com",
        "carol@acme.com",
        "gary@globex.com",
        "erin@globex.com",
    ]
    .map(|u| id_token(&issuer, u));

    // Memory. Ingested notes in each scope; notes people wrote over MCP; facts extracted from them.
    let acme = Scope::org("acme").unwrap();
    let a1 = Scope::team("acme", "a1").unwrap();
    let globex = Scope::org("globex").unwrap();
    // A SQL metric over the deals' owners on each: its kept answer is read below before and after the forget.
    for s in [&acme, &a1, &globex] {
        store
            .set_schema(s, "main", "test", &crate::isolation::owners_schema(), false)
            .await
            .unwrap();
    }
    let org_note = seed(&store, &acme, "xqorgalpha").await;
    let team_note = seed(&store, &a1, "xqteamalpha").await;
    let b_note = seed(&store, &globex, "xqorgbeta").await;
    // An acme episode only Bob may read (its ACL names him alone), and a fact from it.
    let secret = Episode::new(
        "note:owner",
        b"xqbobsecret owner note",
        &["user:bob@acme.com"],
    );
    let secret_fact =
        Fact::new("deal:xqbobsecret", "owner", "xqbobsecret").from_episodes(&[&secret]);
    store
        .commit(&acme, "test", &[secret], &[secret_fact])
        .await
        .unwrap();
    // An acme episode shared with the whole org (`group:acme.all`, which its members and keys hold through the
    // directory), and a fact from it.
    let org_wide = Episode::new(
        "note:handbook",
        b"xqorgwide owner note",
        &["group:acme.all"],
    );
    let org_wide_fact =
        Fact::new("deal:xqorgwide", "owner", "xqorgwide").from_episodes(&[&org_wide]);
    store
        .commit(&acme, "test", &[org_wide], &[org_wide_fact])
        .await
        .unwrap();
    // An acme message, and a fact from it, that its source then deleted: in no export.
    let deleted = Episode::new("slack:C9:1.0", b"xqdeleted owner note", &["group:acme"]);
    let deleted_fact = Fact::new("deal:xqdeleted", "owner", "xqdeleted").from_episodes(&[&deleted]);
    store
        .commit(
            &acme,
            "test",
            std::slice::from_ref(&deleted),
            std::slice::from_ref(&deleted_fact),
        )
        .await
        .unwrap();
    store
        .retract_episodes(&acme, "test", &AclTarget::Source("slack:C9:1.0".into()))
        .await
        .unwrap();
    let carol_team = remember(s, "?org=acme&team=a1", &carol, "xqcarolteam owner note").await;
    let carol_org = remember(s, "?org=acme", &carol, "xqcarolorg owner note").await;
    let bob_org = remember(s, "?org=acme", &bob, "xqbobkeeps owner note").await;
    let erin_org = remember(s, "?org=globex", &erin, "xqerinbeta owner note").await;
    extract(&store, &a1, "xqcarolfact", &[&carol_team]).await;
    extract(&store, &acme, "xqbobfact", &[&bob_org]).await;
    // Cites both: once Carol's episode is gone nobody may read it (a fact needs every source), so it goes.
    extract(&store, &acme, "xqmixedfact", &[&carol_org, &bob_org]).await;
    index(&store, &[acme.clone(), a1.clone(), globex.clone()]).await;
    let carol_tags = ["xqcarolteam", "xqcarolorg", "xqcarolfact", "xqmixedfact"];
    let acme_tags = [
        "xqorgalpha",
        "xqorgwide",
        "xqteamalpha",
        "xqbobkeeps",
        "xqbobfact",
        "xqbobsecret",
    ];
    let globex_tags = ["xqorgbeta", "xqerinbeta"];
    // The file scan sees what is stored (else every "in no file" below would hold trivially).
    for t in carol_tags.iter().chain(&acme_tags).chain(&globex_tags) {
        assert!(!in_files(t).is_empty(), "{t} should be in the warehouse");
    }
    let acme_ids = [org_note.as_str(), bob_org.as_str(), carol_org.as_str()];
    let a1_ids = [team_note.as_str(), carol_team.as_str()];
    let b_ids = [b_note.as_str(), erin_org.as_str()];
    until(s, "?org=acme", &bob, &acme_ids, "bob reads acme", |r| {
        r.as_ref().is_some_and(|r| {
            [
                "xqorgalpha",
                "xqorgwide",
                "xqbobkeeps",
                "xqcarolorg",
                "xqbobfact",
                "xqmixedfact",
            ]
            .iter()
            .all(|t| r.contains(t))
        })
    })
    .await;
    until(
        s,
        "?org=acme&team=a1",
        &carol,
        &a1_ids,
        "carol reads a1",
        |r| {
            r.as_ref().is_some_and(|r| {
                ["xqteamalpha", "xqcarolteam", "xqcarolfact"]
                    .iter()
                    .all(|t| r.contains(t))
            })
        },
    )
    .await;
    // Acme's key reads acme (so its use is in the audit and key usage tables).
    until(s, "", &a_key, &acme_ids, "acme's key reads acme", |r| {
        r.as_ref().is_some_and(|r| r.contains("xqorgalpha"))
    })
    .await;

    // Export, by the operator through the CLI: everything of acme's, nothing else, no keys.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (ok, out, err) = memstore(
        &home,
        &["auth", "login", "--server", s, "--api-key", ADMIN_KEY],
    );
    assert!(ok, "{out}{err}");
    let export = dir.path().join("export");
    let (ok, out, err) = memstore(
        &home,
        &["org", "export", "acme", "--out", export.to_str().unwrap()],
    );
    assert!(ok, "{out}{err}");
    let read = |p: &str| std::fs::read_to_string(export.join(p)).unwrap();
    let (org_facts, org_eps, team_facts, team_eps, entities) = (
        read("org/facts.jsonl"),
        read("org/episodes.jsonl"),
        read("team-a1/facts.jsonl"),
        read("team-a1/episodes.jsonl"),
        read("org/entities.jsonl"),
    );
    for t in ["xqorgalpha", "xqorgwide", "xqbobfact", "xqmixedfact"] {
        assert!(org_facts.contains(t), "{t} in {org_facts}");
    }
    for t in [
        "xqorgalpha",
        "xqorgwide",
        "xqbobkeeps",
        "xqcarolorg",
        "xqbobsecret",
    ] {
        assert!(org_eps.contains(t), "{t} in {org_eps}");
    }
    assert!(org_facts.contains("xqbobsecret"), "{org_facts}");
    assert!(team_facts.contains("xqteamalpha") && team_facts.contains("xqcarolfact"));
    assert!(team_eps.contains("xqteamalpha") && team_eps.contains("xqcarolteam"));
    assert!(entities.contains("deal:xqbobfact"), "{entities}");
    assert!(org_eps.contains(&format!("mcp:remember_memory:{}:notes", "bob@acme.com")));
    let everything: String = [&org_facts, &org_eps, &team_facts, &team_eps, &entities]
        .iter()
        .map(|s| s.as_str())
        .collect();
    for t in globex_tags {
        assert!(!everything.contains(t), "globex's {t} exported with acme");
    }
    assert!(
        !everything.contains("xqdeleted"),
        "a deleted message exported"
    );
    assert!(
        !everything.contains(&a_key_sha) && !everything.contains("msk_"),
        "a key in the export"
    );
    // Acme's admin exports what she may read, as every read path gives it to her: the org's scope, with what
    // acme shares with all its members (she holds it as they do, though `/admin` resolves no read), not team a1
    // (she isn't in it: a team is isolated from its org's admins), not the episode only Bob may read nor the
    // fact drawn from it.
    let (status, body) = admin_as(s, &alice, json!({"op": "export_org", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    for t in [
        "xqorgalpha",
        "xqorgwide",
        "xqbobkeeps",
        "xqcarolorg",
        "xqbobfact",
    ] {
        assert!(body.contains(t), "{t} not in acme's admin's export: {body}");
    }
    for t in [
        "xqorgbeta",
        "xqteamalpha",
        "xqcarolteam",
        "xqcarolfact",
        "xqbobsecret",
        "team-a1",
    ] {
        assert!(!body.contains(t), "{t} in acme's admin's export: {body}");
    }

    // Refused: acme's admin exporting globex, a plain member exporting acme, a service key exporting its own
    // org, globex's admin forgetting an acme member (naming acme, or naming no org), a plain member forgetting
    // anyone, acme's admin deleting acme.
    let refusals = [
        (&alice, json!({"op": "export_org", "org": "globex"})),
        (&bob, json!({"op": "export_org", "org": "acme"})),
        (&a_key, json!({"op": "export_org", "org": "acme"})),
        (
            &a_key,
            json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
        ),
        (
            &a_key,
            json!({"op": "delete_org", "org": "acme", "confirm": "acme"}),
        ),
        (
            &ALICE_FILE_KEY.to_string(),
            json!({"op": "export_org", "org": "acme"}),
        ),
        (
            &ALICE_FILE_KEY.to_string(),
            json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
        ),
        (
            &gary,
            json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
        ),
        (
            &gary,
            json!({"op": "forget_user", "user": "carol@acme.com"}),
        ),
        (
            &bob,
            json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
        ),
        (
            &carol,
            json!({"op": "forget_user", "user": "bob@acme.com", "org": "acme"}),
        ),
        (
            &alice,
            json!({"op": "delete_org", "org": "acme", "confirm": "acme"}),
        ),
        (&a_key, json!({"op": "purge_retracted", "org": "acme"})),
        (
            &ALICE_FILE_KEY.to_string(),
            json!({"op": "purge_retracted", "org": "acme"}),
        ),
        (&gary, json!({"op": "purge_retracted", "org": "acme"})),
        (&bob, json!({"op": "purge_retracted", "org": "acme"})),
    ];
    for (who, req) in &refusals {
        let (status, body) = admin_as(s, who, req.clone()).await;
        assert_eq!(status, 403, "{req} should be refused: {body}");
        assert!(!body.contains("xq"), "{body}");
    }
    // An org that doesn't exist is refused the same way as another's: no probing for org names.
    for req in [
        json!({"op": "export_org", "org": "nosuchorg"}),
        json!({"op": "forget_user", "user": "carol@acme.com", "org": "nosuchorg"}),
        json!({"op": "purge_retracted", "org": "nosuchorg"}),
    ] {
        let (status, body) = admin_as(s, &alice, req.clone()).await;
        assert_eq!(status, 403, "{req} should be refused: {body}");
        assert!(
            body.contains("only its admins may") && !body.contains("no org"),
            "{req} tells whether the org exists: {body}"
        );
    }
    // Globex's admin forgetting in globex finds nothing of an acme member's there, and touches no acme memory.
    let (status, body) = admin_as(
        s,
        &gary,
        json!({"op": "forget_user", "user": "carol@acme.com", "org": "globex"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("0 episodes"), "{body}");
    // Acme's admin forgetting globex's admin touches acme only: Gary still runs globex.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "forget_user", "user": "gary@globex.com", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = admin_as(s, &gary, json!({"op": "export_org", "org": "globex"})).await;
    assert!(status == 200 && body.contains("xqorgbeta"), "{body}");
    assert!(
        !body.contains("xqorgwide"),
        "acme's in globex's export: {body}"
    );
    // Nothing was refused into happening.
    until(
        s,
        "?org=acme&team=a1",
        &carol,
        &a1_ids,
        "carol still reads a1",
        |r| r.as_ref().is_some_and(|r| r.contains("xqcarolteam")),
    )
    .await;
    for t in carol_tags {
        assert!(!in_files(t).is_empty(), "{t} gone after refusals");
    }

    assert!(
        !in_files("xqdeleted").is_empty(),
        "a retracted message stays stored until a purge"
    );

    // Acme's admin purges what acme's sources deleted: the message and its fact leave every file, the rest
    // of acme's memory stays, and running it again finds nothing.
    let (status, body) = admin_as(s, &alice, json!({"op": "purge_retracted", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    let done: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(done["purged"]["episodes"], 1, "{body}");
    assert_eq!(done["purged"]["facts"], 1, "{body}");
    assert_eq!(in_files("xqdeleted"), Vec::<PathBuf>::new());
    for t in acme_tags.iter().chain(&carol_tags) {
        assert!(!in_files(t).is_empty(), "{t} went with a purge");
    }
    let (status, body) = admin_as(s, &alice, json!({"op": "purge_retracted", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""episodes":0"#), "{body}");
    // A late retry of the purged message is still retracted.
    store
        .commit(
            &acme,
            "test",
            std::slice::from_ref(&deleted),
            &[deleted_fact],
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    until(
        s,
        "?org=acme",
        &bob,
        &[deleted.episode_id.as_str(), org_note.as_str()],
        "acme after a late retry",
        |r| {
            r.as_ref()
                .is_some_and(|r| !r.contains("xqdeleted") && r.contains("xqorgalpha"))
        },
    )
    .await;

    // Bob and Carol pin the memory they read, as a long-running agent does (`POST /pin`): Bob acme's, Carol a1's.
    // Through his pin Bob reads what he reads unpinned, Carol's note in acme among it. The forget below wins over
    // both pins: a pin never keeps forgotten memory readable, nor stored.
    let take_pin = async |sel: &str, bearer: &str| -> String {
        let (status, body) =
            post(&format!("{s}/pin{sel}"), bearer, &json!({"ttl_secs": 3600})).await;
        assert_eq!(status, 200, "pin {sel}: {body}");
        serde_json::from_str::<Value>(&body).unwrap()["pin"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let bob_pin = take_pin("?org=acme", &bob).await;
    let carol_pin = take_pin("?org=acme&team=a1", &carol).await;
    let bob_pinned = format!("?org=acme&at={bob_pin}");
    until(
        s,
        &bob_pinned,
        &bob,
        &acme_ids,
        "bob through his pin",
        |r| {
            r.as_ref().is_some_and(|r| {
                ["xqorgalpha", "xqbobkeeps", "xqcarolorg", "xqmixedfact"]
                    .iter()
                    .all(|t| r.contains(t))
            })
        },
    )
    .await;

    // Carol's activity (`/admin {op: activity}`) is her org's admins' to read, no one else's, and none of
    // theirs where the org turned it off.
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "log_answer", "arguments": {"question": "xqcarolasked who owns the launch"}}});
    let (_, raw) = post(&format!("{s}/mcp?org=acme"), &carol, &call).await;
    assert!(raw.contains("answer_id"), "{raw}");
    // The entry's id, as the tool's text names it.
    let carol_entry: String = raw
        .split_once("answer_id")
        .map_or("", |(_, r)| r)
        .chars()
        .skip_while(|c| !c.is_ascii_hexdigit())
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    assert!(carol_entry.len() >= 32, "{raw}");
    let conversation = json!({"op": "conversation", "org": "acme", "id": carol_entry});
    let activity = json!({"op": "activity", "org": "acme"});
    let t = Instant::now();
    loop {
        let (status, body) = admin_as(s, &alice, activity.clone()).await;
        assert_eq!(status, 200, "{body}");
        if body.contains("xqcarolasked") && body.contains("carol@acme.com") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{body}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // A branch's conflicts show their admin each side's values, but only those they may read: a value from Bob's
    // private note is withheld (counted, never shown). A member may not resolve them by merging.
    {
        let ep = |src: &str, body: &str, acl: &[&str]| Episode::new(src, body.as_bytes(), acl);
        let shared = ep("note:owner", "xqres shared note", &["group:acme.all"]);
        let private = ep("note:owner", "xqres bob only note", &["user:bob@acme.com"]);
        store
            .commit(&acme, "test", &[shared.clone(), private.clone()], &[])
            .await
            .unwrap();
        store
            .create_branch(&acme, "resolve", "tester")
            .await
            .unwrap();
        let f =
            |p: &str, v: &str, from: &Episode| Fact::new("deal:xqres", p, v).from_episodes(&[from]);
        store
            .commit(
                &acme,
                "test",
                &[],
                &[
                    f("owner", "xqmainowner", &shared),
                    f("secret", "xqmainsecret", &private),
                ],
            )
            .await
            .unwrap();
        store
            .commit_on(
                &acme,
                "resolve",
                "agent",
                &[],
                &[
                    f("owner", "xqbranchowner", &shared),
                    f("secret", "xqbranchsecret", &private),
                ],
            )
            .await
            .unwrap();
        let diff = json!({"op": "branch_diff", "org": "acme", "branch": "resolve"});
        let t = Instant::now();
        // The server reads the ledger as cached: the branch shows up within its refresh.
        let body = loop {
            let (status, body) = admin_as(s, &alice, diff.clone()).await;
            if status == 200 && body.contains("xqbranchowner") && body.contains("xqmainowner") {
                break body;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "{body}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        assert!(body.contains("deal:xqres.secret"), "{body}");
        assert!(
            !body.contains("xqmainsecret") && !body.contains("xqbranchsecret"),
            "{body}"
        );
        let keep = json!({"op": "merge_branch", "org": "acme", "branch": "resolve",
            "keep": {"deal:xqres.owner": "main", "deal:xqres.secret": "branch"}});
        for who in [&bob, &gary] {
            let (status, body) = admin_as(s, who, keep.clone()).await;
            assert_eq!(status, 403, "{body}");
            assert!(
                !body.contains("xqmainsecret") && !body.contains("xqbranchsecret"),
                "{body}"
            );
        }
    }
    // Her admin reads the entry whole.
    let (status, body) = admin_as(s, &alice, conversation.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("xqcarolasked"), "{body}");
    // And the facts from what she wrote (Studio's "by person" filter): those citing her note, no one else's.
    let user_facts = json!({"op": "user_facts", "org": "acme", "user": "Carol@acme.com"});
    let (status, body) = admin_as(s, &alice, user_facts.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("xqmixedfact"), "{body}");
    assert!(
        !body.contains("xqbobfact") && !body.contains("xqorgalpha"),
        "{body}"
    );
    for (who, req) in [
        (&bob, activity.clone()),
        (&bob, conversation.clone()),
        (&gary, conversation.clone()),
        (&ALICE_FILE_KEY.to_string(), conversation.clone()),
        (
            &alice,
            json!({"op": "conversation", "org": "acme", "team": "a1", "id": carol_entry}),
        ),
        (&gary, activity.clone()),
        (&ALICE_FILE_KEY.to_string(), activity.clone()),
        (&bob, user_facts.clone()),
        (&gary, user_facts.clone()),
        (&ALICE_FILE_KEY.to_string(), user_facts.clone()),
        (
            &alice,
            json!({"op": "user_facts", "org": "acme", "team": "a1", "user": "carol@acme.com"}),
        ),
        (&bob, json!({"op": "advice_trail", "org": "acme"})),
        (&gary, json!({"op": "advice_trail", "org": "acme"})),
        // A team is isolated from the rest of its org, its admins included: Alice isn't in a1.
        (
            &alice,
            json!({"op": "activity", "org": "acme", "team": "a1"}),
        ),
        (
            &alice,
            json!({"op": "advice_trail", "org": "acme", "team": "a1"}),
        ),
        // A branch's diff and its merge into main: the org's admins, a team's only for its readers.
        (
            &bob,
            json!({"op": "branch_diff", "org": "acme", "branch": "trial"}),
        ),
        (
            &bob,
            json!({"op": "merge_branch", "org": "acme", "branch": "trial"}),
        ),
        (
            &gary,
            json!({"op": "merge_branch", "org": "acme", "branch": "trial"}),
        ),
        (
            &alice,
            json!({"op": "branch_diff", "org": "acme", "team": "a1", "branch": "trial"}),
        ),
        (
            &alice,
            json!({"op": "merge_branch", "org": "acme", "team": "a1", "branch": "trial"}),
        ),
    ] {
        let (status, body) = admin_as(s, who, req.clone()).await;
        assert_eq!(status, 403, "{req}: {body}");
        assert!(
            !body.contains("xqcarolasked") && !body.contains("xqmixedfact"),
            "{body}"
        );
    }
    // The setting is the org's: a team's is refused, never taken and ignored.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "set_settings", "org": "acme", "team": "a1", "settings": {"activity": false}}),
    )
    .await;
    assert_ne!(status, 200, "{body}");
    let off = json!({"op": "set_settings", "org": "acme", "settings": {"activity": false}});
    let (status, body) = admin_as(s, &alice, off).await;
    assert_eq!(status, 200, "{body}");
    for req in [
        activity.clone(),
        conversation.clone(),
        user_facts.clone(),
        json!({"op": "advice_trail", "org": "acme"}),
    ] {
        let (status, body) = admin_as(s, &alice, req.clone()).await;
        assert_eq!(status, 403, "{req}: {body}");
        assert!(
            !body.contains("xqcarolasked") && !body.contains("xqmixedfact"),
            "{body}"
        );
    }
    let on = json!({"op": "set_settings", "org": "acme", "settings": {"activity": true}});
    let (status, body) = admin_as(s, &alice, on).await;
    assert_eq!(status, 200, "{body}");

    // Carol is in a session as she is forgotten: its row is held for the next flush (`memstore::sessions`), not
    // committed yet. Acme gives each session a branch of its own: what she remembers in it is on that branch,
    // not yet merged, as she is forgotten.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "set_settings", "org": "acme", "settings": {"session_branches": true}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "start_session", "arguments": {"context": "xqcarolsession plan the launch"}}});
    let (_, raw) = post(&format!("{s}/mcp?org=acme"), &carol, &call).await;
    assert!(raw.contains("session_id"), "{raw}");
    let text: Value = serde_json::from_str(&raw).unwrap();
    let started: Value = serde_json::from_str(
        text["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("{}"),
    )
    .unwrap_or_default();
    let sid = started["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!sid.is_empty(), "{raw}");
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "remember_memory", "arguments": {"source": "notes", "body": "xqcarolbranch owner note",
            "for": "team", "session_id": sid}}});
    let (_, raw) = post(&format!("{s}/mcp?org=acme"), &carol, &call).await;
    assert!(raw.contains("session's branch s-"), "{raw}");
    let branch_note = raw
        .split("episode ")
        .nth(1)
        .and_then(|t| t.split(',').next())
        .unwrap()
        .to_string();
    let branch: String = raw
        .split("branch s-")
        .nth(1)
        .map(|t| {
            format!(
                "s-{}",
                t.chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect::<String>()
            )
        })
        .unwrap();
    let on_branch = format!("?org=acme&branch={branch}");
    until(
        s,
        &on_branch,
        &alice,
        &[&branch_note],
        "carol's note on her session's branch",
        |r| r.as_ref().is_some_and(|r| r.contains("xqcarolbranch")),
    )
    .await;
    // Main doesn't have it until the session's branch merges.
    let main = reads(s, "?org=acme", &alice, &[&branch_note])
        .await
        .unwrap();
    assert!(!main.contains("xqcarolbranch"), "{main}");
    // Owner review ahead of the forget. Alice makes Bob the owner of `deal:xqgov` and Bob transfers it to Carol;
    // Bob drafts a change to it on a branch whose evidence is a note Carol wrote, and proposes it; Carol rejects
    // it with a note. The proposal's row quotes the proposed value, the decision's row her note: the forget takes
    // her ownership, her decision and the proposal (its evidence is hers) out of every file, and the entity
    // publishes freely again. Bob doesn't own it again: the forget takes the entity's whole ownership history,
    // not her row alone (with hers alone gone, his would be the latest row once more).
    let gov_note = remember(s, "?org=acme", &carol, "xqgovsource owner note").await;
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "set_entity_owner", "org": "acme", "entity": "deal:xqgov", "owner": "bob@acme.com"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let owned: Value = serde_json::from_str(&body).unwrap();
    let (status, body) = admin_as(
        s,
        &bob,
        json!({"op": "set_entity_owner", "org": "acme", "entity": "deal:xqgov", "owner": "carol@acme.com",
               "revision": owned["result"]["revision"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("carol@acme.com"), "{body}");
    store
        .create_branch(&acme, "govreview", "agent")
        .await
        .unwrap();
    let mut gov_fact = Fact::new("deal:xqgov", "owner", "xqgovvalue");
    gov_fact.source_episodes = vec![gov_note.clone()];
    store
        .commit_on(&acme, "govreview", "agent", &[], &[gov_fact])
        .await
        .unwrap();
    let (status, body) = admin_as(
        s,
        &bob,
        json!({"op": "propose_branch", "org": "acme", "branch": "govreview"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let proposed: Value = serde_json::from_str(&body).unwrap();
    let gov_proposal = proposed["result"]["proposal"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = admin_as(
        s,
        &carol,
        json!({"op": "review_proposal", "org": "acme", "proposal": gov_proposal, "decision": "reject",
               "digest": proposed["result"]["proposal"]["digest"], "note": "xqgovnote: not our policy"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""status":"rejected""#), "{body}");
    for t in ["xqgovsource", "xqgovvalue", "xqgovnote"] {
        assert!(!in_files(t).is_empty(), "{t} is stored before the forget");
    }
    // Advice traces ahead of the forget. Off until the operator turns them on (an org admin may not): Carol's hook
    // is told so and nothing is kept. On, her session's transcript is kept, tool results and all but an advisor's
    // encrypted reply; acme's admin exports it as one example per call, and neither a plain member nor another
    // org's admin may. The forget below takes it out of every file.
    let trace = json!({"session_id": "s-xqtrace", "file": "s-xqtrace", "lines": [
        {"type": "user", "uuid": "u1", "parentUuid": null, "message": {"role": "user", "content": "fix the launch"}},
        {"type": "assistant", "uuid": "a1", "parentUuid": "u1", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cat plan"}}]}},
        {"type": "user", "uuid": "u2", "parentUuid": "a1", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "xqcarolprefix launch plan"}]}},
        {"type": "assistant", "uuid": "a2", "parentUuid": "u2", "message": {"role": "assistant", "content": [
            {"type": "server_tool_use", "id": "v1", "name": "advisor", "input": {}}]}},
        {"type": "assistant", "uuid": "a3", "parentUuid": "a2", "message": {"role": "assistant", "content": [
            {"type": "advisor_tool_result", "tool_use_id": "v1", "content":
                {"type": "advisor_redacted_result", "encrypted_content": "xqencryptedadvice"}}]}},
        {"type": "assistant", "uuid": "a4", "parentUuid": "a3", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "xqcarolnextturn on it"}]}}],
        "calls": [{"kind": "advisor", "call_id": "v1", "line_uuid": "a2", "result_uuid": "a3", "status": "encrypted"}]});
    let (status, body) = post(&format!("{s}/advice-traces?org=acme"), &carol, &trace).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""off":true"#), "{body}");
    assert!(in_files("xqcarolprefix").is_empty());
    let traces_on =
        json!({"op": "set_settings", "org": "acme", "settings": {"advice_traces": true}});
    let (status, body) = admin_as(s, &alice, traces_on.clone()).await;
    assert_eq!(
        status, 403,
        "an org admin may not turn advice traces on: {body}"
    );
    admin(s, traces_on).await;
    let (status, body) = post(&format!("{s}/advice-traces?org=acme"), &carol, &trace).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""kept":true"#), "{body}");
    assert!(!in_files("xqcarolprefix").is_empty());
    assert!(
        in_files("xqencryptedadvice").is_empty(),
        "an encrypted reply is never kept"
    );
    // As the session goes on, the next part carries only what it gained (the first came as a client from before
    // sends a transcript, whole); a part sent again is kept once; the export merges a session's parts, so the second
    // ask's conversation reaches back into the first part.
    let (status, body) = post(&format!("{s}/advice-traces?org=acme"), &carol, &trace).await;
    assert_eq!(status, 200, "{body}");
    let more = json!({"session_id": "s-xqtrace", "files": [{"file": "s-xqtrace", "lines": [
        {"type": "user", "uuid": "u5", "parentUuid": "a4", "message": {"role": "user", "content": "now ship it"}},
        {"type": "assistant", "uuid": "a5", "parentUuid": "u5", "message": {"role": "assistant", "content": [
            {"type": "server_tool_use", "id": "v2", "name": "advisor", "input": {}}]}},
        {"type": "assistant", "uuid": "a6", "parentUuid": "a5", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "shipping"}]}}],
        "calls": [{"kind": "advisor", "call_id": "v2", "line_uuid": "a5", "result_uuid": "a5", "status": "encrypted"}]}]});
    let (status, body) = post(&format!("{s}/advice-traces?org=acme"), &carol, &more).await;
    assert_eq!(status, 200, "{body}");
    // Bob sends a trace under her session's id: it is his own, and neither joins nor hides hers.
    let bobs: Value =
        serde_json::from_str(&trace.to_string().replace("xqcarol", "xqbobtrace")).unwrap();
    let (status, body) = post(&format!("{s}/advice-traces?org=acme"), &bob, &bobs).await;
    assert_eq!(status, 200, "{body}");
    let export = json!({"op": "advice_traces", "org": "acme"});
    let (status, body) = admin_as(s, &alice, export.clone()).await;
    assert_eq!(status, 200, "{body}");
    let ex: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ex["examples"].as_array().map(Vec::len), Some(3), "{body}");
    let by = |who: &str, call: &str| {
        ex["examples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["author"] == who && e["call_id"] == call)
            .cloned()
    };
    assert!(by("bob@acme.com", "v1").is_some(), "{body}");
    let first = by("carol@acme.com", "v1").expect("Bob's trace hid Carol's");
    assert_eq!(first["prefix"].as_array().map(Vec::len), Some(4), "{body}");
    let second = by("carol@acme.com", "v2").expect("her second part");
    assert_eq!(second["prefix"].as_array().map(Vec::len), Some(8), "{body}");
    assert!(
        body.contains("xqcarolprefix") && body.contains("xqcarolnextturn"),
        "{body}"
    );
    for (who, what) in [(&bob, "a plain member"), (&gary, "another org's admin")] {
        let (status, body) = admin_as(s, who, export.clone()).await;
        assert_eq!(status, 403, "{what} reads acme's advice traces: {body}");
        assert!(!body.contains("xqcarolprefix"), "{body}");
    }
    // Session transcripts ahead of the forget. Off until acme's admins turn them on: Carol's hook is told so and
    // nothing is kept, and so is the hook of acme's reader key, which may keep none, rather than refused at every
    // turn; on, the key is refused. On, each part of her session's transcript is her own episode: she reads it, by
    // its id or its document, and neither Bob nor acme's admin does; a part sent again is no new version. The forget
    // below takes every part out of every file.
    let part = |from: u64, said: &str| {
        json!({"harness": "claude-code", "repo": "launch", "session_id": "s-xqscript", "from": from,
               "text": format!("Claude Code session s-xqscript in launch: its transcript from entry {from}.\n\n\
                                Claude called Bash (call 1):\n  {{\"command\":\"cat plan\"}}\n\n\
                                Result of Bash (call 1):\n  {said} launch plan\n  \n  second line")})
    };
    let (status, body) = post(
        &format!("{s}/transcripts?org=acme"),
        &carol,
        &part(0, "xqcarolran"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""off":true"#), "{body}");
    assert!(in_files("xqcarolran").is_empty());
    let key_check = json!({"check": true, "harness": "claude-code", "session_id": "s-xqkey"});
    let (status, body) = post(&format!("{s}/transcripts"), &a_key, &key_check).await;
    assert_eq!(
        status, 200,
        "a key is refused where transcripts are off: {body}"
    );
    assert!(body.contains(r#""off":true"#), "{body}");
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "set_settings", "org": "acme", "settings": {"transcripts": true}}),
    )
    .await;
    assert_eq!(status, 200, "an org admin turns transcripts on: {body}");
    let (status, body) = post(&format!("{s}/transcripts"), &a_key, &key_check).await;
    assert_eq!(status, 403, "a reader key keeps transcripts: {body}");
    let mut parts = vec![];
    for (from, said) in [(0, "xqcarolran"), (0, "xqcarolran"), (3, "xqcarolsaw")] {
        let (status, body) = post(
            &format!("{s}/transcripts?org=acme"),
            &carol,
            &part(from, said),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kept"], true, "{body}");
        parts.push(v["episode_id"].as_str().unwrap().to_string());
    }
    assert_eq!(parts[0], parts[1], "a part sent again was kept twice");
    assert!(!in_files("xqcarolran").is_empty() && !in_files("xqcarolsaw").is_empty());
    let episode = |who: &str, args: Value| {
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": "get_episode", "arguments": args}});
        let who = who.to_string();
        async move { post(&format!("{s}/mcp?org=acme"), &who, &call).await.1 }
    };
    let listing = json!({"document": "transcripts/claude-code/s-xqscript/"});
    let body = episode(&carol, listing.clone()).await;
    assert!(
        body.contains("s-xqscript/00000000") && body.contains("s-xqscript/00000003"),
        "{body}"
    );
    let body = episode(&carol, json!({"episode_id": parts[2]})).await;
    assert!(body.contains("xqcarolsaw"), "{body}");
    for (who, what) in [(&bob, "a plain member"), (&alice, "acme's admin")] {
        let body = episode(who, json!({"episode_id": parts[0]})).await;
        assert!(
            !body.contains("xqcarolran"),
            "{what} read her transcript: {body}"
        );
        let body = episode(who, listing.clone()).await;
        assert!(
            !body.contains("s-xqscript/0"),
            "{what} listed her transcript: {body}"
        );
        let body = episode(
            who,
            json!({"document": "transcripts/claude-code/s-xqscript/00000000"}),
        )
        .await;
        assert!(
            !body.contains("xqcarolran"),
            "{what} read her transcript: {body}"
        );
    }
    // Sent again with more, a part is a new version of it. Carol forgets the newest, and every version goes: neither
    // is read by its id or its document, listed or matched, exported by the operator, and sending it again keeps
    // nothing, from the same entry or from a hook that lost its marker.
    let more = part(3, "xqcarolsaw xqcarolmore");
    let (status, body) = post(&format!("{s}/transcripts?org=acme"), &carol, &more).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let newest = v["episode_id"].as_str().unwrap().to_string();
    assert_ne!(
        newest, parts[2],
        "sent with more, the part is a new version: {body}"
    );
    let part3 = json!({"document": "transcripts/claude-code/s-xqscript/00000003"});
    assert!(episode(&carol, part3.clone()).await.contains("xqcarolmore"));
    let tool = |name: &str, args: Value| {
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": name, "arguments": args}});
        let who = carol.clone();
        async move { post(&format!("{s}/mcp?org=acme"), &who, &call).await.1 }
    };
    let body = tool("forget_memory", json!({"episode_id": newest})).await;
    assert!(body.contains("and 1 other version"), "{body}");
    for args in [
        json!({"episode_id": parts[2]}),
        json!({"episode_id": newest}),
        part3.clone(),
    ] {
        let body = episode(&carol, args).await;
        assert!(
            !body.contains("xqcarolsaw") && !body.contains("xqcarolmore"),
            "a version of the part she forgot is read: {body}"
        );
    }
    let body = episode(&carol, listing.clone()).await;
    assert!(
        body.contains("s-xqscript/00000000") && !body.contains("s-xqscript/00000003"),
        "{body}"
    );
    let body = tool("forget_memory", json!({"matching": "xqcarolsaw"})).await;
    assert!(
        !body.contains(&parts[2]) && !body.contains(&newest),
        "{body}"
    );
    let (status, body) = post(&format!("{s}/transcripts?org=acme"), &carol, &more).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""forgotten":true"#) && !body.contains(r#""kept":true"#),
        "a part she forgot is kept again: {body}"
    );
    let resume = json!({"check": true, "harness": "claude-code", "session_id": "s-xqscript"});
    let (status, body) = post(&format!("{s}/transcripts?org=acme"), &carol, &resume).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""forgotten":true"#) && !body.contains(r#""from""#),
        "a hook that lost its marker would send the part she forgot again: {body}"
    );
    assert!(!episode(&carol, part3).await.contains("xqcarolsaw"));
    let forgot = dir.path().join("export-forgot");
    let (ok, out, err) = memstore(
        &home,
        &["org", "export", "acme", "--out", forgot.to_str().unwrap()],
    );
    assert!(ok, "{out}{err}");
    let exported = std::fs::read_to_string(forgot.join("org/episodes.jsonl")).unwrap();
    assert!(exported.contains("xqcarolran"), "{exported}");
    for t in ["xqcarolsaw", "xqcarolmore"] {
        assert!(
            !exported.contains(t),
            "the operator exported {t}, which she forgot"
        );
    }
    // Acme's admin is demoted as she forgets Carol: right after the directory takes Carol out of acme, before
    // anything of hers is deleted. The forget stops there, refused, and says how to finish it: Carol is out of
    // acme, nothing of hers is deleted, and no tombstone is left. Made an admin again, Alice runs it again below,
    // and it finishes.
    demote_mid_forget.store(true, Ordering::SeqCst);
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "forget_user", "user": "Carol@acme.com", "org": "acme"}),
    )
    .await;
    assert!(
        !demote_mid_forget.load(Ordering::SeqCst),
        "the forget never took carol out of acme: {body}"
    );
    assert_eq!(status, 403, "a demoted admin finished a forget: {body}");
    assert!(
        body.contains("may not forget") && body.contains("run the forget again"),
        "{body}"
    );
    for t in carol_tags.iter().chain(&["xqgovsource", "xqcarolprefix"]) {
        assert!(!in_files(t).is_empty(), "{t} deleted by a demoted admin");
    }
    assert!(
        !store
            .directory_events()
            .await
            .unwrap()
            .iter()
            .any(|e| e.op == "member.forget"
                && e.org == "acme"
                && e.user.as_deref() == Some("carol@acme.com")),
        "a demoted admin left a tombstone"
    );
    admin(
        s,
        json!({"op": "add_member", "org": "acme", "user": "alice@acme.com", "role": "admin"}),
    )
    .await;
    // Acme's admin forgets Carol.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "forget_user", "user": "Carol@acme.com", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done: Value = serde_json::from_str(&body).unwrap();
    // Her four notes, both parts of her advice trace and her session transcript's three versions of two parts (the
    // two she forgot too).
    assert_eq!(done["forgotten"]["acme"]["episodes"], 9, "{body}");
    assert_eq!(done["forgotten"]["acme"]["facts"], 3, "{body}");
    // Her ownership, her decision and the proposal built on her note are in no file; the entity is unowned, the
    // inbox doesn't list the proposal, and a change to the entity publishes without review.
    for t in [
        "xqgovsource",
        "xqgovvalue",
        "xqgovnote",
        "xqcarolprefix",
        "xqcarolnextturn",
        "xqcarolran",
        "xqcarolsaw",
        "xqcarolmore",
    ] {
        assert_eq!(in_files(t), Vec::<PathBuf>::new(), "{t} still stored");
    }
    let (status, body) = admin_as(s, &alice, export.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains("carol@acme.com") && body.contains("xqbobtrace"),
        "her advice traces outlived her, or took Bob's with them: {body}"
    );
    let (status, body) = admin_as(s, &alice, json!({"op": "entity_owners", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("deal:xqgov"), "{body}");
    let (status, body) = admin_as(s, &bob, json!({"op": "list_proposals", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains(&gov_proposal) && !body.contains("xqgov"),
        "{body}"
    );
    let free = Episode::new("note:owner", b"xqgovfree owner note", &["group:acme.all"]);
    let free_fact = Fact::new("deal:xqgov", "owner", "xqgovfree").from_episodes(&[&free]);
    store
        .commit(&acme, "test", &[free], &[free_fact])
        .await
        .unwrap();
    // Her activity is gone from what her admins read, her conversation too.
    let (status, body) = admin_as(s, &alice, conversation.clone()).await;
    assert_eq!(status, 403, "{body}");
    assert!(!body.contains("xqcarolasked"), "{body}");
    let (status, body) = admin_as(s, &alice, activity.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains("xqcarolasked") && !body.to_lowercase().contains("carol@acme.com"),
        "{body}"
    );
    let (status, body) = admin_as(s, &alice, user_facts.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("xqmixedfact"), "{body}");
    // Gone from her session's branch too.
    until(
        s,
        &on_branch,
        &alice,
        &[&branch_note],
        "her session's branch after forget",
        |r| {
            r.as_ref()
                .is_some_and(|r| !r.contains("xqcarolbranch") && r.contains("xqbobkeeps"))
        },
    )
    .await;
    // Gone from every read path, for everyone; the rest of acme's memory stays.
    until(s, "?org=acme", &bob, &acme_ids, "acme after forget", |r| {
        r.as_ref().is_some_and(|r| {
            carol_tags.iter().all(|t| !r.contains(t))
                && ["xqorgalpha", "xqbobkeeps", "xqbobfact"]
                    .iter()
                    .all(|t| r.contains(t))
        })
    })
    .await;
    until(
        s,
        "?org=acme",
        &alice,
        &acme_ids,
        "alice, acme after forget",
        |r| {
            r.as_ref().is_some_and(|r| {
                carol_tags.iter().all(|t| !r.contains(t)) && r.contains("xqbobkeeps")
            })
        },
    )
    .await;
    // Through Bob's pin, taken before the forget, Carol's memory is gone too; the rest of what it holds stays.
    until(
        s,
        &bob_pinned,
        &bob,
        &acme_ids,
        "bob's pin after forget",
        |r| {
            r.as_ref().is_some_and(|r| {
                carol_tags.iter().all(|t| !r.contains(t))
                    && ["xqorgalpha", "xqbobkeeps"].iter().all(|t| r.contains(t))
            })
        },
    )
    .await;
    // Carol no longer belongs to acme or its team.
    until(
        s,
        "?org=acme&team=a1",
        &carol,
        &a1_ids,
        "carol refused",
        |r| r.is_none(),
    )
    .await;
    // Dan joins a1 to check the team's memory: Carol's note and the fact from it are gone, the rest stays.
    admin(
        s,
        json!({"op": "add_to_team", "org": "acme", "team": "a1", "users": ["dan@acme.com"]}),
    )
    .await;
    let dan = id_token(&issuer, "dan@acme.com");
    until(
        s,
        "?org=acme&team=a1",
        &dan,
        &a1_ids,
        "a1 after forget",
        |r| {
            r.as_ref().is_some_and(|r| {
                carol_tags.iter().all(|t| !r.contains(t)) && r.contains("xqteamalpha")
            })
        },
    )
    .await;
    // Nor through her own pin of a1, read by Dan.
    until(
        s,
        &format!("?org=acme&team=a1&at={carol_pin}"),
        &dan,
        &a1_ids,
        "a1 through carol's pin after forget",
        |r| {
            r.as_ref().is_some_and(|r| {
                carol_tags.iter().all(|t| !r.contains(t)) && r.contains("xqteamalpha")
            })
        },
    )
    .await;
    // And from every file: tables, old snapshots, bodies, the search index. Her held session row too: the flush
    // after the forget doesn't bring it back. The two live pins keep none of it.
    memstore::sessions::flush().await.unwrap();
    for t in carol_tags.iter().chain(&["xqcarolsession"]) {
        assert!(
            in_files(t).is_empty(),
            "{t} still stored: {:?}",
            in_files(t)
        );
    }
    for t in acme_tags.iter().chain(&globex_tags) {
        assert!(!in_files(t).is_empty(), "{t} lost by forgetting carol");
    }
    // Globex reads as before.
    until(
        s,
        "?org=globex",
        &erin,
        &b_ids,
        "globex after forget",
        |r| {
            r.as_ref()
                .is_some_and(|r| r.contains("xqorgbeta") && r.contains("xqerinbeta"))
        },
    )
    .await;
    // The tombstone: in acme's audit, who forgot whom, with counts and no content.
    let audit = admin(
        s,
        json!({"op": "audit", "org": "acme", "kind": "change", "since_secs": 3600}),
    )
    .await;
    let tomb = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["detail"]
                .as_str()
                .unwrap_or("")
                .starts_with("forget carol@acme.com in acme")
        })
        .unwrap_or_else(|| panic!("no tombstone: {audit}"));
    assert_eq!(tomb["actor"], "alice@acme.com", "{tomb}");
    assert!(!audit.to_string().contains("xq"), "{audit}");

    // Reads are audited: acme's key and acme's people read acme above, each read a row naming the caller,
    // the memory and the episodes that came back, in acme's audit only and with none of their text.
    let reads_of = |org: &'static str| {
        let (s, a) = (s.to_string(), org);
        async move {
            admin(
                &s,
                json!({"op": "audit", "org": a, "kind": "read", "since_secs": 3600}),
            )
            .await
        }
    };
    let acme_reads = reads_of("acme").await;
    let rows = acme_reads["events"].as_array().unwrap();
    let key_read = rows
        .iter()
        .find(|e| e["actor"] == "key:acme/dash" && e["scope"] == "org:acme")
        .unwrap_or_else(|| panic!("no read row for acme's key: {acme_reads}"));
    assert!(
        key_read["episodes"]
            .as_array()
            .is_some_and(|e| !e.is_empty()),
        "{key_read}"
    );
    assert!(
        rows.iter().any(|e| e["actor"] == "bob@acme.com"),
        "bob's read: {acme_reads}"
    );
    assert!(!acme_reads.to_string().contains("xq"), "{acme_reads}");
    let globex_reads = reads_of("globex").await;
    assert!(
        !globex_reads.to_string().contains("acme"),
        "acme's reads in globex's audit: {globex_reads}"
    );
    // A caller who must be refused leaves no read row: carol, forgotten above, reads nothing of acme's.
    let (status, _) = admin_as(
        s,
        &carol,
        json!({"op": "audit", "org": "acme", "kind": "read"}),
    )
    .await;
    assert_ne!(status, 200, "a member read the audit");
    // The sink got the same events as lines of JSON, once each, and nothing of anyone's text.
    let sunk = std::fs::read_to_string(&sink_file).unwrap();
    assert!(
        sunk.lines().any(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["kind"] == "read" && v["actor"] == "key:acme/dash" && v["source"] == "memstore"
        }),
        "no read line in the sink: {sunk}"
    );
    assert!(!sunk.contains("xq"), "{sunk}");

    // A delete that stopped after the directory change: initech is gone from the directory, its memory
    // still stored. Its name can't be taken until the delete is run again, which finishes it.
    admin(
        s,
        json!({"op": "create_org", "org": "initech", "admin": "ida@initech.com"}),
    )
    .await;
    let initech_note = seed(&store, &Scope::org("initech").unwrap(), "xqinitech").await;
    let mut halfway = Event {
        at: 0,
        actor: "svc:platform".into(),
        op: "org.delete".into(),
        org: "initech".into(),
        team: None,
        user: None,
        role: None,
        detail: None,
        version: memstore::people::directory::EVENT_VERSION,
    };
    halfway.at = chrono::Utc::now().timestamp_micros();
    store.append_directory(&[halfway]).await.unwrap();
    let (status, body) = admin_as(
        s,
        ADMIN_KEY,
        json!({"op": "create_org", "org": "initech", "admin": "eve@initech.com"}),
    )
    .await;
    assert!(status == 403 && body.contains("isn't purged yet"), "{body}");
    assert!(!in_files("xqinitech").is_empty());
    // An engine from before purging plans as if the name were free: it creates initech again, with an
    // admin of its own. No server serves that org, which would read the old initech's memory.
    let mut ghost_admin =
        Event::new("svc:platform", "member.add", "initech").user("eve@initech.com");
    ghost_admin.role = Some("admin".into());
    store
        .append_directory(&[
            Event::new("svc:platform", "org.create", "initech"),
            ghost_admin,
        ])
        .await
        .unwrap();
    let eve = id_token(&issuer, "eve@initech.com");
    tokio::time::sleep(Duration::from_millis(1500)).await; // past the server's directory staleness
    let got = reads(s, "?org=initech", &eve, &[&initech_note]).await;
    assert!(
        got.is_none(),
        "an org re-created while purging was served: {got:?}"
    );
    let (ok, out, err) = memstore(&home, &["org", "delete", "initech", "--confirm", "initech"]);
    assert!(
        ok && out.contains("purge the memory of org initech"),
        "{out}{err}"
    );
    assert!(
        in_files("xqinitech").is_empty(),
        "{:?}",
        in_files("xqinitech")
    );
    // A server whose directory lagged wrote to initech after the purge. Taking the name again purges
    // that first: the new initech starts with nothing of the old one's.
    let lagging = Store::new(catalog.clone());
    seed(&lagging, &Scope::org("initech").unwrap(), "xqlatewrite").await;
    assert!(!in_files("xqlatewrite").is_empty());
    // Its audit buffer held a sign-in to initech too, flushed now: the delete took initech's rows, and none comes
    // after it.
    let initech_at = store
        .directory_events()
        .await
        .unwrap()
        .iter()
        .filter(|e| e.org == "initech" && e.op == "org.create")
        .map(|e| e.at)
        .max()
        .unwrap();
    lagging
        .append_audit(&[Entry {
            at: chrono::Utc::now().timestamp_micros(),
            kind: Kind::Signin,
            org: Some("initech".into()),
            actor: "xqlateida@initech.com".into(),
            scope: Some("initech".into()),
            path: Some("/search".into()),
            detail: None,
            status: None,
            count: 1,
            reads: 0,
            writes: 0,
            incarnation: Some(initech_at),
        }])
        .await
        .unwrap();
    assert!(
        !Store::new(catalog.clone())
            .audit_rows()
            .await
            .unwrap()
            .iter()
            .any(|e| e.org.as_deref() == Some("initech")),
        "the deleted initech's rows stored after its delete"
    );
    admin(
        s,
        json!({"op": "create_org", "org": "initech", "admin": "ivy@initech.com"}),
    )
    .await;
    assert!(
        in_files("xqlatewrite").is_empty(),
        "{:?}",
        in_files("xqlatewrite")
    );
    let ivy = id_token(&issuer, "ivy@initech.com");
    until(s, "?org=initech", &ivy, &[], "a new initech", |r| {
        r.as_ref().is_some_and(|r| !r.contains("xqlatewrite"))
    })
    .await;

    // Acme is deleted by the operator: only with its name repeated. Its commits are in the ledger until then.
    assert!(
        !ledger_with("memory_org_acme").is_empty(),
        "no ledger file names acme's scope before the delete"
    );
    let (ok, _, err) = memstore(&home, &["org", "delete", "acme"]);
    assert!(!ok && err.contains("--confirm acme"), "{err}");
    let (ok, _, err) = memstore(&home, &["org", "delete", "acme", "--confirm", "acmee"]);
    assert!(!ok && err.contains("--confirm acme"), "{err}");
    let (ok, out, err) = memstore(&home, &["org", "delete", "acme", "--confirm", "acme"]);
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("delete org acme") && out.contains("purge the memory of org acme"),
        "{out}"
    );
    // The delete is of a version an engine from before purging doesn't read: it stops there, so it can't
    // plan acme again (as if the name were free) while acme's memory is purged.
    let deletes: Vec<i64> = store
        .directory_events()
        .await
        .unwrap()
        .iter()
        .filter(|e| e.org == "acme" && e.op == "org.delete")
        .map(|e| e.version)
        .collect();
    assert_eq!(
        deletes,
        vec![memstore::people::directory::PURGE_VERSION],
        "an engine from before purging would read the delete"
    );
    const { assert!(memstore::people::directory::PURGE_VERSION > 1) };
    // Its key and its people are refused; nothing of it is readable.
    for (who, sel) in [
        (&a_key, ""),
        (&a_key, "?org=acme"),
        (&alice, "?org=acme"),
        (&bob, "?org=acme"),
        (&dan, "?org=acme&team=a1"),
    ] {
        until(s, sel, who, &acme_ids, "refused after delete", |r| {
            r.is_none()
        })
        .await;
    }
    // Its tags are in no file under the warehouse or the index.
    for t in acme_tags.iter().chain(&carol_tags) {
        assert!(
            in_files(t).is_empty(),
            "{t} still stored: {:?}",
            in_files(t)
        );
    }
    // Its rows are gone from the audit and key usage tables (read with a fresh store, no cache).
    let fresh = Store::new(catalog.clone());
    assert!(
        !fresh.key_usage().await.unwrap().contains_key(&a_key_sha),
        "acme's key usage kept"
    );
    let rows = fresh.audit_rows().await.unwrap();
    assert!(
        !rows.iter().any(|e| e.org.as_deref() == Some("acme")),
        "acme's audit rows kept"
    );
    // The delete itself is in the audit, under no org.
    assert!(
        rows.iter().any(|e| e.org.is_none()
            && e.detail
                .as_deref()
                .is_some_and(|d| d.starts_with("delete org acme"))),
        "no audit row for the delete"
    );
    // Globex is untouched: its people and key read all of it, its files are all there.
    until(
        s,
        "?org=globex",
        &erin,
        &b_ids,
        "globex after delete",
        |r| {
            r.as_ref()
                .is_some_and(|r| r.contains("xqorgbeta") && r.contains("xqerinbeta"))
        },
    )
    .await;
    until(s, "", &b_key, &b_ids, "globex's key after delete", |r| {
        r.as_ref().is_some_and(|r| r.contains("xqorgbeta"))
    })
    .await;
    for t in globex_tags {
        assert!(!in_files(t).is_empty(), "{t} lost deleting acme");
    }
    // The window compacts the ledger again after the delete (globex's commits since give it files to merge), and
    // brings none of acme's rows back: no ledger file, live or left over, names its scopes.
    let deleted_at = chrono::Utc::now().timestamp_micros();
    for t in ["xqafterdel1", "xqafterdel2", "xqafterdel3"] {
        seed(&store, &Scope::org("globex").unwrap(), t).await;
    }
    let up = Instant::now();
    while !Store::new(catalog.clone())
        .audit_rows()
        .await
        .unwrap()
        .iter()
        .any(|e| {
            e.kind == Kind::Maintenance
                && e.at >= deleted_at
                && e.org.is_none()
                && e.detail
                    .as_deref()
                    .is_some_and(|d| d.starts_with("ledger_compaction ok"))
        })
    {
        assert!(
            up.elapsed() < Duration::from_secs(60),
            "the server never compacted the ledger after the delete"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(ledger_with("memory_org_acme"), Vec::<PathBuf>::new());
    assert!(!ledger_with("memory_org_globex").is_empty());
    // The name is free again, and a new acme reads nothing of the old one.
    admin(
        s,
        json!({"op": "create_org", "org": "acme", "admin": "zed@acme.com"}),
    )
    .await;
    let zed = id_token(&issuer, "zed@acme.com");
    until(s, "?org=acme", &zed, &acme_ids, "a new acme", |r| {
        r.as_ref()
            .is_some_and(|r| acme_tags.iter().chain(&carol_tags).all(|t| !r.contains(t)))
    })
    .await;
    // A server whose directory still held the old acme flushes what it recorded for it after the purge
    // and the new acme's creation: a sign-in, a refusal and a key's use, all this hour.
    let old_acme = store
        .directory_events()
        .await
        .unwrap()
        .iter()
        .filter(|e| e.org == "acme" && e.op == "org.create")
        .map(|e| e.at)
        .min()
        .unwrap();
    let now = chrono::Utc::now().timestamp_micros();
    let late = |kind: Kind, actor: &str| Entry {
        at: now,
        kind,
        org: Some("acme".into()),
        actor: actor.into(),
        scope: Some("acme".into()),
        path: Some("/search".into()),
        detail: None,
        status: None,
        count: 1,
        reads: 1,
        writes: 0,
        incarnation: Some(old_acme),
    };
    let mut key_row = late(Kind::Key, "key:acme/dash");
    key_row.at = now.div_euclid(3_600_000_000) * 3_600_000_000;
    lagging
        .append_audit(&[
            late(Kind::Signin, "alice@acme.com"),
            late(Kind::Refusal, "bob@acme.com"),
            key_row,
        ])
        .await
        .unwrap();
    lagging
        .append_key_usage(&[(a_key_sha.clone(), "acme".into(), "dash".into(), now)])
        .await
        .unwrap();
    // None of it is stored: the old acme's delete removed its rows, and a server that hears of the delete late
    // writes it no more of them, nor its key's uses.
    let fresh = Store::new(catalog.clone());
    let stored: Vec<Entry> = fresh
        .audit_rows()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.org.as_deref() == Some("acme") && e.incarnation == Some(old_acme))
        .collect();
    assert!(
        stored.is_empty(),
        "the deleted acme's rows stored after its delete: {stored:?}"
    );
    assert!(
        !fresh.key_usage().await.unwrap().contains_key(&a_key_sha),
        "the deleted acme's key's use stored after its delete"
    );
    // A stray `org.create` in the log (no planner writes one for an org that exists; the directory keeps
    // the org as it was): the audit's incarnation is the directory's, the one its rows were stamped with.
    store
        .append_directory(&[Event::new("svc:platform", "org.create", "acme")])
        .await
        .unwrap();
    // Nor its audit: the new acme's admin sees the new org's history only, never who ran the old one,
    // whom it forgot or which keys it had (the directory's log keeps the old org's changes forever),
    // whenever a server wrote it.
    let (status, body) = admin_as(
        s,
        &zed,
        json!({"op": "audit", "org": "acme", "since_secs": 86_400}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let events: Value = serde_json::from_str(&body).unwrap();
    let events = events["events"].as_array().unwrap();
    // The new acme's own rows are there: its creation, and zed's sign-in this server recorded for it.
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "signin" && e["actor"] == "zed@acme.com"),
        "{body}"
    );
    for old in ["alice@acme.com", "carol@acme.com", "bob@acme.com", "dash"] {
        assert!(
            !body.contains(old),
            "the old acme's {old} in the new one's audit: {body}"
        );
    }
    // And the old acme's admin reads nothing of the new one's.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "audit", "org": "acme", "since_secs": 86_400}),
    )
    .await;
    assert_eq!(status, 403, "{body}");
}

/// What a person forgets leaves their profile (`memstore::profile`, the setting `profile`), and forgetting the person
/// deletes it. Carol tells her agent five things in acme, which folds profiles; then she forgets one of them
/// (`forget_memory`): the profile she is handed and reads holds the other four and nothing of it, and the version
/// folded before, which held it, is read by no one. Her notes are dreamed over (`memstore::notes_dream`) against a
/// session of hers that says her first note changed (a conflict, as the session's record is the agent's text), and
/// she is handed the conflict; she forgets the session, and the record that concluded it (which quotes the session)
/// goes with it at once: she is handed her note as she said it, and reads no record. Then acme's admin forgets Carol:
/// every version of her profile is gone
/// from every file in the warehouse and the index, with her notes and her notes dream's record, and Bob's note
/// stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_forgotten_note_leaves_its_owners_profile() {
    if !crate::own_process("offboarding::a_forgotten_note_leaves_its_owners_profile") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let warehouse = dir.path().join("warehouse");
    let index_dir = dir.path().join("index");
    std::fs::create_dir_all(&warehouse).unwrap();
    let model = crate::isolation::profile_model(String::new(), Default::default()).await;
    // SAFETY: this test runs in a process of its own (`own_process`), before any store or server.
    unsafe {
        std::env::set_var("AMPUP_MEMORY_INDEX_URI", &index_dir);
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &model);
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
        std::env::set_var("AMPUP_MEMORY_KEEP", "off");
        // The notes that bear on a session's task are picked by their words, never by a decisions model the
        // machine's keys would reach.
        std::env::set_var("AMPUP_MEMORY_DECIDE", "off");
    }
    let roots = [warehouse.as_path(), index_dir.as_path()];
    let in_files =
        |tag: &str| -> Vec<PathBuf> { roots.iter().flat_map(|r| files_with(r, tag)).collect() };
    let issuer = start_idp().await;
    let catalog = platform::local(&format!("file://{}", warehouse.display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
    let token_auth = TokenAuth::new(Some(&keys.to_string()), None)
        .unwrap()
        .allow_local_issuers()
        .with_oidc(
            OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                .await
                .unwrap(),
        );
    let port = free_port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            catalog: Some(catalog.clone()),
            token_auth: Some(token_auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while reqwest::get(s).await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}, {"user": "carol@acme.com"}]}),
        json!({"op": "set_settings", "org": "acme", "settings": {"profile": true}}),
    ] {
        admin(s, change).await;
    }
    let [alice, bob, carol] =
        ["alice@acme.com", "bob@acme.com", "carol@acme.com"].map(|u| id_token(&issuer, u));
    let tool = |bearer: &str, name: &str, args: Value| {
        let (bearer, name) = (bearer.to_string(), name.to_string());
        async move {
            let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                              "params": {"name": name, "arguments": args}});
            let (_, raw) = post(&format!("{s}/mcp?org=acme"), &bearer, &call).await;
            let v: Value = serde_json::from_str(&raw).unwrap();
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };
    // Five notes of Carol's own (the fifth folds her profile), and one of Bob's.
    let tags = [
        "xpfoldzero",
        "xpfoldone",
        "xpfoldtwo",
        "xpfoldthree",
        "xpfoldfour",
    ];
    let mut ids = vec![];
    for t in tags {
        let said = tool(
            &carol,
            "remember_memory",
            json!({"source": "notes", "body": format!("{t} preference: indent with spaces")}),
        )
        .await;
        assert!(said.contains("visible to user:carol@acme.com"), "{said}");
        ids.push(
            said.split("episode ")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .to_string(),
        );
    }
    let said = tool(
        &bob,
        "remember_memory",
        json!({"source": "notes", "body": "xpbobkeeps owner note"}),
    )
    .await;
    assert!(said.contains("visible to user:bob@acme.com"), "{said}");
    let acme = Scope::org("acme").unwrap();
    let doc = memstore::profile::document("carol@acme.com");
    let carol_reads = memstore::Principal::new("carol@acme.com", &[]);
    // Her profile as she reads it: its newest version's id and body.
    let profile = || async {
        store
            .pin(&acme, memstore::MAIN)
            .await
            .unwrap()
            .document(&store, &carol_reads, &doc, None)
            .await
            .unwrap()
            .map(|(id, _, body)| (id, String::from_utf8(body).unwrap()))
    };
    let t = Instant::now();
    let (folded, body) = loop {
        if let Some(p) = profile().await {
            break p;
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "her profile was never folded"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    for t in tags {
        assert!(body.contains(t), "{t} not in her profile: {body}");
    }

    // She forgets the third: the profile holds the others and nothing of it.
    let said = tool(&carol, "forget_memory", json!({"episode_id": ids[2]})).await;
    assert!(said.starts_with("forgot episode"), "{said}");
    let t = Instant::now();
    loop {
        let (id, body) = profile().await.expect("her profile");
        if id != folded {
            assert!(
                !body.contains("xpfoldtwo"),
                "the forgotten note in her profile: {body}"
            );
            for t in ["xpfoldzero", "xpfoldone", "xpfoldthree", "xpfoldfour"] {
                assert!(body.contains(t), "{t} left her profile: {body}");
            }
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "her profile was never repaired: {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // What a session's start hands, with what she asked, and its first ask, after a start with nothing asked.
    let task = "how should I indent this file, with spaces?";
    let handed = tool(
        &carol,
        "start_session",
        json!({"harness": "test", "context": task}),
    )
    .await;
    assert!(
        !handed.contains("xpfoldtwo") && handed.contains("xpfoldzero"),
        "{handed}"
    );
    let started = tool(&carol, "start_session", json!({"harness": "test"})).await;
    let sid = serde_json::from_str::<Value>(&started).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let handed = tool(
        &carol,
        "get_advice",
        json!({"session_id": sid, "question": task}),
    )
    .await;
    assert!(
        !handed.contains("xpfoldtwo") && handed.contains("xpfoldzero"),
        "{handed}"
    );
    let read = tool(&carol, "get_episode", json!({"document": doc})).await;
    assert!(
        !read.contains("xpfoldtwo") && read.contains("xpfoldzero"),
        "{read}"
    );
    // The version folded before, which held it, is read by no one: not her, not acme's admin.
    for bearer in [&carol, &alice] {
        let read = tool(
            bearer,
            "get_episode",
            json!({"episode_id": folded, "whole": true}),
        )
        .await;
        assert_eq!(read, "no such episode (or not visible)");
        let (_, raw) = post(
            &format!("{s}/episode?org=acme"),
            bearer,
            &json!({"calls": [[folded]]}),
        )
        .await;
        assert!(!raw.contains("xpfold"), "{raw}");
    }

    // A session of hers says her first note changed: dreamed over, she is handed the conflict, with the session's
    // words.
    let decided = crate::isolation::dream_notes(
        s,
        "?org=acme",
        &carol,
        (&store, &acme, "carol@acme.com"),
        "User: xpdreamsaid we indent with tabs now\nAssistant: noted",
    )
    .await;
    // (A conflict: an ended session's `User:` line is the agent's text.)
    let conflict = decided
        .iter()
        .find(|d| d["kind"] == "conflict")
        .unwrap_or_else(|| panic!("no conflict: {decided:?}"));
    let [dream_key, dream_id, session] =
        ["record", "episode", "session"].map(|k| conflict[k].as_str().unwrap().to_string());
    // Once the server reads past the record (its snapshot is up to a second old).
    let t = Instant::now();
    loop {
        let handed = tool(
            &carol,
            "start_session",
            json!({"harness": "test", "context": task}),
        )
        .await;
        if handed.contains("xpdreamsaid") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{handed}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // She forgets the session: what the dream concluded from it goes at once (as the server reads), and so does its
    // record.
    let said = tool(&carol, "forget_memory", json!({"episode_id": session})).await;
    assert!(said.starts_with("forgot episode"), "{said}");
    let t = Instant::now();
    loop {
        let handed = tool(
            &carol,
            "start_session",
            json!({"harness": "test", "context": task}),
        )
        .await;
        assert!(handed.contains("xpfoldzero"), "{handed}");
        if !handed.contains("xpdreamsaid") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{handed}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let read = tool(&carol, "get_episode", json!({"document": dream_key})).await;
    assert!(!read.contains("xpdreamsaid"), "{read}");

    // Acme's admin forgets Carol: every version of her profile goes from every file, with her notes.
    assert!(
        !in_files("xpfoldzero").is_empty() && !files_with(&warehouse, &dream_id).is_empty(),
        "her notes, profile and notes dream are stored"
    );
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        profile().await.is_none(),
        "her profile after she was forgotten"
    );
    for t in tags {
        assert_eq!(in_files(t), Vec::<PathBuf>::new(), "{t} still stored");
    }
    let dream_files: Vec<PathBuf> = files_with(&warehouse, &dream_id)
        .into_iter()
        .filter(|f| f.to_string_lossy().contains("/episodes/"))
        .collect();
    assert_eq!(
        dream_files,
        Vec::<PathBuf>::new(),
        "her notes dream's record"
    );
    assert!(
        !in_files("xpbobkeeps").is_empty(),
        "bob's note went with her"
    );
}

/// A fold racing a forget leaves nothing of what was forgotten (`memstore::profile`). Carol tells her agent five
/// things in acme, which folds profiles; her profile's fold reads them and is held at its model call. Acme's admin
/// forgets Carol meanwhile, and then the fold goes on: it checks her notes again right before and after it commits,
/// finds none, and writes nothing, so none of her words is in any file of the warehouse or the index. Bob's note
/// stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_fold_racing_a_forget_leaves_nothing_of_what_was_forgotten() {
    if !crate::own_process(
        "offboarding::a_fold_racing_a_forget_leaves_nothing_of_what_was_forgotten",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let warehouse = dir.path().join("warehouse");
    let index_dir = dir.path().join("index");
    std::fs::create_dir_all(&warehouse).unwrap();
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (open, gate) = tokio::sync::watch::channel(false);
    let model = crate::isolation::gated_profile_model((asked.clone(), gate)).await;
    // SAFETY: this test runs in a process of its own (`own_process`), before any store or server.
    unsafe {
        std::env::set_var("AMPUP_MEMORY_INDEX_URI", &index_dir);
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &model);
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
        std::env::set_var("AMPUP_MEMORY_KEEP", "off");
    }
    let roots = [warehouse.as_path(), index_dir.as_path()];
    let in_files =
        |tag: &str| -> Vec<PathBuf> { roots.iter().flat_map(|r| files_with(r, tag)).collect() };
    let issuer = start_idp().await;
    let catalog = platform::local(&format!("file://{}", warehouse.display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
    let token_auth = TokenAuth::new(Some(&keys.to_string()), None)
        .unwrap()
        .allow_local_issuers()
        .with_oidc(
            OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                .await
                .unwrap(),
        );
    let port = free_port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            catalog: Some(catalog.clone()),
            token_auth: Some(token_auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while reqwest::get(s).await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}, {"user": "carol@acme.com"}]}),
        json!({"op": "set_settings", "org": "acme", "settings": {"profile": true}}),
    ] {
        admin(s, change).await;
    }
    let [alice, bob, carol] =
        ["alice@acme.com", "bob@acme.com", "carol@acme.com"].map(|u| id_token(&issuer, u));
    let remember = |bearer: &str, body: String| {
        let bearer = bearer.to_string();
        async move {
            let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "remember_memory", "arguments": {"source": "notes", "body": body}}});
            let (_, raw) = post(&format!("{s}/mcp?org=acme"), &bearer, &call).await;
            assert!(raw.contains("visible to user:"), "{raw}");
        }
    };
    let tags = [
        "xpracezero",
        "xpraceone",
        "xpracetwo",
        "xpracethree",
        "xpracefour",
    ];
    for t in tags {
        remember(&carol, format!("{t} preference: indent with spaces")).await;
    }
    remember(&bob, "xpracebobkeeps owner note".to_string()).await;
    // Her fold has read her notes and waits on its model call.
    let t = Instant::now();
    while asked.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "no fold asked the model"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for t in tags {
        assert!(!in_files(t).is_empty(), "{t} is stored");
    }
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "forget_user", "user": "carol@acme.com", "org": "acme"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // The fold goes on, and has done what it would before this looks.
    open.send(true).unwrap();
    let acme = Scope::org("acme").unwrap();
    let doc = memstore::profile::document("carol@acme.com");
    let carol_reads = memstore::Principal::new("carol@acme.com", &[]);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        let session = store.pin(&acme, memstore::MAIN).await.unwrap();
        let read = session
            .document(&store, &carol_reads, &doc, None)
            .await
            .unwrap()
            .map(|(id, _, body)| (id, String::from_utf8_lossy(&body).into_owned()));
        assert!(
            read.is_none(),
            "a profile of hers after she was forgotten: {read:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    for t in tags {
        assert_eq!(
            in_files(t),
            Vec::<PathBuf>::new(),
            "{t} stored after she was forgotten"
        );
    }
    assert!(
        !in_files("xpracebobkeeps").is_empty(),
        "bob's note went with her"
    );
}
