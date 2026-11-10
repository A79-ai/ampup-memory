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
//!   or a service key), over `/admin`, `/browse` and the CLI; a dry run sets nothing;
//! - the keys file is read again when it changes: a key taken out stops working within seconds, a new one
//!   works, and a file that doesn't parse keeps the keys last read and says so;
//! - one bearer is one caller on every server: a directory key made with a file key's plaintext (through a
//!   server whose keys file doesn't hold it) leaves the bearer refused where the file does hold it;
//! - removing a person (from the org or a team, or forgetting them) revokes the keys imported in their name,
//!   and a key whose person a planner from before left behind reads nothing;
//! - an org that requires its own provider takes no other sign-in: not the server's provider, an app's
//!   JWT or a file key, only its provider and its own keys; and file keys aren't imported into it;
//! - a member's view of their org is themselves, the whole view is its admins';
//! - closure groups pushed for an email don't reach a sign-in through an org's own provider, which
//!   says what the email is;
//! - an email that only lower-cases to a member's by Unicode rules (a Kelvin sign for k) is not that member;
//! - a request's body is read only once its headers prove who sent it: an unknown caller is refused with the
//!   body unread, here and for an org another cell serves;
//! - a batch costs its rows in rate-limit tokens, and how many requests a caller (and the server) has under
//!   way at once is capped;
//! - an org's identity provider, which can mint any name, can't fill the audit's refusals with its names;
//! - sign-in discovery names the server's configured URL, never the request's `Host`, and a server with
//!   sign-in won't start beyond this machine without one.

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
/// File keys in bob's, carol's, dan's and erin's names (acme's members).
const BOB_FILE_KEY: &str = "msk_file_key_in_bobs_name_for_the_review";
const CAROL_FILE_KEY: &str = "msk_file_key_in_carols_name_for_the_review";
const DAN_FILE_KEY: &str = "msk_file_key_in_dans_name_for_the_review";
const ERIN_FILE_KEY: &str = "msk_file_key_in_erins_name_for_the_review";
/// The app's JWTs (`MEMSTORE_JWT_SECRET`): its users' identity, passed on.
const APP_SECRET: &str = "the-app-jwt-secret-for-the-security-review";
const APP_ISSUER: &str = "https://app.example";
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

/// A person's ID token from the stand-in identity provider, for the server's own client.
fn id_token(issuer: &str, user: &str) -> String {
    id_token_for(issuer, user, CLIENT_ID)
}

