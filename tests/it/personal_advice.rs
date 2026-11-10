//! One person's own learned advice (`advice::Space::mine`) is theirs alone, and goes when they do:
//! - learned from the runs they asked only, it is served to them at once (no evidence bar), under a `mine:` key;
//! - another person, asking as themselves or for their space, is served none of it and can list none of its documents,
//!   and the team's advice (never learned here) offers them nothing from it either;
//! - forgetting the person deletes it (`offboarding`'s path, `Store::forget_in`): their map and notes are gone
//!   for everyone, and the engine's own read of the map finds none.

use memstore::advice::{Format, Serving, Space};
use memstore::extract;
use memstore::{Principal, Scope};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A model API (OpenAI-compatible) that sorts every task into one situation, writes one note, and routes to it.
pub(crate) async fn model() -> String {
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
                let req: serde_json::Value =
                    serde_json::from_slice(&buf[start..start + len]).unwrap();
                let prompt = req["messages"][0]["content"].as_str().unwrap();
                let reply = if prompt.starts_with("You keep the map") {
                    let tasks: Vec<&str> = prompt
                        .lines()
                        .filter_map(|l| l.split_once(": ").map(|(h, _)| h))
                        .filter(|h| {
                            h.len() > 1
                                && h.starts_with('T')
                                && h[1..].bytes().all(|b| b.is_ascii_digit())
                        })
                        .collect();
                    serde_json::json!({"map": {"work": {"description": "d", "situations": {"do-it": {"when": "any task"}}}},
                        "assign": tasks.iter().map(|t| (t.to_string(), serde_json::json!(["work", "do-it"]))).collect::<serde_json::Map<_, _>>()})
                    .to_string()
                } else if prompt.starts_with("You write advice") {
                    format!(
                        "# Advice\n\n## When\nany task\n\n## Steps\n1. [how-to] check first (runs: {})\n\n## Pitfalls\nskipping the check\n",
                        // Every run shown cited: the lesson rests on all of their people.
                        (1..=40)
                            .map(|n| format!("R{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else if prompt.contains("Is the advice for this very task") {
                    // Whether routed advice fits the request: it does.
                    "yes".to_string()
                } else if prompt.contains("Which one line is the kind of task") {
                    "`work/do-it`".to_string()
                } else {
                    "{}".to_string()
                };
                let body = serde_json::json!({
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
    url
}

#[tokio::test]
async fn a_persons_own_advice_is_theirs_alone_and_goes_when_they_do() {
    if !crate::own_process(
        "personal_advice::a_persons_own_advice_is_theirs_alone_and_goes_when_they_do",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
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
    let (alice, bob) = (Principal::new("alice", &[]), Principal::new("bob", &[]));
    let run = |n: usize| {
        serde_json::json!({"task": format!("Rank the top accounts by churn risk for week {n}"),
            "trace": format!("step {n}: queried the table"), "outcome": "done",
            "reward": if n.is_multiple_of(3) { 1 } else { -1 }, "reason": "checked against the report"})
    };
    let runs: Vec<_> = (0..9).map(run).collect();
    memstore::runs::load(&store, &scope, Some(&alice), &[], &runs)
        .await
        .unwrap();
    // Bob asked runs too, none of them in alice's space.
    let theirs: Vec<_> = (20..23).map(run).collect();
    memstore::runs::load(&store, &scope, Some(&bob), &[], &theirs)
        .await
        .unwrap();

    let mine = Space::mine("alice");
    let done = memstore::advice::learn_in(&store, &scope, &ex, &[], 3, &mine)
        .await
        .unwrap();
    assert_eq!(done.runs, 9, "alice's runs only: {done:?}");

    let serving = Serving {
        format: Format::Notes,
        max_chars: None,
        procedures: false,
        skills: Default::default(),
    };
    let session = store.pin(&scope, memstore::MAIN).await.unwrap();
    let ask = |who: &Principal, space: Space| {
        let (session, store, ex) = (&session, &store, &ex);
        let who = who.clone();
        async move {
            memstore::advice::get_in(
                session,
                store,
                &who,
                ex,
                "rank accounts by churn",
                serving,
                None,
                &space,
            )
            .await
            .unwrap()
        }
    };

    // Alice is served her own note, at once, under a key marked as hers.
    let got = ask(&alice, mine.clone())
        .await
        .expect("alice is served her own advice");
    assert_eq!(got.key, "mine:work/do-it");
    assert!(got.note && got.text.contains("check first"));
    assert_eq!(got.skills[0].key, "mine:work/do-it");

    // Bob is served none of it: not for his own space, not asking for hers, and not from the team's.
    assert!(ask(&bob, Space::mine("bob")).await.is_none());
    assert!(ask(&bob, mine.clone()).await.is_none());
    let team = memstore::advice::get(
        &session,
        &store,
        &bob,
        &ex,
        "rank accounts by churn",
        serving,
        None,
    )
    .await
    .unwrap();
    assert!(
        team.is_none(),
        "the team's advice was never learned: {team:?}"
    );
    assert!(
        session
            .documents(&bob, memstore::advice::MINE, None)
            .is_empty()
    );
    assert!(
        session
            .documents(&bob, memstore::advice::PREFIX, None)
            .is_empty()
    );
    let hers = session.documents(&alice, memstore::advice::MINE, None);
    assert!(
        hers.iter().any(|k| k == &mine.note("work/do-it")),
        "alice lists her note: {hers:?}"
    );

    // The judge reads a session's served note from the person's own space.
    assert_eq!(
        Space::served("alice", "mine:work/do-it"),
        mine.note("work/do-it")
    );
    assert_eq!(Space::served("alice", "work/do-it"), "advice/work/do-it");
    // Ids are as an ACL entry names them: an email folds case, any other id is exact, so ids that differ only
    // in case are two people's spaces.
    assert_eq!(
        Space::mine("Amy@X.com").note("k"),
        Space::mine("amy@x.com").note("k")
    );
    assert_ne!(Space::mine("Bob").note("k"), Space::mine("bob").note("k"));

    // Forgetting alice deletes it; nobody, alice included, reads any of it afterwards.
    store.forget_in(&scope.namespace(), "alice").await.unwrap();
    let session = store.pin(&scope, memstore::MAIN).await.unwrap();
    assert!(
        session
            .documents(&alice, memstore::advice::MINE, None)
            .is_empty(),
        "alice's advice survived her being forgotten"
    );
    assert!(
        session
            .engine_document(&store, &mine.map())
            .await
            .unwrap()
            .is_none(),
        "the engine still reads alice's map"
    );
}

/// The server's timer round learns the team's advice and each person's own in one pass, neither starving the other
/// (the team's learn marks every run it saw as tried).
#[tokio::test]
async fn a_round_learns_the_team_and_each_person_apart() {
    if !crate::own_process("personal_advice::a_round_learns_the_team_and_each_person_apart") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
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
    let alice = Principal::new("alice", &[]);
    let runs: Vec<_> = (0..12usize)
        .map(|n| {
            serde_json::json!({"task": format!("Rank the top accounts by churn risk for week {n}"),
                "trace": format!("step {n}: queried the table"), "outcome": "done",
                "reward": if n.is_multiple_of(3) { 1 } else { -1 }, "reason": "checked"})
        })
        .collect();
    memstore::runs::load(&store, &scope, Some(&alice), &[], &runs)
        .await
        .unwrap();
    let mut tried = std::collections::BTreeSet::new();
    let done = memstore::advice_loop::round(&store, &scope, &ex, None, &mut tried)
        .await
        .unwrap();
    // All 12 runs are alice's: the team's map is learned, but no team note comes of one person's traces.
    let team = done.learned.as_ref().expect("the team's map is learned");
    assert_eq!(team.notes, 0, "{done:?}");
    assert_eq!(team.few_people, 1, "{done:?}");
    assert_eq!(done.personal, 1, "and alice's own: {done:?}");
    let session = store.pin(&scope, memstore::MAIN).await.unwrap();
    assert!(
        session
            .documents(&alice, memstore::advice::PREFIX, None)
            .iter()
            .all(|k| !k.contains("do-it")),
        "a team note was written from one person's runs"
    );
    assert!(
        !session
            .documents(&alice, memstore::advice::MINE, None)
            .is_empty()
    );
    assert!(
        session
            .documents(&Principal::new("bob", &[]), memstore::advice::MINE, None)
            .is_empty()
    );
}

/// A team note is written only from the runs of at least `advice_learn_people` (default 3, never below 2) people,
/// whatever the serving bar, so none holds one person's private traces; and a note whose people have since been
/// forgotten is taken out of the map.
#[tokio::test]
async fn a_team_note_needs_runs_from_enough_people() {
    use memstore::advice::{Format, Serving};
    if !crate::own_process("personal_advice::a_team_note_needs_runs_from_enough_people") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
    unsafe {
        std::env::set_var("AMPUP_MEMORY_MODEL_CACHE", "off");
        std::env::set_var("AMPUP_MEMORY_EXTRACT", "openai");
        std::env::set_var("AMPUP_MEMORY_EXTRACT_URL", &url);
        std::env::set_var("AMPUP_MEMORY_EXTRACT_MODEL", "stand-in");
        std::env::remove_var("AMPUP_MEMORY_ADVICE_EVIDENCE");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let scope = Scope::Org;
    store.ensure_scope(&scope).await.unwrap();
    let people: Vec<Principal> = ["alice", "bob", "carol"]
        .iter()
        .map(|p| Principal::new(p, &[]))
        .collect();
    let record = |who: &Principal, from: usize| {
        let runs: Vec<_> = (from..from + 5)
            .map(|n| {
                serde_json::json!({"task": format!("Rank the top accounts by churn risk for week {n}"),
                    "trace": format!("step {n}: queried the table"), "outcome": "done",
                    "reward": if n % 5 == 0 { 1 } else { -1 }, "reason": "checked"})
            })
            .collect();
        let (store, scope, who) = (&store, &scope, who.clone());
        async move {
            memstore::runs::load(store, scope, Some(&who), &[], &runs)
                .await
                .unwrap();
        }
    };
    let everyone = ["*".to_string()];
    let serving = Serving {
        format: Format::Notes,
        max_chars: None,
        procedures: false,
        skills: Default::default(),
    };
    let dave = Principal::new("dave", &[]);
    let asks = |dave: &Principal| {
        let (store, ex, scope, dave) = (&store, &ex, &scope, dave.clone());
        async move {
            let session = store.pin(scope, memstore::MAIN).await.unwrap();
            memstore::advice::get(
                &session,
                store,
                &dave,
                ex,
                "rank accounts by churn",
                serving,
                None,
            )
            .await
            .unwrap()
        }
    };

    // One person's runs: no team note, even where every note is served (an evidence bar of 0) and learning is
    // set to a single person: a team note never rests on fewer than two.
    record(&people[0], 0).await;
    unsafe {
        std::env::set_var("AMPUP_MEMORY_ADVICE_EVIDENCE", "0");
        std::env::set_var("AMPUP_MEMORY_ADVICE_LEARN_PEOPLE", "1");
    }
    let done = memstore::advice::learn(&store, &scope, &ex, &everyone, 3)
        .await
        .unwrap();
    assert_eq!((done.notes, done.few_people), (0, 1), "{done:?}");
    assert!(asks(&dave).await.is_none());
    unsafe {
        std::env::remove_var("AMPUP_MEMORY_ADVICE_EVIDENCE");
        std::env::remove_var("AMPUP_MEMORY_ADVICE_LEARN_PEOPLE");
    }

    // Three people's: the note is written, and readable by the team.
    record(&people[1], 100).await;
    record(&people[2], 200).await;
    let done = memstore::advice::learn(&store, &scope, &ex, &everyone, 3)
        .await
        .unwrap();
    assert_eq!((done.notes, done.few_people), (1, 0), "{done:?}");
    let got = asks(&dave).await.expect("the team's note is served");
    assert_eq!(got.key, "work/do-it");

    // Two of them forgotten: the note is taken out of the map, whatever it was written from.
    for gone in ["bob", "carol"] {
        store.forget_in(&scope.namespace(), gone).await.unwrap();
    }
    let done = memstore::advice::learn(&store, &scope, &ex, &everyone, 3)
        .await
        .unwrap();
    assert_eq!((done.notes, done.few_people), (0, 1), "{done:?}");
    assert!(
        asks(&dave).await.is_none(),
        "a note written from people since forgotten is still served"
    );

    // Written again from three people, then every one of them forgotten: no situation is left, and the note goes.
    record(&people[1], 300).await;
    record(&people[2], 400).await;
    record(&people[0], 500).await;
    let done = memstore::advice::learn(&store, &scope, &ex, &everyone, 3)
        .await
        .unwrap();
    assert_eq!(done.notes, 1, "{done:?}");
    assert!(asks(&dave).await.is_some());
    for gone in ["alice", "bob", "carol"] {
        store.forget_in(&scope.namespace(), gone).await.unwrap();
    }
    memstore::advice::learn(&store, &scope, &ex, &everyone, 3)
        .await
        .unwrap();
    assert!(
        asks(&dave).await.is_none(),
        "a note whose every run was forgotten is still served"
    );
}
