//! `/retract` in an org's memory, end to end: a real `memstore serve` over HTTP on a local catalog, every caller
//! selecting an org (`MEMSTORE_DIRECTORY=required`), two orgs (`acme`, admin alice and member bob, and `globex`),
//! each with its connector's `ingest` key.
//!
//! The rule (`/retract`): it acts in the org the request selects, as reads do. The operator's admin key and the
//! org's admins retract anything there, by prefix too. The org's ingest key retracts in its own memory only what
//! it could write: one exact source (or its other versions, `except` the current one), or one episode that is
//! there, never under a source the server owns, never by prefix, and it is told only how many. A member, a writer
//! or reader key and another org's key are refused, and change nothing. Facts an episode alone supports go with
//! it; facts another episode wrote stay. A delete is for good: a deleted version sent again never comes back,
//! also one a newer version had only retired (`replaces_source`), which sent again alone would.

use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::Bytes;
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Principal, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const ADMIN_KEY: &str = "msk_platform_admin_for_org_retract";

const CLIENT_ID: &str = "memstore-cli";

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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
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

/// A POST to `url` with `bearer` and these headers: the status and the answer.
async fn post(url: &str, bearer: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Value) {
    let mut r = CLIENT.post(url).bearer_auth(bearer).body(body.to_string());
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or_default())
}

/// One episode as a connector sends it.
fn batch(source: &str, body: &str, acl: &[&str]) -> Value {
    json!({"episodes": [{"source": source, "occurred_at": 1_760_000_000_000_000i64, "acl": acl, "body": body}]})
}

