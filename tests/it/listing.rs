//! The filterable lists (`memstore::listing`, `POST /facts/list` and `/advice/list`), driven on a local store as
//! different callers, with whoever must not see something not seeing it:
//!
//! - facts narrow by source prefix, entity type, predicate, tags, status, valid time and text, page without
//!   repeating or skipping one, and a caller sees only the facts whose episodes they may read, each with only
//!   the sources of those;
//! - advice lists a person's own notes to them alone, the team's only once past the evidence bar (their org's
//!   admins also see the candidates), and narrows by space, category, text, evidence and when it was written.
//!
//! `isolation` drives the same two paths over HTTP for every caller and selection.

use memstore::extract;
use memstore::{Episode, Fact, Principal, Scope};
use serde_json::{Value, json};

const US: i64 = 1_000_000;

/// `listing::facts` as `who`: the answer.
async fn facts(store: &memstore::Store, who: &Principal, req: Value) -> Value {
    let session = store.pin(&Scope::Org, memstore::MAIN).await.unwrap();
    memstore::listing::facts(store, &session, who, &req)
        .await
        .unwrap()
}

/// The ids of the facts a page lists, in order, as `entity predicate`.
fn names(answer: &Value) -> Vec<String> {
    answer["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{} {}",
                f["entity"].as_str().unwrap(),
                f["predicate"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn facts_narrow_by_where_they_came_from_and_what_they_say() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::local_store(&dir.path().join("store")).await;
    store.ensure_scope(&Scope::Org).await.unwrap();
    let (alice, bob) = (Principal::new("alice", &[]), Principal::new("bob", &[]));
    // A conversation only alice may read, a Slack thread and a meeting both may.
    let chat = Episode::sourced(
        "mcp:remember_memory:alice:session:1",
        b"we won deal 1",
        &["user:alice"],
    );
    let slack = Episode::sourced(
        "slack:acme:thread:1",
        b"pat owns deal 1",
        &["user:alice", "user:bob"],
    );
    let call = Episode::sourced(
        "ampup:meeting:1",
        b"pat is the AE",
        &["user:alice", "user:bob"],
    );
    let fact = |entity: &str, predicate: &str, value: &str, ep: &Episode, day: i64| {
        let mut f = Fact::new(entity, predicate, value).from_episodes(&[ep]);
        f.valid_from = (1_700_000_000 + day * 86_400) * US;
        f
    };
    let mut won = fact("deal:1", "stage", "won", &chat, 4);
    let mut owner = fact("deal:1", "owner", "pat", &slack, 3);
    owner.tags = vec!["billing".into()];
    let role = fact("person:pat", "role", "ae", &call, 2);
    let mut lost = fact("deal:2", "stage", "lost", &slack, 1);
    lost.valid_to = Some((1_700_000_000 + 9 * 86_400) * US);
    won.entity_type = Some("deal".into());
    store
        .commit(
            &Scope::Org,
            "test",
            &[chat, slack, call],
            &[won, owner, role, lost],
        )
        .await
        .unwrap();

    // Alice reads all four, newest first.
    let all = facts(&store, &alice, json!({})).await;
    assert_eq!(
        names(&all),
        [
            "deal:1 stage",
            "deal:1 owner",
            "person:pat role",
            "deal:2 stage"
        ],
        "{all}"
    );
    assert_eq!(all["count"], 4);
    assert_eq!(
        names(&facts(&store, &alice, json!({"order": "oldest"})).await)[0],
        "deal:2 stage"
    );

    // Each filter, alone.
    let case = |req: Value, want: &'static [&'static str]| {
        let (store, alice) = (&store, &alice);
        async move {
            let got = facts(store, alice, req.clone()).await;
            let mut have = names(&got);
            have.sort();
            let mut want: Vec<_> = want.iter().map(|s| s.to_string()).collect();
            want.sort();
            assert_eq!(have, want, "{req}: {got}");
        }
    };
    case(
        json!({"predicate": "stage"}),
        &["deal:1 stage", "deal:2 stage"],
    )
    .await;
    case(
        json!({"predicate": ["owner", "role"]}),
        &["deal:1 owner", "person:pat role"],
    )
    .await;
    case(json!({"entity_type": "person"}), &["person:pat role"]).await;
    case(
        json!({"entity_type": ["deal"]}),
        &["deal:1 stage", "deal:1 owner", "deal:2 stage"],
    )
    .await;
    case(json!({"entity": "deal:2"}), &["deal:2 stage"]).await;
    case(json!({"source": "mcp:remember_memory:"}), &["deal:1 stage"]).await;
    case(
        json!({"source": ["slack:", "ampup:meeting:"]}),
        &["deal:1 owner", "person:pat role", "deal:2 stage"],
    )
    .await;
    case(json!({"tags": ["billing"]}), &["deal:1 owner"]).await;
    case(json!({"status": "ended"}), &["deal:2 stage"]).await;
    case(
        json!({"status": "open"}),
        &["deal:1 stage", "deal:1 owner", "person:pat role"],
    )
    .await;
    case(json!({"text": "PAT"}), &["deal:1 owner"]).await;
    let day = |d: i64| (1_700_000_000 + d * 86_400) * US;
    case(
        json!({"valid_from": {"gte": day(2), "lt": day(4)}}),
        &["deal:1 owner", "person:pat role"],
    )
    .await;
    // A time as a date or an RFC 3339 time (day 0 is 2023-11-14T22:13:20Z).
    case(
        json!({"valid_from": {"gt": "2023-11-17"}}),
        &["deal:1 stage", "deal:1 owner"],
    )
    .await;
    case(
        json!({"valid_from": {"lte": "2023-11-16T23:00:00Z"}}),
        &["person:pat role", "deal:2 stage"],
    )
    .await;
    // Filters together.
    case(
        json!({"source": "slack:", "predicate": "stage", "status": "ended"}),
        &["deal:2 stage"],
    )
    .await;
    case(
        json!({"source": "mcp:remember_memory:", "predicate": "owner"}),
        &[],
    )
    .await;

    // Pages of one: every fact once, in order, then no cursor.
    let (mut seen, mut cursor) = (vec![], Value::Null);
    loop {
        let req = if cursor.is_null() {
            json!({"limit": 1})
        } else {
            json!({"limit": 1, "cursor": cursor})
        };
        let page = facts(&store, &alice, req).await;
        seen.extend(names(&page));
        cursor = page["cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(seen.len() < 10, "paging never ends");
    }
    assert_eq!(seen, names(&all), "pages are the whole answer");

    // Each fact names what it cites, with the source.
    let one = facts(
        &store,
        &alice,
        json!({"entity": "deal:1", "predicate": "owner"}),
    )
    .await;
    assert_eq!(
        one["facts"][0]["sources"][0]["source"], "slack:acme:thread:1",
        "{one}"
    );
    assert_eq!(one["facts"][0]["tags"], json!(["billing"]));

    // Bob may not read the conversation: its fact is not listed to him, whatever he filters by, and the
    // sources he is shown are only those of episodes he may read.
    let theirs = facts(&store, &bob, json!({})).await;
    assert_eq!(theirs["count"], 3, "{theirs}");
    assert!(
        !names(&theirs).contains(&"deal:1 stage".to_string()),
        "{theirs}"
    );
    for req in [
        json!({"source": "mcp:remember_memory:"}),
        json!({"predicate": "stage", "status": "open"}),
    ] {
        let got = facts(&store, &bob, req.clone()).await;
        assert!(
            !names(&got).contains(&"deal:1 stage".to_string()),
            "{req}: {got}"
        );
    }
    assert!(
        !theirs.to_string().contains("mcp:remember_memory"),
        "{theirs}"
    );

    // A request that can't be read says why.
    let session = store.pin(&Scope::Org, memstore::MAIN).await.unwrap();
    for bad in [
        json!({"status": "weird"}),
        json!({"limit": 0}),
        json!({"valid_from": {"gte": "soon"}}),
        json!({"cursor": "x"}),
        json!({"predicate": 3}),
    ] {
        let err = memstore::listing::facts(&store, &session, &alice, &bad)
            .await
            .unwrap_err();
        assert!(err.is::<memstore::browse::BadRequest>(), "{bad}: {err:#}");
    }
}

#[tokio::test]
async fn advice_lists_a_persons_own_and_the_teams_once_ratified() {
    if !crate::own_process("listing::advice_lists_a_persons_own_and_the_teams_once_ratified") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = crate::personal_advice::model().await;
    unsafe {
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &url);
        std::env::set_var("AMPUP_MEMORY_EXTRACT_MODEL", "stand-in");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let scope = Scope::Org;
    store.ensure_scope(&scope).await.unwrap();
    let who = |u: &str| Principal::new(u, &["org.all"]);
    let (alice, bob, carol) = (who("alice"), who("bob"), who("carol"));
    // Three people ask runs: the team's note needs three, and alice has enough of her own for hers.
    for (p, n) in [(&alice, 12usize), (&bob, 6), (&carol, 6)] {
        let runs: Vec<_> = (0..n)
            .map(|i| {
                json!({"task": format!("Rank the top accounts by churn risk for week {i} by {}", p.user()),
                    "trace": format!("step {i}: queried the table"), "outcome": "done",
                    "reward": if i.is_multiple_of(3) { 1 } else { -1 }, "reason": "checked"})
            })
            .collect();
        memstore::runs::load(&store, &scope, Some(p), &[], &runs)
            .await
            .unwrap();
    }
    let mut tried = std::collections::BTreeSet::new();
    let done = memstore::advice_loop::round(&store, &scope, &ex, None, &mut tried)
        .await
        .unwrap();
    assert!(done.learned.is_some() && done.personal >= 1, "{done:?}");

    let list = |who: &Principal, admin: bool, req: Value| {
        let (store, scope) = (&store, &scope);
        let who = who.clone();
        async move {
            let session = store.pin(scope, memstore::MAIN).await.unwrap();
            memstore::listing::advice(store, &session, &who, &req, admin)
                .await
                .unwrap()
        }
    };
    let spaces = |a: &Value| -> Vec<String> {
        a["advice"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e["space"].as_str().unwrap(),
                    e["key"].as_str().unwrap()
                )
            })
            .collect()
    };

    // Alice: her own note, with no tally; the team's note is a candidate (no one's session has ratified it),
    // and a plain member doesn't see candidates.
    let a = list(&alice, false, json!({})).await;
    assert_eq!(spaces(&a), ["mine mine:work/do-it"], "{a}");
    assert!(a["advice"][0]["evidence"].is_null(), "{a}");
    assert_eq!(a["advice"][0]["ratified"], true);

    // Her org's admin also sees the team's candidate, with its tally, marked as not ratified.
    let a = list(&alice, true, json!({})).await;
    let mut got = spaces(&a);
    got.sort();
    assert_eq!(got, ["mine mine:work/do-it", "team work/do-it"], "{a}");
    let team = a["advice"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["space"] == "team")
        .unwrap();
    assert_eq!(team["ratified"], false, "{a}");
    assert_eq!(team["evidence"]["served"], 0, "{a}");
    assert!(team["note_chars"].as_u64().unwrap() > 0);

    // Bob: none of alice's, and not the candidate without admin rights.
    let b = list(&bob, false, json!({})).await;
    assert!(spaces(&b).is_empty(), "{b}");
    // Admin or not, asking for alice's space gives him his own (none), never hers.
    let b = list(&bob, true, json!({"space": "mine"})).await;
    assert!(spaces(&b).is_empty(), "{b}");

    // Filters, as the admin who sees both.
    let only = |req: Value, want: &[&str]| {
        let list = &list;
        let alice = &alice;
        let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        async move {
            let a = list(alice, true, req.clone()).await;
            let mut have = spaces(&a);
            have.sort();
            assert_eq!(have, want, "{req}: {a}");
        }
    };
    only(json!({"space": "team"}), &["team work/do-it"]).await;
    only(json!({"space": "mine"}), &["mine mine:work/do-it"]).await;
    only(
        json!({"category": "work"}),
        &["mine mine:work/do-it", "team work/do-it"],
    )
    .await;
    only(json!({"category": "other"}), &[]).await;
    only(
        json!({"situation": ["do-it"]}),
        &["mine mine:work/do-it", "team work/do-it"],
    )
    .await;
    only(
        json!({"text": "CHECK FIRST"}),
        &["mine mine:work/do-it", "team work/do-it"],
    )
    .await;
    only(json!({"text": "nothing says this"}), &[]).await;
    only(json!({"ratified": true}), &["mine mine:work/do-it"]).await;
    only(json!({"ratified": false}), &["team work/do-it"]).await;
    only(json!({"min_helped": 1}), &[]).await;
    only(
        json!({"min_served": 0}),
        &["mine mine:work/do-it", "team work/do-it"],
    )
    .await;
    only(
        json!({"learned": {"gte": "2020-01-01"}}),
        &["mine mine:work/do-it", "team work/do-it"],
    )
    .await;
    only(json!({"learned": {"lt": "2020-01-01"}}), &[]).await;

    // The note's text only when asked for.
    let plain = list(&alice, true, json!({"space": "mine"})).await;
    assert!(plain["advice"][0].get("text").is_none(), "{plain}");
    let full = list(&alice, true, json!({"space": "mine", "include": "text"})).await;
    assert!(
        full["advice"][0]["text"]
            .as_str()
            .unwrap()
            .contains("check first"),
        "{full}"
    );

    // Pages of one: both entries once, then no cursor.
    let first = list(&alice, true, json!({"limit": 1})).await;
    assert_eq!(first["advice"].as_array().unwrap().len(), 1, "{first}");
    let cursor = first["cursor"].clone();
    assert!(cursor.is_string(), "{first}");
    let second = list(&alice, true, json!({"limit": 1, "cursor": cursor})).await;
    assert!(second["cursor"].is_null(), "{second}");
    let mut paged = spaces(&first);
    paged.extend(spaces(&second));
    paged.sort();
    assert_eq!(paged, ["mine mine:work/do-it", "team work/do-it"]);

    // Forgetting alice takes her own notes out of the list, and leaves the team's.
    store.forget_in(&scope.namespace(), "alice").await.unwrap();
    let after = list(&alice, true, json!({})).await;
    assert_eq!(spaces(&after), ["team work/do-it"], "{after}");
}
