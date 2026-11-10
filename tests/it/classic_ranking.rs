//! `classic_ranking`, end to end: a real `memstore serve` over HTTP on a local catalog holds org `acme` with
//! teams `classic` and `plain`, each with the same facts. Two of them say the same thing about one account, one
//! from an older note and one from a newer note; a third comes from the older note too.
//!
//! - Unset, both teams rank the same way: the near-duplicates fold into one hit, which brings the third fact as a
//!   group, and the newer note doesn't move it ahead as of any date.
//! - Set on `classic`, that team ranks as before the current ranking: no groups, every fact its own hit, scored by reciprocal
//!   rank (k = 60) over the keyword and meaning lists, each scaled by recency as of the search's date. As of a
//!   month after the newer note it leads both lists; as of now both notes are past four months old, equally
//!   scaled, and the older fact leads on the tie.
//! - `plain`, in the same org, ranks as it did all along.
//! - Set off again, `classic` ranks as it did before it was set.

use std::time::{Duration, Instant};

use memstore::auth::{self, TokenAuth};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Scope, Store, platform};
use serde_json::{Value, json};
use sha2::Digest;

const ADMIN_KEY: &str = "msk_platform_admin_for_classic_ranking";
const QUERY: &str = "zephyr renewal blocked by procurement review";
const DAY: i64 = 86_400_000_000;
/// The older and the newer of the two facts that say the same thing. Their ids order the older first, so a
/// tie between them goes to it, and only recency puts the newer one ahead.
const OLDER: &str = "a-zephyr-renewal-older";
const NEWER: &str = "b-zephyr-renewal-newer";
/// From the older note too, about the same account.
const OWNER: &str = "c-zephyr-owner";

static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
});

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

/// `/admin` as the operator; must succeed.
async fn admin(s: &str, req: Value) {
    let (status, body) = post(&format!("{s}/admin"), ADMIN_KEY, &req).await;
    assert_eq!(status, 200, "{req}: {body}");
}

/// The same facts in `scope`, as of `now`: the account's renewal status from an older and a newer note, its
/// owner from the older note, and another account's facts, all older than four months but the newer note's.
async fn seed(store: &Store, scope: &Scope, now: i64) {
    store.ensure_scope(scope).await.unwrap();
    let older = Episode::new(
        "note:zephyr-q1",
        b"zephyr renewal blocked by procurement review, owner priya",
        &["*"],
    );
    let newer = Episode::new(
        "note:zephyr-q2",
        b"zephyr renewal still blocked by procurement review",
        &["*"],
    );
    let other = Episode::new("note:orbit", b"orbit owner dana, stage negotiation", &["*"]);
    let fact = |id: &str, entity: &str, predicate: &str, value: &str, ep: &Episode, age: i64| {
        let mut f = Fact::new(entity, predicate, value).from_episodes(&[ep]);
        f.fact_id = id.into();
        f.valid_from = now - age * DAY;
        f
    };
    let facts = [
        fact(
            OLDER,
            "account:zephyr",
            "renewal_status",
            "blocked by procurement review",
            &older,
            300,
        ),
        fact(
            NEWER,
            "account:zephyr",
            "renewal_status",
            "blocked by procurement review",
            &newer,
            150,
        ),
        fact(OWNER, "account:zephyr", "owner", "priya", &older, 300),
        fact(
            "d-orbit-owner",
            "account:orbit",
            "owner",
            "dana",
            &other,
            400,
        ),
        fact(
            "e-orbit-stage",
            "account:orbit",
            "stage",
            "negotiation",
            &other,
            400,
        ),
    ];
    store
        .commit(scope, "test", &[older, newer, other], &facts)
        .await
        .unwrap();
    memstore::search::update(store, scope).await.unwrap();
}

/// What `/search` answers `key` for the query, valid at `at` (now when None).
async fn search(s: &str, key: &str, at: Option<i64>) -> Vec<Value> {
    let row = match at {
        Some(t) => json!([0, QUERY, 10, t]),
        None => json!([0, QUERY, 10]),
    };
    let (status, body) = post(&format!("{s}/search"), key, &json!({"data": [row]})).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["data"][0][1].as_array().unwrap().clone()
}

