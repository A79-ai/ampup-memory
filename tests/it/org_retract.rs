//! `/retract` in an org's memory, end to end: a real `memstore serve` over HTTP on a local catalog, every caller
//! selecting an org (`AMPUP_MEMORY_DIRECTORY=required`), two orgs (`acme`, admin alice and member bob, and `globex`),
//! each with its connector's `ingest` key.
//!
//! The rule (`/retract`): it acts in the org the request selects, as reads do. The operator's admin key and the
//! org's admins retract anything there, by prefix too. The org's ingest key retracts in its own memory only what
//! it could write: one exact source (or its other versions, `except` the current one), one episode that is
//! there, or a prefix naming a source family and something in it (`<kind>:<more>`, so a removed connector's
//! source goes), never under a source the server owns, never outside its key's sources, and it is told only how
//! many. A member, a writer or reader key and another org's key are refused, and change nothing. Facts an episode
//! alone supports go with it; facts another episode wrote stay. A delete is for good: a deleted version sent again
//! never comes back, also one a newer version had only retired (`replaces_source`), which sent again alone would.
//! A prefix holds only what exists now. With `"hold": false` (a prefix only) the delete holds nothing back: what
//! it took is gone, purged too, and the same record sent again later is back.
//!
//! An import undone (`/admin {"op": "forget_source"}`): an org's admins in their org and the operator delete every
//! episode of the org whose source a prefix (`<kind>:<more>`, never a whole connector) or a list names, every version
//! of each, with every fact citing them, for every reader and from storage at once; never a source the server files
//! itself, never another org's. A member, a key and another org's admin are refused; a dry run changes nothing. The
//! same record sent again later is back.

use std::path::{Path, PathBuf};
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