#[tokio::test(flavor = "multi_thread")]
async fn an_org_retracts_its_own_episodes_and_nobody_elses() {
    let issuer = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store = Store::new(catalog.clone());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
    let auth = TokenAuth::new(Some(&keys.to_string()), None)
        .unwrap()
        .allow_local_issuers()
        .with_oidc(
            OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                .await
                .unwrap(),
        );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            listener: Some(listener),
            catalog: Some(catalog.clone()),
            token_auth: Some(auth),
            directory_only: true,
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
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
    ] {
        let (status, body) = post(&format!("{s}/admin"), ADMIN_KEY, &[], &change).await;
        assert_eq!(status, 200, "{change}: {body}");
    }
    let key = async |org: &str, name: &str, role: &str| {
        let k = format!("msk_{}{name}", uuid::Uuid::new_v4().simple());
        let change = json!({"op": "create_key", "org": org, "name": name, "role": role, "key": k});
        let (status, body) = post(&format!("{s}/admin"), ADMIN_KEY, &[], &change).await;
        assert_eq!(status, 200, "{change}: {body}");
        k
    };
    let a_ingest = key("acme", "feed", "ingest").await;
    let a_writer = key("acme", "bot", "writer").await;
    let a_reader = key("acme", "dash", "reader").await;
    let b_ingest = key("globex", "feed", "ingest").await;
    let alice = id_token(&issuer, "alice@acme.com");
    let bob = id_token(&issuer, "bob@acme.com");
    let acme = Scope::org("acme").unwrap();
    let in_acme = [("x-memstore-org", "acme")];

    // What acme's connector wrote: an email and the newer body it was re-sent with, another email, two test
    // notes; and one episode under a source the server owns, which only the operator writes.
    let sent = [
        ("email:hubspot:1", "Meet Tuesday?"),
        ("email:hubspot:1", "Meet Tuesday at 3pm?"),
        ("email:hubspot:2", "Pricing attached."),
        ("note:test:1", "a test record"),
        ("note:test:2", "another test record"),
    ];
    let mut ids = vec![];
    for (source, text) in sent {
        let (status, v) = post(
            &format!("{s}/ingest"),
            &a_ingest,
            &[],
            &batch(source, text, &["group:sales"]),
        )
        .await;
        assert_eq!(status, 200, "{v}");
        ids.push(v["episode_ids"][0].as_str().unwrap().to_string());
    }
    let (old, new, other, note1, note2) = (&ids[0], &ids[1], &ids[2], &ids[3], &ids[4]);
    let (status, v) = post(
        &format!("{s}/ingest"),
        ADMIN_KEY,
        &in_acme,
        &batch(
            "mcp:remember_memory:svc:probe",
            "the server's own",
            &["group:sales"],
        ),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let owned = v["episode_ids"][0].as_str().unwrap().to_string();
    // A fact each version of the first email supports.
    let ep = |source: &str, text: &str| Episode::sourced(source, text.as_bytes(), &["group:sales"]);
    let facts = [
        Fact::new("deal:D1", "meeting_proposed", "Tuesday")
            .from_episodes(&[&ep(sent[0].0, sent[0].1)]),
        Fact::new("deal:D1", "meeting_proposed", "Tuesday at 3pm")
            .from_episodes(&[&ep(sent[1].0, sent[1].1)]),
    ];
    store
        .commit(&acme, "extract:test", &[], &facts)
        .await
        .unwrap();

    let retract = async |bearer: &str, sel: &[(&str, &str)], body: Value| {
        post(&format!("{s}/retract"), bearer, sel, &body).await
    };
    let live = async || -> Vec<String> {
        let mut v: Vec<String> = store
            .current_episodes(&acme)
            .await
            .unwrap()
            .into_keys()
            .collect();
        v.sort();
        v
    };
    let before = live().await;
    assert_eq!(before.len(), 6, "{before:?}");

    // Refused, and nothing changes: callers who aren't acme's admins or its connector, and what the connector
    // may not retract.
    let refused = [
        (
            "no credential",
            "",
            vec![],
            json!({"source": "email:hubspot:2"}),
            401,
        ),
        (
            "an unknown key",
            "msk_nobody_knows_this",
            vec![],
            json!({"source": "email:hubspot:2"}),
            401,
        ),
        (
            "acme's writer key",
            a_writer.as_str(),
            vec![],
            json!({"source": "email:hubspot:2"}),
            403,
        ),
        (
            "acme's reader key",
            a_reader.as_str(),
            vec![],
            json!({"source": "email:hubspot:2"}),
            403,
        ),
        (
            "an acme member",
            bob.as_str(),
            in_acme.to_vec(),
            json!({"source": "email:hubspot:2"}),
            403,
        ),
        (
            "globex's key, acme selected",
            b_ingest.as_str(),
            in_acme.to_vec(),
            json!({"source": "email:hubspot:2"}),
            403,
        ),
        (
            "acme's key, globex selected",
            a_ingest.as_str(),
            vec![("x-memstore-org", "globex")],
            json!({"source": "email:hubspot:2"}),
            403,
        ),
        (
            "acme's key, by prefix",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": "email:hubspot:"}),
            403,
        ),
        (
            "acme's key, the server's source",
            a_ingest.as_str(),
            vec![],
            json!({"source": "mcp:remember_memory:svc:probe"}),
            403,
        ),
        (
            "acme's key, the server's episode",
            a_ingest.as_str(),
            vec![],
            json!({"episode_id": owned}),
            403,
        ),
        (
            "acme's key, an episode not here",
            a_ingest.as_str(),
            vec![],
            json!({"episode_id": "0".repeat(64)}),
            403,
        ),
        (
            "acme's key, except without a source",
            a_ingest.as_str(),
            vec![],
            json!({"episode_id": note1, "except": [new]}),
            400,
        ),
    ];
    for (who, bearer, sel, body, want) in &refused {
        let (status, v) = retract(bearer, sel, body.clone()).await;
        assert_eq!(status, *want, "{who}: {v}");
    }
    // Two not-here answers look alike, whether the episode exists or not.
    let not_here = |id: &str| format!("no episode {id} this key may retract here");
    let (_, v) = retract(&a_ingest, &[], json!({"episode_id": owned})).await;
    assert_eq!(v["error"], not_here(&owned));
    // Globex's key in its own memory finds nothing of acme's.
    let (status, v) = retract(&b_ingest, &[], json!({"source": "email:hubspot:2"})).await;
    assert_eq!((status, v["retracted"].as_u64()), (200, Some(0)), "{v}");
    assert_eq!(live().await, before);

    // Acme's connector replaces the first email's old body: the old version and the fact it alone supports
    // go, the new version and its fact stay. It learns how many, not which.
    let (status, v) = retract(
        &a_ingest,
        &[],
        json!({"source": "email:hubspot:1", "except": [new]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({"retracted": 1}));
    let amy = Principal::new("amy@acme.com", &["sales"]);
    let t = Instant::now();
    loop {
        let values: Vec<String> = store
            .read(&acme, &amy, None)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.value)
            .collect();
        if values == ["Tuesday at 3pm"] {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "{values:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The same email re-sent with its newer body again stays: only the versions that were there went.
    let (status, _) = post(
        &format!("{s}/ingest"),
        &a_ingest,
        &[],
        &batch("email:hubspot:1", "Meet Tuesday at 3pm?", &["group:sales"]),
    )
    .await;
    assert_eq!(status, 200);
    // One test note by id.
    let (status, v) = retract(&a_ingest, &[], json!({"episode_id": note1})).await;
    assert_eq!((status, v), (200, json!({"retracted": 1})));
    // Acme's admin sweeps the rest of the test notes by prefix, and sees which.
    let (status, v) = retract(&alice, &in_acme, json!({"source_prefix": "note:test:"})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["episode_ids"], json!([note2]), "{v}");
    // The operator, with acme selected, deletes the other email.
    let (status, v) = retract(ADMIN_KEY, &in_acme, json!({"source": "email:hubspot:2"})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["episode_ids"], json!([other]), "{v}");
    let mut left = vec![new.clone(), owned.clone()];
    left.sort();
    assert_eq!(live().await, left);
    assert!(!live().await.contains(old));

    // A version a newer one replaced (`replaces_source`) is only retired: sent again, it would be current again.
    // A delete holds over that for good, whichever way it names the version.
    let replace = async |bearer: &str, sel: &[(&str, &str)], source: &str, text: &str| -> String {
        let body = json!({"episodes": [{"source": source, "occurred_at": 1_760_000_000_000_000i64,
                          "acl": ["group:sales"], "body": text, "replaces_source": true}]});
        let (status, v) = post(&format!("{s}/ingest"), bearer, sel, &body).await;
        assert_eq!(status, 200, "{v}");
        v["episode_ids"][0].as_str().unwrap().to_string()
    };
    // By its own id: the connector may name a retired version it wrote, though no reader sees it.
    let v1 = replace(&a_ingest, &[], "email:hubspot:3", "Lunch Friday?").await;
    replace(&a_ingest, &[], "email:hubspot:3", "Lunch Friday at noon?").await;
    let (status, v) = retract(&a_ingest, &[], json!({"episode_id": v1})).await;
    assert_eq!((status, v), (200, json!({"retracted": 0})));
    // By its source, with no version current: the retired one is there, so the delete is written.
    let w1 = replace(&a_ingest, &[], "email:hubspot:4", "Invoice 7 due.").await;
    let w2 = replace(&a_ingest, &[], "email:hubspot:4", "Invoice 7 paid.").await;
    let (status, _) = retract(&a_ingest, &[], json!({"episode_id": w2})).await;
    assert_eq!(status, 200);
    let (status, v) = retract(&a_ingest, &[], json!({"source": "email:hubspot:4"})).await;
    assert_eq!((status, v), (200, json!({"retracted": 0})));
    // As one of the source's other versions.
    let u1 = replace(&a_ingest, &[], "email:hubspot:5", "Demo at 2?").await;
    let u2 = replace(&a_ingest, &[], "email:hubspot:5", "Demo at 3?").await;
    let (status, _) = retract(
        &a_ingest,
        &[],
        json!({"source": "email:hubspot:5", "except": [u2]}),
    )
    .await;
    assert_eq!(status, 200);
    // A retired version of a source the server owns is still not the connector's to delete.
    let m1 = replace(ADMIN_KEY, &in_acme, "mcp:remember_memory:svc:probe", "v1").await;
    replace(ADMIN_KEY, &in_acme, "mcp:remember_memory:svc:probe", "v2").await;
    let (status, v) = retract(&a_ingest, &[], json!({"episode_id": m1})).await;
    assert_eq!((status, &v["error"]), (403, &json!(not_here(&m1))), "{v}");
    // Every deleted version sent again as its record's current one: none is back.
    for (source, text, gone) in [
        ("email:hubspot:1", "Meet Tuesday?", old),
        ("email:hubspot:2", "Pricing attached.", other),
        ("note:test:1", "a test record", note1),
        ("note:test:2", "another test record", note2),
        ("email:hubspot:3", "Lunch Friday?", &v1),
        ("email:hubspot:4", "Invoice 7 due.", &w1),
        ("email:hubspot:4", "Invoice 7 paid.", &w2),
        ("email:hubspot:5", "Demo at 2?", &u1),
    ] {
        replace(&a_ingest, &[], source, text).await;
        let now = live().await;
        assert!(!now.contains(gone), "{source} {gone} is back: {now:?}");
    }
}