/// A ranking as ids: each hit's fact, then the facts its group brought.
fn ranking(hits: &[Value]) -> Vec<(String, Vec<String>)> {
    hits.iter()
        .map(|h| {
            let group = h["facts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| f["fact_id"].as_str().unwrap().to_string())
                .collect();
            (h["fact_id"].as_str().unwrap().to_string(), group)
        })
        .collect()
}

/// A team's rankings as of a month after the newer note, and as of now.
async fn rankings(s: &str, key: &str, then: i64) -> [Vec<(String, Vec<String>)>; 2] {
    [
        ranking(&search(s, key, Some(then)).await),
        ranking(&search(s, key, None).await),
    ]
}

/// Waits until `check` holds for `key`'s rankings (the server reads the directory up to a few seconds old).
async fn until(
    s: &str,
    key: &str,
    then: i64,
    what: &str,
    check: impl Fn(&[Vec<(String, Vec<String>)>; 2]) -> bool,
) -> [Vec<(String, Vec<String>)>; 2] {
    let t = Instant::now();
    loop {
        let got = rankings(s, key, then).await;
        if check(&got) {
            return got;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "{what}: {got:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_that_asks_for_classic_ranking_gets_it_and_no_other_team_does() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = platform::local(&format!("file://{}", dir.path().display()))
        .await
        .unwrap();
    let store =
        Store::new(catalog.clone()).with_index_base(dir.path().join("index").display().to_string());
    store.ensure_scope(&Scope::Org).await.unwrap();
    let keys = json!([{"id": "platform", "sha256": auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
                       "user": "svc:platform", "admin": true}]);
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
            token_auth: Some(TokenAuth::new(Some(&keys.to_string()), None).unwrap()),
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

    // One org, two teams, a reader key for each.
    for change in [
        json!({"op": "create_org", "org": "acme", "admin": "alice@acme.com"}),
        json!({"op": "create_team", "org": "acme", "team": "classic"}),
        json!({"op": "create_team", "org": "acme", "team": "plain"}),
    ] {
        admin(s, change).await;
    }
    let key = |team: &str| {
        let k = format!("msk_{}{team}", uuid::Uuid::new_v4().simple());
        let change = json!({"op": "create_key", "org": "acme", "name": format!("ci_{team}"),
                            "team": team, "role": "reader", "key": k});
        (k, change)
    };
    let (classic, c1) = key("classic");
    let (plain, c2) = key("plain");
    for c in [c1, c2] {
        admin(s, c).await;
    }
    let now = memstore::now_micros();
    for team in ["classic", "plain"] {
        seed(&store, &Scope::team("acme", team).unwrap(), now).await;
    }
    // A month after the newer note: it is a month old, the older one past four months.
    let then = now - 120 * DAY;

    // Unset: the near-duplicates are one hit, which brings the owner from its note, first as of either date.
    let default = |r: &[Vec<(String, Vec<String>)>; 2]| {
        r.iter().all(|r| {
            r.first()
                == Some(&(
                    OLDER.to_string(),
                    vec![NEWER.to_string(), OWNER.to_string()],
                ))
        })
    };
    let before = until(s, &classic, then, "classic, unset", default).await;
    let plain_before = until(s, &plain, then, "plain", default).await;
    assert_eq!(
        before, plain_before,
        "two teams, the same facts, one ranking"
    );

    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "team": "classic",
               "settings": {"classic_ranking": true}}),
    )
    .await;
    let ungrouped = |r: &Vec<(String, Vec<String>)>| r.iter().all(|(_, g)| g.is_empty());
    let got = until(s, &classic, then, "classic, set", |r| {
        r.iter().all(ungrouped) && r[0].len() == 5
    })
    .await;
    let order = |r: &Vec<(String, Vec<String>)>| -> Vec<String> {
        r.iter().take(2).map(|(id, _)| id.clone()).collect()
    };
    // A month after it, the newer note leads both lists; now both are past four months old, and the tie goes
    // to the older fact.
    assert_eq!(order(&got[0]), [NEWER, OLDER], "{got:?}");
    assert_eq!(order(&got[1]), [OLDER, NEWER], "{got:?}");
    // Reciprocal rank: first in both lists, then second in both.
    for (at, first, second) in [(Some(then), NEWER, OLDER), (None, OLDER, NEWER)] {
        let hits = search(s, &classic, at).await;
        let score = |id: &str| {
            hits.iter()
                .find(|h| h["fact_id"] == id)
                .and_then(|h| h["score"].as_f64())
                .unwrap()
        };
        assert!((score(first) - 2.0 / 60.0).abs() < 1e-6, "{hits:?}");
        assert!((score(second) - 2.0 / 61.0).abs() < 1e-6, "{hits:?}");
    }
    // The other team in the org ranks as it did.
    assert_eq!(
        rankings(s, &plain, then).await,
        plain_before,
        "plain moved when classic was set"
    );

    // Set off, classic ranks as it did before it was set.
    admin(
        s,
        json!({"op": "set_settings", "org": "acme", "team": "classic",
               "settings": {"classic_ranking": false}}),
    )
    .await;
    until(s, &classic, then, "classic, set off", |r| *r == before).await;
    assert_eq!(rankings(s, &plain, then).await, plain_before);
}
