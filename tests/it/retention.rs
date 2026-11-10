//! Retention, end to end, on a real `memstore serve` over HTTP and a local catalog: org `acme` (team `a1`)
//! holds Slack messages and notes of different ages, each carrying a tag no other memory has. Then:
//!
//! - callers who must be refused are: a service key, a file key, another org's admin, a plain member,
//!   and an org that doesn't exist; a malformed policy changes nothing;
//! - acme's admin keeps Slack for 30 days, a legal channel for good: older Slack messages and their facts
//!   are gone from every read path for every reader at once, younger ones, the legal channel, other
//!   sources and team a1 (its own scope, its own policy) stay;
//! - a message that ages out while the server runs goes on its own, with no write to prompt it;
//! - loosening the policy brings back what no purge has removed; `memstore org retention` shows it;
//! - `memstore org purge acme` removes the aged-out messages from every file in the warehouse and index, though a
//!   member pinned acme's memory before they aged out (a pin holds commits, not deleted memory).
//!
//! And the nightly maintenance window's scheduled purge, held open: org admins alone turn it on (`purge`); in the
//! org that did, what aged out and what its source deleted leave every file once hidden for the grace, in the
//! rewrites that compact the tables, though a member pinned the org's memory before (a pin never keeps it), and the
//! org's audit says how much went; what is younger stays, and so does everything of an org that didn't turn it on.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::Bytes;
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{AclTarget, Episode, Fact, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const CLIENT_ID: &str = "memstore-cli";
const ADMIN_KEY: &str = "msk_platform_admin_for_retention";
const ALICE_FILE_KEY: &str = "msk_plain_file_key_in_alices_name";
const DAY: i64 = 86_400_000_000;

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

/// A memory written straight to the catalog (as ingest would): an episode from `source` that happened
/// `ago` microseconds back, holding `tag`, and a fact whose entity and value carry it. Returns its id.
async fn seed(store: &Store, scope: &Scope, source: &str, tag: &str, ago: i64) -> String {
    store.ensure_scope(scope).await.unwrap();
    let mut ep = Episode::sourced(source, format!("{tag} owner note").as_bytes(), &["*"]);
    ep.occurred_at = chrono::Utc::now().timestamp_micros() - ago;
    let id = ep.episode_id.clone();
    let f = Fact::new(&format!("deal:{tag}"), "owner", tag).from_episodes(&[&ep]);
    store.commit(scope, "connector", &[ep], &[f]).await.unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn old_messages_age_out_for_everyone_and_a_purge_removes_them() {
    // Its index location is set in the environment (below), which no other test may see.
    if !crate::own_process("retention::old_messages_age_out_for_everyone_and_a_purge_removes_them")
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let warehouse = dir.path().join("warehouse");
    let index_dir = dir.path().join("index");
    std::fs::create_dir_all(&warehouse).unwrap();
    // The index lives under this test's directory, so its files are checked too. Set before any store.
    unsafe { std::env::set_var("AMPUP_MEMORY_INDEX_URI", &index_dir) };
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
        json!({"op": "create_team", "org": "acme", "team": "a1"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"}, {"user": "carol@acme.com", "teams": ["a1"]}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
    ] {
        admin(s, change).await;
    }
    let a_key = format!("msk_{}acmereader", uuid::Uuid::new_v4().simple());
    admin(
        s,
        json!({"op": "create_key", "org": "acme", "name": "dash", "role": "reader", "key": a_key}),
    )
    .await;
    let [alice, bob, carol, gary] = [
        "alice@acme.com",
        "bob@acme.com",
        "carol@acme.com",
        "gary@globex.com",
    ]
    .map(|u| id_token(&issuer, u));

    let acme = Scope::org("acme").unwrap();
    let a1 = Scope::team("acme", "a1").unwrap();
    // Ages out when the policy is set; younger; about to age out; kept for good; another source.
    let old = seed(&store, &acme, "slack:C1:1.0", "xqoldslack", 100 * DAY).await;
    let young = seed(&store, &acme, "slack:C1:2.0", "xqyoungslack", DAY).await;
    let legal = seed(
        &store,
        &acme,
        "slack:C-legal:1.0",
        "xqlegalslack",
        400 * DAY,
    )
    .await;
    let note = seed(&store, &acme, "note:1", "xqoldnote", 100 * DAY).await;
    let team_old = seed(&store, &a1, "slack:T1:1.0", "xqteamslack", 100 * DAY).await;
    let ids = [old.as_str(), young.as_str(), legal.as_str(), note.as_str()];
    let all = ["xqoldslack", "xqyoungslack", "xqlegalslack", "xqoldnote"];
    until(s, "?org=acme", &bob, &ids, "bob reads acme", |r| {
        r.as_ref()
            .is_some_and(|r| all.iter().all(|t| r.contains(t)))
    })
    .await;

    // An owner review built on the old message, before any policy: Alice makes Bob the owner of `deal:xqgov`,
    // and Carol proposes a change to it whose evidence is the old message. The proposal's row quotes the proposed
    // value; the purge below, which removes the message, takes the proposal with it.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "set_entity_owner", "org": "acme", "entity": "deal:xqgov", "owner": "bob@acme.com"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    store
        .create_branch(&acme, "govreview", "agent")
        .await
        .unwrap();
    let mut gov_fact = Fact::new("deal:xqgov", "owner", "xqgovvalue");
    gov_fact.source_episodes = vec![old.clone()];
    store
        .commit_on(&acme, "govreview", "agent", &[], &[gov_fact])
        .await
        .unwrap();
    let (status, body) = admin_as(
        s,
        &carol,
        json!({"op": "propose_branch", "org": "acme", "branch": "govreview"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!in_files("xqgovvalue").is_empty(), "the proposal is stored");

    // Bob pins acme's memory as it is now, as a long-running agent does (`POST /pin`): every message in it.
    let (status, body) = post(
        &format!("{s}/pin?org=acme"),
        &bob,
        &json!({"ttl_secs": 3600}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let pin: Value = serde_json::from_str(&body).unwrap();
    let pinned = format!("?org=acme&at={}", pin["pin"].as_str().unwrap());
    until(
        s,
        &pinned,
        &bob,
        &ids,
        "bob reads acme through his pin",
        |r| {
            r.as_ref()
                .is_some_and(|r| all.iter().all(|t| r.contains(t)))
        },
    )
    .await;

    // Refused, and nothing changes: a service key, a file key in alice's name, globex's admin, a member,
    // an org that doesn't exist.
    let policy = json!(["slack:=30d", "slack:C-legal:=forever"]);
    for (who, org) in [
        (a_key.as_str(), "acme"),
        (ALICE_FILE_KEY, "acme"),
        (gary.as_str(), "acme"),
        (bob.as_str(), "acme"),
        (alice.as_str(), "nosuchorg"),
    ] {
        let (status, body) = admin_as(
            s,
            who,
            json!({"op": "retention", "org": org, "rules": policy}),
        )
        .await;
        assert_eq!(status, 403, "{org}: {body}");
    }
    // A malformed policy, or one that would keep less than a day, is rejected whole.
    for bad in [
        json!(["slack:=12h"]),
        json!(["slack:=0d"]),
        json!(["slack"]),
        json!(["slack:=30d", "slack:=60d"]),
    ] {
        let (status, body) = admin_as(
            s,
            &alice,
            json!({"op": "retention", "org": "acme", "rules": bad}),
        )
        .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let got = reads(s, "?org=acme", &bob, &ids).await.unwrap();
    assert!(all.iter().all(|t| got.contains(t)), "{got}");

    // Acme's admin keeps Slack for 30 days and the legal channel for good.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "retention", "org": "acme", "rules": policy}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for who in [&bob, &alice] {
        until(s, "?org=acme", who, &ids, "acme under 30 days", |r| {
            r.as_ref().is_some_and(|r| {
                !r.contains("xqoldslack")
                    && ["xqyoungslack", "xqlegalslack", "xqoldnote"]
                        .iter()
                        .all(|t| r.contains(t))
            })
        })
        .await;
    }
    // A pin holds commits, not who may read what: what aged out is gone through Bob's pin too.
    until(
        s,
        &pinned,
        &bob,
        &ids,
        "acme under 30 days, through a pin",
        |r| {
            r.as_ref().is_some_and(|r| {
                !r.contains("xqoldslack")
                    && ["xqyoungslack", "xqlegalslack", "xqoldnote"]
                        .iter()
                        .all(|t| r.contains(t))
            })
        },
    )
    .await;
    // Team a1 is its own scope with its own (empty) policy.
    until(
        s,
        "?org=acme&team=a1",
        &carol,
        &[&team_old],
        "a1 keeps its own",
        |r| r.as_ref().is_some_and(|r| r.contains("xqteamslack")),
    )
    .await;

    // A message that turns 30 days old while the server runs goes on its own: nothing is written meanwhile.
    let cusp = seed(
        &store,
        &acme,
        "slack:C1:3.0",
        "xqcuspslack",
        30 * DAY - 6_000_000,
    )
    .await;
    until(
        s,
        "?org=acme",
        &bob,
        &[&cusp],
        "the cusp message, still young",
        |r| r.as_ref().is_some_and(|r| r.contains("xqcuspslack")),
    )
    .await;
    until(
        s,
        "?org=acme",
        &bob,
        &[&cusp],
        "the cusp message ages out",
        |r| r.as_ref().is_some_and(|r| !r.contains("xqcuspslack")),
    )
    .await;

    // The operator reads the policy through the CLI; loosening it brings the old message back.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (ok, out, err) = memstore(
        &home,
        &["auth", "login", "--server", s, "--api-key", ADMIN_KEY],
    );
    assert!(ok, "{out}{err}");
    let (ok, out, err) = memstore(&home, &["org", "retention", "acme"]);
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("slack:=30d") && out.contains("slack:C-legal:=forever"),
        "{out}"
    );
    // A new policy replaces the old one whole: the legal channel keeps its own rule only if restated.
    let (ok, out, err) = memstore(
        &home,
        &[
            "org",
            "retention",
            "acme",
            "slack:=1y",
            "slack:C-legal:=forever",
        ],
    );
    assert!(ok, "{out}{err}");
    until(s, "?org=acme", &bob, &ids, "acme under a year", |r| {
        r.as_ref()
            .is_some_and(|r| all.iter().all(|t| r.contains(t)))
    })
    .await;
    let (ok, out, err) = memstore(&home, &["org", "retention", "acme", "slack:=12h"]);
    assert!(
        !ok && err.contains("12h"),
        "a typo is refused before it is sent: {out}{err}"
    );
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "retention", "org": "acme", "rules": policy}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    until(
        s,
        "?org=acme",
        &bob,
        &ids,
        "acme under 30 days again",
        |r| {
            r.as_ref()
                .is_some_and(|r| !r.contains("xqoldslack") && r.contains("xqyoungslack"))
        },
    )
    .await;

    // Until a purge the aged-out messages are still stored; the purge removes them from every file, and
    // nothing else.
    assert!(!in_files("xqoldslack").is_empty());
    let (ok, out, err) = memstore(&home, &["org", "purge", "acme"]);
    assert!(ok, "{out}{err}");
    let done: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(done["purged"]["episodes"], 2, "{out}");
    assert_eq!(done["purged"]["facts"], 3, "{out}");
    // The proposal built on the old message went with it: its quoted value is in no file, and the inbox
    // doesn't list it.
    for t in ["xqoldslack", "xqcuspslack", "xqgovvalue"] {
        assert_eq!(in_files(t), Vec::<PathBuf>::new(), "{t}");
    }
    let (status, body) = admin_as(s, &bob, json!({"op": "list_proposals", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("xqgov"), "{body}");
    for t in ["xqyoungslack", "xqlegalslack", "xqoldnote", "xqteamslack"] {
        assert!(!in_files(t).is_empty(), "{t} went with the purge");
    }
    // Bob's pin, live all along, kept none of it: through it he reads what is left, and nothing purged.
    until(
        s,
        &pinned,
        &bob,
        &ids,
        "acme after the purge, through a pin",
        |r| {
            r.as_ref().is_some_and(|r| {
                !r.contains("xqoldslack")
                    && ["xqyoungslack", "xqlegalslack", "xqoldnote"]
                        .iter()
                        .all(|t| r.contains(t))
            })
        },
    )
    .await;
    // Acme's audit holds each policy change and the purge, counts and rules only, never what went.
    let (status, body) = admin_as(s, &alice, json!({"op": "audit", "org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    for want in [
        "retention: slack:=30d slack:C-legal:=forever",
        "retention: slack:=1y slack:C-legal:=forever",
        r#"purge retracted memory: {\"episodes\":2"#,
    ] {
        assert!(body.contains(want), "{want} not in acme's audit: {body}");
    }
    assert!(!body.contains("xqoldslack"), "{body}");

    // A purge stopped right after its last rewrite (the episodes table's) committed, before the files it replaced
    // were deleted (as if the process were killed there): the old note's episode row is out of the table but still
    // in its old data files. The next purge, which finds nothing left to remove, still deletes them. Notes age out
    // now too, and so do remembered ones: a part of Carol's session transcript, sent 101 days ago and sent again
    // with more the day after, goes for her in both versions, not only the newest, and leaves every file.
    let script = |said: &str, ago: i64| {
        let mut ep = Episode::sourced(
            "mcp:remember_memory:carol@acme.com:memstore:transcript:claude-code:launch:s-xqaged",
            format!("Result of Bash (call 1):\n  {said}").as_bytes(),
            &["user:carol@acme.com"],
        )
        .version_of("transcripts/claude-code/s-xqaged/00000000")
        .without_text_index();
        ep.occurred_at = chrono::Utc::now().timestamp_micros() - ago;
        ep
    };
    let (first, more) = (
        script("xqscriptfirst", 101 * DAY),
        script("xqscriptfirst xqscriptmore", 100 * DAY),
    );
    let script_ids = [first.episode_id.clone(), more.episode_id.clone()];
    let script_ids = [script_ids[0].as_str(), script_ids[1].as_str()];
    store
        .commit(&acme, "carol@acme.com", &[first, more], &[])
        .await
        .unwrap();
    until(
        s,
        "?org=acme",
        &carol,
        &script_ids,
        "carol reads her transcript",
        |r| r.as_ref().is_some_and(|r| r.contains("xqscriptmore")),
    )
    .await;
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "retention", "org": "acme",
            "rules": ["slack:=30d", "slack:C-legal:=forever", "note:=30d", "mcp:remember_memory:=30d"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    until(
        s,
        "?org=acme",
        &carol,
        &script_ids,
        "every version of her transcript's part ages out",
        |r| r.as_ref().is_some_and(|r| !r.contains("xqscriptfirst")),
    )
    .await;
    let listing = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_episode",
                         "arguments": {"document": "transcripts/claude-code/s-xqaged/00000000"}}});
    let (_, body) = post(&format!("{s}/mcp?org=acme"), &carol, &listing).await;
    assert!(!body.contains("xqscriptfirst"), "{body}");
    unsafe { std::env::set_var("AMPUP_MEMORY_STOP_AFTER_REWRITE", "episodes") };
    let (ok, out, err) = memstore(&home, &["org", "purge", "acme"]);
    unsafe { std::env::remove_var("AMPUP_MEMORY_STOP_AFTER_REWRITE") };
    assert!(!ok, "the purge should have stopped: {out}{err}");
    let episode_files = |id: &str| -> Vec<PathBuf> {
        in_files(id)
            .into_iter()
            .filter(|f| f.to_string_lossy().contains("/episodes/"))
            .collect()
    };
    assert_eq!(in_files("xqoldnote"), Vec::<PathBuf>::new(), "{out}{err}");
    assert!(
        !episode_files(&note).is_empty(),
        "no replaced episodes file left to recover: {out}{err}"
    );
    let (ok, out, err) = memstore(&home, &["org", "purge", "acme"]);
    assert!(ok, "{out}{err}");
    assert_eq!(episode_files(&note), Vec::<PathBuf>::new());
    for t in ["xqscriptfirst", "xqscriptmore"] {
        assert_eq!(in_files(t), Vec::<PathBuf>::new(), "{t} still stored");
    }
    for t in ["xqyoungslack", "xqlegalslack", "xqteamslack"] {
        assert!(!in_files(t).is_empty(), "{t} went with the purge");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_nightly_window_purges_what_aged_out_or_was_deleted_where_the_org_asked() {
    // Its maintenance window and index location are set in the environment (below), which no other test may see.
    if !crate::own_process(
        "retention::the_nightly_window_purges_what_aged_out_or_was_deleted_where_the_org_asked",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let warehouse = dir.path().join("warehouse");
    let index_dir = dir.path().join("index");
    std::fs::create_dir_all(&warehouse).unwrap();
    unsafe { std::env::set_var("AMPUP_MEMORY_INDEX_URI", &index_dir) };
    // The window held open, a pass every second; what is purged must have been hidden for 8 seconds first.
    const GRACE: Duration = Duration::from_secs(8);
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_WINDOW", "always") };
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_SECS", "1") };
    unsafe { std::env::set_var("AMPUP_MEMORY_MAINTAIN_PACE", "0") };
    unsafe { std::env::set_var("AMPUP_MEMORY_PURGE_GRACE", "8s") };
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
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
    ] {
        admin(s, change).await;
    }
    let [alice, bob, gary] =
        ["alice@acme.com", "bob@acme.com", "gary@globex.com"].map(|u| id_token(&issuer, u));

    // In each org: a message older than its policy, a young one, and one its source then deletes.
    let (acme, globex) = (Scope::org("acme").unwrap(), Scope::org("globex").unwrap());
    // The server reads acme first, so the messages below are commits its cache hasn't seen when Bob pins acme right
    // after them: a pin still holds every commit made before it was asked for, whoever made it.
    assert!(reads(s, "?org=acme", &bob, &[]).await.is_some());
    let acme_old = seed(&store, &acme, "slack:C1:1.0", "xqpurgeoldslack", 100 * DAY).await;
    let acme_young = seed(&store, &acme, "slack:C1:2.0", "xqpurgeyoungslack", DAY).await;
    let acme_deleted = seed(&store, &acme, "slack:C1:3.0", "xqpurgedeleted", DAY).await;
    seed(&store, &globex, "slack:G1:1.0", "xqkeptoldslack", 100 * DAY).await;
    seed(&store, &globex, "slack:G1:3.0", "xqkeptdeleted", DAY).await;
    // Bob pins acme's memory as it is now, every message in it, and keeps the pin live throughout: it never keeps
    // what the purge deletes.
    let (status, body) = post(
        &format!("{s}/pin?org=acme"),
        &bob,
        &json!({"ttl_secs": 3600}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let pinned = format!(
        "?org=acme&at={}",
        serde_json::from_str::<Value>(&body).unwrap()["pin"]
            .as_str()
            .unwrap()
    );
    // Each is hidden from the commit that hides it on: at or after these.
    let deleting = Instant::now();
    for (scope, source) in [(&acme, "slack:C1:3.0"), (&globex, "slack:G1:3.0")] {
        store
            .retract_episodes(scope, "connector", &AclTarget::Source(source.into()))
            .await
            .unwrap();
    }
    // Only an org's admins (or the operator) turn the purge on: a member and another org's admin are refused.
    let purge_on =
        |org: &str| json!({"op": "set_settings", "org": org, "settings": {"purge": true}});
    for (who, org) in [(bob.as_str(), "acme"), (gary.as_str(), "acme")] {
        let (status, body) = admin_as(s, who, purge_on(org)).await;
        assert_eq!(status, 403, "{org}: {body}");
    }
    // Both keep Slack 30 days; acme asks for the purge, globex doesn't.
    let aging = Instant::now();
    for (who, org) in [(&alice, "acme"), (&gary, "globex")] {
        let (status, body) = admin_as(
            s,
            who,
            json!({"op": "retention", "org": org, "rules": ["slack:=30d"]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = admin_as(s, &alice, purge_on("acme")).await;
    assert_eq!(status, 200, "{body}");

    // Hidden from every read at once, still in storage for the grace (the window runs every second meanwhile).
    until(s, "?org=acme", &bob, &[], "acme hides what went", |r| {
        r.as_ref().is_some_and(|r| {
            r.contains("xqpurgeyoungslack")
                && !r.contains("xqpurgeoldslack")
                && !r.contains("xqpurgedeleted")
        })
    })
    .await;
    for (t, since) in [("xqpurgedeleted", deleting), ("xqpurgeoldslack", aging)] {
        let within_grace = since.elapsed() < GRACE;
        assert!(
            !in_files(t).is_empty() || !within_grace,
            "{t} was purged within the grace"
        );
    }

    // Past the grace, a night's rewrites leave acme's aged-out and deleted messages out of every file: the
    // episodes, their bodies, their facts and the index. The young one stays, and so does all of globex.
    let t = Instant::now();
    while ["xqpurgeoldslack", "xqpurgedeleted"]
        .iter()
        .any(|t| !in_files(t).is_empty())
    {
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the window never purged: {:?}",
            [in_files("xqpurgeoldslack"), in_files("xqpurgedeleted")]
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(aging.elapsed() >= GRACE, "purged within the grace");
    for t in ["xqpurgeyoungslack", "xqkeptoldslack", "xqkeptdeleted"] {
        assert!(!in_files(t).is_empty(), "{t} went with the purge");
    }
    until(
        s,
        "?org=acme",
        &bob,
        &[&acme_young],
        "acme reads its young message",
        |r| r.as_ref().is_some_and(|r| r.contains("xqpurgeyoungslack")),
    )
    .await;
    // Through Bob's pin, live all along: the young message, and nothing purged.
    until(
        s,
        &pinned,
        &bob,
        &[&acme_young, &acme_old, &acme_deleted],
        "acme through a pin taken before the purge",
        |r| {
            r.as_ref().is_some_and(|r| {
                r.contains("xqpurgeyoungslack")
                    && !r.contains("xqpurgeoldslack")
                    && !r.contains("xqpurgedeleted")
            })
        },
    )
    .await;

    // Acme's audit says how much went, never what (each was hidden at its own time, so one night or two); globex's
    // has no purge.
    let t = Instant::now();
    loop {
        let (status, body) = admin_as(s, &alice, json!({"op": "audit", "org": "acme"})).await;
        assert_eq!(status, 200, "{body}");
        let row = r#"scheduled purge (deleted at the source or past retention for over 8s): {\"episodes\":"#;
        let purged: usize = body
            .split(row)
            .skip(1)
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .unwrap()
            })
            .sum();
        if purged == 2 {
            assert!(!body.contains("xqpurge"), "{body}");
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "{purged} episodes purged in acme's audit: {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // Once it says so, their rows are in no file of the episodes table (a purge deletes the bodies first, so their
    // words leave every file a moment before their rows do); the young one's stay.
    let episode_files = |id: &str| -> Vec<PathBuf> {
        files_with(&warehouse, id)
            .into_iter()
            .filter(|f| f.to_string_lossy().contains("/episodes/"))
            .collect()
    };
    for id in [&acme_old, &acme_deleted] {
        assert_eq!(episode_files(id), Vec::<PathBuf>::new(), "episode {id}");
    }
    assert!(!episode_files(&acme_young).is_empty());
    let (status, body) = admin_as(s, &gary, json!({"op": "audit", "org": "globex"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("scheduled purge"), "{body}");
}

/// The nightly window's scheduled purge takes every version of a person's profile (`memstore::profile`) that names
/// a note it purges. Carol's profile in acme (which folds profiles and asked for the purge) is folded from six notes
/// of hers, one of them 100 days old. Then her oldest ages out (acme keeps notes 30 days) and her source deletes
/// another: what she is handed and reads at once holds neither, the profile is folded again without them and its
/// versions that held them are hidden. Past the grace, the window's first try at the purge stops right after it
/// deleted the bodies, before the rows (as a server killed there would): only a version's body said what it names.
/// The next night's purge still deletes both notes and every version naming them from every file of the warehouse
/// and the index; her other notes and the profile now standing stay.
#[tokio::test(flavor = "multi_thread")]
async fn the_nightly_window_purges_the_profile_versions_naming_what_it_purges() {
    if !crate::own_process(
        "retention::the_nightly_window_purges_the_profile_versions_naming_what_it_purges",
    ) {
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
        // The window held open, a pass every second; what is purged must have been hidden for 8 seconds first.
        std::env::set_var("AMPUP_MEMORY_MAINTAIN_WINDOW", "always");
        std::env::set_var("AMPUP_MEMORY_MAINTAIN_SECS", "1");
        std::env::set_var("AMPUP_MEMORY_MAINTAIN_PACE", "0");
        std::env::set_var("AMPUP_MEMORY_PURGE_GRACE", "8s");
        // Profiles are swept every second; folds ask the stand-in model.
        std::env::set_var("AMPUP_MEMORY_SESSION_SWEEP_SECS", "1");
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
        json!({"op": "invite", "org": "acme", "members": [{"user": "carol@acme.com"}]}),
        json!({"op": "set_settings", "org": "acme", "settings": {"purge": true}}),
    ] {
        admin(s, change).await;
    }
    let [alice, carol] = ["alice@acme.com", "carol@acme.com"].map(|u| id_token(&issuer, u));
    let acme = Scope::org("acme").unwrap();
    store.ensure_scope(&acme).await.unwrap();
    // Her oldest note, from 100 days ago (written as her `remember_memory` would be, but dated then).
    let mut aged = Episode::sourced(
        "mcp:remember_memory:carol@acme.com:notes",
        b"xprofagednote preference: indent with tabs",
        &["user:carol@acme.com"],
    );
    aged.occurred_at = chrono::Utc::now().timestamp_micros() - 100 * DAY;
    let aged_id = aged.episode_id.clone();
    store
        .commit(&acme, "carol@acme.com", &[aged], &[])
        .await
        .unwrap();
    let tool = |name: &str, args: Value| {
        let (bearer, name) = (carol.clone(), name.to_string());
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
    let mut deleted = String::new();
    for t in [
        "xprofdeletednote",
        "xprofkeepzero",
        "xprofkeepone",
        "xprofkeeptwo",
        "xprofkeepthree",
    ] {
        let said = tool(
            "remember_memory",
            json!({"source": "notes", "body": format!("{t} preference: indent with spaces")}),
        )
        .await;
        assert!(said.contains("visible to user:carol@acme.com"), "{said}");
        if t == "xprofdeletednote" {
            deleted = said
                .split("episode ")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .to_string();
        }
    }
    // Her profile, folded from all six: turned on once they are written, since her 100-day-old note makes it due at
    // the server's next sweep, and one in the middle of her writes would fold only some (the rest, fewer than five,
    // wait a day).
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "settings": {"profile": true}}),
    )
    .await;
    let doc = memstore::profile::document("carol@acme.com");
    let carol_reads = memstore::Principal::new("carol@acme.com", &[]);
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
    let (first, _) = loop {
        if let Some((id, body)) = profile().await
            && body.contains("xprofagednote")
            && body.contains("xprofdeletednote")
            && body.contains("xprofkeepthree")
        {
            break (id, body);
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "her profile was never folded: {:?}",
            profile().await
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    // Her oldest note ages out, and her source deletes another.
    let (status, body) = admin_as(
        s,
        &alice,
        json!({"op": "retention", "org": "acme", "rules": ["mcp:remember_memory:=30d"]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    store
        .retract_episodes(&acme, "connector", &AclTarget::Episode(deleted.clone()))
        .await
        .unwrap();
    // What she is handed holds neither once the server reads past them (its snapshot is up to a second old): by a
    // start with what she asked, and by the first ask of a session whose start had nothing asked. The profile is
    // folded again without them.
    let task = "how should I indent this file, with spaces?";
    let t = Instant::now();
    loop {
        let handed = tool("start_session", json!({"harness": "test", "context": task})).await;
        let started = tool("start_session", json!({"harness": "test"})).await;
        let sid = serde_json::from_str::<Value>(&started).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let asked = tool("get_advice", json!({"session_id": sid, "question": task})).await;
        assert!(asked.contains("xprofkeepzero"), "{asked}");
        let handed = format!("{handed}\n{asked}");
        assert!(handed.contains("xprofkeepzero"), "{handed}");
        if !handed.contains("xprofagednote") && !handed.contains("xprofdeletednote") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{handed}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let t = Instant::now();
    loop {
        let now = profile().await;
        // An empty read is a version's body caught mid-write (hiding the older versions writes theirs again, in place
        // on a local disk): not one yet.
        if let Some((id, body)) = &now
            && !body.is_empty()
            && !body.contains("xprofagednote")
            && !body.contains("xprofdeletednote")
        {
            assert!(id != &first && body.contains("xprofkeepzero"), "{body}");
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "her profile was never folded again: {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // Past the grace, the window's purge deletes both notes' and those versions' bodies and the indexes, and its
    // first try stops there, before the rows go.
    // SAFETY: this test runs in a process of its own (`own_process`); the server reads it only there.
    unsafe { std::env::set_var("AMPUP_MEMORY_STOP_BEFORE_REWRITE", "episodes") };
    for t in ["xprofagednote", "xprofdeletednote"] {
        assert!(!in_files(t).is_empty(), "{t} is stored until the purge");
    }
    let t = Instant::now();
    while ["xprofagednote", "xprofdeletednote"]
        .iter()
        .any(|t| !in_files(t).is_empty())
    {
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the window never purged them: {:?}",
            [in_files("xprofagednote"), in_files("xprofdeletednote")]
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let episode_files = |id: &str| -> Vec<PathBuf> {
        files_with(&warehouse, id)
            .into_iter()
            .filter(|f| f.to_string_lossy().contains("/episodes/"))
            .collect()
    };
    assert!(
        !episode_files(&first).is_empty(),
        "the stopped purge left the rows of the version naming them"
    );
    // SAFETY: as above.
    unsafe { std::env::remove_var("AMPUP_MEMORY_STOP_BEFORE_REWRITE") };

    // The next night's purge finishes: once acme's audit says it went, both notes and every version naming them are
    // in no file.
    let t = Instant::now();
    loop {
        let (status, body) = admin_as(s, &alice, json!({"op": "audit", "org": "acme"})).await;
        assert_eq!(status, 200, "{body}");
        let row = r#"scheduled purge (deleted at the source or past retention for over 8s): {\"episodes\":"#;
        let purged: usize = body
            .split(row)
            .skip(1)
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .unwrap()
            })
            .sum();
        if purged >= 2 {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "{purged} episodes purged in acme's audit: {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    for t in ["xprofagednote", "xprofdeletednote"] {
        assert_eq!(in_files(t), Vec::<PathBuf>::new(), "{t}");
    }
    assert_eq!(
        episode_files(&first),
        Vec::<PathBuf>::new(),
        "the version naming them"
    );
    // Every version of hers still stored has its body, and cites neither note.
    store.invalidate(&acme.namespace(), memstore::EPISODES);
    let source = memstore::profile::source("carol@acme.com");
    let versions: Vec<String> = store
        .rows(&acme.namespace(), memstore::EPISODES)
        .await
        .unwrap()
        .rows
        .iter()
        .flat_map(|b| {
            memstore::strings(b, "episode_id")
                .into_iter()
                .zip(memstore::strings(b, "source"))
        })
        .filter(|(_, s)| *s == source)
        .map(|(id, _)| id)
        .collect();
    let bodies = store.episode_bodies(&acme, versions.clone()).await.unwrap();
    for v in &versions {
        let body = String::from_utf8_lossy(
            bodies
                .get(v)
                .unwrap_or_else(|| panic!("version {v} is stored without its body")),
        );
        assert!(
            !body.contains(&aged_id) && !body.contains(&deleted),
            "version {v} names a purged note: {body}"
        );
    }
    for t in ["xprofkeepzero", "xprofkeepthree"] {
        assert!(!in_files(t).is_empty(), "{t} went with the purge");
    }
    let (_, body) = profile().await.expect("the profile now standing");
    assert!(
        body.contains("xprofkeepzero") && !body.contains("xprofagednote"),
        "{body}"
    );
}