/// A person's ID token from the stand-in identity provider, issued to client `aud` (an org's own).
fn id_token_for(issuer: &str, user: &str, aud: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("k1".into());
    jsonwebtoken::encode(
        &header,
        &json!({"iss": issuer, "aud": aud, "sub": format!("sub-{user}"), "email": user,
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
    let (s, issuer, dir, _) = start_on(local_issuers).await;
    (s, issuer, dir)
}

/// [`start_with`], its options as `tune` makes them.
async fn start_tuned(
    local_issuers: bool,
    tune: impl FnOnce(Options) -> Options,
) -> (String, String, tempfile::TempDir) {
    let (s, issuer, dir, _) = start_on_tuned(local_issuers, tune).await;
    (s, issuer, dir)
}

/// [`start_with`], also returning the catalog (to start another server on).
async fn start_on(
    local_issuers: bool,
) -> (
    String,
    String,
    tempfile::TempDir,
    std::sync::Arc<dyn iceberg::Catalog>,
) {
    start_on_tuned(local_issuers, |o| o).await
}

/// [`start_on`], its options as `tune` makes them.
async fn start_on_tuned(
    local_issuers: bool,
    tune: impl FnOnce(Options) -> Options,
) -> (
    String,
    String,
    tempfile::TempDir,
    std::sync::Arc<dyn iceberg::Catalog>,
) {
    let dir = tempfile::tempdir().unwrap();
    let issuer = start_idp().await;
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    Store::new(catalog.clone())
        .ensure_scope(&Scope::Org)
        .await
        .unwrap();
    let s = serve_on(
        &catalog,
        dir.path(),
        "keys.json",
        &keys_file_rows(),
        &issuer,
        local_issuers,
        tune,
    )
    .await;
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com"},
            {"user": "dan@acme.com"}, {"user": "erin@acme.com"}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
    ] {
        let (status, body) = post(&s, "/admin", ADMIN_KEY, &change).await;
        assert_eq!(status, 200, "{change}: {body}");
    }
    (s, issuer, dir, catalog)
}

/// The keys file's rows: the operator's admin key and a plain file key in each of acme's people's names.
fn keys_file_rows() -> Value {
    json!([
        {"id": "platform", "sha256": sha(ADMIN_KEY), "user": "svc:platform", "admin": true},
        {"id": "alicefile", "sha256": sha(ALICE_FILE_KEY), "user": "alice@acme.com"},
        {"id": "bobfile", "sha256": sha(BOB_FILE_KEY), "user": "bob@acme.com"},
        {"id": "carolfile", "sha256": sha(CAROL_FILE_KEY), "user": "carol@acme.com"},
        {"id": "danfile", "sha256": sha(DAN_FILE_KEY), "user": "dan@acme.com"},
        {"id": "erinfile", "sha256": sha(ERIN_FILE_KEY), "user": "erin@acme.com"},
    ])
}

/// A server on `catalog` whose keys file (`file` in `dir`, holding `keys`, looked at every 200 ms) is its own,
/// signing people in through `issuer` and taking the app's JWTs. Returns its URL once it answers.
async fn serve_on(
    catalog: &std::sync::Arc<dyn iceberg::Catalog>,
    dir: &std::path::Path,
    file: &str,
    keys: &Value,
    issuer: &str,
    local_issuers: bool,
    tune: impl FnOnce(Options) -> Options,
) -> String {
    let path = dir.join(file);
    std::fs::write(&path, keys.to_string()).unwrap();
    let jwt = auth::JwtConfig::new(
        auth::KeySource::Secret(APP_SECRET.into()),
        auth::JwtOptions::new(APP_ISSUER, "memstore"),
    )
    .unwrap();
    let mut token_auth = TokenAuth::from_keys_file(&path, Some(jwt))
        .unwrap()
        .reload_every(Duration::from_millis(200));
    if local_issuers {
        token_auth = token_auth.allow_local_issuers();
    }
    let token_auth = token_auth.with_oidc(
        OidcConfig::discover(issuer, vec![CLIENT_ID.into()])
            .await
            .unwrap(),
    );
    let port = free_port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        tune(Options {
            catalog: Some(catalog.clone()),
            token_auth: Some(token_auth),
            secret_env: Some(
                [(
                    "MEMSTORE_IDP_SECRET_ACME".to_string(),
                    ACME_SECRET.to_string(),
                )]
                .into(),
            ),
            ..Default::default()
        }),
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
    s
}

/// An app's JWT for `user` (the app passes on who its user is).
fn app_jwt(user: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({"iss": APP_ISSUER, "aud": "memstore", "sub": user, "email": user, "exp": now + 3600}),
        &jsonwebtoken::EncodingKey::from_secret(APP_SECRET.as_bytes()),
    )
    .unwrap()
}

/// `/me` with `bearer`, selecting org `org`: its status and answer (`scope` is the scope it gets, `refused`
/// why not).
async fn me_in(server: &str, bearer: &str, org: &str) -> (u16, Value) {
    post(server, &format!("/me?org={org}"), bearer, &json!({})).await
}

/// Poll `check` every 100 ms until it holds, for at most 10 s; whether it did.
async fn within<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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

/// The keys file is read again when it changes, on a running server: a key taken out of it is refused within
/// seconds, one put in works, and a file that doesn't parse (or is gone) keeps the keys last read, with
/// `memstore_keys_file_failing` at 1 until it reads again.
#[tokio::test(flavor = "multi_thread")]
async fn the_keys_file_is_read_again_when_it_changes() {
    let (s, _issuer, dir) = start().await;
    let path = dir.path().join("keys.json");
    let me_status = |bearer: &'static str| {
        let s = s.clone();
        async move { post(&s, "/me", bearer, &json!({})).await.0 }
    };
    assert_eq!(me_status(BOB_FILE_KEY).await, 200);
    const NEW_KEY: &str = "msk_a_key_put_in_the_file_while_serving";
    assert_eq!(me_status(NEW_KEY).await, 401);

    // Bob's key taken out, a new one put in: no restart.
    let mut rows: Vec<Value> = keys_file_rows()
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["id"] != "bobfile")
        .cloned()
        .collect();
    rows.push(json!({"id": "newfile", "sha256": sha(NEW_KEY), "user": "carol@acme.com"}));
    std::fs::write(&path, json!(rows).to_string()).unwrap();
    assert!(
        within(|| async { me_status(BOB_FILE_KEY).await == 401 }).await,
        "a key taken out of the keys file still works"
    );
    assert!(
        within(|| async { me_status(NEW_KEY).await == 200 }).await,
        "a key put in the keys file doesn't work"
    );

    // Half-written, then gone: the keys last read stay, and the server says the file is failing.
    let failing = || {
        let s = s.clone();
        async move {
            let r = reqwest::Client::new()
                .get(format!("{s}/metrics"))
                .bearer_auth(ADMIN_KEY)
                .send()
                .await
                .unwrap();
            r.text()
                .await
                .unwrap()
                .lines()
                .any(|l| l == "memstore_keys_file_failing 1")
        }
    };
    for broken in [Some("[{\"id\": \"platform\", \"sha256\": "), None] {
        match broken {
            Some(text) => std::fs::write(&path, text).unwrap(),
            None => std::fs::remove_file(&path).unwrap(),
        }
        assert!(within(failing).await, "a broken keys file isn't reported");
        assert_eq!(
            me_status(NEW_KEY).await,
            200,
            "a broken keys file dropped the keys"
        );
        assert_eq!(
            me_status(BOB_FILE_KEY).await,
            401,
            "a broken keys file brought a key back"
        );
    }
    std::fs::write(&path, json!(rows).to_string()).unwrap();
    assert!(
        within(|| async { !failing().await }).await,
        "the keys file reads again, but is still reported failing"
    );
}

