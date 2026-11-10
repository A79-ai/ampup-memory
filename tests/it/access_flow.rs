//! Who can read what, end to end: a real `memstore serve` over HTTP, callers authenticating with API keys and
//! signed JWTs, episodes written through the store the way the app's writer would, membership pushed to
//! `/closure` the way Slack join/leave events would be.

use std::sync::Arc;
use std::time::{Duration, Instant};

use memstore::auth::{self, JwtConfig, TokenAuth, slack};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Principal, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const ORG: &str = "o1";
const SECRET: &str = "0123456789abcdef0123456789abcdef";
const APP_KEY: &str = "msk_app_key_for_tests";
const OPS_KEY: &str = "msk_ops_admin_key_for_tests";

struct Env {
    port: u16,
    store: Store,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

fn token_auth() -> TokenAuth {
    let key_row = |id: &str, key: &str, groups: &[&str], admin: bool| {
        json!({"id": id, "sha256": auth::hex(&sha2::Sha256::digest(key.as_bytes())),
               "user": format!("svc:{id}"), "groups": groups, "admin": admin})
    };
    let keys = json!([
        key_row("app", APP_KEY, &[ORG], false),
        key_row("ops", OPS_KEY, &[], true)
    ]);
    let jwt = JwtConfig::new(
        auth::KeySource::Secret(SECRET.into()),
        auth::JwtOptions::new("ampup", "memstore").with_org("org", ORG),
    )
    .unwrap();
    TokenAuth::new(Some(&keys.to_string()), Some(jwt)).unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A store on a local catalog, served over HTTP with token auth. The server shares the store's catalog.
async fn start() -> Env {
    start_with(None).await
}

/// The same, keeping disabled users in `disabled_file`.
async fn start_with(disabled_file: Option<std::path::PathBuf>) -> Env {
    start_auth(token_auth(), disabled_file).await
}

/// The same, with the given authentication.
async fn start_auth(auth: TokenAuth, disabled_file: Option<std::path::PathBuf>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    // Its own index, not the default location every other test (and test run) on this machine shares.
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    serve_on(store, auth, disabled_file, None, dir).await
}

/// A server on `store`'s catalog: another server beside one already running there (as during a rolling
/// deploy), when `store` is that one's. `team_csv`: its own team csv (`MEMSTORE_TEAM_CSV`).
async fn serve_on(
    store: Store,
    auth: TokenAuth,
    disabled_file: Option<std::path::PathBuf>,
    team_csv: Option<std::path::PathBuf>,
    dir: tempfile::TempDir,
) -> Env {
    let catalog = store.catalog();
    let port = free_port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            disabled_file,
            team_csv,
            ..Default::default()
        },
    ));
    let http = reqwest::Client::new();
    let up = Instant::now();
    while http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_err()
    {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Env {
        port,
        store,
        http,
        _dir: dir,
    }
}

