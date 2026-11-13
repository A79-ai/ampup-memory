//! Isolation across every read path, end to end. A real `memstore serve` over HTTP on a local catalog holds
//! two orgs, `acme` (teams `a1`, `a2`) and `globex` (team `b1`), plus the served scope, each with memories
//! carrying a tag no other scope has. Every caller (signed-in people, org and team service keys, an ingest
//! key, another org's writer key, members' personal keys, the operator's `AMPUP_MEMORY_API_KEYS` key) reads through every read path
//! with every selection, and every answer must hold no tag from outside the scope the caller is entitled to
//! for that selection, while an entitled caller does see their own.
//!
//! The rule (README, "Orgs, teams and signing in"; `Directory::resolve`): every org and every team is its
//! own memory. Selecting an org gives its members the org's memory and never its teams'. Selecting a team
//! gives that team's members the team's memory and never the org's, and an org admin who isn't in the team
//! gets nothing there. A person who selects nothing gets their one org. A service key reads only its own
//! scope (a team key never its org's), and an ingest key reads nothing. A member's personal key reads what
//! they do (their teams and their own notes too), and manages nothing even when they are an admin. A key or a person never falls back
//! to the served scope; the operator's key, which is in no org, reads only the served scope.
//!
//! Not covered: `ask_memory` needs an answer model (`AMPUP_MEMORY_ANSWER_MODEL`, Gemini on Vertex AI) and isn't
//! offered without one; `/whoami` exists only behind Google sign-in; the internal port's service functions
//! (`AMPUP_MEMORY_FN_PORT`) read the served scope as the app's wrapper names the caller, with no selection.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use futures::StreamExt;
use http_body_util::Full;
use hyper::body::Bytes;
use memstore::auth::tokens::Auth0;
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

/// The tag of a note one person remembered for themselves: no one else may read it.
const PRIVATE: &str = "xmineprivate";

/// The tag of a session's transcript (`start_session` … `end_session`): its owner's alone.
const SESSION: &str = "xsessionprivate";

pub(crate) const CLIENT_ID: &str = "memstore-cli";
pub(crate) const ADMIN_KEY: &str = "msk_platform_admin_for_isolation";

/// A person's ID token from the stand-in identity provider (as `tests/it/org_team_flow.rs` mints them).
pub(crate) fn id_token(issuer: &str, user: &str) -> String {
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

/// A token as the platform's Auth0 tenant would issue it to the CLI: `sub` names the connection (`google-oauth2|…`
/// is the platform's own, `samlp|…` an Organization's enterprise one), and no `org_id`: the person signed in as
/// themselves, not into an Organization.
pub(crate) fn tenant_token(issuer: &str, user: &str, sub: &str) -> String {
    tenant_token_for(issuer, user, sub, None)
}

/// [`tenant_token`] issued for an Auth0 Organization (its `org_id`), as signing in to an org does.
pub(crate) fn tenant_token_for(
    issuer: &str,
    user: &str,
    sub: &str,
    org_id: Option<&str>,
) -> String {
    tenant_token_verified(issuer, user, sub, org_id, true)
}

/// [`tenant_token_for`], saying whether the tenant verified the email (a password account may not have).
pub(crate) fn tenant_token_verified(
    issuer: &str,
    user: &str,
    sub: &str,
    org_id: Option<&str>,
    verified: bool,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("k1".into());
    jsonwebtoken::encode(
        &header,
        &json!({"iss": issuer, "aud": CLIENT_ID, "sub": sub, "email": user, "org_id": org_id,
                "email_verified": verified, "iat": now, "exp": now + 3600}),
        &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!("../fixtures/idp_key.pem"))
            .unwrap(),
    )
    .unwrap()
}

/// The identity provider's public side: OpenID discovery and its signing key. People's tokens are minted
/// with [`id_token`], as if they had signed in.
pub(crate) async fn start_idp() -> String {
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

/// One client for every request (building one per request costs more than the request). Every request has a
/// deadline: a server that stops answering fails the test, naming the request, instead of parking it.
static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
});

/// A POST with `bearer`: the HTTP status and the raw answer.
pub(crate) async fn post(url: &str, bearer: &str, body: &Value) -> (u16, String) {
    let r = CLIENT
        .post(url)
        .bearer_auth(bearer)
        .body(body.to_string())
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url} {body}: {e}"));
    let status = r.status().as_u16();
    let text = r
        .text()
        .await
        .unwrap_or_else(|e| panic!("POST {url} {body}, reading the answer: {e}"));
    (status, text)
}

/// A GET with `bearer`: the HTTP status and the raw answer.
async fn get(url: &str, bearer: &str) -> (u16, String) {
    let r = CLIENT
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url}: {e}"));
    let status = r.status().as_u16();
    let text = r
        .text()
        .await
        .unwrap_or_else(|e| panic!("GET {url}, reading the answer: {e}"));
    (status, text)
}

/// A directory change through `/admin` as the operator.
pub(crate) async fn admin(server: &str, change: Value) {
    let (status, body) = post(&format!("{server}/admin"), ADMIN_KEY, &change).await;
    assert_eq!(status, 200, "{change}: {body}");
}

/// One MCP `tools/call`: the HTTP status and the JSON-RPC answer (raw).
pub(crate) async fn mcp_call(
    server: &str,
    sel: &str,
    bearer: &str,
    tool: &str,
    args: Value,
) -> (u16, String) {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": tool, "arguments": args}});
    post(&format!("{server}/mcp{sel}"), bearer, &call).await
}

/// The text a tool answered with, and whether it is an error.
pub(crate) fn tool_text(raw: &str) -> (String, bool) {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let r = &v["result"];
    (
        r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        r["isError"] == true,
    )
}

/// A memory written straight to the catalog: an episode with `tag` in its body and a fact whose entity and
/// value carry it. Returns the episode id.
async fn seed(store: &Store, scope: &Scope, tag: &str) -> String {
    store.ensure_scope(scope).await.unwrap();
    let ep = Episode::new(
        &format!("note:{tag}"),
        format!("{tag} owner note").as_bytes(),
        &["*"],
    );
    let id = ep.episode_id.clone();
    let f = Fact::new(&format!("deal:{tag}"), "owner", tag).from_episodes(&[&ep]);
    store.commit(scope, "test", &[ep], &[f]).await.unwrap();
    id
}

/// A schema whose SQL metric reads every deal's owner and cites it (the facts `seed` writes): a named metric is
/// one more read path, and its kept answer one more place memory could outlive a forget.
pub(crate) fn owners_schema() -> memstore::query::schema::Schema {
    serde_json::from_value(
        json!({"name": "owners", "mode": "open", "types": {"deal": {}},
        "metrics": {"owners": {"tags": ["deals"], "types": ["deal"],
            "sql": "SELECT value AS owner, count(*) AS deals, array_agg(fact_id) AS fact_ids \
                    FROM current WHERE predicate = 'owner' GROUP BY value"}}}),
    )
    .unwrap()
}

/// Who reads.
#[derive(Clone)]
enum Kind {
    /// A person signed in through the identity provider: their orgs and their teams in each.
    Person(Vec<(&'static str, Vec<&'static str>)>),
    /// An org's service key, for its org or one team; `ingest` keys only write.
    Key {
        org: &'static str,
        team: Option<&'static str>,
        ingest: bool,
    },
    /// A member's personal key: reads as the person does in their orgs and teams, and manages nothing.
    Personal(Vec<(&'static str, Vec<&'static str>)>),
    /// The platform operator's `AMPUP_MEMORY_API_KEYS` key: in no org.
    Operator,
}

/// Carol, signed in or through her personal key: the one caller who reads her own note and session.
fn is_carol(c: &Caller) -> bool {
    c.name.starts_with("a1 member")
}

struct Caller {
    name: &'static str,
    bearer: String,
    kind: Kind,
}

/// The memory a caller is entitled to for a selection (`Err`: refused), per the rule in this file's header.
/// For an ingest key this is where its selection points; it reads nothing there.
fn entitled(kind: &Kind, org: Option<&str>, team: Option<&str>) -> Result<Scope, ()> {
    match kind {
        Kind::Operator => match (org, team) {
            (None, None) => Ok(Scope::Org),
            _ => Err(()),
        },
        Kind::Key {
            org: own,
            team: own_team,
            ..
        } => {
            let (org, team) = match (org, team) {
                (None, None) => (Some(*own), *own_team),
                sel => sel,
            };
            if org.unwrap_or(own) != *own {
                return Err(());
            }
            match (team, own_team) {
                (None, None) => Ok(Scope::org(own).unwrap()),
                (Some(t), Some(mine)) if t == *mine => Ok(Scope::team(own, t).unwrap()),
                _ => Err(()),
            }
        }
        Kind::Person(orgs) | Kind::Personal(orgs) => {
            let org = match org {
                Some(o) => o,
                None if orgs.len() == 1 => orgs[0].0,
                None => return Err(()),
            };
            let teams = &orgs.iter().find(|(o, _)| *o == org).ok_or(())?.1;
            match team {
                None => Ok(Scope::org(org).unwrap()),
                Some(t) if teams.contains(&t) => Ok(Scope::team(org, t).unwrap()),
                Some(_) => Err(()),
            }
        }
    }
}

/// Every tag in the store, by the scope (or org profile) it belongs to.
struct Tags {
    /// Memory tags per scope: in facts and episodes, or episodes only (written by `remember_memory`).
    memory: BTreeMap<String, Vec<String>>,
    /// Tags written only as episodes (no fact), so `/search` and facts queries don't return them.
    episode_only: Vec<String>,
    /// Each org's memory profile tag.
    profile: HashMap<&'static str, &'static str>,
    /// Every episode: its id, the scope it belongs to and the tag it holds.
    episodes: Vec<(String, String, String)>,
}

impl Tags {
    /// The tags a reader of `scope` may see: the scope's memory and its org's profile.
    fn of(&self, scope: &Scope) -> Vec<String> {
        let mut out = self
            .memory
            .get(&scope.to_string())
            .cloned()
            .unwrap_or_default();
        if let Some(p) = scope.org_name().and_then(|o| self.profile.get(o)) {
            out.push(p.to_string());
        }
        out
    }

    fn all(&self) -> Vec<String> {
        self.memory
            .values()
            .flatten()
            .cloned()
            .chain(self.profile.values().map(|p| p.to_string()))
            .collect()
    }

    /// The tags `body` must not hold when read as `scope` (`None`: refused, so none at all).
    fn foreign(&self, scope: Option<&Scope>) -> Vec<String> {
        let mine = scope.map(|s| self.of(s)).unwrap_or_default();
        self.all()
            .into_iter()
            .filter(|t| !mine.contains(t))
            .collect()
    }
}

/// Fails if `body` (one read path's whole answer) holds any of `foreign`. Entity ids the request itself
/// named (`asked`) may be echoed back, so they are taken out first: the facts behind them, whose values
/// carry the tag, must still not be there.
fn assert_clean(what: &str, body: &str, foreign: &[String], asked: &[String]) {
    let body = &asked
        .iter()
        .fold(body.to_string(), |b, a| b.replace(a.as_str(), "<asked>"));
    let leaked: Vec<&String> = foreign
        .iter()
        .filter(|t| body.contains(t.as_str()))
        .collect();
    assert!(leaked.is_empty(), "LEAK {what}: {leaked:?} in {body}");
}