/// A bearer is one caller on every server. Where the keys file holds a key, no directory key is made with it.
/// One made anyway, through a server whose keys file doesn't hold it, is that directory key there; where the
/// keys file holds it, the bearer then names two callers, and is refused rather than served as the file key.
#[tokio::test(flavor = "multi_thread")]
async fn one_bearer_is_one_caller_on_every_server() {
    let (s, issuer, dir, catalog) = start_on(true).await;
    let alice = id_token(&issuer, "alice@acme.com");
    let create = |key: &str| json!({"op": "create_key", "org": "acme", "name": "twin", "role": "reader", "key": key});
    // Where the keys file holds it (an admin key above all), it isn't made.
    for key in [CAROL_FILE_KEY, ADMIN_KEY] {
        let (status, body) = post(&s, "/admin", &alice, &create(key)).await;
        assert_eq!(status, 403, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("already in use"),
            "{body}"
        );
    }
    let (_, me) = me_in(&s, CAROL_FILE_KEY, "acme").await;
    assert_eq!(me["user"], "carol@acme.com", "{me}");

    // A second server on the same catalog, whose keys file holds the admin key only: there, carol's file key
    // is nobody's, and alice makes a directory key of it.
    let other = serve_on(
        &catalog,
        dir.path(),
        "other-keys.json",
        &json!([{"id": "platform", "sha256": sha(ADMIN_KEY), "user": "svc:platform", "admin": true}]),
        &issuer,
        true,
        |o| o,
    )
    .await;
    assert_eq!(post(&other, "/me", CAROL_FILE_KEY, &json!({})).await.0, 401);
    let (status, body) = post(&other, "/admin", &alice, &create(CAROL_FILE_KEY)).await;
    assert_eq!(status, 200, "{body}");
    let (_, me) = me_in(&other, CAROL_FILE_KEY, "acme").await;
    assert_eq!(me["user"], "key:acme/twin", "{me}");

    // Where the keys file holds it, the bearer is now two callers: neither (once the first server sees the
    // key, within its 5 s cache), never carol there and the key here.
    assert!(
        within(|| async { post(&s, "/me", CAROL_FILE_KEY, &json!({})).await.0 == 401 }).await,
        "a bearer that is a file key here and a directory key there is served as the file key here"
    );
    let (status, me) = me_in(&s, CAROL_FILE_KEY, "acme").await;
    assert_eq!(status, 401, "{me}");

    // Revoked, the file key is the file's again.
    let (status, body) = post(
        &other,
        "/admin",
        &alice,
        &json!({"op": "revoke_key", "org": "acme", "name": "twin"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        within(|| async { me_in(&s, CAROL_FILE_KEY, "acme").await.1["user"] == "carol@acme.com" })
            .await,
        "the file key stayed refused after the directory key was revoked"
    );
}

/// A file key imported into the directory reads no more than its person does. Removing them from the org, or
/// from the team the key reads, or forgetting them, revokes it in the same commit (and its file twin with it);
/// and a key whose person a planner from before this left behind (a removal with no revoke) reads nothing.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_person_revokes_the_keys_imported_in_their_name() {
    let (s, issuer, _dir, catalog) = start_on(true).await;
    let alice = id_token(&issuer, "alice@acme.com");
    for change in [
        json!({"op": "create_team", "org": "acme", "team": "deals"}),
        json!({"op": "add_to_team", "org": "acme", "team": "deals", "users": ["carol@acme.com"]}),
    ] {
        let (status, body) = post(&s, "/admin", &alice, &change).await;
        assert_eq!(status, 200, "{change}: {body}");
    }
    // Each person's file key imports itself: carol's into her team, the others' into the org.
    for (key, team) in [
        (BOB_FILE_KEY, None),
        (CAROL_FILE_KEY, Some("deals")),
        (DAN_FILE_KEY, None),
        (ERIN_FILE_KEY, None),
    ] {
        let (status, body) = post(
            &s,
            "/admin",
            key,
            &json!({"op": "import_keys", "org": "acme", "team": team, "role": "reader", "self": true}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    // A key selecting nothing gets its own scope.
    let scope_of = |key: &'static str| {
        let s = s.clone();
        async move { post(&s, "/me", key, &json!({})).await.1["scope"].clone() }
    };
    assert_eq!(scope_of(BOB_FILE_KEY).await, "org:acme");
    assert_eq!(scope_of(CAROL_FILE_KEY).await, "team:acme/deals");
    let keys = || {
        let s = s.clone();
        async move {
            let (_, v) = post(
                &s,
                "/admin",
                ADMIN_KEY,
                &json!({"op": "show", "org": "acme"}),
            )
            .await;
            v["view"]["keys"]
                .as_object()
                .map(|k| k.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        }
    };
    assert_eq!(
        keys().await,
        ["bobfile", "carolfile", "danfile", "erinfile"]
    );

    // Bob out of the org, carol out of her team, dan forgotten: their keys go in the same change.
    for (change, gone) in [
        (
            json!({"op": "remove_member", "org": "acme", "user": "bob@acme.com"}),
            "bobfile",
        ),
        (
            json!({"op": "remove_from_team", "org": "acme", "team": "deals", "users": ["carol@acme.com"]}),
            "carolfile",
        ),
        (
            json!({"op": "forget_user", "org": "acme", "user": "dan@acme.com"}),
            "danfile",
        ),
    ] {
        let (status, body) = post(&s, "/admin", &alice, &change).await;
        assert_eq!(status, 200, "{change}: {body}");
        assert!(
            body["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap().contains(&format!("revoke key {gone}"))),
            "{change} left the key imported in their name: {body}"
        );
        assert!(!keys().await.contains(&gone.to_string()), "{change}");
    }
    // Refused (within the 5 s key cache), the file twin too: the revoke retired its hash, so it is not read
    // as the person any more either.
    for key in [BOB_FILE_KEY, CAROL_FILE_KEY, DAN_FILE_KEY] {
        assert!(
            within(|| async { post(&s, "/me", key, &json!({})).await.0 == 401 }).await,
            "a key imported in a removed person's name still signs in"
        );
    }
    let (status, body) = post(
        &s,
        "/admin",
        BOB_FILE_KEY,
        &json!({"op": "import_keys", "org": "acme", "role": "reader", "self": true}),
    )
    .await;
    assert_eq!(status, 401, "a revoked key imported itself again: {body}");

    // Erin removed by a planner from before this (a removal, and no revoke): her key reads nothing.
    Store::new(catalog)
        .append_directory_with(|_| {
            Ok(vec![
                memstore::people::directory::Event::new("old-cli", "member.remove", "acme")
                    .user("erin@acme.com"),
            ])
        })
        .await
        .unwrap();
    assert!(
        within(|| async { scope_of(ERIN_FILE_KEY).await.is_null() }).await,
        "a key imported in a removed person's name still reads the org"
    );
    let (_, me) = post(&s, "/me", ERIN_FILE_KEY, &json!({})).await;
    assert!(me["memberships"].as_object().unwrap().is_empty(), "{me}");
}

/// An org that requires its own identity provider takes no other way in, a person's or a service's: not the
/// server's provider, not an app's JWT, not a file key in a member's name. Its provider and its own keys get
/// in; a file key isn't imported into it. Its members see themselves in its view, its admins the whole of it.
#[tokio::test(flavor = "multi_thread")]
async fn an_org_that_requires_its_provider_takes_no_other_sign_in() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let carol = id_token(&issuer, "carol@acme.com");
    // Acme's own provider: another issuer.
    let acme_idp = start_idp().await;
    let carol_own = id_token_for(&acme_idp, "carol@acme.com", "acme-cli");
    let carol_app = app_jwt("carol@acme.com");
    let provider = |required: bool| {
        json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc", "issuer": acme_idp,
               "client_ids": ["acme-cli"], "groups_claim": "groups", "required": required}})
    };
    let (status, body) = post(&s, "/admin", &alice, &provider(false)).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &s,
        "/admin",
        &alice,
        &json!({"op": "create_key", "org": "acme", "name": "svc", "role": "reader",
                "key": "msk_acmes_own_reader_key_for_the_review_x"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ways: [(&str, &str); 5] = [
        ("the server's provider", &carol),
        ("an app's JWT", &carol_app),
        ("a file key", CAROL_FILE_KEY),
        ("acme's provider", &carol_own),
        (
            "acme's own key",
            "msk_acmes_own_reader_key_for_the_review_x",
        ),
    ];
    // Not required: every way in reads acme.
    for (way, bearer) in ways {
        let (status, me) = me_in(&s, bearer, "acme").await;
        assert_eq!(status, 200, "{way}: {me}");
        assert!(me["scope"].is_string(), "{way}: {me}");
    }

    // Required: its provider and its keys only.
    let (status, body) = post(&s, "/admin", &alice, &provider(true)).await;
    assert_eq!(status, 200, "{body}");
    for (way, bearer) in ways {
        let (status, me) = me_in(&s, bearer, "acme").await;
        assert_eq!(status, 200, "{way}: {me}");
        let gets_in = matches!(way, "acme's provider" | "acme's own key");
        assert_eq!(me["scope"].is_string(), gets_in, "{way}: {me}");
        if !gets_in {
            assert!(
                me["refused"]
                    .as_str()
                    .unwrap()
                    .contains("own identity provider only"),
                "{way}: {me}"
            );
        }
    }
    // A file key isn't imported into it: the import would read more than the key does.
    let (status, body) = post(
        &s,
        "/admin",
        CAROL_FILE_KEY,
        &json!({"op": "import_keys", "org": "acme", "role": "reader", "self": true}),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("own identity provider only"),
        "{body}"
    );

    // The org's view: a member (through its provider) sees themselves; its admin the whole org.
    let alice_own = id_token_for(&acme_idp, "alice@acme.com", "acme-cli");
    let show = json!({"op": "show", "org": "acme"});
    let (status, body) = post(&s, "/admin", &carol_own, &show).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["view"]["members"],
        json!({"carol@acme.com": "member"}),
        "{body}"
    );
    for hidden in ["keys", "provider", "domains", "pending"] {
        assert!(body["view"].get(hidden).is_none(), "{hidden}: {body}");
    }
    let (status, body) = post(&s, "/admin", &alice_own, &show).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["view"]["members"]["bob@acme.com"].is_string(),
        "{body}"
    );
    assert!(body["view"]["keys"]["svc"].is_object(), "{body}");
}

/// Closure groups (`/closure`) are pushed for an email. A sign-in through an org's own provider, which says
/// what the email is, doesn't get them: only one through the server's provider (or a key, an app's JWT, a
/// gateway) does.
#[tokio::test(flavor = "multi_thread")]
async fn closure_groups_don_t_reach_a_sign_in_through_an_org_s_own_provider() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let acme_idp = start_idp().await;
    let (status, body) = post(
        &s,
        "/admin",
        &alice,
        &json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc", "issuer": acme_idp,
               "client_ids": ["acme-cli"], "groups_claim": "groups"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // Gary's groups, pushed for his email. Acme's provider could put that email in a token of its own.
    let (status, body) = post(
        &s,
        "/closure",
        ADMIN_KEY,
        &json!({"user": "gary@globex.com", "add": ["group:board"], "remove": []}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let groups = |bearer: String| {
        let s = s.clone();
        async move {
            let (status, me) = post(&s, "/me", &bearer, &json!({})).await;
            assert_eq!(status, 200, "{me}");
            me["groups"].clone()
        }
    };
    let held = groups(id_token(&issuer, "gary@globex.com")).await;
    assert!(
        held.as_array().unwrap().contains(&json!("group:board")),
        "{held}"
    );
    let posed = groups(id_token_for(&acme_idp, "gary@globex.com", "acme-cli")).await;
    assert!(
        !posed.as_array().unwrap().contains(&json!("group:board")),
        "acme's provider got gary's pushed groups: {posed}"
    );
}

/// User ids fold ASCII letters only: an email that only Unicode case rules fold into a member's (a Kelvin sign
/// for k) signs in as someone else, who is not that member.
#[tokio::test(flavor = "multi_thread")]
async fn an_email_that_only_folds_into_a_members_is_not_that_member() {
    let (s, issuer, _dir) = start().await;
    let (status, me) = me_in(&s, &id_token(&issuer, "BOB@ACME.COM"), "acme").await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(
        me["scope"], "org:acme",
        "an upper-case email is the member: {me}"
    );
    let (status, body) = post(
        &s,
        "/admin",
        ADMIN_KEY,
        &json!({"op": "add_member", "org": "acme", "user": "kim@acme.com"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // U+212A KELVIN SIGN lower-cases to a plain k by Unicode rules.
    for spoof in ["\u{212A}im@acme.com", "\u{212A}IM@ACME.COM"] {
        let (status, me) = me_in(&s, &id_token(&issuer, spoof), "acme").await;
        assert_eq!(status, 200, "{me}");
        assert!(me["scope"].is_null(), "{spoof:?} signed in as kim: {me}");
        assert!(
            me["memberships"].as_object().unwrap().is_empty(),
            "{spoof:?}: {me}"
        );
    }
    let (_, me) = me_in(&s, &id_token(&issuer, "Kim@Acme.com"), "acme").await;
    assert_eq!(me["scope"], "org:acme", "{me}");
}

/// A request written by hand: `head` (its request line and headers, ending in a blank line), then `sent`, the
/// start of a body that never ends. What the server answers within `wait` (its status line and headers), or
/// None; the connection stays open until then, so the server is still waiting for the rest.
async fn answer_to(server: &str, head: &str, sent: &[u8], wait: Duration) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut c = tokio::net::TcpStream::connect(server.trim_start_matches("http://"))
        .await
        .unwrap();
    c.write_all(head.as_bytes()).await.unwrap();
    c.write_all(sent).await.unwrap();
    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(wait, c.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// A request's head: `POST path` with `content-length: len` and these headers.
fn head(server: &str, path: &str, len: usize, headers: &[(&str, &str)]) -> String {
    let host = server.trim_start_matches("http://");
    let mut h = format!(
        "POST {path} HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\ncontent-length: {len}\r\n"
    );
    for (k, v) in headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    h
}

/// A request's head that is still sending its body (it never ends) keeps `server` waiting: the connection,
/// held open, is a request under way.
async fn under_way(server: &str, path: &str, headers: &[(&str, &str)]) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let mut c = tokio::net::TcpStream::connect(server.trim_start_matches("http://"))
        .await
        .unwrap();
    c.write_all(head(server, path, 1000, headers).as_bytes())
        .await
        .unwrap();
    c.write_all(b"{").await.unwrap();
    c
}

/// A `/search` of `rows` queries in acme, as `bearer`: the status and the `retry-after` header.
async fn search_rows(server: &str, bearer: &str, rows: usize) -> (u16, Option<String>) {
    let data: Vec<Value> = (0..rows).map(|i| json!([i, "who owns billing"])).collect();
    let r = reqwest::Client::new()
        .post(format!("{server}/search"))
        .bearer_auth(bearer)
        .header("x-memstore-org", "acme")
        .json(&json!({ "data": data }))
        .send()
        .await
        .unwrap();
    let retry = r
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    (r.status().as_u16(), retry)
}

/// Nobody's body is read before their headers prove who they are: a POST that says it carries most of a
/// megabyte (most of eight on `/ingest`) and never sends it is refused at once without a credential, with a key
/// nobody holds or with a forged token, while a signed-in member's same request is waited for.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_is_read_only_once_its_sender_is_known() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let forged = id_token("http://127.0.0.1:9/not-an-issuer", "alice@acme.com");
    let unknown = [
        vec![],
        vec![("authorization", "Bearer msk_no_one_holds_this_key_at_all")],
        vec![("authorization", format!("Bearer {forged}").leak() as &str)],
    ];
    for (path, len) in [
        ("/search", 900_000),
        ("/mcp", 900_000),
        ("/ingest", 7_000_000),
    ] {
        for headers in &unknown {
            let got = answer_to(
                &s,
                &head(&s, path, len, headers),
                b"{\"data\": [",
                Duration::from_secs(5),
            )
            .await;
            let got =
                got.unwrap_or_else(|| panic!("{path} {headers:?}: the server waited for the body"));
            assert!(got.starts_with("HTTP/1.1 401"), "{path} {headers:?}: {got}");
        }
    }
    // A member is known from her headers: the server takes her body.
    let bearer = format!("Bearer {alice}");
    let got = answer_to(
        &s,
        &head(
            &s,
            "/search",
            900_000,
            &[("authorization", &bearer), ("x-memstore-org", "acme")],
        ),
        b"{\"data\": [",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(got, None, "a member's body was not waited for");
}

/// A batch costs its rows: with five requests a minute, a `/search` of five queries spends alice's minute and
/// her next query waits, while bob's bucket is his own.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_costs_a_token_per_query() {
    let (s, issuer, _dir) = start_tuned(true, |o| Options {
        rate_limit: Some("5/min".into()),
        ..o
    })
    .await;
    let alice = id_token(&issuer, "alice@acme.com");
    let bob = id_token(&issuer, "bob@acme.com");
    let (status, _) = search_rows(&s, &alice, 5).await;
    assert_eq!(status, 200);
    let (status, retry) = search_rows(&s, &alice, 1).await;
    assert_eq!(status, 429, "five queries cost one token");
    assert!(retry.is_some_and(|r| r.parse::<u64>().unwrap() >= 1));
    // A batch larger than a whole bucket still gets through once, at the bucket's whole cost.
    let (status, _) = search_rows(&s, &bob, 50).await;
    assert_eq!(status, 200);
    let (status, _) = search_rows(&s, &bob, 1).await;
    assert_eq!(status, 429);
}

/// How much is under way at once is capped, each refusal answered at once with when to retry: two requests per
/// caller (a third is 429) and three on the server (a fourth is 503), the readiness probe always answered. A
/// body for an org another cell serves is read here only once its sender is known, too: an unknown sender is
/// refused without that cell hearing of it.
#[tokio::test(flavor = "multi_thread")]
async fn requests_under_way_are_capped_and_another_cell_s_body_waits_for_its_sender() {
    if !crate::own_process(
        "security_review::requests_under_way_are_capped_and_another_cell_s_body_waits_for_its_sender",
    ) {
        return;
    }
    let (far, heard) = counting_listener("127.0.0.1:0").await.unwrap();
    // SAFETY: this test runs in a process of its own (`own_process`), before the server reads them.
    unsafe {
        std::env::set_var("MEMSTORE_MAX_IN_FLIGHT_PER_CALLER", "2");
        std::env::set_var("MEMSTORE_MAX_IN_FLIGHT", "3");
        std::env::set_var("MEMSTORE_CELL_MAP", r#"{"farco": "far"}"#);
        std::env::set_var(
            "MEMSTORE_CELL_URLS",
            format!(r#"{{"far": "http://127.0.0.1:{far}"}}"#),
        );
    }
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let bob = id_token(&issuer, "bob@acme.com");
    let carol = id_token(&issuer, "carol@acme.com");
    let as_alice = format!("Bearer {alice}");
    let alices = [
        ("authorization", as_alice.as_str()),
        ("x-memstore-org", "acme"),
    ];

    let held = [
        under_way(&s, "/search", &alices).await,
        under_way(&s, "/search", &alices).await,
    ];
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, retry) = search_rows(&s, &alice, 1).await;
    assert_eq!(status, 429, "a third request of alice's at once");
    assert_eq!(retry.as_deref(), Some("1"));
    let (status, _) = search_rows(&s, &bob, 1).await;
    assert_eq!(status, 200, "bob's requests are his own");

    let as_bob = format!("Bearer {bob}");
    let _bobs = under_way(
        &s,
        "/search",
        &[("authorization", &as_bob), ("x-memstore-org", "acme")],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, retry) = search_rows(&s, &carol, 1).await;
    assert_eq!(status, 503, "a fourth request on the server at once");
    assert_eq!(retry.as_deref(), Some("1"));
    let ready: Value = reqwest::get(format!("{s}/ready"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        ready.get("ready").is_some(),
        "the probe is answered whatever is under way: {ready}"
    );

    drop(held);
    drop(_bobs);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, _) = search_rows(&s, &alice, 1).await;
    assert_eq!(status, 200, "once alice's requests are done");

    // Farco is another cell's: a body for it from nobody known is refused here, and that cell never hears of it.
    let got = answer_to(
        &s,
        &head(&s, "/search", 900_000, &[("x-memstore-org", "farco")]),
        b"{\"data\": [",
        Duration::from_secs(5),
    )
    .await
    .expect("the server waited for the body of a request for another cell");
    assert!(got.starts_with("HTTP/1.1 401"), "{got}");
    assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Acme's own identity provider can sign in any name it likes, and each one it signs in that acme doesn't hold
/// is refused and audited (under no org: they aren't acme's). Past a hundred of them, the rest are counted in one
/// row under the provider, so its names can't take the audit's room for refusals from anyone else's: an
/// outsider's refusal is still there, and so is bob's in acme's own audit.
#[tokio::test(flavor = "multi_thread")]
async fn an_org_s_provider_can_t_fill_the_audit_with_names_it_mints() {
    let (s, issuer, _dir) = start().await;
    let alice = id_token(&issuer, "alice@acme.com");
    let bob = id_token(&issuer, "bob@acme.com");
    let acme_idp = start_idp().await;
    let (status, body) = post(
        &s,
        "/admin",
        &alice,
        &json!({"op": "set_provider", "org": "acme", "provider": {"kind": "oidc", "issuer": acme_idp,
                "client_ids": [CLIENT_ID], "groups_claim": "groups"}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for i in 0..150 {
        let minted = id_token(&acme_idp, &format!("minted{i}@acme.com"));
        let (status, _) = search_rows(&s, &minted, 1).await;
        assert_eq!(status, 403, "minted{i}");
    }
    let dave = id_token(&issuer, "dave@elsewhere.com");
    let (status, _) = search_rows(&s, &dave, 1).await;
    assert_eq!(status, 403);
    let (status, body) = post(
        &s,
        "/admin",
        &bob,
        &json!({"op": "create_key", "org": "acme", "name": "bobs", "role": "reader"}),
    )
    .await;
    assert_eq!(status, 403, "{body}");

    let refusals_in = |audit: &Value| -> Vec<Value> {
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "refusal")
            .cloned()
            .collect()
    };
    let (status, acme) = post(&s, "/admin", &alice, &json!({"op": "audit", "org": "acme"})).await;
    assert_eq!(status, 200, "{acme}");
    assert!(
        refusals_in(&acme)
            .iter()
            .any(|e| e["actor"] == "bob@acme.com"),
        "{acme}"
    );
    let (status, audit) = post(
        &s,
        "/admin",
        ADMIN_KEY,
        &json!({"op": "audit", "operator": true}),
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let refusals = refusals_in(&audit);
    let minted = refusals
        .iter()
        .filter(|e| e["actor"].as_str().unwrap().starts_with("minted"))
        .count();
    assert_eq!(minted, 100, "{refusals:?}");
    let pooled: i64 = refusals
        .iter()
        .filter(|e| e["actor"] == "idp:acme")
        .map(|e| e["count"].as_i64().unwrap())
        .sum();
    assert_eq!(pooled, 50, "{refusals:?}");
    assert!(
        refusals.iter().any(|e| e["actor"] == "dave@elsewhere.com"),
        "{refusals:?}"
    );
}

/// Sign-in discovery names the URL the server was given, never the one a request's `Host` claims, and a
/// server with sign-in won't listen beyond this machine without being given one (MEMSTORE_PUBLIC_URL).
#[tokio::test(flavor = "multi_thread")]
async fn sign_in_discovery_never_names_the_caller_s_host() {
    let (s, _issuer, _dir) = start().await;
    let port = s.rsplit(':').next().unwrap();
    let forged = "GET /.well-known/oauth-protected-resource HTTP/1.1\r\nhost: evil.example\r\n\r\n";
    let got = answer_to(&s, forged, b"", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        got.contains(&format!("http://127.0.0.1:{port}/mcp")),
        "{got}"
    );
    assert!(!got.contains("evil.example"), "{got}");
    let refused = head(&s, "/mcp", 2, &[]).replacen(
        &format!("host: 127.0.0.1:{port}"),
        "host: evil.example",
        1,
    );
    let got = answer_to(&s, &refused, b"{}", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(got.starts_with("HTTP/1.1 401"), "{got}");
    assert!(
        got.contains(&format!(
            "resource_metadata=\"http://127.0.0.1:{port}/.well-known/oauth-protected-resource\""
        )),
        "{got}"
    );
    assert!(!got.contains("evil.example"), "{got}");

    // Beyond this machine, a server with sign-in and no URL of its own refuses to start.
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let keys = json!([{"id": "platform", "sha256": sha(ADMIN_KEY), "user": "svc:platform", "admin": true}]);
    let started = tokio::time::timeout(
        Duration::from_secs(20),
        serve_with(
            free_port(),
            Scope::Org,
            vec![],
            Options {
                catalog: Some(catalog),
                token_auth: Some(TokenAuth::new(Some(&keys.to_string()), None).unwrap()),
                bind: Some("0.0.0.0".into()),
                ..Default::default()
            },
        ),
    )
    .await;
    let err = match started {
        Ok(Err(e)) => format!("{e:#}"),
        Ok(Ok(())) => panic!("the server stopped without an error"),
        Err(_) => {
            panic!("a server with sign-in listened on every interface without MEMSTORE_PUBLIC_URL")
        }
    };
    assert!(err.contains("MEMSTORE_PUBLIC_URL"), "{err}");
}
