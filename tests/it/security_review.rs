//! Holes found in hardening the server, each driven end to end on a real
//! `memstore serve` over HTTP and a local catalog, with the caller who must be refused refused:
//!
//! - a file key the keys file gives an org admin's name manages nothing: no keys, no members, no
//!   identity provider, no schema (it is a service key, as the audit and offboarding already say);
//! - a member who writes the same text as someone else's `remember_memory` episode writes an episode of
//!   their own: they can't re-ACL (hide or widen) the other author's episode;
//! - an org's admin can't name another org's `env:MEMSTORE_IDP_SECRET_…` variable as their client
//!   secret, which the server would then hand out as theirs;
//! - an org's admin can't make the server fetch from internal hosts (loopback, the private network,
//!   the cloud's metadata service) through their provider's issuer, its keys' URL or a redirect, and the
//!   refusal reads the same whether or not the host is up;
//! - an org's schema is read by its members and set only by its admins (not a member, another org's admin
//!   or a service key), over `/admin`, `/browse` and the CLI; a dry run sets nothing.

use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::Bytes;
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const CLIENT_ID: &str = "memstore-cli";
const ADMIN_KEY: &str = "msk_platform_admin_for_security_review";
/// A plain (non-admin) file key the operator's keys file gives alice's name.
const ALICE_FILE_KEY: &str = "msk_file_key_in_alices_name_for_review";
const ACME_SECRET: &str = "acme-cli-client-secret-value";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn sha(k: &str) -> String {
    auth::hex(&sha2::Sha256::digest(k.as_bytes()))
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

/// A server on a fresh local catalog: orgs `acme` (alice its admin; bob and carol members) and
/// `globex` (gary its admin). Its keys file holds the operator's admin key and a plain file key in
/// alice's name; its environment holds acme's client secret. Returns (server URL, issuer, catalog dir).
async fn start() -> (String, String, tempfile::TempDir) {
    start_with(true).await
}

/// [`start`]; `local_issuers`: orgs' providers may be on this machine (`MEMSTORE_ALLOW_LOCAL_ISSUERS`).
async fn start_with(local_issuers: bool) -> (String, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    Store::new(catalog.clone())
        .ensure_scope(&Scope::Org)
        .await
        .unwrap();
    let keys = json!([
        {"id": "platform", "sha256": sha(ADMIN_KEY), "user": "svc:platform", "admin": true},
        {"id": "alicefile", "sha256": sha(ALICE_FILE_KEY), "user": "alice@acme.com"},
    ]);
    let mut token_auth = TokenAuth::new(Some(&keys.to_string()), None).unwrap();
    if local_issuers {
        token_auth = token_auth.allow_local_issuers();
    }
    let token_auth = token_auth.with_oidc(
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
            catalog: Some(catalog),
            token_auth: Some(token_auth),
            secret_env: Some(
                [(
                    "MEMSTORE_IDP_SECRET_ACME".to_string(),
                    ACME_SECRET.to_string(),
                )]
                .into(),
            ),
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
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com"}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
    ] {
        let (status, body) = post(&s, "/admin", ADMIN_KEY, &change).await;
        assert_eq!(status, 200, "{change}: {body}");
    }
    (s, issuer, dir)
}

/// A POST with `bearer`: the HTTP status and the JSON answer.
async fn post(server: &str, path: &str, bearer: &str, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{server}{path}"))
        .bearer_auth(bearer)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_key_in_an_org_admin_s_name_manages_nothing() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");

    // The file key acts as alice where it reads (her org's memory), so it is no stranger to acme.
    let (status, me) = post(&s, "/me", ALICE_FILE_KEY, &json!({})).await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["memberships"]["acme"]["role"], "admin", "{me}");

    // But it is a service key: every change to the org is refused, whatever name the keys file gives it.
    // Each of these would hand its holder the org for good (a directory key that outlives the file key,
    // a new admin, the org's sign-in redirected to a provider of their own, the memory's schema).
    let key = format!("msk_{}backdoor", uuid::Uuid::new_v4().simple());
    let changes = [
        json!({"op": "create_key", "org": "acme", "name": "backdoor", "role": "writer", "key": key}),
        json!({"op": "invite", "org": "acme", "members": [{"user": "mallory@evil.com", "role": "admin"}]}),
        json!({"op": "add_member", "org": "acme", "user": "mallory@evil.com", "role": "admin"}),
        json!({"op": "remove_member", "org": "acme", "user": "bob@acme.com"}),
        json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc", "issuer": issuer,
               "client_ids": ["evil-cli"], "groups_claim": "groups", "required": true}}),
        json!({"op": "set_schema", "org": "acme", "schema": "sales"}),
    ];
    for change in &changes {
        let (status, body) = post(&s, "/admin", ALICE_FILE_KEY, change).await;
        assert_eq!(status, 403, "{change}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("a service key"),
            "{change}: {body}"
        );
    }
    let (_, view) = post(
        &s,
        "/admin",
        ADMIN_KEY,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    let view = view["view"].clone();
    assert!(view["keys"].get("backdoor").is_none(), "{view}");
    assert!(view["members"].get("mallory@evil.com").is_none(), "{view}");
    assert!(view["members"].get("bob@acme.com").is_some(), "{view}");
    assert!(view.get("provider").is_none(), "{view}");

    // Alice herself, signed in as a person, still runs her org.
    let (status, body) = post(&s, "/admin", &alice, &changes[0]).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_org_admin_names_only_their_own_org_s_secret_variable() {
    let (s, issuer, _dir) = start().await;
    let [alice, gary] = ["alice@acme.com", "gary@globex.com"].map(|u| id_token(&issuer, u));
    let provider = |org: &str, var: &str| {
        json!({"op": "set_provider", "org": org, "provider": {"kind": "oidc", "issuer": issuer,
               "client_ids": [format!("{org}-cli")], "client_type": "public",
               "client_secret": format!("env:{var}"),
               "groups_claim": "groups", "required": false}})
    };
    let meta = |org: &'static str| {
        let s = s.clone();
        async move {
            reqwest::get(format!(
                "{s}/.well-known/oauth-protected-resource?org={org}"
            ))
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
        }
    };

    // Gary, globex's admin, points globex's client secret at acme's variable: refused, and the server
    // hands acme's secret out under no org but acme.
    let (status, body) = post(
        &s,
        "/admin",
        &gary,
        &provider("globex", "MEMSTORE_IDP_SECRET_ACME"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not org globex's"),
        "{body}"
    );
    assert!(!meta("globex").await.contains(ACME_SECRET));

    // Acme's admin names acme's own variable, and people signing in to acme get it.
    let (status, body) = post(
        &s,
        "/admin",
        &alice,
        &provider("acme", "MEMSTORE_IDP_SECRET_ACME"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(meta("acme").await.contains(ACME_SECRET));
}

/// An org's schema (what its memory is typed with, and which episodes it reads): its members read it, only its
/// admins set it, and a dry run sets nothing. Through `/admin`, `/browse` and the CLI (`org schema`,
/// `org set-schema`), with the caller who must be refused refused each way.
#[tokio::test(flavor = "multi_thread")]
async fn an_org_s_schema_is_read_by_its_members_and_set_by_its_admins() {
    let (s, issuer, dir) = start().await;
    let [alice, bob, gary] =
        ["alice@acme.com", "bob@acme.com", "gary@globex.com"].map(|u| id_token(&issuer, u));
    let schema = |prefixes: &[&str]| {
        json!({"name": "acme", "version": 1,
               "types": {"topic": {"predicates": {"note": {"type": "string"}}}},
               "extract": {"sources": {"prefixes": prefixes}}})
    };
    let set = |s: &Schema, dry: bool| json!({"op": "set_schema", "org": "acme", "schema": s, "dry_run": dry});
    type Schema = Value;
    let (v1, v2) = (
        schema(&["doc:alpha:"]),
        schema(&["doc:alpha:", "doc:beta:"]),
    );
    let get = json!({"op": "get_schema", "org": "acme", "history": true});

    // Nobody but acme's admin sets its schema: not a member, not another org's admin, not a service key
    // in the admin's name.
    for (who, bearer) in [
        ("a member", &bob),
        ("another org's admin", &gary),
        ("a service key", &ALICE_FILE_KEY.to_string()),
    ] {
        let (status, body) = post(&s, "/admin", bearer, &set(&v1, false)).await;
        assert_eq!(status, 403, "{who} set acme's schema: {body}");
    }
    let (_, got) = post(&s, "/admin", &alice, &get).await;
    assert!(got["schema"].is_null(), "a refused set changed it: {got}");

    // The admin sets it. A dry run checks and commits nothing.
    let (status, body) = post(&s, "/admin", &alice, &set(&v1, false)).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&s, "/admin", &alice, &set(&v2, true)).await;
    assert_eq!(status, 200, "{body}");
    let (_, got) = post(&s, "/admin", &alice, &get).await;
    assert_eq!(
        got["history"].as_array().unwrap().len(),
        1,
        "a dry run set it: {got}"
    );
    let (status, body) = post(&s, "/admin", &alice, &set(&v2, false)).await;
    assert_eq!(status, 200, "{body}");

    // Members read it, with every version oldest first; someone outside the org reads none of it.
    for member in [&alice, &bob] {
        let (status, got) = post(&s, "/admin", member, &get).await;
        assert_eq!(status, 200, "{got}");
        let prefixes = |v: &Value| v["extract"]["sources"]["prefixes"].clone();
        assert_eq!(
            prefixes(&got["schema"]),
            json!(["doc:alpha:", "doc:beta:"]),
            "{got}"
        );
        let history: Vec<Value> = got["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(prefixes)
            .collect();
        assert_eq!(
            history,
            [json!(["doc:alpha:"]), json!(["doc:alpha:", "doc:beta:"])],
            "{got}"
        );
    }
    let (status, body) = post(&s, "/admin", &gary, &get).await;
    assert_eq!(
        status, 403,
        "another org's admin read acme's schema: {body}"
    );
    assert!(body.get("schema").is_none(), "{body}");

    // `/browse` shows the same to a member, and is refused to the outsider selecting acme.
    let browse = |bearer: String| {
        let s = s.clone();
        async move {
            let r = reqwest::Client::new()
                .post(format!("{s}/browse"))
                .bearer_auth(bearer)
                .header("x-memstore-org", "acme")
                .body(json!({"op": "schema"}).to_string())
                .send()
                .await
                .unwrap();
            (
                r.status().as_u16(),
                r.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };
    let (status, got) = browse(bob.clone()).await;
    assert_eq!(status, 200, "{got}");
    assert_eq!(got["history"].as_array().unwrap().len(), 2, "{got}");
    let (status, got) = browse(gary.clone()).await;
    assert!(status == 403 || status == 404, "{status}: {got}");
    assert!(got.get("schema").is_none(), "{got}");

    // The CLI, signed in with a key (a person's browser sign-in can't be scripted): the operator's reads and
    // sets; a file key in the org admin's name is a service key, refused.
    let file = dir.path().join("schema.json");
    let v3 = schema(&["doc:alpha:", "doc:beta:", "doc:gamma:"]);
    std::fs::write(&file, v3.to_string()).unwrap();
    let cli = |home: &str, args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_memstore"))
            .args(args)
            .env_remove("MEMSTORE_CALLER_IDENTITY")
            .env("MEMSTORE_CONFIG_DIR", dir.path().join(home))
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let path = file.to_str().unwrap();
    for (home, key) in [("operator", ADMIN_KEY), ("file-key", ALICE_FILE_KEY)] {
        let (ok, _, err) = cli(home, &["auth", "login", "--server", &s, "--api-key", key]);
        assert!(ok, "{home} login: {err}");
    }
    let (ok, _, _) = cli("file-key", &["org", "set-schema", "acme", path]);
    assert!(!ok, "a service key set the schema");
    let (_, got) = post(&s, "/admin", &alice, &get).await;
    assert_eq!(got["history"].as_array().unwrap().len(), 2, "{got}");
    let (ok, out, err) = cli("operator", &["org", "schema", "acme", "--history"]);
    assert!(ok, "the operator reads the schema: {err}");
    let read: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(read["history"].as_array().unwrap().len(), 2, "{out}");
    let (ok, out, err) = cli(
        "operator",
        &["org", "set-schema", "acme", path, "--dry-run"],
    );
    assert!(ok && out.starts_with("would: "), "{out}{err}");
    let (_, got) = post(&s, "/admin", &alice, &get).await;
    assert_eq!(
        got["history"].as_array().unwrap().len(),
        2,
        "the CLI's dry run set it: {got}"
    );
    let (ok, out, err) = cli("operator", &["org", "set-schema", "acme", path]);
    assert!(ok, "{out}{err}");
    let (_, got) = post(&s, "/admin", &alice, &get).await;
    assert_eq!(got["history"].as_array().unwrap().len(), 3, "{got}");
    assert_eq!(
        got["schema"]["extract"]["sources"]["prefixes"],
        json!(["doc:alpha:", "doc:beta:", "doc:gamma:"])
    );
}

/// What a refused provider change says, with the issuer it names taken out: everything else must be the
/// same, whatever the host is and whether or not it is up.
fn refusal(body: &Value, issuer: &str) -> String {
    body["error"]
        .as_str()
        .unwrap_or_default()
        .replace(issuer.trim_end_matches('/'), "<issuer>")
}

type Count = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// A listener that answers nothing and counts who connects: a host that is up.
async fn counting_listener(addr: &str) -> Option<(u16, Count)> {
    let l = tokio::net::TcpListener::bind(addr).await.ok()?;
    let port = l.local_addr().unwrap().port();
    let n = Count::default();
    let seen = n.clone();
    tokio::spawn(async move {
        while let Ok((conn, _)) = l.accept().await {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(conn);
        }
    });
    Some((port, n))
}

/// acme's admin points acme's provider at hosts only the server can reach. Each is refused
/// before anything is fetched (a host that is up sees no connection), quickly (an unreachable private
/// address doesn't wait for a timeout), and in the same words whether the host is up, down or never
/// tried, so the refusals map nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_org_admin_can_t_point_the_provider_at_internal_hosts() {
    // As deployed: no MEMSTORE_ALLOW_LOCAL_ISSUERS.
    let (s, issuer, _dir) = start_with(false).await;
    let alice = id_token(&issuer, "alice@acme.com");
    let (up, connections) = counting_listener("127.0.0.1:0").await.unwrap();
    // The same port on ::1, where the machine has IPv6.
    let up6 = counting_listener(&format!("[::1]:{up}")).await;
    let down = free_port();
    let targets = [
        format!("https://127.0.0.1:{up}"),
        format!("https://127.0.0.1:{down}"),
        format!("https://localhost:{up}"),
        format!("https://[::1]:{up}"),
        format!("https://[::1]:{down}"),
        // 127.0.0.1 written as one decimal number, in octal, in hex, and IPv4-mapped.
        format!("https://2130706433:{up}"),
        format!("https://0177.0.0.1:{up}"),
        format!("https://0x7f.1:{up}"),
        format!("https://[::ffff:127.0.0.1]:{up}"),
        format!("https://0.0.0.0:{up}"),
        format!("https://[::]:{up}"),
        // Another name in the URL's userinfo; the host is 127.0.0.1.
        format!("https://accounts.google.com@127.0.0.1:{up}"),
        "https://10.1.2.3".into(),
        "https://172.16.0.1".into(),
        "https://192.168.0.1".into(),
        "https://100.64.0.1".into(),
        "https://169.254.169.254".into(),
        "https://[::ffff:169.254.169.254]".into(),
        "https://[::ffff:a9fe:a9fe]".into(),
        "https://[fe80::1]".into(),
        "https://[fd00:ec2::254]".into(),
        "https://224.0.0.1".into(),
        // The stand-in provider itself, which works where local providers are allowed.
        issuer.clone(),
    ];
    let mut said = std::collections::BTreeSet::new();
    for target in &targets {
        let change = json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc",
            "issuer": target, "client_ids": ["acme-cli"], "groups_claim": "groups", "required": false}});
        let t = Instant::now();
        let (status, body) = post(&s, "/admin", &alice, &change).await;
        assert_eq!(status, 403, "{target}: {body}");
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{target} took {:?}: was it tried?",
            t.elapsed()
        );
        said.insert(refusal(&body, target));
    }
    assert_eq!(said.len(), 1, "the refusals differ: {said:#?}");
    let said = said.into_iter().next().unwrap();
    assert_eq!(
        said,
        format!(
            "can't use <issuer> as an identity provider: {}",
            auth::fetch::UNREADABLE
        )
    );
    assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 0);
    if let Some((_, n)) = up6 {
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    let (_, view) = post(
        &s,
        "/admin",
        ADMIN_KEY,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    assert!(view["view"].get("provider").is_none(), "{view}");
}

/// A stand-in provider whose discovery documents, under `/<case>/`, each try to send the server
/// somewhere else: through their keys' URL, or a redirect. `/good/` is a working provider, reached by a
/// redirect from `/hop/`. Returns its base URL.
async fn start_tricky_idp() -> String {
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let b = base.clone();
    tokio::spawn(async move {
        loop {
            let (conn, _) = listener.accept().await.unwrap();
            let b = b.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let b = b.clone();
                        async move {
                            let doc =
                                |jwks: &str| (200, None, json!({"issuer": "x", "jwks_uri": jwks}));
                            let to = |loc: &str| (302, Some(loc.to_string()), Value::Null);
                            let path = req
                                .uri()
                                .path()
                                .trim_end_matches("/.well-known/openid-configuration");
                            let (status, location, body) = match path {
                                "/jwks-private" => doc("https://10.0.0.1/jwks"),
                                "/jwks-metadata" => doc("http://169.254.169.254/latest/meta-data/"),
                                "/jwks-mapped" => doc("https://[::ffff:a9fe:a9fe]/jwks"),
                                // 10.0.0.1 as one decimal number.
                                "/jwks-decimal" => doc("https://167772161/jwks"),
                                "/jwks-redirect" => doc(&format!("{b}/keys-elsewhere")),
                                "/keys-elsewhere" => to("http://10.0.0.1/keys"),
                                "/redirect-metadata" => {
                                    to("http://169.254.169.254/latest/meta-data/")
                                }
                                "/redirect-private" => {
                                    to("https://10.0.0.1/.well-known/openid-configuration")
                                }
                                "/hop" => to("/good/.well-known/openid-configuration"),
                                "/good" => doc(&format!("{b}/jwks")),
                                "/jwks" => (
                                    200,
                                    None,
                                    json!({"keys": [{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256",
                                        "n": include_str!("../fixtures/idp_key.n").trim(), "e": "AQAB"}]}),
                                ),
                                _ => (404, None, Value::Null),
                            };
                            let mut r = hyper::Response::builder()
                                .status(status)
                                .header("content-type", "application/json");
                            if let Some(l) = location {
                                r = r.header("location", l);
                            }
                            Ok::<_, hyper::Error>(
                                r.body(Full::new(Bytes::from(body.to_string()))).unwrap(),
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
    base
}

/// With providers on this machine allowed (as these tests run them), a provider's own answers
/// still can't send the server to the private network or the metadata service: not through the keys'
/// URL its discovery document names (maybe on another host), nor through a redirect. Each hop is vetted,
/// so a redirect to a good place is still followed.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_s_answers_can_t_send_the_server_to_internal_hosts() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let idp = start_tricky_idp().await;
    let set = |issuer: String| {
        json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc",
            "issuer": issuer, "client_ids": ["acme-cli"], "groups_claim": "groups", "required": false}})
    };
    let mut said = std::collections::BTreeSet::new();
    for case in [
        "jwks-private",
        "jwks-metadata",
        "jwks-mapped",
        "jwks-decimal",
        "jwks-redirect",
        "redirect-metadata",
        "redirect-private",
    ] {
        let target = format!("{idp}/{case}");
        let t = Instant::now();
        let (status, body) = post(&s, "/admin", &alice, &set(target.clone())).await;
        assert_eq!(status, 403, "{case}: {body}");
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{case}: {:?}",
            t.elapsed()
        );
        said.insert(refusal(&body, &target));
    }
    assert_eq!(said.len(), 1, "the refusals differ: {said:#?}");
    let (_, view) = post(
        &s,
        "/admin",
        ADMIN_KEY,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    assert!(view["view"].get("provider").is_none(), "{view}");

    // A redirect on this machine is vetted and followed, and the provider works.
    let (status, body) = post(&s, "/admin", &alice, &set(format!("{idp}/hop"))).await;
    assert_eq!(status, 200, "{body}");
}