/// Fails unless `body` holds every one of `want`.
fn assert_sees(what: &str, body: &str, want: &[String]) {
    let missing: Vec<&String> = want.iter().filter(|t| !body.contains(t.as_str())).collect();
    assert!(
        missing.is_empty(),
        "{what} should see its own {missing:?}: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_read_path_returns_memory_from_outside_the_callers_scope() {
    let issuer = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    // Its own index, not the default location every other test (and test run) on this machine shares.
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
    // Bound here and handed over: a port picked and bound later could be taken in between by a test of
    // another run, whose server would then answer this one's requests.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Callers may select nothing (the operator then reads the served scope); people and keys never fall
    // back to it.
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            listener: Some(listener),
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while CLIENT.get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The operator sets up both orgs, their teams, members, profiles and keys through `/admin`.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "a1"}),
        json!({"op": "create_team", "org": "acme", "team": "a2"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"},
            {"user": "carol@acme.com", "teams": ["a1"]},
            {"user": "dan@acme.com", "teams": ["a2"]}]}),
        json!({"op": "set_profile", "org": "acme", "profile": {"description": "xprofalpha"}}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
        json!({"op": "create_team", "org": "globex", "team": "b1"}),
        json!({"op": "invite", "org": "globex", "members": [{"user": "erin@globex.com", "teams": ["b1"]}]}),
        json!({"op": "set_profile", "org": "globex", "profile": {"description": "xprofbeta"}}),
    ] {
        admin(s, change).await;
    }
    let key = |org: &str, name: &str, team: Option<&str>, role: &str| {
        let k = format!("msk_{}{}", uuid::Uuid::new_v4().simple(), name);
        let change = json!({"op": "create_key", "org": org, "name": name, "team": team, "role": role,
                            "key": k});
        (k, change)
    };
    let (a_reader, c1) = key("acme", "dash", None, "reader");
    let (a1_reader, c2) = key("acme", "ci_a1", Some("a1"), "reader");
    let (a_ingest, c3) = key("acme", "feed", None, "ingest");
    let (b_writer, c4) = key("globex", "bot", None, "writer");
    for c in [c1, c2, c3, c4] {
        admin(s, c).await;
    }

    // Each scope's memory, with a tag of its own.
    let scopes = [
        (Scope::Org, "xserved"),
        (Scope::org("acme").unwrap(), "xorgalpha"),
        (Scope::team("acme", "a1").unwrap(), "xteamalpha1"),
        (Scope::team("acme", "a2").unwrap(), "xteamalpha2"),
        (Scope::org("globex").unwrap(), "xorgbeta"),
        (Scope::team("globex", "b1").unwrap(), "xteambeta1"),
    ];
    let mut tags = Tags {
        memory: BTreeMap::new(),
        episode_only: vec![],
        profile: HashMap::from([("acme", "xprofalpha"), ("globex", "xprofbeta")]),
        episodes: vec![],
    };
    for (scope, tag) in &scopes {
        store
            .set_schema(scope, "main", "test", &owners_schema(), false)
            .await
            .unwrap();
        let id = seed(&store, scope, tag).await;
        tags.memory
            .entry(scope.to_string())
            .or_default()
            .push(tag.to_string());
        tags.episodes.push((id, scope.to_string(), tag.to_string()));
    }
    // And memories agents wrote over MCP: a team member into her team, the ingest key into its org, the other
    // org's writer key into its org.
    let carol = id_token(&issuer, "carol@acme.com");
    for (bearer, sel, tag, scope) in [
        (
            carol.as_str(),
            "?org=acme&team=a1",
            "xnotealpha1",
            "team:acme/a1",
        ),
        (a_ingest.as_str(), "", "xfeedalpha", "org:acme"),
        (b_writer.as_str(), "", "xwritebeta", "org:globex"),
    ] {
        let note =
            json!({"source": "test", "body": format!("{tag} owner remembered"), "for": "team"});
        let (_, raw) = mcp_call(s, sel, bearer, "remember_memory", note).await;
        let (text, failed) = tool_text(&raw);
        assert!(
            !failed && text.contains(&format!("remembered in {scope}")),
            "{raw}"
        );
        let id = text
            .split("episode ")
            .nth(1)
            .and_then(|t| t.split(',').next())
            .unwrap()
            .to_string();
        tags.memory
            .entry(scope.to_string())
            .or_default()
            .push(tag.into());
        tags.episode_only.push(tag.into());
        tags.episodes.push((id, scope.to_string(), tag.to_string()));
    }
    // What a person tells their agent is theirs alone by default (`for` "me", the server naming their entry):
    // no one else, in their team or not, reads it on any path.
    let note = json!({"source": "test", "body": format!("{PRIVATE} carol's own preference")});
    let (_, raw) = mcp_call(s, "?org=acme&team=a1", &carol, "remember_memory", note).await;
    let (text, failed) = tool_text(&raw);
    assert!(
        !failed && text.contains("visible to user:carol@acme.com"),
        "{raw}"
    );
    let person = |email: &str| id_token(&issuer, email);
    // A session is its owner's alone, though it lives in the org's memory: its transcript reaches no one else on
    // any path (by id included), and no one else, in the org or not, can advise in it, keep in it or end it.
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &carol,
        "start_session",
        json!({"question": format!("{SESSION} plan the launch")}),
    )
    .await;
    let (text, failed) = tool_text(&raw);
    assert!(!failed, "{raw}");
    let sid = serde_json::from_str::<Value>(&text).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    // A session its owner names (a harness's own session id) is opened by its first step, and is as much theirs.
    let named = "carol-named-session-01";
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &carol,
        "get_advice",
        json!({"session_id": named, "question": format!("{SESSION} next"), "moment": "done"}),
    )
    .await;
    assert!(!tool_text(&raw).1, "{raw}");
    for other in ["alice@acme.com", "bob@acme.com", "erin@globex.com"] {
        for (tool, args) in [
            ("get_advice", json!({"session_id": sid, "question": "next"})),
            (
                "get_advice",
                json!({"session_id": sid, "question": "-", "event": {"kind": "stop"},
                       "conversation_so_far": "User: planted"}),
            ),
            (
                "end_session",
                json!({"session_id": sid, "transcript": "User: planted"}),
            ),
        ] {
            let (_, raw) = mcp_call(s, "?org=acme", &person(other), tool, args).await;
            // Refused as a session that isn't theirs (or, outside the org, before it), not for any other reason.
            let (text, failed) = tool_text(&raw);
            assert!(
                (failed && text.contains("that you started"))
                    || raw.contains(&format!("{other} is not a member of org acme")),
                "{other} {tool} in carol's session: {raw}"
            );
            assert!(!text.contains(SESSION), "{other} {tool}: {raw}");
        }
        // Naming carol's name for a session reaches a session of their own, never hers.
        let (_, raw) = mcp_call(
            s,
            "?org=acme",
            &person(other),
            "end_session",
            json!({"session_id": named, "transcript": "User: planted", "outcome": "planted"}),
        )
        .await;
        assert!(!raw.contains(SESSION), "{other} ending carol's name: {raw}");
    }
    let transcript = format!("User: {SESSION} plan the launch\nClaude: {SESSION} launch planned");
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &carol,
        "end_session",
        json!({"session_id": sid, "transcript": transcript, "outcome": format!("{SESSION} planned")}),
    )
    .await;
    assert!(!tool_text(&raw).1, "{raw}");
    // Session rows are held and committed in batches: what the log holds once they are.
    memstore::sessions::flush().await.unwrap();
    let logged = memstore::qa_log::list(&store, &Scope::org("acme").unwrap(), None)
        .await
        .unwrap();
    let hers: Vec<_> = logged
        .iter()
        .filter(|l| l.answer_id.starts_with(&format!("{named}.")) && l.asker == "carol@acme.com")
        .collect();
    assert_eq!(hers.len(), 1, "carol's named session: {logged:?}");
    let part = hers[0].session.as_ref().unwrap();
    assert!(
        part.ended_at.is_none() && part.conversation.is_none() && hers[0].answer.is_none(),
        "carol's named session untouched by the others naming it: {:?}",
        hers[0]
    );
    let session_episode = logged
        .iter()
        .find(|l| l.answer_id == sid)
        .and_then(|l| l.episodes.first().cloned())
        .expect("the session's transcript");
    // Members' personal keys, each made by the member signed in as themselves: carol's (in team a1), and the
    // admin alice's, which reads as a member and manages nothing.
    let in_30_days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 30 * 24 * 3600;
    let personal = |name: &str, role: &str| {
        let k = format!("msk_{}{}", uuid::Uuid::new_v4().simple(), name);
        let change = json!({"op": "create_key", "org": "acme", "name": name, "role": role,
                            "personal": true, "expires_at": in_30_days, "key": k});
        (k, change)
    };
    let (carol_key, c) = personal("carol_cli", "writer");
    let (status, raw) = post(&format!("{s}/admin"), &carol, &c).await;
    assert_eq!(status, 200, "carol's personal key: {raw}");
    let (alice_key, c) = personal("alice_cli", "reader");
    let (status, raw) = post(&format!("{s}/admin"), &person("alice@acme.com"), &c).await;
    assert_eq!(status, 200, "alice's personal key: {raw}");
    // Refused: a member making a service key, a personal key of a team or a writing-only role or with no expiry,
    // one in an org they aren't in, one made with a key, and revoking someone else's.
    let (_, mut svc) = personal("carol_svc", "reader");
    svc.as_object_mut().unwrap().remove("personal");
    let (_, mut team) = personal("carol_team", "reader");
    team["team"] = json!("a1");
    let (_, mut never) = personal("carol_never", "reader");
    never.as_object_mut().unwrap().remove("expires_at");
    let (_, mut globex) = personal("carol_globex", "reader");
    globex["org"] = json!("globex");
    for (bearer, change, want) in [
        (&carol, svc, 403),
        (&carol, team, 400),
        (&carol, personal("carol_ingest", "ingest").1, 400),
        (&carol, never, 400),
        (&carol, globex, 403),
        (&carol_key, personal("carol_again", "reader").1, 403),
        (
            &person("bob@acme.com"),
            json!({"op": "revoke_key", "org": "acme", "name": "carol_cli"}),
            403,
        ),
    ] {
        let (status, raw) = post(&format!("{s}/admin"), bearer, &change).await;
        assert_eq!(status, want, "{change}: {raw}");
    }
    // A personal key reaches its member's memory routes only: not the org's admin, ingest or another member's runs,
    // even an admin's.
    let (status, raw) = post(
        &format!("{s}/admin"),
        &alice_key,
        &json!({"op": "create_team", "org": "acme", "team": "a3"}),
    )
    .await;
    assert_eq!(status, 403, "alice's personal key made a team: {raw}");
    let (status, raw) = post(&format!("{s}/ingest"), &carol_key, &json!({"episodes": []})).await;
    assert!(
        status == 403 && raw.contains("a personal key reaches"),
        "carol's personal key ingested: {status} {raw}"
    );
    let r = CLIENT
        .post(format!("{s}/advice"))
        .bearer_auth(&carol_key)
        .header("x-memstore-as", "bob@acme.com")
        .body(json!({"question": "next"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        403,
        "carol's personal key acted as bob"
    );
    // Her key's own view lists it as hers, and no one else's.
    let (_, raw) = post(
        &format!("{s}/admin"),
        &carol,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    assert!(
        raw.contains("carol_cli") && !raw.contains("alice_cli"),
        "carol's view of her keys: {raw}"
    );
    let callers = [
        Caller {
            name: "a1 member's personal key",
            bearer: carol_key.clone(),
            kind: Kind::Personal(vec![("acme", vec!["a1"])]),
        },
        Caller {
            name: "acme admin's personal key",
            bearer: alice_key.clone(),
            kind: Kind::Personal(vec![("acme", vec![])]),
        },
        Caller {
            name: "acme admin",
            bearer: person("alice@acme.com"),
            kind: Kind::Person(vec![("acme", vec![])]),
        },
        Caller {
            name: "acme member",
            bearer: person("bob@acme.com"),
            kind: Kind::Person(vec![("acme", vec![])]),
        },
        Caller {
            name: "a1 member",
            bearer: carol.clone(),
            kind: Kind::Person(vec![("acme", vec!["a1"])]),
        },
        Caller {
            name: "a2 member",
            bearer: person("dan@acme.com"),
            kind: Kind::Person(vec![("acme", vec!["a2"])]),
        },
        Caller {
            name: "globex member",
            bearer: person("erin@globex.com"),
            kind: Kind::Person(vec![("globex", vec!["b1"])]),
        },
        Caller {
            name: "outsider",
            bearer: person("oscar@initech.com"),
            kind: Kind::Person(vec![]),
        },
        Caller {
            name: "acme reader key",
            bearer: a_reader,
            kind: Kind::Key {
                org: "acme",
                team: None,
                ingest: false,
            },
        },
        Caller {
            name: "a1 reader key",
            bearer: a1_reader,
            kind: Kind::Key {
                org: "acme",
                team: Some("a1"),
                ingest: false,
            },
        },
        Caller {
            name: "acme ingest key",
            bearer: a_ingest.clone(),
            kind: Kind::Key {
                org: "acme",
                team: None,
                ingest: true,
            },
        },
        Caller {
            name: "globex writer key",
            bearer: b_writer.clone(),
            kind: Kind::Key {
                org: "globex",
                team: None,
                ingest: false,
            },
        },
        Caller {
            name: "operator",
            bearer: ADMIN_KEY.into(),
            kind: Kind::Operator,
        },
    ];
    let selections: [(Option<&str>, Option<&str>); 9] = [
        (None, None),
        (Some("acme"), None),
        (Some("acme"), Some("a1")),
        (Some("acme"), Some("a2")),
        (None, Some("a1")),
        (None, Some("a2")),
        (Some("globex"), None),
        (Some("globex"), Some("b1")),
        (Some("default"), None),
    ];

    // The server reads a snapshot up to ~1 s old: wait until each scope's facts reach an entitled reader.
    let readers = [
        (ADMIN_KEY, "", "xserved"),
        (callers[3].bearer.as_str(), "", "xorgalpha"),
        (carol.as_str(), "?team=a1", "xteamalpha1"),
        (callers[5].bearer.as_str(), "?team=a2", "xteamalpha2"),
        (b_writer.as_str(), "", "xorgbeta"),
        (callers[6].bearer.as_str(), "?team=b1", "xteambeta1"),
    ];
    for (bearer, sel, tag) in readers {
        let t = Instant::now();
        loop {
            let (_, body) = post(
                &format!("{s}/search{sel}"),
                bearer,
                &json!({"calls": [["owner", 50]]}),
            )
            .await;
            if body.contains(tag) {
                break;
            }
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "{tag} never showed: {body}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let all_ids: Vec<Value> = tags.episodes.iter().map(|(id, _, _)| json!([id])).collect();
    let entity_ids: Vec<String> = scopes.iter().map(|(_, t)| format!("deal:{t}")).collect();
    // Every caller with every selection, several at a time.
    let runs = callers
        .iter()
        .flat_map(|c| selections.iter().map(move |&sel| (c, sel)));
    let (tags, entity_ids, all_ids) = (&tags, &entity_ids, &all_ids);
    let session_episode = session_episode.as_str();
    let mut checked: usize = futures::stream::iter(runs)
        .map(|(c, (org, team))| async move {
            let mut checked = 0;
            let ingest = matches!(c.kind, Kind::Key { ingest: true, .. });
            let mut q: Vec<String> = vec![];
            if let Some(o) = org {
                q.push(format!("org={o}"));
            }
            if let Some(t) = team {
                q.push(format!("team={t}"));
            }
            let sel = if q.is_empty() {
                String::new()
            } else {
                format!("?{}", q.join("&"))
            };
            let scope = entitled(&c.kind, org, team).ok();
            let reads = scope.as_ref().filter(|_| !ingest);
            let mut foreign = tags.foreign(reads);
            if !is_carol(c) {
                foreign.push(PRIVATE.to_string());
                foreign.push(SESSION.to_string());
            }
            let mine: Vec<String> = reads.map(|s| tags.of(s)).unwrap_or_default();
            let mine_facts: Vec<String> = mine
                .iter()
                .filter(|t| !t.starts_with("xprof") && !tags.episode_only.contains(t))
                .cloned()
                .collect();
            let mine_memory: Vec<String> = mine
                .iter()
                .filter(|t| !t.starts_with("xprof"))
                .cloned()
                .collect();
            // The selected org's profile, shown to those who read its memory.
            let mine_profile: Vec<String> = mine
                .iter()
                .filter(|t| t.starts_with("xprof"))
                .cloned()
                .collect();
            let what = |path: &str| format!("{} {path}{sel}", c.name);
            // A refused selection, or an ingest key's read: the HTTP read paths answer 403.
            let want_status = if reads.is_some() { 200 } else { 403 };

            // HTTP: /search, /query (facts, entities with evidence, passages), /episode (every episode id),
            // /document (the list of documents).
            let no_documents: Vec<String> = vec![];
            let http = [
                (
                    "/search",
                    json!({"calls": [["owner", 50, -1, 400]]}),
                    &mine_facts,
                ),
                (
                    "/query",
                    json!({"calls": [[{"find": "facts", "time": "all", "limit": 500}]]}),
                    &mine_facts,
                ),
                (
                    "/query",
                    json!({"calls": [[{"find": "entities", "type": "deal", "include": ["evidence"], "limit": 500}]]}),
                    &mine_facts,
                ),
                (
                    "/query",
                    json!({"calls": [[{"id": entity_ids, "include": ["evidence"]}]]}),
                    &mine_facts,
                ),
                // A count's citations: each group's members and the facts that put them there.
                (
                    "/query",
                    json!({"calls": [[{"find": "count", "type": "deal", "group_by": "owner", "include": ["evidence"], "limit": 500}]]}),
                    &mine_facts,
                ),
                // The named metrics, a SQL one among them: its rows and the facts they cite.
                (
                    "/query",
                    json!({"calls": [[{"find": "metrics", "include": ["evidence"], "limit": 500}]]}),
                    &mine_facts,
                ),
                (
                    "/query",
                    json!({"calls": [[{"find": "passages", "text": "owner", "limit": 100}]]}),
                    &mine_memory,
                ),
                ("/episode", json!({"calls": all_ids}), &mine_memory),
                // The memory UI's tree: every scope's facts are on entities `deal:<tag>`.
                (
                    "/browse",
                    json!({"op": "ls", "path": ["entities", "deal"]}),
                    &mine_facts,
                ),
                // The filterable lists: every fact the caller may read (each with the sources of the episodes
                // it cites, which are the caller's to see too), and the advice (none is learned here).
                ("/facts/list", json!({"limit": 500}), &mine_facts),
                ("/advice/list", json!({}), &no_documents),
                // Documents kept in versions: there are none here, so a reader lists nothing.
                ("/document", json!({"calls": [[""]]}), &no_documents),
            ];
            for (path, body, want) in http {
                let (status, answer) = post(&format!("{s}{path}{sel}"), &c.bearer, &body).await;
                let asked = if body.to_string().contains("deal:") {
                    entity_ids.clone()
                } else {
                    vec![]
                };
                assert_clean(&what(path), &answer, &foreign, &asked);
                assert_eq!(status, want_status, "{}: {answer}", what(path));
                assert_sees(&what(path), &answer, want);
                checked += 1;
            }
            // `/browse` reads each episode by id: its own scope's, and 404 for every other (403 when refused).
            for (id, home, tag) in &tags.episodes {
                let own = reads.is_some_and(|s| &s.to_string() == home);
                let req = json!({"op": "episode", "id": id});
                let (status, answer) = post(&format!("{s}/browse{sel}"), &c.bearer, &req).await;
                let w = what("/browse episode");
                assert_clean(&w, &answer, &foreign, &[]);
                let want = match (reads.is_some(), own) {
                    (false, _) => 403,
                    (true, true) => 200,
                    (true, false) => 404,
                };
                assert_eq!(status, want, "{w} {id}: {answer}");
                if own {
                    assert_sees(&w, &answer, std::slice::from_ref(tag));
                }
                checked += 1;
            }

            // `/me`: the caller's scope, memberships and the selected org's profile.
            let (status, me) = get(&format!("{s}/me{sel}"), &c.bearer).await;
            assert_clean(&what("/me"), &me, &foreign, &[]);
            assert_eq!(status, 200, "{}: {me}", what("/me"));
            assert_sees(&what("/me"), &me, &mine_profile);
            let mev: Value = serde_json::from_str(&me).unwrap();
            assert_eq!(
                mev["scope"],
                scope.as_ref().map_or(Value::Null, |s| json!(s.to_string())),
                "{}: {me}",
                what("/me")
            );
            // Memberships name only the caller's own orgs (none for an ingest key or the operator).
            let orgs: Vec<&str> = match &c.kind {
                Kind::Person(o) | Kind::Personal(o) => o.iter().map(|(o, _)| *o).collect(),
                Kind::Key { org, ingest, .. } if !ingest => vec![*org],
                _ => vec![],
            };
            let held: Vec<&str> = mev["memberships"]
                .as_object()
                .map(|m| m.keys().map(String::as_str).collect())
                .unwrap_or_default();
            assert_eq!(held, orgs, "{}: {me}", what("/me"));
            checked += 1;

            // MCP: initialize (the org's profile), tools/list, and every read tool.
            let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
            let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
            // A person who names no org and is in several or none is answered from the lobby (200, and still
            // clean: it reads nothing); anyone else without a scope is refused.
            let lobby = scope.is_none()
                && org.is_none()
                && team.is_none()
                && matches!(&c.kind, Kind::Person(o) if o.len() != 1);
            for (m, body, want) in [("initialize", init, &mine_profile), ("tools/list", list, &vec![])] {
                let (status, answer) = post(&format!("{s}/mcp{sel}"), &c.bearer, &body).await;
                let w = what(&format!("/mcp {m}"));
                assert_clean(&w, &answer, &foreign, &[]);
                assert_eq!(
                    status,
                    if scope.is_some() || lobby { 200 } else { 403 },
                    "{w}: {answer}"
                );
                assert_sees(&w, &answer, want);
                checked += 1;
            }
            let mut tools = vec![
                // The text answer reads the episodes its results cite (titles, lines, passages); JSON the facts.
                (
                    "search_memory",
                    json!({"query": "owner", "k": 50}),
                    mine_facts.clone(),
                ),
                (
                    "search_memory",
                    json!({"query": "owner", "k": 50, "format": "json"}),
                    mine_facts.clone(),
                ),
                // A dated search says what arrived in its window from the episodes' rows, whatever its words (these
                // share none with any episode): every episode the caller may read there, and no other reader's.
                (
                    "search_memory",
                    json!({"query": "zzunrelated", "start_date": "2000-01-01", "end_date": "2100-01-01"}),
                    mine_memory.clone(),
                ),
                (
                    "search_memory",
                    json!({"query": "zzunrelated", "start_date": "2000-01-01", "end_date": "2100-01-01",
                           "format": "json"}),
                    mine_memory.clone(),
                ),
                (
                    "query_memory",
                    json!({"query": {"find": "facts", "time": "all", "limit": 500}}),
                    mine_facts.clone(),
                ),
                (
                    "query_memory",
                    json!({"query": {"find": "count", "type": "deal", "group_by": "owner", "include": ["evidence"], "limit": 500}}),
                    mine_facts.clone(),
                ),
                (
                    "query_memory",
                    json!({"query": {"find": "metrics", "include": ["evidence"], "limit": 500}}),
                    mine_facts.clone(),
                ),
                (
                    "query_memory",
                    json!({"query": {"find": "passages", "text": "owner", "limit": 100}}),
                    mine_memory.clone(),
                ),
                // The memory browser's list tools: the same reads as /facts/list and /advice/list.
                ("list_facts", json!({"limit": 500}), mine_facts.clone()),
                ("list_advice", json!({}), vec![]),
            ];
            for e in entity_ids {
                let own = mine_facts.iter().any(|t| e == &format!("deal:{t}"));
                let want = if own { vec![e.clone()] } else { vec![] };
                tools.push(("get_entity", json!({"id": e}), want));
                // get_episode by a fact's subject reads the episodes its facts cite: only its own scope's.
                let tag = e.trim_start_matches("deal:").to_string();
                let want = if own { vec![tag] } else { vec![] };
                tools.push(("get_episode", json!({"episode_id": e}), want.clone()));
                tools.push(("get_episode", json!({"document": e}), want));
            }
            for (id, home, tag) in &tags.episodes {
                let own = reads.is_some_and(|s| &s.to_string() == home);
                let want = if own { vec![tag.clone()] } else { vec![] };
                tools.push(("get_episode", json!({"episode_id": id}), want));
            }
            // The session's transcript by its id: its owner reads it in the org's memory, no one else anywhere.
            let own = is_carol(c) && reads.is_some_and(|s| s.to_string() == "org:acme");
            let want = if own { vec![SESSION.to_string()] } else { vec![] };
            tools.push(("get_episode", json!({"episode_id": session_episode}), want));
            for (tool, args, want) in tools {
                let (status, raw) = mcp_call(s, &sel, &c.bearer, tool, args.clone()).await;
                let w = what(&format!("/mcp {tool}"));
                let asked: Vec<String> = ["id", "document"]
                    .iter()
                    .filter_map(|k| args[*k].as_str().map(String::from))
                    .collect();
                assert_clean(&w, &raw, &foreign, &asked);
                match (&scope, ingest) {
                    // The lobby has no memory tool: each answers as an error naming what to do.
                    (None, _) if lobby => {
                        assert_eq!(status, 200, "{w}: {raw}");
                        let (text, failed) = tool_text(&raw);
                        assert!(failed && text.starts_with("no tool"), "{w}: {raw}");
                    }
                    (None, _) => assert_eq!(status, 403, "{w}: {raw}"),
                    (Some(_), true) => {
                        let (text, failed) = tool_text(&raw);
                        assert!(failed && text.contains("is an ingest key"), "{w}: {raw}");
                    }
                    (Some(_), false) => {
                        assert_eq!(status, 200, "{w}: {raw}");
                        let (text, _) = tool_text(&raw);
                        assert_sees(&w, &text, &want);
                        // What it may not read answers as what doesn't exist.
                        if tool == "get_episode" && want.is_empty() {
                            assert!(
                                text == "no such episode (or not visible)"
                                    || text.starts_with("no document "),
                                "{w} {args}: {text}"
                            );
                        }
                    }
                }
                checked += 1;
            }
            // The browser page's tools are listed on its own surface only, and never to a caller that can't read.
            let listing = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
            let sep = if sel.contains('?') { '&' } else { '?' };
            for (url, listed) in [
                (format!("{s}/mcp{sel}"), false),
                (format!("{s}/mcp{sel}{sep}surface=browser"), scope.is_some() && !ingest),
            ] {
                let (status, raw) = post(&url, &c.bearer, &listing).await;
                let w = what("/mcp tools/list");
                assert_eq!(
                    status == 200 && raw.contains("\"list_facts\"") && raw.contains("\"list_advice\""),
                    listed,
                    "{w} {url}: {raw}"
                );
                // query_memory (the page's raw-episode search) is listed on the same surface and no other.
                assert_eq!(
                    status == 200 && raw.contains("\"name\":\"query_memory\""),
                    listed,
                    "{w} {url} query_memory: {raw}"
                );
                checked += 1;
            }
            checked
        })
        .buffer_unordered(8)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum();

    for c in &callers {
        // `/admin` views: a person sees the orgs they belong to and no other; keys and the outsider see none.
        let (status, all) = post(&format!("{s}/admin"), &c.bearer, &json!({"op": "show"})).await;
        assert_clean(&format!("{} /admin show", c.name), &all, &tags.all(), &[]);
        for org in ["acme", "globex", "default"] {
            let (status_one, one) = post(
                &format!("{s}/admin"),
                &c.bearer,
                &json!({"op": "show", "org": org}),
            )
            .await;
            // An org's view holds its own profile, and no memory or other org's profile.
            let foreign: Vec<String> = tags
                .all()
                .into_iter()
                .filter(|t| tags.profile.get(org) != Some(&t.as_str()))
                .collect();
            assert_clean(
                &format!("{} /admin show {org}", c.name),
                &one,
                &foreign,
                &[],
            );
            let member = matches!(&c.kind, Kind::Person(o) if o.iter().any(|(m, _)| *m == org));
            let expect = match &c.kind {
                Kind::Operator => 200,
                Kind::Person(_) if member => 200,
                _ => 403,
            };
            assert_eq!(status_one, expect, "{} /admin show {org}: {one}", c.name);
            if status_one == 200 && !matches!(c.kind, Kind::Operator) {
                // An org's view names its own members, never another org's.
                let other = if org == "acme" {
                    "globex.com"
                } else {
                    "acme.com"
                };
                assert!(!one.contains(other), "{} /admin show {org}: {one}", c.name);
            }
            checked += 1;
        }
        match &c.kind {
            Kind::Key { .. } | Kind::Personal(_) => assert_eq!(status, 403, "{}: {all}", c.name),
            Kind::Operator => assert_eq!(status, 200, "{all}"),
            Kind::Person(orgs) => {
                assert_eq!(status, 200, "{}: {all}", c.name);
                let v: Value = serde_json::from_str(&all).unwrap();
                let view: Vec<&str> = v["view"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                let mine: Vec<&str> = orgs.iter().map(|(o, _)| *o).collect();
                assert_eq!(view, mine, "{} /admin show: {all}", c.name);
            }
        }
    }

    checked += pinned_reads(s, &callers, &selections, tags).await;
    checked += cross_scope_reads(s, &store, &callers, &selections, tags, &person).await;

    // Taken out of the org, carol's personal key goes with her: refused on every read, at once.
    admin(
        s,
        json!({"op": "remove_member", "org": "acme", "user": "carol@acme.com"}),
    )
    .await;
    // A server's view of the keys may be up to 5 s behind.
    let t = Instant::now();
    loop {
        let (status, raw) = post(
            &format!("{s}/search?team=a1"),
            &carol_key,
            &json!({"calls": [["owner", 50]]}),
        )
        .await;
        if status == 401 || status == 403 {
            assert!(
                !raw.contains("xteamalpha1") && !raw.contains(PRIVATE),
                "{raw}"
            );
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "carol's personal key still reads after she left: {status} {raw}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("checked {checked} responses");
}

/// Pinned reads (`memstore::pins`): a long-running reader pins the memory it reads (`POST /pin`) and reads it
/// through the pin (`?at=pin_…`). A pin holds nothing its reader couldn't read. Only a caller who may read a scope
/// takes one there (an ingest key, a caller refused there, may not). A pin is read only in its own scope: with any
/// other selection it is refused and answers nothing. Read through it, every caller sees what they would unpinned:
/// their own scope's memory, never another's, and never another person's own note.
async fn pinned_reads(
    s: &str,
    callers: &[Caller],
    selections: &[(Option<&str>, Option<&str>)],
    tags: &Tags,
) -> usize {
    let sel_of = |org: Option<&str>, team: Option<&str>| {
        let q: Vec<String> = [
            org.map(|o| format!("org={o}")),
            team.map(|t| format!("team={t}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        q.join("&")
    };
    let mut checked = 0;
    // A pin of each scope some caller may read, taken by the first who may; every other caller and selection that
    // may not read there is refused one.
    let mut pins: Vec<(Scope, String)> = vec![];
    for c in callers {
        for &(org, team) in selections {
            let ingest = matches!(c.kind, Kind::Key { ingest: true, .. });
            let scope = entitled(&c.kind, org, team).ok().filter(|_| !ingest);
            let q = sel_of(org, team);
            let url = format!(
                "{s}/pin{}",
                if q.is_empty() {
                    String::new()
                } else {
                    format!("?{q}")
                }
            );
            let taken = scope
                .as_ref()
                .is_some_and(|sc| pins.iter().any(|(p, _)| p == sc));
            if taken {
                continue;
            }
            let (status, body) = post(&url, &c.bearer, &json!({"ttl_secs": 3600})).await;
            checked += 1;
            match &scope {
                Some(sc) => {
                    assert_eq!(status, 200, "{} pins {url}: {body}", c.name);
                    let v: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(v["scope"], sc.to_string(), "{} pins {url}: {body}", c.name);
                    pins.push((sc.clone(), v["pin"].as_str().unwrap().to_string()));
                }
                None => {
                    assert_ne!(status, 200, "{} may not pin {url}: {body}", c.name);
                    assert_clean(&format!("{} /pin {url}", c.name), &body, &tags.all(), &[]);
                }
            }
        }
    }
    assert!(pins.len() >= 5, "a pin of every readable scope: {pins:?}");
    let pins = &pins;
    let runs: Vec<_> = callers
        .iter()
        .flat_map(|c| selections.iter().map(move |&sel| (c, sel)))
        .flat_map(|(c, sel)| pins.iter().map(move |p| (c, sel, p)))
        .collect();
    let pinned: usize = futures::stream::iter(runs)
        .map(|(c, (org, team), (pin_scope, pin))| async move {
            let ingest = matches!(c.kind, Kind::Key { ingest: true, .. });
            let scope = entitled(&c.kind, org, team).ok();
            let q = sel_of(org, team);
            let sel = if q.is_empty() {
                format!("?at={pin}")
            } else {
                format!("?{q}&at={pin}")
            };
            let reads = scope.as_ref().filter(|s| !ingest && *s == pin_scope);
            let mut foreign = tags.foreign(reads);
            if !is_carol(c) {
                foreign.push(PRIVATE.to_string());
                foreign.push(SESSION.to_string());
            }
            let mine: Vec<String> = reads
                .map(|s| tags.of(s))
                .unwrap_or_default()
                .into_iter()
                .filter(|t| !t.starts_with("xprof") && !tags.episode_only.contains(t))
                .collect();
            let what = |path: &str| format!("{} {path}{sel} (pin of {pin_scope})", c.name);
            let (status, body) = post(
                &format!("{s}/query{sel}"),
                &c.bearer,
                &json!({"calls": [[{"find": "facts", "time": "all", "limit": 500}]]}),
            )
            .await;
            assert_clean(&what("/query"), &body, &foreign, &[]);
            match (scope.as_ref(), reads) {
                // Refused here, pin or no pin.
                (None, _) => assert_eq!(status, 403, "{}: {body}", what("/query")),
                // An ingest key reads nothing, through a pin too (another scope's pin isn't one here either).
                (Some(_), None) if ingest => {
                    assert!([400, 403].contains(&status), "{}: {body}", what("/query"))
                }
                // Another scope's pin: no pin here.
                (Some(_), None) => assert_eq!(status, 400, "{}: {body}", what("/query")),
                (Some(_), Some(_)) => {
                    assert_eq!(status, 200, "{}: {body}", what("/query"));
                    assert_sees(&what("/query"), &body, &mine);
                }
            }
            let (status, raw) = mcp_call(
                s,
                &sel,
                &c.bearer,
                "search_memory",
                json!({"query": "owner note deal", "k": 50}),
            )
            .await;
            assert_clean(&what("search_memory"), &raw, &foreign, &[]);
            if reads.is_some() {
                assert_eq!(status, 200, "{}: {raw}", what("search_memory"));
                let (text, err) = tool_text(&raw);
                assert!(!err, "{}: {text}", what("search_memory"));
            } else {
                assert_ne!(
                    (status, tool_text(&raw).1),
                    (200, false),
                    "{}: {raw}",
                    what("search_memory")
                );
            }
            2
        })
        .buffer_unordered(8)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum();
    checked + pinned
}

/// Cross-scope reads (`Settings::reads`). Only the operator lets one scope's readers also search others, and only
/// scopes of the same org that exist. Once org acme reads team a1, whoever reads acme's memory finds a1's too on the
/// routes they search through (`/search`, `/episode`, `search_memory`, `get_episode`), each hit naming its scope and
/// an episode both hold shown once, while every episode's ACL still applies in its own scope (carol's own note in
/// a1 reaches no one else through acme). No other route and no other scope changes: a1's own readers still read
/// a1 alone (a team never takes its org's setting), and a2, globex and the served scope read as before. Unset, acme
/// reads itself alone again.
async fn cross_scope_reads(
    s: &str,
    store: &Store,
    callers: &[Caller],
    selections: &[(Option<&str>, Option<&str>)],
    before: &Tags,
    person: &impl Fn(&str) -> String,
) -> usize {
    let mut checked = 0;
    let set = |settings: Value| json!({"op": "set_settings", "org": "acme", "settings": settings});
    let alice = person("alice@acme.com");
    // Refused: a caller who may not set it (403), the operator naming scopes no one may (another org's, the scope
    // itself, not a scope: 400), and a team the org doesn't have (403).
    for (bearer, settings, why, want) in [
        (
            alice.as_str(),
            json!({"reads": ["team:acme/a1"]}),
            "only the operator",
            403,
        ),
        (
            ADMIN_KEY,
            json!({"reads": ["team:globex/b1"]}),
            "scopes of org acme only",
            400,
        ),
        (
            ADMIN_KEY,
            json!({"reads": ["org:globex"]}),
            "scopes of org acme only",
            400,
        ),
        (
            ADMIN_KEY,
            json!({"reads": ["org:acme"]}),
            "the scope itself",
            400,
        ),
        (
            ADMIN_KEY,
            json!({"reads": ["team:acme/nope"]}),
            "no team acme/nope",
            403,
        ),
        (
            ADMIN_KEY,
            json!({"reads": ["user:u1"]}),
            "reads names scopes",
            400,
        ),
    ] {
        let (status, body) = post(&format!("{s}/admin"), bearer, &set(settings.clone())).await;
        assert_eq!(status, want, "{settings}: {body}");
        assert!(body.contains(why), "{settings}: {body}");
        checked += 1;
    }
    // Nothing refused was set: acme's readers still read acme alone.
    let (_, body) = post(
        &format!("{s}/admin"),
        ADMIN_KEY,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    assert!(!body.contains("\"reads\""), "{body}");

    // An episode both acme and a1 hold (the same id), and one source each holds a version of: each found once.
    let mut tags = Tags {
        memory: before.memory.clone(),
        episode_only: before.episode_only.clone(),
        profile: before.profile.clone(),
        episodes: before.episodes.clone(),
    };
    let twice = Episode::new("note:shared", b"xdupsame owner note", &["*"]);
    for (scope, version) in [("org:acme", "one"), ("team:acme/a1", "two")] {
        let sc = Scope::parse(scope).unwrap();
        let f = Fact::new("deal:xdupsame", "owner", "xdupsame").from_episodes(&[&twice]);
        store
            .commit(&sc, "test", std::slice::from_ref(&twice), &[f])
            .await
            .unwrap();
        let ep = Episode::new(
            "note:versioned",
            format!("xdupsource owner note, version {version}").as_bytes(),
            &["*"],
        );
        let f = Fact::new("deal:xdupsource", "owner", "xdupsource").from_episodes(&[&ep]);
        tags.episodes
            .push((ep.episode_id.clone(), scope.into(), "xdupsource".into()));
        store.commit(&sc, "test", &[ep], &[f]).await.unwrap();
        tags.memory
            .entry(scope.into())
            .or_default()
            .extend(["xdupsame".to_string(), "xdupsource".to_string()]);
    }
    tags.episodes.push((
        twice.episode_id.clone(),
        "org:acme".into(),
        "xdupsame".into(),
    ));

    admin(s, set(json!({"reads": ["team:acme/a1"]}))).await;
    let (_, body) = post(
        &format!("{s}/admin"),
        ADMIN_KEY,
        &json!({"op": "show", "org": "acme"}),
    )
    .await;
    assert!(body.contains("team:acme/a1"), "{body}");
    let acme = Scope::org("acme").unwrap();
    let a1 = Scope::team("acme", "a1").unwrap();
    // What a caller reads on the routes that search across scopes: their scope's, and a1's too through acme.
    let reads = |scope: &Scope| -> Vec<Scope> {
        match *scope == acme {
            true => vec![acme.clone(), a1.clone()],
            false => vec![scope.clone()],
        }
    };
    // The server reads a snapshot up to ~1 s old: wait until a1's new memory reaches acme's reader key.
    let a_reader = &callers
        .iter()
        .find(|c| c.name == "acme reader key")
        .unwrap()
        .bearer;
    let t = Instant::now();
    loop {
        let (_, body) = post(
            &format!("{s}/search"),
            a_reader,
            &json!({"calls": [["owner", 50]]}),
        )
        .await;
        if body.contains("xteamalpha1") && body.contains("xdupsource") {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "a1 never showed: {body}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The search above can pass on one scope's xdupsource alone: a request pins the scopes it also reads (a1)
    // fresh, while its own scope's snapshot (acme's) may lag a second behind. Wait for both versions' episodes.
    let dups: Vec<(String, &str)> = tags
        .episodes
        .iter()
        .filter(|(_, _, tag)| tag == "xdupsource")
        .map(|(id, home, _)| {
            let v = if home == "org:acme" {
                "version one"
            } else {
                "version two"
            };
            (id.clone(), v)
        })
        .collect();
    assert_eq!(dups.len(), 2);
    loop {
        let mut seen = 0;
        for (id, version) in &dups {
            let (_, raw) =
                mcp_call(s, "", a_reader, "get_episode", json!({"episode_id": id})).await;
            seen += usize::from(tool_text(&raw).0.contains(version));
        }
        if seen == dups.len() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "acme's and a1's xdupsource episodes never both showed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let entity_ids: Vec<String> = tags
        .memory
        .values()
        .flatten()
        .filter(|t| !tags.episode_only.contains(t))
        .map(|t| format!("deal:{t}"))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let all_ids: Vec<Value> = tags.episodes.iter().map(|(id, _, _)| json!([id])).collect();
    let (tags, entity_ids, all_ids, reads) = (&tags, &entity_ids, &all_ids, &reads);
    let runs = callers
        .iter()
        .flat_map(|c| selections.iter().map(move |&sel| (c, sel)));
    checked += futures::stream::iter(runs)
        .map(|(c, (org, team))| async move {
            let mut checked = 0;
            let ingest = matches!(c.kind, Kind::Key { ingest: true, .. });
            let mut q: Vec<String> = vec![];
            if let Some(o) = org {
                q.push(format!("org={o}"));
            }
            if let Some(t) = team {
                q.push(format!("team={t}"));
            }
            let sel = match q.is_empty() {
                true => String::new(),
                false => format!("?{}", q.join("&")),
            };
            let scope = entitled(&c.kind, org, team).ok();
            // As in the first sweep: a person in several orgs or none, naming none, gets the lobby.
            let lobby = scope.is_none()
                && org.is_none()
                && team.is_none()
                && matches!(&c.kind, Kind::Person(o) if o.len() != 1);
            let read: Vec<Scope> = scope.iter().filter(|_| !ingest).flat_map(reads).collect();
            let mine: Vec<String> = read
                .iter()
                .flat_map(|s| tags.of(s))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut foreign: Vec<String> = tags
                .all()
                .into_iter()
                .filter(|t| !mine.contains(t))
                .collect();
            if !is_carol(c) {
                foreign.push(PRIVATE.to_string());
                foreign.push(SESSION.to_string());
            }
            let mine_facts: Vec<String> = mine
                .iter()
                .filter(|t| !t.starts_with("xprof") && !tags.episode_only.contains(t))
                .cloned()
                .collect();
            let mine_memory: Vec<String> = mine
                .iter()
                .filter(|t| !t.starts_with("xprof"))
                .cloned()
                .collect();
            let what = |path: &str| format!("{} {path}{sel} (acme reads a1)", c.name);
            let fans = read.len() > 1;
            for (path, body, want) in [
                (
                    "/search",
                    json!({"calls": [["owner", 50, -1, 400]]}),
                    &mine_facts,
                ),
                ("/episode", json!({"calls": all_ids}), &mine_memory),
            ] {
                let (status, answer) = post(&format!("{s}{path}{sel}"), &c.bearer, &body).await;
                assert_clean(&what(path), &answer, &foreign, &[]);
                assert_eq!(
                    status,
                    if read.is_empty() { 403 } else { 200 },
                    "{}: {answer}",
                    what(path)
                );
                assert_sees(&what(path), &answer, want);
                if path == "/search" && status == 200 {
                    let hits: Vec<Value> =
                        serde_json::from_str::<Value>(&answer).unwrap()["replies"][0]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                    // Each hit names its scope when more than one is read, and only then: as before.
                    for h in &hits {
                        // A fact's value is its tag; a note's fact holds the note, its tag first.
                        let tag = h["value"]
                            .as_str()
                            .unwrap_or_default()
                            .split_whitespace()
                            .next()
                            .unwrap_or_default();
                        let mut home = tags
                            .memory
                            .iter()
                            .filter(|(_, t)| t.iter().any(|t| t == tag))
                            .map(|(s, _)| s.clone())
                            .collect::<Vec<_>>();
                        // Carol's own note, found by her alone (`assert_clean` above): where she wrote it.
                        if tag == PRIVATE {
                            home.push("team:acme/a1".to_string());
                        }
                        match fans {
                            true => assert!(
                                home.contains(&h["scope"].as_str().unwrap_or_default().to_string()),
                                "{}: {h} names {home:?}",
                                what(path)
                            ),
                            false => assert!(h.get("scope").is_none(), "{}: {h}", what(path)),
                        }
                    }
                    // What acme and a1 both hold is found once.
                    for dup in ["xdupsame", "xdupsource"] {
                        let n = hits.iter().filter(|h| h["value"] == dup).count();
                        let held = mine_facts.iter().any(|t| t == dup);
                        assert_eq!(n, usize::from(held), "{} {dup}: {answer}", what(path));
                    }
                }
                checked += 1;
            }
            let mut tools = vec![
                (
                    "search_memory",
                    json!({"query": "owner", "k": 50}),
                    mine_facts.clone(),
                ),
                (
                    "search_memory",
                    json!({"query": "owner", "k": 50, "format": "json"}),
                    mine_facts.clone(),
                ),
                // What arrived, counted in every scope read: never another scope's, nor another person's own note.
                (
                    "search_memory",
                    json!({"query": "zzunrelated", "start_date": "2000-01-01", "end_date": "2100-01-01"}),
                    vec![],
                ),
                (
                    "search_memory",
                    json!({"query": "zzunrelated", "start_date": "2000-01-01", "end_date": "2100-01-01",
                           "format": "json"}),
                    vec![],
                ),
            ];
            for e in entity_ids {
                let tag = e.trim_start_matches("deal:").to_string();
                let want = match mine_facts.contains(&tag) {
                    true => vec![tag],
                    false => vec![],
                };
                tools.push(("get_episode", json!({"episode_id": e}), want.clone()));
                tools.push(("get_episode", json!({"document": e}), want));
            }
            for (id, home, tag) in &tags.episodes {
                let want = match read.iter().any(|s| &s.to_string() == home) {
                    true => vec![tag.clone()],
                    false => vec![],
                };
                tools.push(("get_episode", json!({"episode_id": id}), want));
            }
            for (tool, args, want) in tools {
                let (status, raw) = mcp_call(s, &sel, &c.bearer, tool, args.clone()).await;
                let w = what(&format!("/mcp {tool} {args}"));
                let asked: Vec<String> = ["episode_id", "document"]
                    .iter()
                    .filter_map(|k| args[*k].as_str().map(String::from))
                    .filter(|a| a.starts_with("deal:"))
                    .collect();
                assert_clean(&w, &raw, &foreign, &asked);
                match (&scope, ingest) {
                    (None, _) if lobby => {
                        assert_eq!(status, 200, "{w}: {raw}");
                        let (text, failed) = tool_text(&raw);
                        assert!(failed && text.starts_with("no tool"), "{w}: {raw}");
                    }
                    (None, _) => assert_eq!(status, 403, "{w}: {raw}"),
                    (Some(_), true) => assert!(tool_text(&raw).1, "{w}: {raw}"),
                    (Some(_), false) => {
                        assert_eq!(status, 200, "{w}: {raw}");
                        let (text, _) = tool_text(&raw);
                        assert_sees(&w, &text, &want);
                        // A result from a1 says so in acme's answer; no answer of a scope read alone names a scope.
                        if tool == "search_memory" && args["format"].is_null() && args["start_date"].is_null() {
                            assert_eq!(text.contains("in team:acme/a1"), fans, "{w}: {text}");
                        }
                    }
                }
                checked += 1;
            }
            checked
        })
        .buffer_unordered(8)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum::<usize>();

    // Unset, acme reads itself alone again.
    admin(s, set(json!({"reads": []}))).await;
    let (_, body) = post(
        &format!("{s}/search"),
        a_reader,
        &json!({"calls": [["owner", 50]]}),
    )
    .await;
    assert!(
        body.contains("xorgalpha") && !body.contains("xteamalpha1"),
        "{body}"
    );
    checked + 1
}

/// Asking to join an org (`memstore join` as someone the org doesn't hold) is recorded and grants nothing:
/// the person reads nothing of the org until an admin lets them in. Only the org's admins answer (a member
/// can't invite or decline), and the approval link is the server's sealed token, which a request can't forge.
#[tokio::test(flavor = "multi_thread")]
async fn asking_to_join_grants_nothing_until_an_admin_approves() {
    let issuer = start_idp().await;
    // A second stand-in: the platform's Auth0 tenant, whose sign-ins name no Organization.
    let tenant = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
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
        )
        .with_auth0(
            Auth0::discover(&tenant, vec![CLIENT_ID.into()], vec![], None, true)
                .await
                .unwrap(),
        )
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_with(
        port,
        Scope::Org,
        vec![],
        Options {
            listener: Some(listener),
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while CLIENT.get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    admin(
        s,
        json!({"op": "create_org", "org": "acme", "admin": "ann@acme.com"}),
    )
    .await;
    admin(
        s,
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}]}),
    )
    .await;
    seed(&store, &Scope::org("acme").unwrap(), "xacmeonly").await;
    let (ann, bob, sam) = (
        id_token(&issuer, "ann@acme.com"),
        id_token(&issuer, "bob@acme.com"),
        id_token(&issuer, "sam@x.com"),
    );
    let me = async |t: &str| -> Value {
        let (status, body) = get(&format!("{s}/me"), t).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    };
    let reads_acme = async |t: &str| -> bool {
        let (status, body) = mcp_call(
            s,
            "?org=acme",
            t,
            "search_memory",
            json!({"query": "xacmeonly"}),
        )
        .await;
        assert!(status == 200 || status == 403, "{status}: {body}");
        status == 200
    };
    let changes = async |t: &str, c: Value| -> (u16, Value) {
        let (status, body) = post(&format!("{s}/admin"), t, &c).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    };
    let request = json!({"op": "request_join", "org": "acme"});

    // Sam asks. Recorded once; asking again writes nothing more; still not a member, reading nothing.
    assert!(!reads_acme(&sam).await, "a stranger read acme");
    let (status, b) = changes(&sam, request.clone()).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!(b["changes"], json!(["sam@x.com asked to join acme"]));
    let (status, b) = changes(&sam, request.clone()).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!(b["changes"], json!([]), "asked twice: nothing more");
    let v = me(&sam).await;
    assert_eq!(
        v["memberships"],
        json!({}),
        "a request is no membership: {v}"
    );
    assert_eq!(v["pending"], json!(["acme"]), "{v}");
    assert!(
        !reads_acme(&sam).await,
        "asking to join let the stranger read acme"
    );
    let (status, b) = changes(&sam, json!({"op": "show", "org": "acme"})).await;
    assert_eq!(status, 403, "the stranger sees the org: {b}");
    // A service key asks for nobody; a member asks for nothing (they're in).
    let (status, b) = changes(ADMIN_KEY, request.clone()).await;
    assert_eq!(status, 403, "{b}");
    let (status, b) = changes(&bob, request.clone()).await;
    assert_eq!(status, 403, "{b}");

    // The admin sees who waits; a member can't answer; the stranger can't answer for themselves.
    let (status, b) = changes(&ann, json!({"op": "show", "org": "acme"})).await;
    assert_eq!(status, 200, "{b}");
    assert!(b["view"]["pending"]["sam@x.com"].is_number(), "{b}");
    let decline = json!({"op": "decline", "org": "acme", "user": "sam@x.com"});
    let invite = json!({"op": "invite", "org": "acme", "members": [{"user": "sam@x.com"}]});
    for (who, what) in [
        (&bob, &decline),
        (&bob, &invite),
        (&sam, &invite),
        (&sam, &decline),
    ] {
        let (status, b) = changes(who, what.clone()).await;
        assert_eq!(status, 403, "{what} by a non-admin: {b}");
    }
    assert!(!reads_acme(&sam).await);

    // The approval link is the server's own sealed token: a made-up one, or none, approves nothing, and a
    // GET changes nothing (this server has no secret key, so it made no links; one with a key opens only
    // tokens it sealed).
    for t in ["", "?t=", "?t=not-a-token", "?t=c2VhbGVkOnh4eHh4"] {
        let r = CLIENT
            .get(format!("{s}/join/approve{t}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "GET {t:?}");
        let r = CLIENT
            .post(format!("{s}/join/approve"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(t.trim_start_matches('?').to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "POST {t:?}");
    }
    assert_eq!(
        me(&sam).await["memberships"],
        json!({}),
        "still no membership"
    );
    assert!(!reads_acme(&sam).await);

    // The admin declines: the request is gone, Sam is still out, and asking again the same day is refused
    // (a decline doesn't mail every admin again at once). The admin invites them anyway: in, and reading
    // as a member.
    let (status, b) = changes(&ann, decline.clone()).await;
    assert_eq!(status, 200, "{b}");
    let v = me(&sam).await;
    assert_eq!(v["pending"], json!([]), "{v}");
    assert_eq!(v["memberships"], json!({}), "{v}");
    assert!(!reads_acme(&sam).await);
    let (status, b) = changes(&sam, request.clone()).await;
    assert_eq!(status, 403, "asked again right after a decline: {b}");
    // Asking to join an org that isn't there, or a workspace, gets one answer: no name is confirmed.
    let (s1, b1) = changes(&sam, json!({"op": "request_join", "org": "nosuchorg"})).await;
    admin(s, json!({"op": "create_workspace", "org": "wsx"})).await;
    let (s2, b2) = changes(&sam, json!({"op": "request_join", "org": "wsx"})).await;
    assert_eq!((s1, s2), (403, 403), "{b1} {b2}");
    let same = |b: &Value, o: &str| b["error"].as_str().unwrap_or("").replace(o, "<org>");
    assert_eq!(
        same(&b1, "nosuchorg"),
        same(&b2, "wsx"),
        "a missing org and a workspace answer alike"
    );
    let (status, b) = changes(&ann, invite.clone()).await;
    assert_eq!(status, 200, "{b}");
    let v = me(&sam).await;
    assert_eq!(v["pending"], json!([]), "{v}");
    assert_eq!(v["memberships"]["acme"]["role"], json!("member"), "{v}");
    let t = Instant::now();
    while !reads_acme(&sam).await {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "approved, still refused"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Through the platform's Auth0 tenant, as themselves (no Organization): the directory says which orgs
    // are theirs. Bob, signed in with Google, is acme's member as the directory lists him. Kim, a stranger
    // with Google, is in nothing, reads nothing, and may ask; an admin's invite lets her in. Through an
    // Organization's enterprise connection with no Organization named, a token proves nothing: the
    // connection asserts whatever email it likes, so even Bob's email reaches nothing and asks nothing.
    let bob_google = tenant_token(&tenant, "bob@acme.com", "google-oauth2|bob");
    let kim_google = tenant_token(&tenant, "kim@x.com", "google-oauth2|kim");
    let bob_saml = tenant_token(&tenant, "bob@acme.com", "samlp|corp|bob");
    // The tenant's own database (a username and password) vouches as Google does.
    let bob_password = tenant_token(&tenant, "bob@acme.com", "auth0|bob");
    let lee_password = tenant_token(&tenant, "lee@x.com", "auth0|lee");
    for (who, t) in [("Google", &bob_google), ("a password", &bob_password)] {
        let v = me(t).await;
        assert_eq!(
            v["memberships"]["acme"]["role"],
            json!("member"),
            "{who}: {v}"
        );
        assert!(
            reads_acme(t).await,
            "a member through the tenant with {who}"
        );
    }
    let v = me(&lee_password).await;
    assert_eq!(v["memberships"], json!({}), "{v}");
    assert!(
        !reads_acme(&lee_password).await,
        "a stranger with a password read acme"
    );
    let (status, b) = changes(&lee_password, request.clone()).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!(b["changes"], json!(["lee@x.com asked to join acme"]));
    assert!(!reads_acme(&lee_password).await, "asking let him in");
    let v = me(&kim_google).await;
    assert_eq!(v["memberships"], json!({}), "{v}");
    assert!(
        !reads_acme(&kim_google).await,
        "a stranger through the tenant read acme"
    );
    let (status, b) = changes(&kim_google, request.clone()).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!(b["changes"], json!(["kim@x.com asked to join acme"]));
    assert!(!reads_acme(&kim_google).await, "asking let her in");
    let v = me(&bob_saml).await;
    assert_eq!(
        v["memberships"],
        json!({}),
        "an enterprise connection's token with no Organization vouched for an email: {v}"
    );
    assert!(!reads_acme(&bob_saml).await);
    let (status, b) = changes(&bob_saml, request.clone()).await;
    assert_eq!(status, 403, "an unvouched email asked to join: {b}");
    let (status, b) = changes(
        &ann,
        json!({"op": "invite", "org": "acme", "members": [{"user": "kim@x.com"}]}),
    )
    .await;
    assert_eq!(status, 200, "{b}");
    let v = me(&kim_google).await;
    assert_eq!(v["memberships"]["acme"]["role"], json!("member"), "{v}");
    assert_eq!(v["pending"], json!([]), "{v}");
    let t = Instant::now();
    while !reads_acme(&kim_google).await {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "approved through the tenant, still refused"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A stranger's MCP client that names no org gets the lobby, not a refusal: it reads nothing, and its one
    // tool asks to join, which grants nothing until an admin approves.
    let lin = id_token(&issuer, "lin@x.com");
    let (status, body) =
        mcp_call(s, "", &lin, "search_memory", json!({"query": "xacmeonly"})).await;
    assert_eq!(status, 200, "{body}");
    let (text, is_error) = tool_text(&body);
    assert!(
        is_error && !body.contains("xacmeonly"),
        "the lobby read acme: {body}"
    );
    assert!(text.contains("no org"), "{text}");
    let (status, body) = mcp_call(s, "", &lin, "request_join", json!({"org": "acme"})).await;
    assert_eq!(status, 200, "{body}");
    let (text, is_error) = tool_text(&body);
    assert!(!is_error && text.contains("Asked to join acme"), "{body}");
    let v = me(&lin).await;
    assert_eq!(v["pending"], json!(["acme"]), "{v}");
    assert_eq!(v["memberships"], json!({}), "{v}");
    assert!(
        !reads_acme(&lin).await,
        "asking through the lobby let her in"
    );

    // Where a person belongs, with each org's Auth0 Organization: what the control plane adds them to, so
    // every org they were added to shows where they choose one, however they were added. Told to a sign-in
    // for no org and to one already in another org's Organization, through the tenant's own connections.
    // An enterprise connection's token learns nothing, with an Organization or without, nor does a stranger.
    admin(
        s,
        json!({"op": "create_org", "org": "beta", "admin": "ann@acme.com"}),
    )
    .await;
    for (org, id) in [("acme", "org_acme"), ("beta", "org_beta")] {
        admin(
            s,
            json!({"op": "set_auth0_org", "org": org, "auth0_org": id}),
        )
        .await;
    }
    let acme = json!([{"org": "acme", "auth0_org": "org_acme"}]);
    let in_beta = |user: &str, sub: &str| tenant_token_for(&tenant, user, sub, Some("org_beta"));
    for (who, t) in [
        ("no Organization", bob_google.clone()),
        (
            "beta's Organization",
            in_beta("bob@acme.com", "google-oauth2|bob"),
        ),
        (
            "a password, in beta's",
            in_beta("bob@acme.com", "auth0|bob"),
        ),
    ] {
        let t0 = Instant::now();
        loop {
            let v = me(&t).await;
            if v["auth0_orgs"] == acme {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(15), "{who}: {v}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    for (who, t) in [
        ("an enterprise connection", bob_saml.clone()),
        (
            "an enterprise connection, in beta's",
            in_beta("bob@acme.com", "samlp|corp|bob"),
        ),
        (
            "a stranger, in beta's",
            in_beta("zed@x.com", "google-oauth2|zed"),
        ),
    ] {
        let v = me(&t).await;
        assert_eq!(v["auth0_orgs"], json!([]), "{who}: {v}");
    }

    // A password account whose email the tenant never verified: anyone can make one under any email, so
    // it reads nothing on its own. Signed in for no Organization it is refused outright; for an org's
    // Organization it asks that org's admins, and once one approves, that account (and no other) reads
    // that org (and no other). A member can't approve it, nor can it approve itself.
    let unverified = |user: &str, sub: &str, org: Option<&str>| {
        tenant_token_verified(&tenant, user, sub, org, false)
    };
    let (status, body) = get(
        &format!("{s}/me"),
        &unverified("bob@acme.com", "auth0|rob", None),
    )
    .await;
    assert_eq!(
        status, 401,
        "an unverified email signed in for no org: {body}"
    );
    let (status, body) = get(
        &format!("{s}/me"),
        &unverified("bob@acme.com", "google-oauth2|rob", Some("org_acme")),
    )
    .await;
    assert_eq!(
        status, 401,
        "an unverified email that isn't a password account: {body}"
    );
    let rob = unverified("bob@acme.com", "auth0|rob", Some("org_acme"));
    let v = me(&rob).await;
    assert_eq!(v["memberships"], json!({}), "{v}");
    assert_eq!(
        v["signin"],
        json!({"org": "acme", "status": "waiting"}),
        "{v}"
    );
    assert_eq!(v["auth0_orgs"], json!([]), "{v}");
    assert!(
        !reads_acme(&rob).await,
        "an unverified password account read acme"
    );
    let (status, b) = changes(&rob, json!({"op": "show", "org": "acme"})).await;
    assert_eq!(status, 403, "{b}");
    // Someone acme doesn't hold asks nobody.
    let v = me(&unverified("zed@x.com", "auth0|zed", Some("org_acme"))).await;
    assert_eq!(
        v["signin"],
        json!({"org": "acme", "status": "not_member"}),
        "{v}"
    );
    let (status, b) = changes(&ann, json!({"op": "show", "org": "acme"})).await;
    assert_eq!(status, 200, "{b}");
    assert_eq!(
        b["view"]["signin_requests"]["bob@acme.com"]["sub"],
        json!("auth0|rob"),
        "{b}"
    );
    assert!(
        b["view"]["signin_requests"].get("zed@x.com").is_none(),
        "{b}"
    );
    let approve = json!({"op": "approve_signin", "org": "acme", "user": "bob@acme.com"});
    for (who, t) in [("a member", &bob), ("the account itself", &rob)] {
        let (status, b) = changes(t, approve.clone()).await;
        assert_eq!(status, 403, "{who} approved an unverified sign-in: {b}");
    }
    assert!(!reads_acme(&rob).await);
    let (status, b) = changes(&ann, approve.clone()).await;
    assert_eq!(status, 200, "{b}");
    let t = Instant::now();
    while !reads_acme(&rob).await {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "approved, still refused"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let v = me(&rob).await;
    assert_eq!(v["memberships"]["acme"]["role"], json!("member"), "{v}");
    assert_eq!(v["signin"], Value::Null, "{v}");
    // Approved is that account only: another password account under the same email still waits.
    let other = unverified("bob@acme.com", "auth0|other", Some("org_acme"));
    assert!(
        !reads_acme(&other).await,
        "an approval let in another account"
    );
    assert_eq!(me(&other).await["signin"]["status"], json!("waiting"));
    // And in acme only: beta's admins vouched for nobody, even once beta holds bob.
    let (status, b) = changes(
        &ann,
        json!({"op": "invite", "org": "beta", "members": [{"user": "bob@acme.com"}]}),
    )
    .await;
    assert_eq!(status, 200, "{b}");
    let rob_beta = unverified("bob@acme.com", "auth0|rob", Some("org_beta"));
    let v = me(&rob_beta).await;
    assert_eq!(
        v["memberships"],
        json!({}),
        "acme's approval reached beta: {v}"
    );
    assert_eq!(
        v["signin"],
        json!({"org": "beta", "status": "waiting"}),
        "{v}"
    );
    // Declined: still out, and told so.
    let (status, b) = changes(
        &ann,
        json!({"op": "decline_signin", "org": "acme", "user": "bob@acme.com"}),
    )
    .await;
    assert_eq!(status, 200, "{b}");
    assert!(!reads_acme(&other).await);
    assert_eq!(me(&other).await["signin"]["status"], json!("declined"));
    assert!(
        reads_acme(&rob).await,
        "a decline of another account undid the approval"
    );
    // Removed from acme, the approval goes too: an invite back starts over.
    let (status, b) = changes(
        &ann,
        json!({"op": "remove_member", "org": "acme", "user": "bob@acme.com"}),
    )
    .await;
    assert_eq!(status, 200, "{b}");
    let (status, b) = changes(
        &ann,
        json!({"op": "invite", "org": "acme", "members": [{"user": "bob@acme.com"}]}),
    )
    .await;
    assert_eq!(status, 200, "{b}");
    let t = Instant::now();
    while reads_acme(&rob).await {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "the approval outlived the removal"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `/agent/run` (the console's Ask, on its arms) answers with what the tools its caller names returned, run as
/// that caller: a member reads their org's memory there and nothing of another org's or a team's, a person
/// outside the org and an ingest key are refused, and the model gets no tool it wasn't offered, a write
/// above all. The model is a stand-in that calls tools on its first turn and answers with what they returned.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_run_reads_only_what_its_caller_may() {
    if !crate::own_process("isolation::an_agent_run_reads_only_what_its_caller_may") {
        return;
    }
    let url = agent_model().await;
    // SAFETY: this test runs in a process of its own (`own_process`), before its server starts.
    unsafe {
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &url);
    }
    let issuer = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
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
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while CLIENT.get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let feed = format!("msk_{}feed", uuid::Uuid::new_v4().simple());
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "a1"}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
        json!({"op": "create_key", "org": "acme", "name": "feed", "role": "ingest", "key": feed}),
    ] {
        admin(s, change).await;
    }
    for (scope, tag) in [
        (Scope::org("acme").unwrap(), "xorgalpha"),
        (Scope::team("acme", "a1").unwrap(), "xteamalpha1"),
        (Scope::org("globex").unwrap(), "xorgbeta"),
    ] {
        seed(&store, &scope, tag).await;
    }
    let tags = |t: &[&str]| t.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let ask = json!({"question": "who owns what?", "tools": ["search_memory", "get_episode"]});
    let run = |bearer: String| {
        let ask = ask.clone();
        async move { post(&format!("{s}/agent/run?org=acme"), &bearer, &ask).await }
    };

    // A member reads her org's memory through the run, and nothing of a team's or the other org's.
    // The server reads a snapshot up to ~1 s old: run until the org's memory reaches her.
    let alice = id_token(&issuer, "alice@acme.com");
    let t = Instant::now();
    let (status, body) = loop {
        let (status, body) = run(alice.clone()).await;
        if status != 200 || body.contains("xorgalpha") || t.elapsed() > Duration::from_secs(15) {
            break (status, body);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(status, 200, "{body}");
    assert_sees("alice's run in acme", &body, &tags(&["xorgalpha"]));
    assert_clean(
        "alice's run in acme",
        &body,
        &tags(&["xteamalpha1", "xorgbeta"]),
        &[],
    );
    // The model asked for a write and a tool it wasn't offered: neither ran, and nothing was written.
    let v: Value = serde_json::from_str(&body).unwrap();
    for name in ["remember_memory", "query_memory"] {
        let step = v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|st| st["tool"] == name)
            .unwrap_or_else(|| panic!("the model's {name} call is in the trace: {body}"));
        assert_eq!(step["ran"], json!(false), "{name} ran: {body}");
    }
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &alice,
        "query_memory",
        json!({"query": {"find": "passages", "text": "xagentwrote"}}),
    )
    .await;
    let (text, _) = tool_text(&raw);
    assert!(!text.contains("xagentwrote"), "the run wrote: {raw}");

    // Another org's admin, selecting acme, and acme's ingest key are refused, and read nothing.
    let gary = id_token(&issuer, "gary@globex.com");
    for (who, bearer) in [("gary in acme", gary), ("acme's ingest key", feed)] {
        let (status, body) = run(bearer).await;
        assert_ne!(status, 200, "{who} ran: {body}");
        assert_clean(
            who,
            &body,
            &tags(&["xorgalpha", "xteamalpha1", "xorgbeta"]),
            &[],
        );
    }
}

/// A model API (OpenAI-compatible) standing in for an agent: on its first turn it calls search_memory, plus a
/// write (remember_memory) and a tool this run doesn't offer (query_memory); then it answers with every tool
/// result it was given, so whatever a tool returned shows in the answer.
async fn agent_model() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
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
                let results: Vec<&str> = req["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .filter_map(|m| m["content"].as_str())
                    .collect();
                let call = |id: &str, name: &str, args: Value| {
                    json!({"id": id, "type": "function",
                           "function": {"name": name, "arguments": args.to_string()}})
                };
                let message = if results.is_empty() {
                    json!({"role": "assistant", "content": null, "tool_calls": [
                        call("c1", "search_memory", json!({"query": "owner", "k": 50})),
                        call("c2", "remember_memory", json!({"source": "agent", "body": "xagentwrote"})),
                        call("c3", "query_memory", json!({"query": {"find": "passages", "text": "owner"}})),
                    ]})
                } else {
                    json!({"role": "assistant", "content": results.join("\n")})
                };
                let body = json!({"choices": [{"message": message, "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 10, "completion_tokens": 10}})
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
    url
}

/// A person's profile (`memstore::profile`, the setting `profile`) is theirs alone, as the notes it is folded from
/// are. Carol and Bob each tell their agents five things in acme, which folds profiles, so each has one. No one
/// else reads Carol's on any path: acme's admin, another member who has a profile of their own, a member of another
/// team, another org's member, acme's reader key and the operator, by `start_session`, its episode id and its
/// document key (`get_episode`, `/episode`, `/browse`, `/document`, including the list of documents), passage and
/// fact search, nor the first ask for advice in their own session, which hands the notes that bear on its task (an
/// agent's on `/mcp`, the hooks' on `/mcp/hook`); nor an agent run (`/agent/run`) for someone else that asks for it by
/// id and by key. Carol reads hers: by a start that says what she asked, and by the first ask in a session that
/// started with nothing asked, whose start hands none. So is what dreaming over her notes concluded
/// (`memstore::notes_dream`): a session of hers says her first note changed, in a record the agent wrote, so the
/// replacement proposed stands as a conflict; the record that says so (which quotes the session) is handed and read by
/// her alone, on every path.
/// The server has a BigQuery inbox (`AMPUP_MEMORY_INBOX_TABLE`, as a GCP deployment does): the deployment's one inbox,
/// drained into the default org's memory, takes the served scope's writes only. What acme's members remember goes
/// into acme's memory, never the inbox; acme's reader key is refused a write still.
#[tokio::test(flavor = "multi_thread")]
async fn a_persons_profile_reaches_no_one_else() {
    if !crate::own_process("isolation::a_persons_profile_reaches_no_one_else") {
        return;
    }
    let carol_doc = memstore::profile::document("carol@acme.com");
    let carol_profile = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let url = profile_model(carol_doc.clone(), carol_profile.clone()).await;
    // SAFETY: this test runs in a process of its own (`own_process`), before its server starts.
    unsafe {
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &url);
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
        std::env::set_var("AMPUP_MEMORY_KEEP", "off");
        // The notes that bear on a session's task are picked by their words, never by a decisions model the
        // machine's keys would reach.
        std::env::set_var("AMPUP_MEMORY_DECIDE", "off");
        // An inbox with no project to reach: a write sent to it fails.
        std::env::set_var("AMPUP_MEMORY_INBOX_TABLE", "project.memstore_api.inbox");
        std::env::remove_var("GCP_PROJECT");
    }
    let issuer = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
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
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(auth),
            ..Default::default()
        },
    ));
    let s = format!("http://127.0.0.1:{port}");
    let s = s.as_str();
    let up = Instant::now();
    while CLIENT.get(s).send().await.is_err() {
        assert!(
            up.elapsed() < Duration::from_secs(30),
            "server never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reader = format!("msk_{}dash", uuid::Uuid::new_v4().simple());
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "a1"}),
        json!({"op": "create_team", "org": "acme", "team": "a2"}),
        json!({"op": "invite", "org": "acme", "members": [
            {"user": "bob@acme.com"},
            {"user": "carol@acme.com", "teams": ["a1"]},
            {"user": "dan@acme.com", "teams": ["a2"]}]}),
        json!({"op": "create_org", "org": "globex", "admin": "gary@globex.com"}),
        json!({"op": "invite", "org": "globex", "members": [{"user": "erin@globex.com"}]}),
        json!({"op": "create_key", "org": "acme", "name": "dash", "role": "reader", "key": reader}),
        json!({"op": "set_settings", "org": "acme", "settings": {"profile": true}}),
    ] {
        admin(s, change).await;
    }
    let person = |email: &str| id_token(&issuer, email);
    let (carol, bob) = (person("carol@acme.com"), person("bob@acme.com"));
    // Five notes each, theirs alone: the fifth folds each profile.
    for (bearer, tag) in [(&carol, "xprofcarol"), (&bob, "xprofbob")] {
        for i in 0..5 {
            let note = json!({"source": "notes", "body": format!("{tag} preference {i}: indent with {i} spaces")});
            let (_, raw) = mcp_call(s, "?org=acme", bearer, "remember_memory", note).await;
            let (text, failed) = tool_text(&raw);
            assert!(
                !failed
                    && text.contains("remembered in org:acme")
                    && text.contains("visible to user:"),
                "an acme member's note not in acme's memory: {raw}"
            );
        }
    }
    // The served scope's writes go to the inbox, which this server can't reach: the call fails inside (with no
    // inbox, the default org's memory takes no writes, and the tool isn't offered). Acme's reader key writes nowhere.
    let note = json!({"source": "notes", "body": "xprofserved for the inbox"});
    let (_, raw) = mcp_call(s, "", ADMIN_KEY, "remember_memory", note.clone()).await;
    assert_eq!(tool_text(&raw), ("tool failed".to_string(), true), "{raw}");
    let (_, raw) = mcp_call(s, "", &reader, "remember_memory", note).await;
    let (text, failed) = tool_text(&raw);
    assert!(
        (failed || raw.contains("\"error\"")) && !text.contains("remembered in"),
        "acme's reader key wrote: {raw}"
    );
    let acme = Scope::org("acme").unwrap();
    let doc = |user: &str| memstore::profile::document(user);
    let t = Instant::now();
    let (carol_id, carol_body) = loop {
        let session = store.pin(&acme, memstore::MAIN).await.unwrap();
        let mine = |user: &str| {
            let (session, store) = (&session, &store);
            let user = user.to_string();
            async move {
                session
                    .document(
                        store,
                        &memstore::Principal::new(&user, &[]),
                        &doc(&user),
                        None,
                    )
                    .await
                    .unwrap()
            }
        };
        if let (Some((id, _, body)), Some(_)) =
            (mine("carol@acme.com").await, mine("bob@acme.com").await)
        {
            break (id, String::from_utf8(body).unwrap());
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "no profiles were folded"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        carol_body.contains("xprofcarol") && carol_body.contains("\"lines\""),
        "{carol_body}"
    );
    *carol_profile.lock().unwrap() = carol_id.clone();
    // A session of hers says her first note changed; her notes are dreamed over. An ended session's record is the
    // agent's text, whose `User:` line may be the agent's own: the replacement the model proposes from it stands as a
    // conflict, the line no one's.
    let decided = dream_notes(
        s,
        "?org=acme",
        &carol,
        (&store, &acme, "carol@acme.com"),
        "User: xprofdreamcarol we indent with tabs now, not spaces\nAssistant: noted",
    )
    .await;
    let conflict = decided
        .iter()
        .find(|d| d["kind"] == "conflict" && d["proposed"] == "replaced" && d["from"] == "unknown")
        .unwrap_or_else(|| panic!("no replacement made a conflict: {decided:?}"));
    assert!(
        !decided.iter().any(|d| d["kind"] == "replaced"),
        "{decided:?}"
    );
    let (dream_key, dream_id) = (
        conflict["record"].as_str().unwrap().to_string(),
        conflict["episode"].as_str().unwrap().to_string(),
    );

    // What a session's start or first ask is about: the notes that bear on it are handed.
    let task = "how should I indent this file, with spaces?";
    // Carol reads her own: by her session's start, its id and its key.
    for (tool, args) in [
        ("start_session", json!({"harness": "test", "context": task})),
        (
            "get_episode",
            json!({"episode_id": carol_id, "whole": true}),
        ),
        ("get_episode", json!({"document": carol_doc})),
    ] {
        let (_, raw) = mcp_call(s, "?org=acme", &carol, tool, args).await;
        let (text, _) = tool_text(&raw);
        assert_sees(&format!("carol {tool}"), &text, &["xprofcarol".to_string()]);
        assert_clean(
            &format!("carol {tool}"),
            &text,
            &["xprofbob".to_string()],
            &[],
        );
    }
    // A start with nothing asked hands none of her notes, and says they come with the session's first ask, which
    // hands them (an agent's ask, and the hooks').
    for path in ["/mcp", "/mcp/hook"] {
        let (_, raw) = mcp_call(
            s,
            "?org=acme",
            &carol,
            "start_session",
            json!({"harness": "test"}),
        )
        .await;
        let (text, _) = tool_text(&raw);
        assert!(
            !text.contains("xprofcarol") && text.contains("remembered_later"),
            "carol's context-less start: {text}"
        );
        let sid = serde_json::from_str::<Value>(&text).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let ask = first_ask(path, &sid, task);
        let (_, raw) = post(&format!("{s}{path}?org=acme"), &carol, &ask).await;
        let (text, _) = tool_text(&raw);
        assert_sees(
            &format!("carol's first ask on {path}"),
            &text,
            &["xprofcarol".to_string()],
        );
        assert_clean(
            &format!("carol's first ask on {path}"),
            &text,
            &["xprofbob".to_string()],
            &[],
        );
    }
    // And what her notes dream concluded: her start hands the note in conflict with the session's words, and the
    // record reads, by its key and its id, once the server reads past it (its snapshot is up to a second old).
    for (tool, args) in [
        ("start_session", json!({"harness": "test", "context": task})),
        ("get_episode", json!({"document": dream_key})),
        (
            "get_episode",
            json!({"episode_id": dream_id, "whole": true}),
        ),
    ] {
        let t = std::time::Instant::now();
        loop {
            let (_, raw) = mcp_call(s, "?org=acme", &carol, tool, args.clone()).await;
            let text = tool_text(&raw).0;
            if text.contains("xprofdreamcarol") || t.elapsed() > std::time::Duration::from_secs(10)
            {
                assert_sees(
                    &format!("carol's dream {tool}"),
                    &text,
                    &["xprofdreamcarol".to_string()],
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    // No one else reads it, on any path.
    let foreign = vec!["xprofcarol".to_string(), "xprofdreamcarol".to_string()];
    let asked = vec![carol_doc.clone(), dream_key.clone()];
    let others = [
        ("acme admin", person("alice@acme.com"), "?org=acme"),
        ("acme member with a profile", bob.clone(), "?org=acme"),
        ("a2 member", person("dan@acme.com"), "?org=acme"),
        (
            "a2 member in a2",
            person("dan@acme.com"),
            "?org=acme&team=a2",
        ),
        ("globex member", person("erin@globex.com"), "?org=globex"),
        (
            "globex member naming acme",
            person("erin@globex.com"),
            "?org=acme",
        ),
        ("acme reader key", reader.clone(), ""),
        ("operator", ADMIN_KEY.to_string(), ""),
        ("operator naming acme", ADMIN_KEY.to_string(), "?org=acme"),
    ];
    let mut checked = 0;
    for (name, bearer, sel) in &others {
        let what = |p: &str| format!("{name} {p}{sel}");
        // Their own session's first ask (one whose start had nothing asked), an agent's and the hooks'.
        for path in ["/mcp", "/mcp/hook"] {
            let (_, raw) =
                mcp_call(s, sel, bearer, "start_session", json!({"harness": "test"})).await;
            assert_clean(&what(&format!("{path} start_session")), &raw, &foreign, &[]);
            let started = serde_json::from_str::<Value>(&tool_text(&raw).0).ok();
            let Some(sid) = started.as_ref().and_then(|v| v["session_id"].as_str()) else {
                continue;
            };
            let (_, answer) = post(
                &format!("{s}{path}{sel}"),
                bearer,
                &first_ask(path, sid, task),
            )
            .await;
            assert_clean(
                &what(&format!("{path} first get_advice")),
                &answer,
                &foreign,
                &[],
            );
            checked += 1;
        }
        for (tool, args) in [
            ("start_session", json!({"harness": "test", "context": task})),
            (
                "get_episode",
                json!({"episode_id": carol_id, "whole": true}),
            ),
            ("get_episode", json!({"document": carol_doc})),
            // Her notes dream's record, by its key and its id, and the list of its documents.
            ("get_episode", json!({"document": dream_key})),
            (
                "get_episode",
                json!({"episode_id": dream_id, "whole": true}),
            ),
            ("get_episode", json!({"document": "notes-dream/"})),
            // By her notes' words: a query is echoed, so not by the tag.
            (
                "search_memory",
                json!({"query": "preference indent spaces", "k": 50}),
            ),
            (
                "query_memory",
                json!({"query": {"find": "passages", "text": "preference indent", "limit": 100}}),
            ),
            ("list_facts", json!({"limit": 500})),
        ] {
            let (_, raw) = mcp_call(s, sel, bearer, tool, args).await;
            assert_clean(&what(&format!("/mcp {tool}")), &raw, &foreign, &asked);
            checked += 1;
        }
        for (path, body) in [
            ("/episode", json!({"calls": [[carol_id]]})),
            ("/document", json!({"calls": [[""]]})),
            ("/document", json!({"calls": [[carol_doc]]})),
            ("/browse", json!({"op": "episode", "id": carol_id})),
            ("/episode", json!({"calls": [[dream_id]]})),
            ("/document", json!({"calls": [[dream_key]]})),
            ("/browse", json!({"op": "episode", "id": dream_id})),
            (
                "/search",
                json!({"calls": [["preference indent spaces", 50, -1, 400]]}),
            ),
        ] {
            let (_, answer) = post(&format!("{s}{path}{sel}"), bearer, &body).await;
            assert_clean(&what(path), &answer, &foreign, &asked);
            // The list of documents never names her profile's key, nor her notes dream's.
            for key in &asked {
                assert!(
                    !answer.contains(key.as_str()) || body.to_string().contains(key.as_str()),
                    "{}: {answer}",
                    what(path)
                );
            }
            checked += 1;
        }
    }
    // Bob's session's start hands his own profile, never hers.
    let (_, raw) = mcp_call(
        s,
        "?org=acme",
        &bob,
        "start_session",
        json!({"harness": "test", "context": task}),
    )
    .await;
    assert_sees(
        "bob start_session",
        &tool_text(&raw).0,
        &["xprofbob".to_string()],
    );

    // An agent run for someone else, asking for her profile by id and by key: nothing of it.
    let ask =
        json!({"question": "what does carol prefer?", "tools": ["search_memory", "get_episode"]});
    for bearer in [person("alice@acme.com"), bob.clone()] {
        let (status, body) = post(&format!("{s}/agent/run?org=acme"), &bearer, &ask).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("get_episode"),
            "the run's model asked for her profile: {body}"
        );
        assert_clean(
            "an agent run for someone else",
            &body,
            &foreign,
            std::slice::from_ref(&carol_doc),
        );
        checked += 1;
    }
    eprintln!("checked {checked} responses");
}

/// A session's first ask for advice on `path`: an agent's (`/mcp`), or the hooks' (`/mcp/hook`, `remembered`).
fn first_ask(path: &str, session_id: &str, task: &str) -> Value {
    let mut args = json!({"session_id": session_id, "question": task});
    if path == "/mcp/hook" {
        args["remembered"] = json!(true);
        args["max_chars"] = json!(7_500);
    }
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
           "params": {"name": "get_advice", "arguments": args}})
}

/// Ends a session of `bearer`'s in `sel` with `transcript`, then dreams over `user`'s notes in `scope`
/// (`memstore::notes_dream`, as `memstore dream --notes --person` does, with the stand-in model, which proposes
/// replacing their oldest note by the session's first `User:` line, a conflict as the record is the agent's text):
/// the decisions it committed, each naming its record and episode.
pub(crate) async fn dream_notes(
    server: &str,
    sel: &str,
    bearer: &str,
    (store, scope, user): (&Store, &Scope, &str),
    transcript: &str,
) -> Vec<Value> {
    let (_, raw) = mcp_call(
        server,
        sel,
        bearer,
        "start_session",
        json!({"harness": "test"}),
    )
    .await;
    let started: Value = serde_json::from_str(&tool_text(&raw).0).unwrap();
    let id = started["session_id"]
        .as_str()
        .expect("a session")
        .to_string();
    let (_, raw) = mcp_call(
        server,
        sel,
        bearer,
        "end_session",
        json!({"session_id": id, "transcript": transcript, "outcome": "done"}),
    )
    .await;
    assert!(!tool_text(&raw).1, "{raw}");
    let ex = memstore::extract::Extractor::notes_dream_from_env()
        .unwrap()
        .expect("a model");
    let budget = memstore::notes_dream::Budget::from_env().unwrap();
    memstore::notes_dream::pass(
        store,
        scope,
        &ex,
        &budget,
        false,
        Some(user),
        false,
        false,
        &mut |_| {},
    )
    .await
    .unwrap()
    .decisions
}

/// A model API (OpenAI-compatible) standing in for both models the profile flow calls: a profile fold adds each note
/// as its own line, in its own words; an agent run (a request offering tools) first asks for Carol's profile by its
/// key (`carol_doc`) and by its episode id (`carol_profile`, once known) and searches for it, then answers with every
/// tool result it was given. Anything else (extraction) finds nothing.
pub(crate) async fn profile_model(
    carol_doc: String,
    carol_profile: std::sync::Arc<std::sync::Mutex<String>>,
) -> String {
    stand_in(carol_doc, carol_profile, None).await
}

/// A fold's model call held until the test lets it answer: how many folds have asked, and whether they may answer.
pub(crate) type Gate = (
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::sync::watch::Receiver<bool>,
);

/// [`profile_model`] whose fold answers wait at `gate` (a fold's read done, its commit not yet made).
pub(crate) async fn gated_profile_model(gate: Gate) -> String {
    stand_in(String::new(), Default::default(), Some(gate)).await
}

async fn stand_in(
    carol_doc: String,
    carol_profile: std::sync::Arc<std::sync::Mutex<String>>,
    gate: Option<Gate>,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (carol_doc, carol_profile, gate) =
                (carol_doc.clone(), carol_profile.clone(), gate.clone());
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
                let messages = req["messages"].as_array().cloned().unwrap_or_default();
                let prompt = messages
                    .iter()
                    .find_map(|m| m["content"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let message = if req["tools"].is_array() {
                    let results: Vec<&str> = messages
                        .iter()
                        .filter(|m| m["role"] == "tool")
                        .filter_map(|m| m["content"].as_str())
                        .collect();
                    let call = |id: &str, name: &str, args: Value| {
                        json!({"id": id, "type": "function",
                               "function": {"name": name, "arguments": args.to_string()}})
                    };
                    if results.is_empty() {
                        let id = carol_profile.lock().unwrap().clone();
                        json!({"role": "assistant", "content": null, "tool_calls": [
                            call("c1", "get_episode", json!({"document": carol_doc})),
                            call("c2", "get_episode", json!({"episode_id": id, "whole": true})),
                            call("c3", "search_memory", json!({"query": "preference indent spaces", "k": 50})),
                        ]})
                    } else {
                        json!({"role": "assistant", "content": results.join("\n")})
                    }
                } else if prompt.starts_with("You check one person's memory") {
                    // A notes dream (`memstore::notes_dream`): the oldest note replaced by the first session, in the
                    // words of the first thing the session has the user say.
                    let said = prompt
                        .lines()
                        .skip_while(|l| *l != "Sessions:")
                        .find_map(|l| l.strip_prefix("User: "))
                        .unwrap_or_default();
                    let reply = json!({"replaced": [{"note": "N1", "by": "S1", "now": said, "quote": said,
                                                     "why": "the session says it changed"}]});
                    json!({"role": "assistant", "content": reply.to_string()})
                } else if prompt.starts_with("You keep one person's profile") {
                    if let Some((asked, mut open)) = gate {
                        asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        open.wait_for(|o| *o).await.unwrap();
                    }
                    // Every note `N<k> (<day>) <text>` added as a line in its own words.
                    let ops: Vec<Value> = prompt
                        .lines()
                        .filter_map(|l| {
                            let (n, rest) = l.strip_prefix('N')?.split_once(" (")?;
                            n.parse::<usize>().ok()?;
                            let (_, text) = rest.split_once(") ")?;
                            Some(json!({"note": format!("N{n}"), "op": "add", "text": text}))
                        })
                        .collect();
                    json!({"role": "assistant", "content": json!({"ops": ops}).to_string()})
                } else {
                    json!({"role": "assistant", "content": "{}"})
                };
                let body = json!({"choices": [{"message": message, "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 10, "completion_tokens": 10}})
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
    url
}

/// A value an owner approved outranks a newer statement no one approved on read, but only for readers of the approved
/// fact: someone who may not read it gets the newest value they may read, never the approved one, never an empty answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_approved_value_reaches_only_its_readers() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local_dir(dir.path()).await.unwrap();
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let issuer = start_idp().await;
    let keys = json!([{"id":"platform","sha256":auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),"user":"svc:platform","admin":true}]);
    let tauth = TokenAuth::new(Some(&keys.to_string()), None)
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
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(tauth),
            ..Default::default()
        },
    ));
    let url = format!("http://127.0.0.1:{port}");
    for change in [
        json!({"op":"create_org","org":"acme","admin":"alice@acme.com"}),
        json!({"op":"invite","org":"acme","members":[{"user":"carol@acme.com"},{"user":"dave@acme.com"}]}),
        json!({"op":"set_schema","org":"acme","schema":{"name":"policies","types":{"policy":{"predicates":{"refund_days":{"type":"string"}}}}}}),
    ] {
        admin(&url, change).await;
    }
    let scope = Scope::org("acme").unwrap();
    let (alice, carol, dave) = (
        id_token(&issuer, "alice@acme.com"),
        id_token(&issuer, "carol@acme.com"),
        id_token(&issuer, "dave@acme.com"),
    );
    let call = async |token: &str, req: Value| {
        let mut req = req;
        req["org"] = "acme".into();
        let (status, body) = post(&format!("{url}/admin"), token, &req).await;
        assert_eq!(status, 200, "{req}: {body}");
        serde_json::from_str::<Value>(&body).unwrap()["result"].clone()
    };
    let owner = call(
        &alice,
        json!({"op":"set_entity_owner","entity":"policy:refunds","owner":"carol@acme.com"}),
    )
    .await;
    // A note only Carol and Alice may read (one only Carol read would be her own, which no one reviews), proposed
    // as a change to what Carol owns, and approved by her.
    store.create_branch(&scope, "draft", "agent").await.unwrap();
    let mine = Episode::sourced(
        "policy:refunds-draft",
        b"policy:refunds refund days forty-five",
        &["user:carol@acme.com", "user:alice@acme.com"],
    );
    let approved = Fact::new("policy:refunds", "refund_days", "forty-five").from_episodes(&[&mine]);
    store
        .commit_on(
            &scope,
            "draft",
            "agent",
            std::slice::from_ref(&mine),
            std::slice::from_ref(&approved),
        )
        .await
        .unwrap();
    let p = call(&alice, json!({"op":"propose_branch","branch":"draft"})).await;
    let r = call(&carol, json!({"op":"review_proposal","proposal":p["proposal"]["id"],"digest":p["proposal"]["digest"],"decision":"approve","expires_days":30})).await;
    assert_eq!(r["status"], "published", "{r}");
    // No one owns it now, so a later call goes straight to main (an owned thing's would wait for its owner), and
    // everyone may read that one.
    call(
        &alice,
        json!({"op":"remove_entity_owner","entity":"policy:refunds","revision":owner["revision"]}),
    )
    .await;
    let call_ep = Episode::sourced(
        "call:later",
        b"policy:refunds refund days thirty",
        &["group:acme.all"],
    );
    let newer = Fact::new("policy:refunds", "refund_days", "thirty").from_episodes(&[&call_ep]);
    store
        .commit(
            &scope,
            "extract",
            std::slice::from_ref(&call_ep),
            std::slice::from_ref(&newer),
        )
        .await
        .unwrap();
    let served = async |token: &str| -> Vec<String> {
        let (status, body) = post(
            &format!("{url}/search?org=acme"),
            token,
            &json!({"calls":[["refund days of the refunds policy",10]]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        v["replies"][0]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|h| std::iter::once(h).chain(h["facts"].as_array().into_iter().flatten()))
            .filter(|f| f["entity"] == "policy:refunds" && f["predicate"] == "refund_days")
            .map(|f| f["value"].as_str().unwrap().to_string())
            .collect()
    };
    // Both facts reach the index before anyone reads.
    let start = Instant::now();
    while served(&dave).await.is_empty() && start.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    while served(&carol).await.first().map(String::as_str) != Some("forty-five")
        && start.elapsed() < Duration::from_secs(20)
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (theirs, outsider) = (served(&carol).await, served(&dave).await);
    assert_eq!(theirs, ["forty-five"], "the owner reads her approved value");
    assert_eq!(
        outsider,
        ["thirty"],
        "someone who may not read the approved fact gets the newest they may read"
    );
    // Once the approved value ends, it hides nothing: its readers get the newest that holds, not an empty answer.
    let why = Episode::sourced(
        "policy:refunds-retired",
        b"the forty-five day refund rule is retired",
        &["user:carol@acme.com", "user:alice@acme.com"],
    );
    store
        .close_facts(
            &scope,
            "main",
            "alice@acme.com",
            &[(approved.fact_id.as_str(), memstore::now_micros(), &why)],
        )
        .await
        .unwrap();
    let start = Instant::now();
    while served(&carol).await.first().map(String::as_str) != Some("thirty")
        && start.elapsed() < Duration::from_secs(20)
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        served(&carol).await,
        ["thirty"],
        "an ended approval hides nothing"
    );
}

/// A claim that nothing arrived ("no new Linear tickets in the last hour") is checked against what arrived since, and
/// the check counts only what its reader may read: a count with another reader's episodes, or another org's, would
/// tell them those exist. Carol may read one new Linear ticket, Dave three others, and another org holds two more:
/// each reads a count of their own, never one with what they can't read. So does a session's start, handing each the
/// same claim in a note of their own written after Carol's ticket: Dave's tickets, which only he may read, leave out
/// his and not hers.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_of_absence_counts_only_what_its_reader_may_read() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local_dir(dir.path()).await.unwrap();
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let issuer = start_idp().await;
    let keys = json!([{"id":"platform","sha256":auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),"user":"svc:platform","admin":true}]);
    let tauth = TokenAuth::new(Some(&keys.to_string()), None)
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
            catalog: Some(catalog),
            index_base: Some(store.index_base().to_string()),
            token_auth: Some(tauth),
            ..Default::default()
        },
    ));
    let url = format!("http://127.0.0.1:{port}");
    for change in [
        json!({"op":"create_org","org":"acme","admin":"alice@acme.com"}),
        json!({"op":"invite","org":"acme","members":[{"user":"carol@acme.com"},{"user":"dave@acme.com"}]}),
        json!({"op":"create_org","org":"globex","admin":"gary@globex.com"}),
    ] {
        admin(&url, change).await;
    }
    let (acme, globex) = (Scope::org("acme").unwrap(), Scope::org("globex").unwrap());
    // The claims, an hour old, everyone in acme may read: one about Linear, one about memory as a whole.
    let said = Episode::sourced(
        "slack:C0OPS:#ops:1762900000.000100",
        b"health-bot: No new Linear tickets have reached memory in the last hour. Nothing has reached memory.",
        &["group:acme.all"],
    );
    let hour = 3_600_000_000;
    let claims: Vec<Fact> = [
        (
            "linear tickets",
            "No new Linear tickets have reached memory in the last hour",
        ),
        (
            "ingestion",
            "Nothing has reached memory since it was last checked",
        ),
    ]
    .into_iter()
    .map(|(entity, value)| {
        let mut f = Fact::new(entity, "issue", value).from_episodes(&[&said]);
        f.valid_from = memstore::now_micros() - hour;
        f
    })
    .collect();
    store
        .commit(&acme, "extract", std::slice::from_ref(&said), &claims)
        .await
        .unwrap();
    // Then tickets arrive: one Carol may read, three only Dave may, and two in another org.
    let ticket = |n: u32, acl: &str| {
        Episode::sourced(
            &format!("linear:acme/OPS-{n}"),
            format!("# OPS-{n} Something broke").as_bytes(),
            &[acl],
        )
    };
    store
        .commit(&acme, "ingest", &[ticket(1, "user:carol@acme.com")], &[])
        .await
        .unwrap();
    // Carol and Dave each remember the claim in a note of their own, after Carol's ticket and before Dave's.
    let note = |user: &str| {
        Episode::sourced(
            &format!("mcp:remember_memory:{user}:hourly-check"),
            format!("No new Linear tickets have reached memory since the last check ({user})")
                .as_bytes(),
            &[&format!("user:{user}")],
        )
    };
    store
        .commit(
            &acme,
            "remember",
            &[note("carol@acme.com"), note("dave@acme.com")],
            &[],
        )
        .await
        .unwrap();
    let acme_tickets = vec![
        ticket(2, "user:dave@acme.com"),
        ticket(3, "user:dave@acme.com"),
        ticket(4, "user:dave@acme.com"),
    ];
    store
        .commit(&acme, "ingest", &acme_tickets, &[])
        .await
        .unwrap();
    store.ensure_scope(&globex).await.unwrap();
    store
        .commit(
            &globex,
            "ingest",
            &[ticket(5, "group:globex.all"), ticket(6, "group:globex.all")],
            &[],
        )
        .await
        .unwrap();
    // What a reader is told of each claim, read as of tomorrow so a contradicted one is kept and marked.
    let tomorrow = chrono::Utc::now()
        .checked_add_days(chrono::Days::new(1))
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let notes = async |user: &str| -> BTreeMap<String, String> {
        let token = id_token(&issuer, user);
        let (status, raw) = mcp_call(
            &url,
            "?org=acme",
            &token,
            "search_memory",
            json!({"query": "linear tickets memory ingestion", "format": "json", "as_of": tomorrow}),
        )
        .await;
        assert_eq!(status, 200, "{raw}");
        let (text, err) = tool_text(&raw);
        assert!(!err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        let hits = v.get("hits").cloned().unwrap_or(v);
        hits.as_array()
            .into_iter()
            .flatten()
            .flat_map(|h| std::iter::once(h).chain(h["facts"].as_array().into_iter().flatten()))
            .filter(|h| h["predicate"] == "issue")
            .map(|h| {
                (
                    h["entity"].as_str().unwrap_or_default().to_string(),
                    h["freshness"]["note"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    };
    let start = Instant::now();
    while notes("dave@acme.com").await.len() < 2 && start.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (carol, dave) = (notes("carol@acme.com").await, notes("dave@acme.com").await);
    for (who, seen, want) in [
        ("carol", &carol, "1 linear item"),
        ("dave", &dave, "3 linear items"),
    ] {
        assert_eq!(seen.len(), 2, "{who} reads both claims: {seen:?}");
        for (entity, note) in seen {
            assert!(
                note.starts_with(&format!("contradicted: {want} arrived since")),
                "{who}'s {entity}: {note:?}, not counted as {who} may read"
            );
        }
    }
    // What each one's session's start hands of their own notes: Carol's claim holds as far as she may read, so it is
    // handed, marked so; Dave's tickets contradict his, so it is left out.
    let handed = async |user: &str| -> Vec<String> {
        let (status, raw) = mcp_call(
            &url,
            "?org=acme",
            &id_token(&issuer, user),
            "start_session",
            json!({"context": "check that new linear tickets reach memory", "harness": "claude-code"}),
        )
        .await;
        assert_eq!(status, 200, "{raw}");
        let (text, err) = tool_text(&raw);
        assert!(!err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        v["remembered_for_you"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    };
    let carol = handed("carol@acme.com").await;
    assert!(
        carol.len() == 1
            && carol[0].contains("(carol@acme.com)")
            && carol[0].contains(" — a claim of absence; no linear item has arrived since "),
        "carol's note, unchanged by tickets she can't read: {carol:?}"
    );
    let dave = handed("dave@acme.com").await;
    assert!(
        dave.iter().all(|n| !n.contains("Linear tickets")),
        "dave's note, contradicted by his tickets: {dave:?}"
    );
}