/// Every file under `root` whose path doesn't hold `besides` and whose bytes hold `tag`.
fn files_with(root: &Path, besides: &str, tag: &str) -> Vec<PathBuf> {
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
            } else if !p.to_string_lossy().contains(besides)
                && let Ok(bytes) = std::fs::read(&p)
                && bytes.windows(tag.len()).any(|w| w == tag.as_bytes())
            {
                out.push(p);
            }
        }
    }
    out
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
    // Its own index, beside the catalog, so a check of what is left on disk sees the index's files too.
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
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
            index_base: Some(store.index_base().to_string()),
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
            "acme's key, a whole connector by prefix",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": "email:"}),
            403,
        ),
        (
            "acme's key, a prefix with no family",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": ":hubspot"}),
            403,
        ),
        (
            "acme's key, the whole org by prefix",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": ""}),
            400,
        ),
        (
            "acme's key, a prefix too short",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": "x"}),
            400,
        ),
        (
            "acme's key, the server's sources by prefix",
            a_ingest.as_str(),
            vec![],
            json!({"source_prefix": "mcp:remember_memory:"}),
            403,
        ),
        (
            "acme's key, a source not held back",
            a_ingest.as_str(),
            vec![],
            json!({"source": "email:hubspot:2", "hold": false}),
            400,
        ),
        (
            "the operator, an episode not held back",
            ADMIN_KEY,
            in_acme.to_vec(),
            json!({"episode_id": note1, "hold": false}),
            400,
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

    // A connector removed from the org: its ingest key retracts what it wrote by prefix, with the facts citing
    // it, and nothing of another org's, though globex wrote the very same source and body.
    let globex = Scope::org("globex").unwrap();
    let ingest = async |bearer: &str, source: &str, text: &str| -> String {
        let (status, v) = post(
            &format!("{s}/ingest"),
            bearer,
            &[],
            &batch(source, text, &["group:sales"]),
        )
        .await;
        assert_eq!(status, 200, "{v}");
        v["episode_ids"][0].as_str().unwrap().to_string()
    };
    let issue1 = ingest(&a_ingest, "sentry:acme/api/1", "TypeError in checkout").await;
    let issue2 = ingest(&a_ingest, "sentry:acme/api/2", "Timeout on login").await;
    let web1 = ingest(&a_ingest, "sentry:acme/web/1", "Hydration mismatch").await;
    let theirs = ingest(&b_ingest, "sentry:acme/api/1", "TypeError in checkout").await;
    store
        .commit(
            &acme,
            "extract:test",
            &[],
            &[Fact::new("issue:acme/api/1", "error", "TypeError")
                .from_episodes(&[&ep("sentry:acme/api/1", "TypeError in checkout")])],
        )
        .await
        .unwrap();
    let values = async |want: &[&str], absent: &[&str]| {
        let t = Instant::now();
        loop {
            let v: Vec<String> = store
                .read(&acme, &amy, None)
                .await
                .unwrap()
                .into_iter()
                .map(|f| f.value)
                .collect();
            if want.iter().all(|w| v.iter().any(|x| x == w))
                && absent.iter().all(|a| !v.iter().any(|x| x == a))
            {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(15), "{v:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    values(&["TypeError"], &[]).await;
    // A key held to some sources retracts no prefix wider than they are.
    let web_key = format!("msk_{}web", uuid::Uuid::new_v4().simple());
    let (status, v) = post(
        &format!("{s}/admin"),
        ADMIN_KEY,
        &[],
        &json!({"op": "create_key", "org": "acme", "name": "web", "role": "ingest",
                "key": web_key, "sources": ["sentry:acme/web/"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    for prefix in ["sentry:acme/", "sentry:acme/api/"] {
        let (status, v) = retract(&web_key, &[], json!({"source_prefix": prefix})).await;
        assert_eq!(status, 403, "{prefix}: {v}");
    }
    let (status, v) = retract(&a_ingest, &[], json!({"source_prefix": "sentry:"})).await;
    assert_eq!(status, 403, "{v}");
    let (status, v) = retract(&a_ingest, &[], json!({"source_prefix": "sentry:acme/api/"})).await;
    assert_eq!((status, v), (200, json!({"retracted": 2})));
    let now = live().await;
    assert!(!now.contains(&issue1) && !now.contains(&issue2), "{now:?}");
    assert!(now.contains(&web1), "{now:?}");
    values(&[], &["TypeError"]).await;
    assert!(
        store
            .current_episodes(&globex)
            .await
            .unwrap()
            .contains_key(&theirs),
        "globex's episode went with acme's"
    );
    // Sent again, it stays gone: a prefix delete holds for what it took.
    ingest(&a_ingest, "sentry:acme/api/1", "TypeError in checkout").await;
    assert!(!live().await.contains(&issue1));
    // A prefix with nothing under it retracts nothing and holds nothing back.
    let (status, v) = retract(
        &a_ingest,
        &[],
        json!({"source_prefix": "sentry:acme/jobs/"}),
    )
    .await;
    assert_eq!((status, v), (200, json!({"retracted": 0})));
    let job1 = ingest(&a_ingest, "sentry:acme/jobs/1", "Queue backed up").await;
    assert!(live().await.contains(&job1));

    // Removed with `"hold": false`, as a source the org may add back: gone now with its facts, and back when
    // the same record is sent again, before a purge and after it. One the source deleted for good stays gone.
    let pr1 = ingest(&a_ingest, "github:acme/app#1", "Fix the login redirect").await;
    let pr2 = ingest(&a_ingest, "github:acme/app#2", "Bump the parser").await;
    let pr3 = ingest(&a_ingest, "github:acme/app#3", "Drop the old cache").await;
    // An extractor read the first two.
    let read = |id: &str, facts: i64| memstore::Extraction {
        episode_id: id.to_string(),
        extractor: "extract:test".into(),
        facts,
        tokens: None,
    };
    store
        .commit_extracted(
            &acme,
            "extract:test",
            &[Fact::new("pr:acme/app#1", "fixes", "login redirect")
                .from_episodes(&[&ep("github:acme/app#1", "Fix the login redirect")])],
            &[read(&pr1, 1), read(&pr2, 0)],
            &[],
            None,
        )
        .await
        .unwrap();
    values(&["login redirect"], &[]).await;
    let (status, _) = retract(&a_ingest, &[], json!({"source": "github:acme/app#3"})).await;
    assert_eq!(status, 200);
    let (status, v) = retract(
        &a_ingest,
        &[],
        json!({"source_prefix": "github:acme/app#", "hold": false}),
    )
    .await;
    assert_eq!((status, v), (200, json!({"retracted": 2})));
    let now = live().await;
    assert!(
        [&pr1, &pr2, &pr3].iter().all(|id| !now.contains(id)),
        "{now:?}"
    );
    values(&[], &["login redirect"]).await;
    // Sent again before a purge: back, with the fact it supports.
    assert_eq!(
        ingest(&a_ingest, "github:acme/app#1", "Fix the login redirect").await,
        pr1
    );
    assert!(live().await.contains(&pr1));
    values(&["login redirect"], &[]).await;
    // A purge erases the one not sent again, and keeps the one that came back.
    let ns = acme.namespace();
    let purgeable = store.purgeable_in(&ns, Duration::ZERO).await.unwrap();
    assert!(
        purgeable.contains(&pr2) && !purgeable.contains(&pr1),
        "{purgeable:?}"
    );
    store.purge_retracted_in(&ns).await.unwrap();
    assert!(
        store
            .purgeable_in(&ns, Duration::ZERO)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(live().await.contains(&pr1));
    // Sent again after the purge: back, and read again (its facts went with the purge). The one deleted for good
    // is not.
    assert_eq!(
        ingest(&a_ingest, "github:acme/app#2", "Bump the parser").await,
        pr2
    );
    let extracted = store.extracted(&acme, "extract:test").await.unwrap();
    assert!(
        extracted.contains(&pr1) && !extracted.contains(&pr2),
        "{extracted:?}"
    );
    ingest(&a_ingest, "github:acme/app#3", "Drop the old cache").await;
    let now = live().await;
    assert!(now.contains(&pr1) && now.contains(&pr2), "{now:?}");
    assert!(!now.contains(&pr3), "{now:?}");
    // A delete for good by the same prefix after it holds for what came back.
    let (status, v) = retract(&a_ingest, &[], json!({"source_prefix": "github:acme/app#"})).await;
    assert_eq!((status, v), (200, json!({"retracted": 2})));
    ingest(&a_ingest, "github:acme/app#2", "Bump the parser").await;
    assert!(!live().await.contains(&pr2));
    // An admin may hold nothing back too, and sees which.
    let (status, v) = retract(
        &alice,
        &in_acme,
        json!({"source_prefix": "sentry:acme/web/", "hold": false}),
    )
    .await;
    assert_eq!((status, &v["episode_ids"]), (200, &json!([web1])), "{v}");
    assert!(!live().await.contains(&web1));
    ingest(&a_ingest, "sentry:acme/web/1", "Hydration mismatch").await;
    assert!(live().await.contains(&web1));

    // An import undone: what acme's connector sent under `doc:policy/` (a document and the newer version that retired
    // it, another document), a fact drawn from them, and one document outside the prefix; globex sent one of the
    // very same records.
    let tag = "zephyrimport";
    let all = format!("group:{}", memstore::auth::acl::org_wide("acme"));
    let doc = async |bearer: &str, source: &str, text: &str| -> String {
        let body = json!({"episodes": [{"source": source, "occurred_at": 1_760_000_000_000_000i64,
                          "acl": [all.as_str()], "body": text, "replaces_source": true}]});
        let (status, v) = post(&format!("{s}/ingest"), bearer, &[], &body).await;
        assert_eq!(status, 200, "{v}");
        v["episode_ids"][0].as_str().unwrap().to_string()
    };
    let leave1 = doc(
        &a_ingest,
        "doc:policy/leave",
        &format!("{tag} leave: 10 days"),
    )
    .await;
    let leave2 = doc(
        &a_ingest,
        "doc:policy/leave",
        &format!("{tag} leave: 20 days"),
    )
    .await;
    let travel = doc(
        &a_ingest,
        "doc:policy/travel",
        &format!("{tag} travel: economy"),
    )
    .await;
    let intro = doc(&a_ingest, "doc:handbook/intro", "welcome aboard").await;
    let theirs = doc(
        &b_ingest,
        "doc:policy/travel",
        &format!("{tag} travel: economy"),
    )
    .await;
    let mut f = Fact::new("policy:leave", "days", &format!("{tag} 20"));
    f.source_episodes = vec![leave2.clone(), intro.clone()];
    store
        .commit(&acme, "extract:test", &[], &[f])
        .await
        .unwrap();
    let reader = Principal::new("ann@acme.com", &["acme.all"]);
    let fact_values = async || -> Vec<String> {
        store
            .read(&acme, &reader, None)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.value)
            .collect()
    };
    let t = Instant::now();
    while !fact_values().await.iter().any(|v| v.contains(tag)) {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "the fact never showed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Globex's copy of the same record is the only one that may stay on disk.
    let globex_ns = globex.namespace();
    // Acme's admin reads it on the read paths checked after the delete.
    let paths = |id: &str| {
        [
            (
                "/query",
                json!({"data": [[0, {"find": "passages", "text": tag, "limit": 100}]]}),
            ),
            ("/episode", json!({"data": [[0, id]]})),
        ]
    };
    for id in [&leave2, &travel] {
        for (path, body) in paths(id) {
            let t = Instant::now();
            loop {
                let (status, v) = post(&format!("{s}{path}"), &alice, &in_acme, &body).await;
                assert_eq!(status, 200, "{path}: {v}");
                if v.to_string().contains(tag) {
                    break;
                }
                assert!(
                    t.elapsed() < Duration::from_secs(15),
                    "{path} never read it: {v}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    let before_files = files_with(dir.path(), &globex_ns, tag);
    assert!(
        !before_files.is_empty(),
        "the import is not on disk: {before_files:?}"
    );
    let before = live().await;
    for id in [&leave2, &travel, &intro] {
        assert!(before.contains(id), "{id} isn't current: {before:?}");
    }
    let forget = async |bearer: &str, body: Value| {
        let mut body = body;
        body["op"] = json!("forget_source");
        post(&format!("{s}/admin"), bearer, &[], &body).await
    };
    let gary = id_token(&issuer, "gary@globex.com");
    let policy = json!({"org": "acme", "source_prefix": "doc:policy/"});
    // Refused, and nothing changes: who may not, and what nobody may name this way.
    for (who, bearer, body, want) in [
        ("an acme member", bob.as_str(), policy.clone(), 403),
        ("acme's ingest key", a_ingest.as_str(), policy.clone(), 403),
        ("acme's reader key", a_reader.as_str(), policy.clone(), 403),
        ("globex's admin", gary.as_str(), policy.clone(), 403),
        ("no credential", "", policy.clone(), 401),
        (
            "a whole connector",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": "doc:"}),
            400,
        ),
        (
            "no family",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": "doc"}),
            400,
        ),
        (
            "the whole org",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": ""}),
            400,
        ),
        (
            "the server's notes",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": "mcp:remember_memory:"}),
            400,
        ),
        (
            "the server's notes, as the operator",
            ADMIN_KEY,
            json!({"org": "acme", "source_prefix": "MCP:remember_memory:"}),
            400,
        ),
        (
            "learned advice",
            alice.as_str(),
            json!({"org": "acme", "sources": ["advice:learn"]}),
            400,
        ),
        (
            "a dream's links",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": "dream:same/"}),
            400,
        ),
        (
            "both",
            alice.as_str(),
            json!({"org": "acme", "source_prefix": "doc:policy/", "sources": ["doc:policy/leave"]}),
            400,
        ),
        ("neither", alice.as_str(), json!({"org": "acme"}), 400),
        (
            "an empty list",
            alice.as_str(),
            json!({"org": "acme", "sources": []}),
            400,
        ),
    ] {
        let (status, v) = forget(bearer, body).await;
        assert_eq!(status, want, "{who}: {v}");
    }
    assert_eq!(live().await, before);
    // A dry run says what would go, and changes nothing.
    let mut dry = policy.clone();
    dry["dry_run"] = json!(true);
    let (status, v) = forget(&alice, dry).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        (&v["episodes"], &v["current"], &v["facts"]),
        (&json!(3), &json!(2), &json!(1)),
        "{v}"
    );
    assert_eq!(live().await, before);
    assert!(fact_values().await.iter().any(|v| v.contains(tag)));
    // Acme's admin deletes it: every version of each record, the fact citing one, for every reader and from disk.
    let (status, v) = forget(&alice, policy.clone()).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        (
            &v["current"],
            &v["deleted"]["episodes"],
            &v["deleted"]["facts"]
        ),
        (&json!(2), &json!(3), &json!(1)),
        "{v}"
    );
    let now = live().await;
    for id in [&leave1, &leave2, &travel] {
        assert!(!now.contains(id), "{id} survived: {now:?}");
    }
    assert!(now.contains(&intro), "{now:?}");
    assert!(!fact_values().await.iter().any(|v| v.contains(tag)));
    for (path, body) in [
        ("/search", json!({"data": [[0, tag, 50, -1]]})),
        (
            "/query",
            json!({"data": [[0, {"find": "passages", "text": tag, "limit": 100}]]}),
        ),
        (
            "/query",
            json!({"data": [[0, {"find": "facts", "time": "all", "limit": 500}]]}),
        ),
        ("/episode", json!({"data": [[0, leave1]]})),
        ("/episode", json!({"data": [[0, leave2]]})),
        ("/episode", json!({"data": [[0, travel]]})),
    ] {
        let (status, v) = post(&format!("{s}{path}"), &alice, &in_acme, &body).await;
        assert_eq!(status, 200, "{path}: {v}");
        assert!(!v.to_string().contains(tag), "{path} still reads it: {v}");
    }
    let left = files_with(dir.path(), &globex_ns, tag);
    assert!(left.is_empty(), "still on disk: {left:?}");
    // Globex's own copy of the same record stays, as it was.
    assert!(
        store
            .current_episodes(&globex)
            .await
            .unwrap()
            .contains_key(&theirs),
        "globex's episode went with acme's"
    );
    // Run again, nothing is left to delete.
    let (status, v) = forget(&alice, policy.clone()).await;
    assert_eq!((status, &v["deleted"]["episodes"]), (200, &json!(0)), "{v}");
    // A corrected import of the same record is current again.
    assert_eq!(
        doc(
            &a_ingest,
            "doc:policy/travel",
            &format!("{tag} travel: economy")
        )
        .await,
        travel
    );
    assert!(live().await.contains(&travel));
    // The operator deletes by a list of exact sources.
    let (status, v) = forget(
        ADMIN_KEY,
        json!({"org": "acme", "sources": ["doc:policy/travel"]}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(&v["deleted"]["episodes"], &json!(1), "{v}");
    assert!(!live().await.contains(&travel));
}