/// A token the app's auth would mint for a user, with their flattened groups.
fn jwt(user: &str, groups: &[&str], org: &str, valid_for: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &json!({"sub": user, "email": user, "org": org, "iss": "ampup", "aud": "memstore",
                "exp": now + valid_for, "groups": groups}),
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

impl Env {
    /// Once `/ready` says 200: the server has read the directory (and put its disabled-users file's users in it).
    async fn ready(&self) {
        let t = Instant::now();
        loop {
            let res = self
                .http
                .get(format!("http://127.0.0.1:{}/ready", self.port))
                .send()
                .await
                .unwrap();
            if res.status() == 200 {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "never ready");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// POST to the server; the status and JSON body.
    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self
            .http
            .post(format!("http://127.0.0.1:{}{path}", self.port))
            .body(body.to_string());
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }

    /// The episodes a caller can read, out of `ids`: what a user sees when they open cited sources.
    async fn readable(&self, token: &str, ids: &[&str]) -> Vec<bool> {
        let rows: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| json!([i, id]))
            .collect();
        let (status, body) = self
            .post("/episode", Some(token), json!({"data": rows}))
            .await;
        assert_eq!(status, 200, "{body}");
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| !r[1].is_null())
            .collect()
    }

    async fn can_read(&self, token: &str, id: &str) -> bool {
        self.readable(token, &[id]).await[0]
    }

    /// The same, once the server's view has caught up with a write (its snapshot cache is ~1 s stale).
    async fn eventually(&self, token: &str, id: &str, want: bool) {
        let t = Instant::now();
        while self.can_read(token, id).await != want {
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "{id}: still not readable={want}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn push_closure(&self, token: &str, user: &str, add: &[&str], remove: &[&str]) -> u16 {
        self.post(
            "/closure",
            Some(token),
            json!({"user": user, "add": add, "remove": remove}),
        )
        .await
        .0
    }

    /// Writes an episode (with a fact it supports) the way the writer would.
    async fn ingest(&self, source: &str, text: &str, acl: &[String]) -> Episode {
        let acl: Vec<&str> = acl.iter().map(String::as_str).collect();
        let ep = Episode::new(source, text.as_bytes(), &acl);
        let fact = Fact::new("acme", "note", text).from_episodes(&[&ep]);
        self.store
            .commit(&Scope::Org, "writer", std::slice::from_ref(&ep), &[fact])
            .await
            .unwrap();
        ep
    }
}

/// A rep's week: they're refused until they authenticate, see only what their teams and accounts allow, gain
/// a private Slack channel when they join it and lose it when they leave, and lose a call when its source
/// revokes them.
#[tokio::test]
async fn a_users_access_follows_their_token_their_slack_channels_and_their_sources() {
    let env = start().await;
    let general = env
        .ingest(
            "slack:C00",
            "#general: renewal is on track",
            &slack::episode_acl(ORG, "C01", false),
        )
        .await;
    let deal_room = env
        .ingest(
            "slack:C00",
            "#deal-room: legal wants a discount",
            &slack::episode_acl(ORG, "C09", true),
        )
        .await;
    let acme_call = env
        .ingest(
            "meeting:acme",
            "acme call: champion is leaving",
            &["group:o1.acct.acme".to_string()],
        )
        .await;

    let org_wide = jwt("ben@x.com", &[ORG], ORG, 3600);
    env.eventually(&org_wide, &general.episode_id, true).await; // the server has caught up with the writes

    // Nobody gets in without proving who they are.
    for token in [None, Some("msk_not_a_real_key"), Some("garbage")] {
        let (status, _) = env
            .post(
                "/episode",
                token,
                json!({"data": [[0, general.episode_id]]}),
            )
            .await;
        assert_eq!(status, 401, "{token:?}");
    }
    for (why, token) in [
        ("expired", jwt("amy@x.com", &[ORG], ORG, -600)),
        ("another org", jwt("amy@x.com", &[ORG], "o2", 60)),
    ] {
        let (status, _) = env
            .post(
                "/episode",
                Some(&token),
                json!({"data": [[0, general.episode_id]]}),
            )
            .await;
        assert_eq!(status, 401, "{why}");
    }

    // Amy is in the org and on the Acme account team; Ben is only in the org.
    let amy = jwt("amy@x.com", &[ORG, "o1.acct.acme"], ORG, 3600);
    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    assert_eq!(
        env.readable(
            &amy,
            &[
                &general.episode_id,
                &deal_room.episode_id,
                &acme_call.episode_id
            ]
        )
        .await,
        [true, false, true]
    );
    assert_eq!(
        env.readable(
            &ben,
            &[
                &general.episode_id,
                &deal_room.episode_id,
                &acme_call.episode_id
            ]
        )
        .await,
        [true, false, false]
    );

    // Amy joins the private channel: Slack's event is pushed, and her same token now reads its history.
    let join = slack::closure_entry(ORG, "C09");
    assert_eq!(
        env.push_closure(OPS_KEY, "amy@x.com", &[&join], &[]).await,
        200
    );
    assert!(env.can_read(&amy, &deal_room.episode_id).await);
    assert!(
        !env.can_read(&ben, &deal_room.episode_id).await,
        "a push to Amy reaches only Amy"
    );

    // A service key, or a user's token, can't push membership: Ben's attempt changes nothing.
    assert_eq!(
        env.push_closure(APP_KEY, "ben@x.com", &[&join], &[]).await,
        401
    );
    assert_eq!(
        env.push_closure(&ben, "ben@x.com", &[&join], &[]).await,
        401
    );
    assert!(!env.can_read(&ben, &deal_room.episode_id).await);

    // She leaves it: access goes at once, no re-minting, no re-indexing.
    assert_eq!(
        env.push_closure(OPS_KEY, "amy@x.com", &[], &[&join]).await,
        200
    );
    assert!(!env.can_read(&amy, &deal_room.episode_id).await);
    assert!(env.can_read(&amy, &general.episode_id).await);

    // Acme's call arrives again from its source with Amy's team revoked: the newest ACL wins.
    assert!(env.can_read(&amy, &acme_call.episode_id).await);
    let revoked = Episode::new(
        "meeting:acme",
        b"acme call: champion is leaving",
        &["group:o1.acct.acme-execs"],
    );
    assert_eq!(revoked.episode_id, acme_call.episode_id);
    env.store
        .commit(&Scope::Org, "writer", &[revoked], &[])
        .await
        .unwrap();
    env.eventually(&amy, &acme_call.episode_id, false).await;
    let exec = jwt("eve@x.com", &[ORG, "o1.acct.acme-execs"], ORG, 3600);
    assert!(env.can_read(&exec, &acme_call.episode_id).await);

    // The service key reads as its own groups: the org, nothing private.
    assert_eq!(
        env.readable(APP_KEY, &[&general.episode_id, &deal_room.episode_id])
            .await,
        [true, false]
    );
}

/// A Slack channel goes private: the source's change feed tells the engine, and everyone who isn't a member
/// loses the channel's whole history (and the facts it supports) on their next read, while other channels
/// are untouched. Only an admin key can send the feed, and replaying it changes nothing.
#[tokio::test]
async fn a_channel_going_private_removes_its_history_from_non_members() {
    let env = start().await;
    let public = slack::episode_acl(ORG, "C05", false);
    let history: Vec<Episode> = channel_history(&env, &public).await;
    let other = env
        .ingest("slack:C06:9.9", "#c06: lunch is at noon", &public)
        .await;
    let amy = jwt(
        "amy@x.com",
        &[ORG, &slack::closure_entry(ORG, "C05")],
        ORG,
        3600,
    );
    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    for e in &history {
        env.eventually(&ben, &e.episode_id, true).await;
    }

    let private = slack::episode_acl(ORG, "C05", true);
    let feed = async |token: Option<&str>, body: Value| env.post("/acl", token, body).await;
    let change = json!({"source_prefix": "slack:C05:", "acl": private});

    // Only an admin key may send the feed; everything else is refused and changes nothing.
    for token in [None, Some(APP_KEY), Some(ben.as_str())] {
        assert_eq!(feed(token, change.clone()).await.0, 401, "{token:?}");
    }
    assert!(env.can_read(&ben, &history[0].episode_id).await);
    // A malformed change is rejected whole.
    for bad in [
        json!({"acl": private}),
        json!({"source_prefix": "slack:C05:", "source": "slack:C05:1.1", "acl": private}),
        json!({"source_prefix": "", "acl": private}),
        json!({"source_prefix": "slack:C05:", "acl": []}),
        json!({"source_prefix": "slack:C05:", "acl": ["bogus"]}),
        json!({"source_prefix": "slack:C05:"}),
    ] {
        assert_eq!(feed(Some(OPS_KEY), bad.clone()).await.0, 400, "{bad}");
    }
    assert!(env.can_read(&ben, &history[0].episode_id).await);

    // The channel goes private: its three messages change, nothing else does.
    let (status, body) = feed(Some(OPS_KEY), change.clone()).await;
    assert_eq!((status, body["changed"].as_u64()), (200, Some(3)), "{body}");
    for e in &history {
        env.eventually(&ben, &e.episode_id, false).await;
        assert!(
            env.can_read(&amy, &e.episode_id).await,
            "a member keeps their channel"
        );
    }
    assert!(
        env.can_read(&ben, &other.episode_id).await,
        "another channel is untouched"
    );
    let ben_facts = env
        .store
        .read(&Scope::Org, &Principal::new("ben@x.com", &[ORG]), None)
        .await
        .unwrap();
    assert_eq!(
        ben_facts.len(),
        1,
        "only the other channel's fact is left for a non-member"
    );

    // Replaying the feed changes nothing; the text and sources survive the change.
    let (_, body) = feed(Some(OPS_KEY), change).await;
    assert_eq!(body["changed"].as_u64(), Some(0), "{body}");
    let (_, one) = env
        .post(
            "/episode",
            Some(&amy),
            json!({"data": [[0, history[1].episode_id]]}),
        )
        .await;
    assert!(
        one["data"][0][1].as_str().unwrap().contains("renewal"),
        "{one}"
    );

    // One episode can be named by id: it goes to a single user, and the member loses it.
    let (_, body) = feed(
        Some(OPS_KEY),
        json!({"episode_id": history[2].episode_id, "acl": ["user:ben@x.com"]}),
    )
    .await;
    assert_eq!(body["changed"].as_u64(), Some(1), "{body}");
    env.eventually(&amy, &history[2].episode_id, false).await;
    assert!(env.can_read(&ben, &history[2].episode_id).await);
}

/// Every read path a member has over HTTP, as one string: fact search, facts at every time, passages and the
/// episodes `ids` name.
async fn everything_readable(env: &Env, token: &str, text: &str, ids: &[&str]) -> String {
    let mut out = String::new();
    for (path, body) in [
        ("/search", json!({"calls": [[text, 50, -1, 400]]})),
        (
            "/query",
            json!({"calls": [[{"find": "facts", "time": "all", "limit": 500}]]}),
        ),
        (
            "/query",
            json!({"calls": [[{"find": "passages", "text": text, "limit": 100}]]}),
        ),
        ("/episode", json!({"calls": ids})),
    ] {
        let (status, answer) = env.post(path, Some(token), body).await;
        assert_eq!(status, 200, "{path}: {answer}");
        out.push_str(&answer.to_string());
    }
    out
}

/// A Slack message is deleted at its source: the connector sends the delete, and the message and the fact it
/// supports are gone for every reader, now and as of any earlier time, on every read path. A late retry of
/// the message and a later ACL change don't bring it back, replaying the delete does nothing, a whole channel
/// can go at once, and other channels are untouched. Only an admin key can send it.
#[tokio::test]
async fn a_message_deleted_at_its_source_is_gone_for_everyone_and_stays_gone() {
    let env = start().await;
    let public = slack::episode_acl(ORG, "C05", false);
    let history = channel_history(&env, &public).await;
    let other = env
        .ingest("slack:C06:9.9", "#c06: lunch is at noon", &public)
        .await;
    let ids: Vec<&str> = history
        .iter()
        .chain([&other])
        .map(|e| e.episode_id.as_str())
        .collect();
    let deleted = &history[1];
    let text = "legal wants a discount on the renewal";
    let amy = jwt(
        "amy@x.com",
        &[ORG, &slack::closure_entry(ORG, "C05")],
        ORG,
        3600,
    );
    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    env.eventually(&ben, &other.episode_id, true).await;
    for who in [&amy, &ben] {
        assert!(
            everything_readable(&env, who, text, &ids)
                .await
                .contains(text)
        );
    }
    let before_delete = chrono::Utc::now().timestamp_micros();
    let retract = async |token: Option<&str>, body: Value| env.post("/retract", token, body).await;
    let one = json!({"source": deleted.source});

    // Only an admin may send it; everything else is refused and changes nothing: no credential is
    // unauthorized, and a caller who signed in but isn't an admin is forbidden.
    assert_eq!(retract(None, one.clone()).await.0, 401);
    for token in [APP_KEY, ben.as_str(), amy.as_str()] {
        assert_eq!(retract(Some(token), one.clone()).await.0, 403, "{token:?}");
    }
    // A malformed delete is rejected whole.
    for bad in [
        json!({}),
        json!({"source": "slack:C05:1.1", "episode_id": deleted.episode_id}),
        json!({"source_prefix": "s"}),
        json!({"source": ""}),
    ] {
        assert_eq!(retract(Some(OPS_KEY), bad.clone()).await.0, 400, "{bad}");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(env.can_read(&ben, &deleted.episode_id).await);

    // The message is deleted: it goes for everyone, member or not, on every read path.
    let (status, body) = retract(Some(OPS_KEY), one.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["episode_ids"], json!([deleted.episode_id]), "{body}");
    for who in [&amy, &ben] {
        env.eventually(who, &deleted.episode_id, false).await;
        let seen = everything_readable(&env, who, text, &ids).await;
        assert!(!seen.contains(text), "{seen}");
        assert!(
            seen.contains("lunch is at noon"),
            "other channels stay: {seen}"
        );
        assert!(
            seen.contains("renewal moved to june"),
            "the rest of the channel stays: {seen}"
        );
    }
    // Gone at every time, not just now: the fact it supported was never true.
    let reader = Principal::new("ben@x.com", &[ORG]);
    for at in [None, Some(before_delete)] {
        let facts = facts_once_caught_up(&env, &reader, at, 3).await;
        assert!(facts.iter().all(|f| !f.value.contains(text)), "{facts:?}");
    }

    // Replaying the delete retracts nothing.
    let (_, body) = retract(Some(OPS_KEY), one).await;
    assert_eq!(body["retracted"].as_u64(), Some(0), "{body}");
    // A late retry of the message, and the channel's next ACL change, don't bring it back.
    env.ingest(
        &deleted.source,
        "#c05: legal wants a discount on the renewal",
        &public,
    )
    .await;
    let (_, body) = env
        .post(
            "/acl",
            Some(OPS_KEY),
            json!({"source_prefix": "slack:C05:", "acl": slack::episode_acl(ORG, "C05", true)}),
        )
        .await;
    assert_eq!(
        body["changed"].as_u64(),
        Some(2),
        "only the live messages change: {body}"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!env.can_read(&amy, &deleted.episode_id).await);
    assert!(
        !everything_readable(&env, &amy, text, &ids)
            .await
            .contains(text)
    );

    // The whole channel is deleted; the other channel stays.
    let (_, body) = retract(Some(OPS_KEY), json!({"source_prefix": "slack:C05:"})).await;
    assert_eq!(body["retracted"].as_u64(), Some(2), "{body}");
    for e in &history {
        env.eventually(&amy, &e.episode_id, false).await;
    }
    assert!(env.can_read(&ben, &other.episode_id).await);
    let facts = facts_once_caught_up(&env, &reader, None, 1).await;
    assert_eq!(facts[0].value, "#c06: lunch is at noon");
}

/// The facts `who` reads as of `at`, once this test's store has caught up to `n` of them (its snapshot cache
/// is ~1 s stale after the server's writes).
async fn facts_once_caught_up(env: &Env, who: &Principal, at: Option<i64>, n: usize) -> Vec<Fact> {
    let t = Instant::now();
    loop {
        let facts = env.store.read(&Scope::Org, who, at).await.unwrap();
        if facts.len() == n {
            return facts;
        }
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "as of {at:?}: {facts:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A branch made before a delete at the source doesn't keep what was deleted: the source's delete holds on
/// every branch, not just main.
#[tokio::test]
async fn a_branch_made_before_a_delete_loses_the_deleted_message_too() {
    let env = start().await;
    let public = slack::episode_acl(ORG, "C05", false);
    let history = channel_history(&env, &public).await;
    env.store
        .create_branch(&Scope::Org, "trial", "tester")
        .await
        .unwrap();
    let reader = Principal::new("ben@x.com", &[ORG]);
    let on_branch = env
        .store
        .read_on(&Scope::Org, "trial", &reader, None)
        .await
        .unwrap();
    assert_eq!(on_branch.len(), 3, "{on_branch:?}");

    let (status, body) = env
        .post(
            "/retract",
            Some(OPS_KEY),
            json!({"source": history[1].source}),
        )
        .await;
    assert_eq!(
        (status, body["retracted"].as_u64()),
        (200, Some(1)),
        "{body}"
    );
    let t = Instant::now();
    loop {
        let facts = env
            .store
            .read_on(&Scope::Org, "trial", &reader, None)
            .await
            .unwrap();
        if facts.len() == 2 {
            assert!(
                facts.iter().all(|f| !f.value.contains("legal wants")),
                "{facts:?}"
            );
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "the branch kept it: {facts:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The channel's three messages, one source each, readable by `acl`.
async fn channel_history(env: &Env, acl: &[String]) -> Vec<Episode> {
    let mut out = vec![];
    for (i, text) in [
        "#c05: renewal moved to june",
        "#c05: legal wants a discount on the renewal",
        "#c05: champion is leaving acme",
    ]
    .iter()
    .enumerate()
    {
        out.push(env.ingest(&format!("slack:C05:1.{i}"), text, acl).await);
    }
    out
}

/// The feed acts on what is true now: changes sent back to back each see the last one (so a channel made
/// private, then public, then private again ends private), an episode now sourced from another channel
/// isn't swept up by its old channel's change, and the feed refuses the grants that would expose everything.
#[tokio::test]
async fn the_change_feed_acts_on_current_state_and_refuses_over_broad_grants() {
    let env = start().await;
    let public = slack::episode_acl(ORG, "C05", false);
    let private = slack::episode_acl(ORG, "C05", true);
    let msg = env
        .ingest("slack:C05:1.0", "#c05: renewal moved to june", &public)
        .await;
    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    env.eventually(&ben, &msg.episode_id, true).await;

    // private, public, private again, with no pause for the server to catch up in between.
    for (acl, changed, readable) in [
        (&private, 1, false),
        (&public, 1, true),
        (&private, 1, false),
    ] {
        let (status, body) = env
            .post(
                "/acl",
                Some(OPS_KEY),
                json!({"source_prefix": "slack:C05:", "acl": acl}),
            )
            .await;
        assert_eq!(
            (status, body["changed"].as_u64()),
            (200, Some(changed)),
            "{body}"
        );
        env.eventually(&ben, &msg.episode_id, readable).await;
    }

    // The same text arrives again from channel C07 (a cross-post): that is now its source. C05's change
    // doesn't touch it, and doesn't bring back C05 as its source.
    let cross = Episode::new(
        "slack:C07:2.0",
        b"#c05: renewal moved to june",
        &["group:o1"],
    );
    assert_eq!(cross.episode_id, msg.episode_id);
    env.store
        .commit(&Scope::Org, "writer", &[cross], &[])
        .await
        .unwrap();
    env.eventually(&ben, &msg.episode_id, true).await;
    let (_, body) = env
        .post(
            "/acl",
            Some(OPS_KEY),
            json!({"source_prefix": "slack:C05:", "acl": private}),
        )
        .await;
    assert_eq!(body["changed"].as_u64(), Some(0), "{body}");
    assert!(env.can_read(&ben, &msg.episode_id).await);

    // Grants that would expose everything, or sweep up whole sources by accident, are refused.
    for bad in [
        json!({"source_prefix": "slack:C07:", "acl": ["*"]}),
        json!({"source_prefix": "s", "acl": private}),
    ] {
        assert_eq!(
            env.post("/acl", Some(OPS_KEY), bad.clone()).await.0,
            400,
            "{bad}"
        );
    }
}

/// An account is deprovisioned: every request as that user is refused at once, whatever the casing the IdP
/// uses, their group edits survive a re-enable, other users carry on, only an admin key can do it, an admin
/// can't lock out their own key, and a restart doesn't let them back in.
#[tokio::test]
async fn a_deprovisioned_user_is_refused_at_once_and_stays_refused_after_a_restart() {
    let file_dir = tempfile::tempdir().unwrap();
    let file = file_dir.path().join("disabled.json");
    let env = start_with(Some(file.clone())).await;
    let general = env
        .ingest(
            "slack:C01:1.0",
            "#general: renewal is on track",
            &slack::episode_acl(ORG, "C01", false),
        )
        .await;
    let private = env
        .ingest(
            "slack:C09:1.0",
            "#deal-room: legal wants a discount",
            &slack::episode_acl(ORG, "C09", true),
        )
        .await;
    let (amy, amy_upper) = (
        jwt("amy@x.com", &[ORG], ORG, 3600),
        jwt("Amy@X.com", &[ORG], ORG, 3600),
    );
    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    env.eventually(&amy, &general.episode_id, true).await;
    let read = async |token: &str, id: &str| {
        env.post("/episode", Some(token), json!({"data": [[0, id]]}))
            .await
    };
    let join = slack::closure_entry(ORG, "C09");
    assert_eq!(
        env.push_closure(OPS_KEY, "amy@x.com", &[&join], &[]).await,
        200
    );
    assert!(env.can_read(&amy, &private.episode_id).await);

    // Nobody but an admin key can disable anyone.
    let disable = async |token: &str, user: &str, on: bool| {
        env.post(
            "/closure",
            Some(token),
            json!({"user": user, "disabled": on}),
        )
        .await
    };
    for token in [APP_KEY, ben.as_str()] {
        assert_eq!(disable(token, "amy@x.com", true).await.0, 401);
    }
    assert_eq!(read(&amy, &general.episode_id).await.0, 200);

    // Disabled: her still-valid token, and the same address in another case, are refused straight away.
    assert_eq!(disable(OPS_KEY, "amy@x.com", true).await.0, 200);
    for token in [&amy, &amy_upper] {
        assert_eq!(read(token, &general.episode_id).await.0, 401);
    }
    assert_eq!(
        read(&ben, &general.episode_id).await.0,
        200,
        "others carry on"
    );

    // Another server on the same catalog (a rolling deploy's new server, beside the old one) refuses her
    // too, though she was disabled through the first. Let back in through it, she is in on the first as
    // well, though the first's file still lists her; disabled again through it, the first refuses her.
    let other = serve_on(
        env.store.clone(),
        token_auth(),
        None,
        None,
        tempfile::tempdir().unwrap(),
    )
    .await;
    let status_on = async |e: &Env, token: &str| {
        e.post(
            "/episode",
            Some(token),
            json!({"data": [[0, general.episode_id]]}),
        )
        .await
        .0
    };
    // Within the directory's staleness bound (MEMSTORE_MAX_STALENESS_MS, 1s), never later.
    let settles = async |e: &Env, token: &str, want: u16| {
        let t = Instant::now();
        while status_on(e, token).await != want {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "still not {want} on the other server"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    assert_eq!(
        status_on(&other, &amy).await,
        401,
        "disabled through one server, refused by another"
    );
    assert_eq!(status_on(&other, &amy_upper).await, 401);
    assert_eq!(status_on(&other, &ben).await, 200);
    let disable_on = async |e: &Env, on: bool| {
        e.post(
            "/closure",
            Some(OPS_KEY),
            json!({"user": "amy@x.com", "disabled": on}),
        )
        .await
        .0
    };
    assert_eq!(disable_on(&other, false).await, 200);
    assert_eq!(status_on(&other, &amy).await, 200);
    settles(&env, &amy, 200).await;
    assert_eq!(disable_on(&other, true).await, 200);
    settles(&env, &amy, 401).await;
    assert_eq!(status_on(&other, &amy).await, 401);
    // A service key's user can be disabled too; an admin can't lock out their own key.
    assert_eq!(disable(OPS_KEY, "svc:app", true).await.0, 200);
    assert_eq!(read(APP_KEY, &general.episode_id).await.0, 401);
    assert_eq!(disable(OPS_KEY, "svc:ops", true).await.0, 401);
    assert_eq!(disable(OPS_KEY, "svc:app", false).await.0, 200);
    assert_eq!(read(APP_KEY, &general.episode_id).await.0, 200);

    // A restart on the same file keeps her out without anyone telling it.
    let again = start_with(Some(file.clone())).await;
    let (status, _) = again
        .post(
            "/episode",
            Some(&amy),
            json!({"data": [[0, general.episode_id]]}),
        )
        .await;
    assert_eq!(status, 401, "disabled users stay disabled across a restart");
    let (status, _) = again
        .post(
            "/episode",
            Some(&ben),
            json!({"data": [[0, general.episode_id]]}),
        )
        .await;
    assert_eq!(status, 200);

    // Re-enabled, she is back with the channel she had joined: disabling didn't wipe her group edits.
    assert_eq!(disable(OPS_KEY, "amy@x.com", false).await.0, 200);
    assert!(env.can_read(&amy, &general.episode_id).await);
    assert!(env.can_read(&amy, &private.episode_id).await);

    // A server started with a disabled-users file listing someone the directory has never heard of puts them
    // in the directory: every other server on the catalog refuses them too, not only the one with the file.
    let carl = jwt("carl@x.com", &[ORG], ORG, 3600);
    assert_eq!(status_on(&other, &carl).await, 200);
    let carl_file = file_dir.path().join("carl.json");
    std::fs::write(&carl_file, r#"["Carl@X.com"]"#).unwrap();
    let with_file = serve_on(
        env.store.clone(),
        token_auth(),
        Some(carl_file),
        None,
        tempfile::tempdir().unwrap(),
    )
    .await;
    assert_eq!(status_on(&with_file, &carl).await, 401);
    with_file.ready().await;
    settles(&other, &carl, 401).await;
    settles(&env, &carl, 401).await;
    assert_eq!(status_on(&other, &ben).await, 200, "others carry on");

    // A team csv in a server's config and one pushed to the directory: a leaver taken out of either loses
    // the team's memory, on every server; clearing the push leaves the server's own csv.
    let team_dir = tempfile::tempdir().unwrap();
    let csv = |people: &[&str]| {
        let mut s = "email,name,manager,role,team\n".to_string();
        for p in people {
            s.push_str(&format!("{p},{p},,ae,sales\n"));
        }
        s
    };
    let team_file = team_dir.path().join("team.csv");
    std::fs::write(&team_file, csv(&["dan@x.com", "eve@x.com"])).unwrap();
    let sales = env
        .ingest(
            "notes:sales:1",
            "sales: the forecast is down",
            &["group:team.sales".to_string()],
        )
        .await;
    let (dan, eve) = (
        jwt("dan@x.com", &[ORG], ORG, 3600),
        jwt("eve@x.com", &[ORG], ORG, 3600),
    );
    let teamed = serve_on(
        env.store.clone(),
        token_auth(),
        None,
        Some(team_file.clone()),
        tempfile::tempdir().unwrap(),
    )
    .await;
    teamed.ready().await;
    teamed.eventually(&dan, &sales.episode_id, true).await;
    assert!(
        !other.can_read(&dan, &sales.episode_id).await,
        "no csv there"
    );
    let push = async |e: &Env, body: Option<String>| {
        let req = match &body {
            Some(b) => e
                .http
                .post(format!("http://127.0.0.1:{}/team", e.port))
                .body(b.clone()),
            None => e.http.delete(format!("http://127.0.0.1:{}/team", e.port)),
        };
        let res = req
            .header("authorization", format!("Bearer {OPS_KEY}"))
            .send()
            .await
            .unwrap();
        res.status().as_u16()
    };
    // Nobody but an admin key pushes a team.
    let res = teamed
        .http
        .post(format!("http://127.0.0.1:{}/team", teamed.port))
        .header("authorization", format!("Bearer {APP_KEY}"))
        .body(csv(&["mallory@x.com"]))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 401);
    // Pushed through another server, without Dan (he left): out on the server whose own csv still lists him.
    assert_eq!(push(&other, Some(csv(&["eve@x.com"]))).await, 200);
    teamed.eventually(&dan, &sales.episode_id, false).await;
    assert!(teamed.can_read(&eve, &sales.episode_id).await);
    other.eventually(&eve, &sales.episode_id, true).await;
    assert!(!other.can_read(&dan, &sales.episode_id).await);
    // Pushed back with Dan, while a restart's config no longer lists him: out on that server all the same.
    assert_eq!(
        push(&other, Some(csv(&["dan@x.com", "eve@x.com"]))).await,
        200
    );
    other.eventually(&dan, &sales.episode_id, true).await;
    std::fs::write(&team_file, csv(&["eve@x.com"])).unwrap();
    let restarted = serve_on(
        env.store.clone(),
        token_auth(),
        None,
        Some(team_file.clone()),
        tempfile::tempdir().unwrap(),
    )
    .await;
    restarted.ready().await;
    assert!(!restarted.can_read(&dan, &sales.episode_id).await);
    assert!(restarted.can_read(&eve, &sales.episode_id).await);
    // Cleared: each server's own csv alone (none on `other`).
    assert_eq!(push(&other, None).await, 200);
    other.eventually(&eve, &sales.episode_id, false).await;
    restarted.eventually(&eve, &sales.episode_id, true).await;
    assert!(!restarted.can_read(&dan, &sales.episode_id).await);

    // A change of a directory version newer than this engine reads (a newer engine wrote it, perhaps a
    // disable it can't tell): this engine signs no one in, on every server, rather than guess.
    let mut newer = memstore::people::directory::Event::new("svc:ops", "user.someday", "");
    newer.version = memstore::people::directory::EVENT_VERSION + 1;
    env.store.append_directory(&[newer]).await.unwrap();
    settles(&env, &ben, 401).await;
    settles(&other, &ben, 401).await;
    assert_eq!(status_on(&other, APP_KEY).await, 401);
    assert_eq!(disable_on(&other, false).await, 401, "nor any admin change");
}

/// A disabled-users file that can't be read stops the server starting: coming up without it would let
/// disabled users back in.
#[tokio::test]
async fn a_server_wont_start_with_an_unreadable_disabled_users_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("disabled.json");
    std::fs::write(&file, "not json").unwrap();
    let catalog = platform::local(&format!("file://{}/wh", dir.path().display()))
        .await
        .unwrap();
    let started = serve_with(
        free_port(),
        Scope::Org,
        vec![],
        Options {
            catalog: Some(catalog),
            token_auth: Some(token_auth()),
            disabled_file: Some(file),
            ..Default::default()
        },
    )
    .await;
    assert!(
        started.is_err(),
        "the server came up without its disabled users"
    );
}

fn okta_like_auth(claims: impl FnOnce(&mut auth::JwtOptions)) -> TokenAuth {
    let mut opts = auth::JwtOptions::new("https://acme.okta.com", "api://memstore");
    claims(&mut opts);
    let jwt = JwtConfig::new(auth::KeySource::Secret(SECRET.into()), opts).unwrap();
    let keys = json!([{"id": "ops", "sha256": auth::hex(&sha2::Sha256::digest(OPS_KEY.as_bytes())),
                       "user": "svc:ops", "groups": [], "admin": true}]);
    TokenAuth::new(Some(&keys.to_string()), Some(jwt)).unwrap()
}

/// An issuer's signing key: its private half signs tokens, its public half is published in the JWKS.
struct SigningKey {
    kid: String,
    private: jsonwebtoken::EncodingKey,
    jwk: Value,
}

fn signing_key(kid: &str) -> SigningKey {
    use base64::Engine;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::traits::PublicKeyParts;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let b64 = |b: Vec<u8>| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    SigningKey {
        kid: kid.into(),
        private: jsonwebtoken::EncodingKey::from_rsa_pem(
            key.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap(),
        jwk: json!({"kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
                    "n": b64(key.n().to_bytes_be()), "e": b64(key.e().to_bytes_be())}),
    }
}

fn rs256_token(key: &SigningKey, email: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(
        &header,
        &json!({"iss": "https://acme.okta.com", "aud": "api://memstore", "exp": now + 3600,
                "sub": "00u1", "email": email, "groups": [ORG]}),
        &key.private,
    )
    .unwrap()
}

/// A stand-in for an issuer's JWKS endpoint: serves whatever keys it is told to, and counts requests.
struct JwksServer {
    url: String,
    keys: Arc<std::sync::Mutex<Vec<Value>>>,
    hits: Arc<std::sync::atomic::AtomicUsize>,
}

async fn jwks_server(initial: &[&SigningKey]) -> JwksServer {
    use http_body_util::Full;
    use hyper::body::Bytes;
    let keys = Arc::new(std::sync::Mutex::new(
        initial.iter().map(|k| k.jwk.clone()).collect::<Vec<_>>(),
    ));
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/jwks.json", listener.local_addr().unwrap());
    let (k, h) = (keys.clone(), hits.clone());
    tokio::spawn(async move {
        loop {
            let (conn, _) = listener.accept().await.unwrap();
            let (k, h) = (k.clone(), h.clone());
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |_req| {
                    h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = json!({"keys": *k.lock().unwrap()}).to_string();
                    async move {
                        Ok::<_, hyper::Error>(hyper::Response::new(Full::new(Bytes::from(body))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(conn), svc)
                    .await;
            });
        }
    });
    JwksServer { url, keys, hits }
}

/// An issuer rotates its signing key: tokens from the new key work once a token names it, the retired key's
/// tokens stop, a forged or unknown key never verifies, an HS256 token can't pass by using the published key
/// as its secret, and a flood of bad tokens doesn't become a flood of requests to the issuer.
#[tokio::test]
async fn jwks_keys_rotate_and_bad_tokens_do_not_hammer_the_issuer() {
    let (old, new, evil) = (signing_key("k1"), signing_key("k2"), signing_key("k1"));
    let jwks = jwks_server(&[&old]).await;
    let auth = okta_like_auth(|o| {
        o.jwks_min_refetch = Duration::from_secs(2);
    });
    // The same auth, but keyed by the issuer's JWKS.
    let mut opts = auth::JwtOptions::new("https://acme.okta.com", "api://memstore");
    opts.jwks_min_refetch = Duration::from_secs(2);
    let jwt_cfg = JwtConfig::new(auth::KeySource::Jwks(jwks.url.clone()), opts).unwrap();
    drop(auth);
    let keys = json!([{"id": "ops", "sha256": auth::hex(&sha2::Sha256::digest(OPS_KEY.as_bytes())),
                       "user": "svc:ops", "groups": [], "admin": true}]);
    let env = start_auth(
        TokenAuth::new(Some(&keys.to_string()), Some(jwt_cfg)).unwrap(),
        None,
    )
    .await;
    let doc = env
        .ingest("doc:plan", "org-wide plan", &[format!("group:{ORG}")])
        .await;
    let read = async |t: &str| {
        env.post("/episode", Some(t), json!({"data": [[0, doc.episode_id]]}))
            .await
            .0
    };
    let hits = || jwks.hits.load(std::sync::atomic::Ordering::SeqCst);

    let t_old = rs256_token(&old, "amy@x.com");
    let mut tries = 0;
    while read(&t_old).await != 200 {
        tries += 1;
        assert!(
            tries < 100,
            "a valid token from the published key is refused"
        );
        tokio::time::sleep(Duration::from_millis(200)).await; // the server's snapshot catching up
    }
    assert_eq!(hits(), 1, "the keys are fetched once, then cached");

    // A key with the right id but the wrong private half, and a key id nobody published, never verify, and 20
    // of them cost the issuer at most one more request per refetch interval (2 s here): a slow machine that
    // spans more than one interval may make one more each time, never one per token.
    let flood = Instant::now();
    for _ in 0..10 {
        assert_eq!(read(&rs256_token(&evil, "amy@x.com")).await, 401);
        assert_eq!(
            read(&rs256_token(&signing_key("unpublished"), "amy@x.com")).await,
            401
        );
    }
    let intervals = (flood.elapsed().as_secs_f64() / 2.0).ceil() as usize;
    assert!(
        hits() <= 1 + intervals.max(1),
        "{} requests for 20 bad tokens over {intervals} refetch intervals",
        hits()
    );

    // HS256 using the public modulus as the secret: the algorithm is pinned, so the token is refused.
    let forged = jsonwebtoken::encode(
        &{
            let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
            h.kid = Some("k1".into());
            h
        },
        &json!({"iss": "https://acme.okta.com", "aud": "api://memstore", "sub": "x", "email": "x@x.com",
                "exp": chrono::Utc::now().timestamp() + 3600, "groups": [ORG]}),
        &jsonwebtoken::EncodingKey::from_secret(old.jwk["n"].as_str().unwrap().as_bytes()),
    )
    .unwrap();
    assert_eq!(read(&forged).await, 401);

    // Rotation: the issuer now publishes only k2. A k2 token is accepted as soon as the refetch interval allows,
    // and the retired k1 stops verifying.
    *jwks.keys.lock().unwrap() = vec![new.jwk.clone()];
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let t_new = rs256_token(&new, "amy@x.com");
    assert_eq!(read(&t_new).await, 200);
    assert_eq!(
        read(&t_old).await,
        401,
        "the retired key no longer verifies"
    );
}

/// What an IdP can put in a token can't be used to become someone else: an email that only lower-cases to
/// the victim's (a Kelvin sign for K, a dotted capital I) is a different person, and a group an IdP admin
/// names `role:SALES`, or one that smuggles in a comma, grants nothing, while the ordinary groups still work.
#[tokio::test]
async fn lookalike_emails_and_odd_group_names_from_an_idp_grant_nothing() {
    let env = start().await;
    let victim = env
        .ingest(
            "note:kent",
            "kent's private note",
            &["user:kent@x.com".to_string()],
        )
        .await;
    let role = env
        .ingest("doc:report", "pipeline report", &["role:SALES".to_string()])
        .await;
    let eng = env
        .ingest("doc:plan", "engineering plan", &["group:eng".to_string()])
        .await;
    let real = jwt("KENT@x.com", &[ORG], ORG, 3600);
    env.eventually(&real, &victim.episode_id, true).await;

    // U+212A KELVIN SIGN lower-cases to a plain k; U+0130 lower-cases to i + a combining dot.
    for spoof in [
        "\u{212A}ent@x.com",
        "kent@x.com\u{0307}",
        "k\u{0307}ent@x.com",
    ] {
        let t = jwt(spoof, &[ORG], ORG, 3600);
        assert!(
            !env.can_read(&t, &victim.episode_id).await,
            "{spoof:?} read kent's note"
        );
    }
    // Disabling Kent doesn't disable the lookalike, and doesn't leave Kent in.
    let kelvin = jwt("\u{212A}ent@x.com", &[ORG], ORG, 3600);
    assert_eq!(
        env.post(
            "/closure",
            Some(OPS_KEY),
            json!({"user": "kent@x.com", "disabled": true})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.post(
            "/episode",
            Some(&real),
            json!({"data": [[0, victim.episode_id]]})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        env.post(
            "/episode",
            Some(&kelvin),
            json!({"data": [[0, victim.episode_id]]})
        )
        .await
        .0,
        200,
        "a different account isn't disabled by Kent's"
    );

    // Groups: a plain one works; one named like a role, or with a comma, a colon or a space, matches nothing.
    let odd = jwt(
        "amy@x.com",
        &[ORG, "eng", "role:SALES", "a,b", "group:eng", "x y"],
        ORG,
        3600,
    );
    assert_eq!(
        env.readable(&odd, &[&eng.episode_id, &role.episode_id])
            .await,
        [true, false]
    );
    let only_role = jwt("amy@x.com", &[ORG, "role:SALES"], ORG, 3600);
    assert!(
        !env.can_read(&only_role, &role.episode_id).await,
        "an IdP group can't be a held role"
    );
}

/// One episode as a connector sends it to `/ingest`.
fn wire(source: &str, body: &str, acl: &[&str]) -> Value {
    json!({"source": source, "occurred_at": 1_760_000_000_000_000i64, "acl": acl,
           "about": ["deal:42"], "index_text": true, "body": body})
}

#[tokio::test]
async fn ingest_refuses_batches_that_would_widen_or_smuggle_access() {
    let env = start().await;
    let refused = [
        (
            "readable by everyone",
            json!({"episodes": [wire("note:1", "b", &["*"])]}),
        ),
        (
            "readable by nobody",
            json!({"episodes": [wire("note:1", "b", &[])]}),
        ),
        (
            "a second entry smuggled in by a comma",
            json!({"episodes": [wire("note:1", "b", &["group:o1,*"])]}),
        ),
        (
            "an entry the engine doesn't know",
            json!({"episodes": [wire("note:1", "b", &["team:o1"])]}),
        ),
        (
            "no body",
            json!({"episodes": [wire("note:1", "", &["group:o1"])]}),
        ),
        (
            "no time",
            json!({"episodes": [{"source": "note:1", "body": "b", "acl": ["group:o1"]}]}),
        ),
        ("empty", json!({"episodes": []})),
        (
            "too many",
            json!({"episodes": vec![wire("note:1", "b", &["group:o1"]); 501]}),
        ),
        ("not a list", json!({"episodes": "nope"})),
    ];
    for (why, body) in refused {
        let (status, reply) = env.post("/ingest", Some(OPS_KEY), body).await;
        assert_eq!(status, 400, "{why}: {reply}");
    }
    // One bad episode refuses the whole batch: the good one beside it isn't committed either.
    let good = Episode::sourced("note:good", b"good", &[]).episode_id;
    let (status, _) = env
        .post(
            "/ingest",
            Some(OPS_KEY),
            json!({"episodes": [wire("note:good", "good", &["group:o1"]), wire("note:bad", "bad", &["*"])]}),
        )
        .await;
    assert_eq!(status, 400);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !env.can_read(&jwt("amy@x.com", &[ORG], ORG, 3600), &good)
            .await
    );

    let res = env
        .http
        .get(format!("http://127.0.0.1:{}/ingest", env.port))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 405);
}

#[tokio::test]
async fn the_same_text_from_another_source_cannot_take_over_an_episodes_acl() {
    let env = start().await;
    // A private DM, written the way the app's writer does (body-only id)...
    let dm = env
        .ingest("slack:C00", "ok, ship it", &["user:amy@x.com".to_string()])
        .await;
    // ...then a connector ingests a public record with the very same text, readable by the whole org.
    let (status, body) = env
        .post(
            "/ingest",
            Some(OPS_KEY),
            json!({"episodes": [
                wire("github:acme/app#15", "ok, ship it", &["group:o1"]),
                wire("slack:C01:1.0", "ok, ship it", &["group:o1.slack.C01"]),
            ]}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let ids: Vec<&str> = body["episode_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_ne!(ids[0], ids[1], "one id per source");
    assert!(!ids.contains(&dm.episode_id.as_str()));

    let ben = jwt("ben@x.com", &[ORG], ORG, 3600);
    env.eventually(&ben, ids[0], true).await;
    // The org sees the public record; the DM stays Amy's alone, and the channel's stays the channel's.
    assert_eq!(
        env.readable(&ben, &[ids[0], ids[1], &dm.episode_id]).await,
        vec![true, false, false]
    );
    let amy = jwt("amy@x.com", &[ORG], ORG, 3600);
    assert!(env.can_read(&amy, &dm.episode_id).await);
}

/// A model API (OpenAI-compatible) that keeps every prompt it is sent and misbehaves on purpose: asked to tidy facts,
/// it ends every fact of a thing by every other and calls every pair the same; asked about names, it makes one thing
/// of every cluster. Only the dream's own rules stand between that and the memory.
async fn eager_model() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let asked: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let log = asked.clone();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut chunk = [0u8; 8192];
                let (start, len) = loop {
                    let n = conn.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < start + len {
                    let n = conn.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req: Value = serde_json::from_slice(&buf[start..start + len]).unwrap();
                let prompt = req["messages"][0]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                log.lock().unwrap().push(prompt.clone());
                // The handles a line starts with (`F3 | ...`, `N2 name | ...`, `C1`).
                let handle = |l: &str, p: char| -> Option<String> {
                    let h = l.split([' ', '|']).next()?;
                    (h.len() > 1 && h.starts_with(p) && h[1..].bytes().all(|b| b.is_ascii_digit()))
                        .then(|| h.to_string())
                };
                let reply = if prompt.contains("Find two things") {
                    let f: Vec<String> = prompt.lines().filter_map(|l| handle(l, 'F')).collect();
                    let (mut ends, mut same) = (vec![], vec![]);
                    for a in &f {
                        for b in f.iter().filter(|b| *b != a) {
                            ends.push(json!({"old": a, "by": b, "why": "eager"}));
                            same.push(json!({"keep": a, "dup": b, "why": "eager"}));
                        }
                    }
                    json!({"ends": ends, "same": same}).to_string()
                } else {
                    let mut groups: Vec<Vec<String>> = vec![];
                    for l in prompt.lines() {
                        if handle(l, 'C').is_some() {
                            groups.push(vec![]);
                        } else if let (Some(h), Some(g)) = (handle(l, 'N'), groups.last_mut()) {
                            g.push(h);
                        }
                    }
                    let aliases: Vec<Value> = groups
                        .iter()
                        .filter(|g| g.len() > 1)
                        .map(|g| json!({"names": g, "main": g[0], "why": "eager"}))
                        .collect();
                    json!({ "aliases": aliases }).to_string()
                };
                let body = json!({
                    "choices": [{"message": {"content": reply}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": prompt.len() / 4, "completion_tokens": reply.len() / 4}
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                conn.write_all(head.as_bytes()).await.unwrap();
                conn.write_all(body.as_bytes()).await.unwrap();
                conn.shutdown().await.ok();
            });
        }
    });
    (url, asked)
}

impl Env {
    /// One episode readable by `acl`, on `day`, with the facts it gives (entity, predicate, value), written the way
    /// the writer would.
    async fn note(
        &self,
        day: &str,
        text: &str,
        acl: &str,
        facts: &[(&str, &str, &str)],
    ) -> (Episode, Vec<Fact>) {
        let at = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        let mut ep = Episode::new(&format!("note:{day}"), text.as_bytes(), &[acl]);
        ep.occurred_at = at;
        let facts: Vec<Fact> = facts
            .iter()
            .map(|(e, p, v)| {
                let mut f = Fact::new(e, p, v).from_episodes(&[&ep]);
                f.valid_from = at;
                f
            })
            .collect();
        self.store
            .commit(&Scope::Org, "writer", std::slice::from_ref(&ep), &facts)
            .await
            .unwrap();
        (ep, facts)
    }

    /// The values of the facts `token` reads under `time`, joined (`entity|predicate|value` per line).
    async fn values(&self, token: &str, time: Value) -> String {
        let (status, body) = self
            .post(
                "/query",
                Some(token),
                json!({"calls": [[{"find": "facts", "time": time, "limit": 500}]]}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body["replies"][0]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| format!("{}|{}|{}\n", f["entity"], f["predicate"], f["value"]))
            .collect()
    }

    /// The values `token` reads now, once `done` says the server has caught up with the store.
    async fn values_once(&self, token: &str, done: impl Fn(&str) -> bool) -> String {
        let t = Instant::now();
        loop {
            let v = self.values(token, json!("now")).await;
            if done(&v) {
                return v;
            }
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "never caught up: {v}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// A dream (`memstore::dream`) reads and changes only what one audience may read, whatever its model says. Two
/// teams' memories and one person's private note all hold a value for the same thing; a model that wants to end and
/// merge everything is asked about them. The model is never shown the private note, and never one team's facts with
/// the other's; each ending and alias is readable by, and applies to, only the team whose facts it rests on, and
/// the private note and the other team's facts are untouched. A forget of what a decision rests on undoes it (an
/// alias at once, an ending at the next pass), and withdrawing the dreamer takes back everything it wrote.
#[tokio::test]
async fn a_dream_reads_and_changes_only_what_its_audience_may_read() {
    if !crate::own_process("access_flow::a_dream_reads_and_changes_only_what_its_audience_may_read")
    {
        return;
    }
    let (url, asked) = eager_model().await;
    unsafe {
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_DREAM", "openai");
        std::env::set_var("MEMSTORE_DREAM_URL", &url);
        std::env::set_var("MEMSTORE_DREAM_KEY", "stand-in");
        std::env::set_var("MEMSTORE_DREAM_MODEL", "stand-in");
    }
    let env = start().await;
    let (eng, sales) = ("group:o1.eng", "group:o1.sales");
    env.note(
        "2026-03-02",
        "billing export deploys to gke-a",
        eng,
        &[("billing export", "deploy_target", "gke-a")],
    )
    .await;
    let (moved, _) = env
        .note(
            "2026-06-10",
            "billing export now deploys to gke-b",
            eng,
            &[("billing export", "deploy_target", "gke-b")],
        )
        .await;
    env.note(
        "2026-05-01",
        "rep memory keeps call notes",
        eng,
        &[("rep memory", "how_it_works", "keeps call notes")],
    )
    .await;
    let (page, _) = env
        .note(
            "2026-06-01",
            "the rep memory page lists notes",
            eng,
            &[("rep memory page", "how_it_works", "lists notes")],
        )
        .await;
    let (_, sales_old) = env
        .note(
            "2026-07-01",
            "sales: billing export on zzsales-xx",
            sales,
            &[("billing export", "deploy_target", "zzsales-xx")],
        )
        .await;
    env.note(
        "2026-08-01",
        "sales: billing export on zzsales-yy",
        sales,
        &[("billing export", "deploy_target", "zzsales-yy")],
    )
    .await;
    // Bob's own note: the newest value of all, and a name near the team's.
    let (_, private) = env
        .note(
            "2026-09-01",
            "bobsecret: billing export to bobsecret-cluster; rep memory profile is mine",
            "user:bob@x.com",
            &[
                ("billing export", "deploy_target", "bobsecret-cluster"),
                ("rep memory profile", "decision", "bobsecret profile"),
            ],
        )
        .await;

    let ex = memstore::extract::Extractor::dream_from_env()
        .unwrap()
        .unwrap();
    let budget = memstore::dream::Budget::default();
    let r = memstore::dream::pass(&env.store, &Scope::Org, &ex, &budget, false)
        .await
        .unwrap();

    // What the model was shown: never the private note, and never one team's facts with the other's.
    let prompts = asked.lock().unwrap().clone();
    assert!(
        prompts.iter().any(|p| p.contains("gke-a"))
            && prompts.iter().any(|p| p.contains("zzsales-xx"))
    );
    for p in &prompts {
        assert!(
            !p.contains("bobsecret"),
            "the private note reached the model: {p}"
        );
        assert!(
            !(p.contains("gke-") && p.contains("zzsales")),
            "two audiences in one call: {p}"
        );
    }

    assert_eq!(r.audiences, [eng, sales], "{r:?}");
    assert_eq!((r.ended, r.aliased), (2, 2), "{r:?}");

    // Each record is one team's, and rests only on facts that team may read.
    let (weighed, records): (Vec<Episode>, Vec<Episode>) = env
        .store
        .current_episodes(&Scope::Org)
        .await
        .unwrap()
        .into_values()
        .filter(|e| e.source.starts_with(memstore::dream::SOURCE))
        .partition(|e| {
            e.document
                .as_deref()
                .is_some_and(|d| d.starts_with(memstore::dream::WEIGHED))
        });
    assert_eq!(records.len(), 4, "two endings and two names: {records:?}");
    // What the pass weighed, one record per team: that team's alone, and hashes only, never a fact, a value or a
    // name of the memory (they outlive what they hash, so they must say nothing of it).
    assert_eq!(weighed.len(), 2, "{weighed:?}");
    let bodies = env
        .store
        .episode_bodies(
            &Scope::Org,
            weighed.iter().map(|e| e.episode_id.clone()).collect(),
        )
        .await
        .unwrap();
    // And read by no one but the dreamer: hashes of keys a reader could guess would tell them a forgotten person
    // was in memory.
    for rec in &weighed {
        assert_eq!(rec.acl, [memstore::dream::WEIGHED_ACL], "{rec:?}");
        let body = String::from_utf8_lossy(&bodies[&rec.episode_id]).to_string();
        assert!(
            body.contains(&format!("\"audience\":\"{eng}\""))
                || body.contains(&format!("\"audience\":\"{sales}\"")),
            "{body}"
        );
        for word in [
            "gke",
            "zzsales",
            "bobsecret",
            "billing",
            "rep memory",
            "deploy",
        ] {
            assert!(!body.contains(word), "{word} in {body}");
        }
        for f in private.iter().chain(&sales_old) {
            assert!(!body.contains(&f.fact_id), "{body}");
        }
    }
    for rec in &records {
        let key = rec.document.as_deref().unwrap();
        assert!(
            rec.acl == [eng] || rec.acl == [sales],
            "{key}: {:?}",
            rec.acl
        );
        for f in &private {
            assert!(!key.contains(&f.fact_id), "{key} rests on the private note");
        }
        if rec.acl == [eng] {
            assert!(!key.contains(&sales_old[0].fact_id), "{key}");
        }
    }

    let alice = jwt("alice@x.com", &[ORG, "o1.eng"], ORG, 3600);
    let carol = jwt("carol@x.com", &[ORG, "o1.sales"], ORG, 3600);
    let bob = jwt("bob@x.com", &[ORG], ORG, 3600);
    // Each team sees its own older value ended, and the other team's and Bob's values are as they were.
    let seen = env
        .values_once(&alice, |v| {
            v.contains("gke-b")
                && !v.contains("gke-a")
                && v.contains("\"_alias\"|\"rep memory page\"")
        })
        .await;
    assert!(
        !seen.contains("gke-a") && !seen.contains("zzsales") && !seen.contains("bobsecret"),
        "{seen}"
    );
    assert!(seen.contains("\"_alias\"|\"rep memory page\""), "{seen}");
    let then = env.values(&alice, json!({"as_of": "2026-04-01"})).await;
    assert!(
        then.contains("gke-a"),
        "history keeps the ended value: {then}"
    );
    let seen = env
        .values_once(&carol, |v| {
            v.contains("zzsales-yy") && !v.contains("zzsales-xx")
        })
        .await;
    assert!(
        !seen.contains("zzsales-xx") && !seen.contains("gke") && !seen.contains("_alias"),
        "{seen}"
    );
    let seen = env
        .values_once(&bob, |v| v.contains("bobsecret-cluster"))
        .await;
    assert!(
        seen.contains("bobsecret-cluster") && seen.contains("bobsecret profile"),
        "{seen}"
    );
    assert!(
        !seen.contains("gke") && !seen.contains("zzsales") && !seen.contains("_alias"),
        "{seen}"
    );
    // Only the team a record is for may read it (and see why its fact ended).
    let eng_records: Vec<&str> = records
        .iter()
        .filter(|e| e.acl == [eng])
        .map(|e| e.episode_id.as_str())
        .collect();
    assert!(env.readable(&alice, &eng_records).await.iter().all(|r| *r));
    assert!(env.readable(&carol, &eng_records).await.iter().all(|r| !r));
    assert!(env.readable(&bob, &eng_records).await.iter().all(|r| !r));
    let weighed_ids: Vec<&str> = weighed.iter().map(|e| e.episode_id.as_str()).collect();
    for who in [&alice, &carol, &bob] {
        assert!(env.readable(who, &weighed_ids).await.iter().all(|r| !r));
    }

    // A forget of what a decision rests on: the alias that drew on the page's note goes at once, and the ending
    // whose newer value is forgotten is released by the next pass.
    for gone in [&page, &moved] {
        assert!(
            env.store
                .forget(&Scope::Org, "writer", &gone.episode_id, |_| true)
                .await
                .unwrap()
        );
    }
    env.values_once(&alice, |v| {
        !v.contains("rep memory page") && !v.contains("gke-b")
    })
    .await;
    let r = memstore::dream::pass(&env.store, &Scope::Org, &ex, &budget, false)
        .await
        .unwrap();
    assert!(r.released >= 2, "{r:?}");
    let seen = env.values_once(&alice, |v| v.contains("gke-a")).await;
    assert!(
        !seen.contains("bobsecret") && !seen.contains("zzsales"),
        "{seen}"
    );

    // Withdrawing the dreamer takes back what it wrote: the other team's ended value is current again.
    let dreamer = format!(
        "{}{}",
        memstore::dream::SOURCE,
        memstore::dream::dreamer(&ex)
    );
    env.store
        .retract_facts(&Scope::Org, "ops", None, Some(&dreamer))
        .await
        .unwrap();
    let seen = env.values_once(&carol, |v| v.contains("zzsales-xx")).await;
    assert!(seen.contains("zzsales-yy"), "{seen}");
    let (status, body) = env
        .post("/document", Some(&alice), json!({"calls": [["dream/"]]}))
        .await;
    assert_eq!((status, &body["replies"][0]), (200, &json!([])), "{body}");
}
