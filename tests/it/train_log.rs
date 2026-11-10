//! The training log (`MEMSTORE_TRAIN_LOG`) keeps what the memory has forgotten for no longer than the memory does: an
//! extraction run against a stand-in model logs every call it makes, and then, per path by which an episode stops
//! being readable, its calls leave the log (an exported example never holds what the memory has lost):
//! - `forget` of an episode, before any sweep: the export already leaves it out, and the sweep removes its calls;
//! - an ACL narrowed until none of the episode's original readers can read it: the sweep removes its calls;
//! - the immediate purge the server runs on `forget_memory` (`train_log::purge_ref`);
//! - an org's delete (`train_log::purge_org`) takes that org's prefix and no other's.
//!
//! The export keeps to episodes shared with `*` unless asked, and splits what it writes by episode.

use std::collections::BTreeSet;

use memstore::extract::{self, train_log};
use memstore::{AclTarget, Episode, Scope};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A model API (OpenAI-compatible) that answers an extraction prompt with one fact quoted from the text, and
/// anything else with an empty answer: enough for the engine to commit, reconcile and log.
async fn model() -> String {
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
                let reply = if prompt.contains("Known facts:") {
                    "{}".to_string()
                } else if prompt.starts_with("You keep the map") {
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
                } else if prompt.starts_with("You review one run") {
                    "{\"reward\": 1, \"reason\": \"it got there\"}".to_string()
                } else {
                    // The text is between the last two fence lines.
                    let lines: Vec<&str> = prompt.lines().collect();
                    let fences: Vec<usize> = (0..lines.len())
                        .filter(|&i| lines[i].starts_with("----- ") && lines[i].ends_with(" -----"))
                        .collect();
                    let text =
                        lines[fences[fences.len() - 2] + 1..fences[fences.len() - 1]].join("\n");
                    let words: Vec<&str> = text
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|w| w.len() >= 4)
                        .collect();
                    serde_json::json!({"rows": [{"kind": "decision", "feature": words[0],
                        "value": format!("{} {}", words[1], words[2]),
                        "quote": text.chars().take(40).collect::<String>()}]})
                    .to_string()
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

/// The episodes the log's extract calls for `scope` read.
async fn logged(scope: &Scope) -> BTreeSet<String> {
    train_log::calls(scope)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|c| c.record["refs"]["episode_id"].as_str().map(String::from))
        .collect()
}

/// An export of `scope` to a file: the episodes of the examples it wrote, and its report.
async fn exported(
    store: &memstore::Store,
    scope: &Scope,
    restricted: bool,
    file: &std::path::Path,
) -> (BTreeSet<String>, memstore::train_data::Exported) {
    let mut out = std::fs::File::create(file).unwrap();
    let report = memstore::train_data::export(store, scope, restricted, &mut out)
        .await
        .unwrap();
    let ids = std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter_map(|l| l["meta"]["refs"]["episode_id"].as_str().map(String::from))
        .collect();
    (ids, report)
}

#[tokio::test]
async fn forgotten_memory_leaves_the_training_log() {
    if !crate::own_process("train_log::forgotten_memory_leaves_the_training_log") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", &log);
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_EXTRACT_GOAL_BLIND", "1");
    }
    assert!(train_log::enabled());
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let default = Scope::Org;
    let acme = Scope::parse("org:acme").unwrap();
    store.ensure_scope(&default).await.unwrap();
    store.ensure_scope(&acme).await.unwrap();

    let body = |what: &str| {
        format!("The {what} export moved to Postgres after the migration review in March.")
    };
    let a = Episode::new("doc:a", body("billing").as_bytes(), &["*"]);
    let b = Episode::new("doc:b", body("invoice").as_bytes(), &["*"]);
    let c = Episode::new("doc:c", body("payroll").as_bytes(), &["user:alice"]);
    let (a_id, b_id, c_id) = (
        a.episode_id.clone(),
        b.episode_id.clone(),
        c.episode_id.clone(),
    );
    store
        .commit(&default, "alice", &[a, b, c], &[])
        .await
        .unwrap();
    let z = Episode::new("doc:z", body("acme").as_bytes(), &["*"]);
    let z_id = z.episode_id.clone();
    store.commit(&acme, "alice", &[z], &[]).await.unwrap();
    extract::run(&store, &default, &ex, 100).await.unwrap();
    extract::run(&store, &acme, &ex, 100).await.unwrap();

    // Every episode's extraction is logged, each under its own org's prefix and no other.
    let all: BTreeSet<String> = [&a_id, &b_id, &c_id].into_iter().cloned().collect();
    assert_eq!(logged(&default).await, all);
    assert_eq!(logged(&acme).await, BTreeSet::from([z_id.clone()]));
    for org in ["default", "acme"] {
        assert!(log.join("v1").join(org).is_dir(), "no prefix for {org}");
    }
    let one = &train_log::calls(&default).await.unwrap()[0];
    assert!(one.record["prompt"].as_str().unwrap().contains("Postgres"));
    assert!(one.record["reply"].as_str().unwrap().contains("rows"));
    assert_eq!(one.record["usable"], true);
    assert!(
        !one.verdicts.is_empty(),
        "the caller's own checks were logged"
    );

    // An export keeps to episodes shared with everyone, and says what it left out; `restricted` adds the rest.
    let file = dir.path().join("examples.jsonl");
    let (ids, report) = exported(&store, &default, false, &file).await;
    assert_eq!(ids, BTreeSet::from([a_id.clone(), b_id.clone()]));
    assert_eq!(report.left_out["read what not every reader may"], 1);
    assert_eq!(
        report.splits.values().sum::<usize>(),
        report.written.values().sum::<usize>()
    );
    let (ids, _) = exported(&store, &default, true, &file).await;
    assert_eq!(ids, all);
    let line: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(line["messages"][0]["role"], "user");
    assert_eq!(line["messages"][1]["role"], "assistant");
    assert!(["train", "dev", "test"].contains(&line["meta"]["split"].as_str().unwrap()));

    // Deleting an org takes its prefix, and leaves the others'.
    train_log::purge_org("acme").await.unwrap();
    assert!(!log.join("v1").join("acme").exists());
    assert_eq!(logged(&default).await, all);

    // A forgotten episode is left out of the export at once, before any sweep ...
    assert!(
        store
            .forget(&default, "alice", &a_id, |_| true)
            .await
            .unwrap()
    );
    let (ids, report) = exported(&store, &default, true, &file).await;
    assert_eq!(ids, BTreeSet::from([b_id.clone(), c_id.clone()]));
    assert_eq!(report.left_out["what it read is gone"], 1);
    // ... and the sweep removes its calls from the log, and only its calls.
    let done = memstore::train_data::sweep(&store, None).await.unwrap();
    assert!(done.purged >= 1, "{done:?}");
    assert_eq!(
        logged(&default).await,
        BTreeSet::from([b_id.clone(), c_id.clone()])
    );

    // An ACL narrowed to someone who wasn't a reader leaves no original reader of the episode: its calls go too.
    store
        .change_acl(
            &default,
            "alice",
            &AclTarget::Episode(c_id.clone()),
            &["user:bob".to_string()],
        )
        .await
        .unwrap();
    memstore::train_data::sweep(&store, None).await.unwrap();
    assert_eq!(logged(&default).await, BTreeSet::from([b_id.clone()]));

    // What the server does when `forget_memory` forgets one: purge its calls on the spot.
    assert!(
        train_log::purge_ref(&default, "episode", &b_id)
            .await
            .unwrap()
            >= 1
    );
    assert!(logged(&default).await.is_empty());
    let done = memstore::train_data::sweep(&store, None).await.unwrap();
    assert_eq!(done.purged, 0, "{done:?}");
}

/// Advice's calls are logged too: sorting, the note, the router and the judge, each under what it read; a note's
/// `outcome` in an export is what the runs served its situation afterwards were rewarded; and a run the memory loses
/// takes with it every call that read it.
#[tokio::test]
async fn advice_calls_are_logged_and_a_notes_outcome_is_joined() {
    use memstore::advice::{Format, Serving};
    if !crate::own_process("train_log::advice_calls_are_logged_and_a_notes_outcome_is_joined") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", dir.path().join("log"));
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        // One recorder's runs: a team note needs no more people than that here.
        std::env::set_var("MEMSTORE_ADVICE_EVIDENCE", "0");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let scope = Scope::Org;
    store.ensure_scope(&scope).await.unwrap();
    let run = |n: usize, reward: Option<i32>, advice: Option<&str>| {
        // The first run's trace is longer than a note's writer sees: learning reflects it first.
        let trace = match n {
            0 => format!("step {n}: queried the table\n").repeat(200),
            _ => format!("step {n}: queried the table"),
        };
        let mut r = serde_json::json!({"task": format!("Rank the top accounts by churn risk for week {n}"),
            "trace": trace, "outcome": "done"});
        if let Some(w) = reward {
            r["reward"] = w.into();
            r["reason"] = "checked against the report".into();
        }
        if let Some(a) = advice {
            r["advice"] = a.into();
        }
        r
    };
    let groups = ["local".to_string()];
    let first: Vec<_> = (0..9)
        .map(|n| run(n, Some(if n % 3 == 0 { 1 } else { -1 }), None))
        .collect();
    // Three people's runs: a team note is learned from no fewer.
    let mut ids = vec![];
    for (i, part) in first.chunks(3).enumerate() {
        let p = memstore::Principal::new(["alice", "bob", "carol"][i], &[]);
        ids.extend(
            memstore::runs::load(&store, &scope, Some(&p), &[], part)
                .await
                .unwrap(),
        );
    }
    memstore::runs::load(&store, &scope, None, &groups, &[run(20, None, None)])
        .await
        .unwrap();

    memstore::advice::learn(&store, &scope, &ex, &["*".to_string()], 3)
        .await
        .unwrap();
    memstore::advice_loop::judge(&store, &scope, &ex, 0, 10)
        .await
        .unwrap();
    let who = memstore::Principal::new("alice", &[]);
    let session = store.pin(&scope, memstore::MAIN).await.unwrap();
    let serving = Serving {
        format: Format::Notes,
        max_chars: None,
        procedures: false,
        skills: Default::default(),
    };
    let got = memstore::advice::get(
        &session,
        &store,
        &who,
        &ex,
        "rank accounts by churn",
        serving,
        None,
    )
    .await
    .unwrap()
    .expect("the router names the situation");
    assert_eq!(got.key, "work/do-it");

    // Each call is logged under its role, with what it read and what its work came to.
    let calls = train_log::calls(&scope).await.unwrap();
    let of = |role: &str| {
        calls
            .iter()
            .filter(|c| c.record["role"] == role)
            .collect::<Vec<_>>()
    };
    let note = of("advice_note");
    assert_eq!(note.len(), 1);
    assert_eq!(note[0].record["refs"]["situation"], "work/do-it");
    assert_eq!(
        note[0].record["refs"]["run_ids"].as_array().unwrap().len(),
        9
    );
    assert_eq!(note[0].verdicts[0]["well_formed"], true);
    assert_eq!(of("advice_sort").len(), 1);
    let reflect = of("advice_reflect");
    assert_eq!(reflect.len(), 1);
    assert_eq!(
        reflect[0].record["refs"]["run_ids"],
        serde_json::json!([ids[0]])
    );
    assert_eq!(of("advice_judge").len(), 1);
    assert_eq!(of("advice_route")[0].verdicts[0]["picked"], "work/do-it");

    // Runs served the note's situation afterwards, and rewarded: what came of the note.
    let after = [
        run(30, Some(1), Some("work/do-it")),
        run(31, Some(1), Some("work/do-it")),
        run(32, Some(-1), Some("work/do-it")),
        run(33, Some(1), Some("other/thing")),
    ];
    memstore::runs::load(&store, &scope, None, &groups, &after)
        .await
        .unwrap();
    let file = dir.path().join("examples.jsonl");
    let (_, report) = exported(&store, &scope, false, &file).await;
    assert!(
        !report.written.contains_key("advice_note"),
        "a note read runs' private traces"
    );
    exported(&store, &scope, true, &file).await;
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let note = lines
        .iter()
        .find(|l| l["meta"]["role"] == "advice_note")
        .expect("the note is exported");
    assert_eq!(note["meta"]["outcome"]["runs_served_after"], 3);
    assert_eq!(note["meta"]["outcome"]["runs_right"], 2);
    assert!((note["meta"]["outcome"]["mean_reward"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);

    // The router's question reads nothing the memory can forget, so its call is kept for a while only: a sweep
    // leaves it for the default 30 days, and removes it where none are kept.
    let done = memstore::train_data::sweep(&store, None).await.unwrap();
    assert_eq!(done.purged, 0, "{done:?}");
    unsafe { std::env::set_var("MEMSTORE_TRAIN_LOG_UNTRACKED_DAYS", "0") };
    let done = memstore::train_data::sweep(&store, None).await.unwrap();
    assert!(done.purged >= 1, "{done:?}");
    let calls = train_log::calls(&scope).await.unwrap();
    assert!(calls.iter().all(|c| c.record["role"] != "advice_route"));
    assert!(calls.iter().any(|c| c.record["role"] == "advice_note"));
    assert!(
        calls.iter().any(|c| c.record["role"] == "advice_reflect"),
        "a reflection read its run: it is kept as long as the run is, no shorter and no longer"
    );

    // A run the memory loses takes every call that read it: the sort, its reflection and the note read it, the
    // judge didn't.
    let gone = &ids[0];
    assert!(train_log::purge_ref(&scope, "run", gone).await.unwrap() >= 3);
    let calls = train_log::calls(&scope).await.unwrap();
    assert!(calls.iter().all(|c| {
        !["advice_note", "advice_sort", "advice_reflect"]
            .contains(&c.record["role"].as_str().unwrap_or(""))
    }));
    assert!(calls.iter().any(|c| c.record["role"] == "advice_judge"));
}

/// A call that showed another episode's fact goes with that episode too, and the same words read in two scopes of one
/// org are two records, each its scope's own.
#[tokio::test]
async fn a_prompt_that_showed_another_episode_goes_with_it() {
    if !crate::own_process("train_log::a_prompt_that_showed_another_episode_goes_with_it") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", dir.path().join("log"));
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_EXTRACT_GOAL_BLIND", "1");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let org = Scope::parse("org:acme").unwrap();
    let team = Scope::parse("team:acme/ops").unwrap();
    store.ensure_scope(&org).await.unwrap();
    store.ensure_scope(&team).await.unwrap();

    // The stand-in states a fact about the text's first long word, so two texts that open alike state one thing two
    // ways, and reading the second asks the model to reconcile it with the first.
    let first = Episode::new(
        "doc:first",
        b"Billing export moved to Postgres after the review.",
        &["*"],
    );
    let second = Episode::new(
        "doc:second",
        b"Billing ledger shifted to Snowflake after the audit.",
        &["*"],
    );
    let (first_id, second_id) = (first.episode_id.clone(), second.episode_id.clone());
    store
        .commit(&org, "alice", std::slice::from_ref(&first), &[])
        .await
        .unwrap();
    extract::run(&store, &org, &ex, 100).await.unwrap();
    store.commit(&org, "alice", &[second], &[]).await.unwrap();
    extract::run(&store, &org, &ex, 100).await.unwrap();

    let calls = train_log::calls(&org).await.unwrap();
    let count = |id: &str, role: &str| {
        calls
            .iter()
            .filter(|c| c.record["role"] == role && c.record["refs"]["episode_id"] == id)
            .count()
    };
    assert_eq!(count(&first_id, "extract"), 1);
    assert_eq!(count(&second_id, "extract"), 1);
    let reconcile: Vec<_> = calls
        .iter()
        .filter(|c| c.record["role"] == "reconcile")
        .collect();
    assert_eq!(
        reconcile.len(),
        1,
        "the second read asked the model to reconcile"
    );
    assert_eq!(
        reconcile[0].record["refs"]["episode_id"],
        second_id.as_str()
    );
    assert_eq!(
        reconcile[0].record["refs"]["also_read"][0],
        first_id.as_str()
    );

    // The same words in another scope of the org: a record of that scope's own.
    store.commit(&team, "alice", &[first], &[]).await.unwrap();
    extract::run(&store, &team, &ex, 100).await.unwrap();
    assert_eq!(logged(&team).await, BTreeSet::from([first_id.clone()]));
    assert_eq!(
        count(&first_id, "extract"),
        1,
        "the org's own record is untouched"
    );

    // Forgetting the first in the org takes its own read and the reconcile that showed its fact; the second's own
    // read stays; and the team's read of the same words is another scope's record, and stays.
    assert!(
        store
            .forget(&org, "alice", &first_id, |_| true)
            .await
            .unwrap()
    );
    // An export before any sweep already leaves out the reconcile that showed the forgotten episode's fact.
    let file = dir.path().join("examples.jsonl");
    let (ids, report) = exported(&store, &org, true, &file).await;
    assert_eq!(ids, BTreeSet::from([second_id.clone()]));
    assert!(!report.written.contains_key("reconcile"), "{report:?}");
    memstore::train_data::sweep(&store, None).await.unwrap();
    let calls = train_log::calls(&org).await.unwrap();
    assert_eq!(calls.len(), 1, "only the second's own read is left");
    assert_eq!(calls[0].record["role"], "extract");
    assert_eq!(calls[0].record["refs"]["episode_id"], second_id.as_str());
    assert_eq!(logged(&team).await, BTreeSet::from([first_id]));
}

/// An ACL narrowed since a call was logged closes the export to it: what is open to every reader is judged by the
/// memory as it is now, not by the ACL the call was made under.
#[tokio::test]
async fn an_acl_narrowed_since_closes_the_export() {
    if !crate::own_process("train_log::an_acl_narrowed_since_closes_the_export") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", dir.path().join("log"));
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_EXTRACT_GOAL_BLIND", "1");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let scope = Scope::Org;
    store.ensure_scope(&scope).await.unwrap();
    let ep = Episode::new(
        "doc:shared",
        b"Payroll export moved to Postgres after the review.",
        &["*", "user:alice"],
    );
    let id = ep.episode_id.clone();
    store.commit(&scope, "alice", &[ep], &[]).await.unwrap();
    extract::run(&store, &scope, &ex, 100).await.unwrap();

    let file = dir.path().join("examples.jsonl");
    let (ids, _) = exported(&store, &scope, false, &file).await;
    assert_eq!(
        ids,
        BTreeSet::from([id.clone()]),
        "shared with everyone when it was read"
    );
    store
        .change_acl(
            &scope,
            "alice",
            &AclTarget::Episode(id.clone()),
            &["user:alice".to_string()],
        )
        .await
        .unwrap();
    let (ids, report) = exported(&store, &scope, false, &file).await;
    assert!(ids.is_empty(), "alice's alone now: {report:?}");
    assert_eq!(report.left_out["read what not every reader may"], 1);
    let (ids, _) = exported(&store, &scope, true, &file).await;
    assert_eq!(
        ids,
        BTreeSet::from([id]),
        "still alice's, for a model only she will use"
    );
}

/// A server logs only the orgs whose settings say `train_log`, and only the operator says it: an org admin is refused,
/// and the org beside the one that was turned on has nothing logged.
#[tokio::test]
async fn only_an_org_the_operator_turned_on_is_logged() {
    use memstore::people::directory::{self, Actor, Change, Directory, Settings};
    if !crate::own_process("train_log::only_an_org_the_operator_turned_on_is_logged") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", dir.path().join("log"));
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_EXTRACT_GOAL_BLIND", "1");
    }
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    let operator = Actor {
        user: "ops".into(),
        superuser: true,
        ..Actor::default()
    };
    for org in ["acme", "other"] {
        let create = Change::CreateOrg {
            org: org.into(),
            admin: Some("alice".into()),
            auth0_org: None,
        };
        directory::apply(&store, &operator, &create).await.unwrap();
    }
    let on = Change::SetSettings {
        org: "acme".into(),
        team: None,
        settings: Settings {
            train_log: Some(true),
            ..Settings::default()
        },
    };
    // The org's own admin is refused; the operator is not.
    let admin = Actor {
        user: "alice".into(),
        ..Actor::default()
    };
    let refused = directory::apply(&store, &admin, &on).await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("training log"),
        "{refused:#}"
    );
    assert!(
        !Directory::load(&store)
            .await
            .unwrap()
            .trains(&Scope::parse("org:acme").unwrap())
    );
    directory::apply(&store, &operator, &on).await.unwrap();
    train_log::follow(Directory::load(&store).await.unwrap());

    for org in ["acme", "other"] {
        let scope = Scope::parse(&format!("org:{org}")).unwrap();
        store.ensure_scope(&scope).await.unwrap();
        let ep = Episode::new(
            "doc:a",
            b"Billing export moved to Postgres after the review.",
            &["*"],
        );
        store.commit(&scope, "alice", &[ep], &[]).await.unwrap();
        extract::run(&store, &scope, &ex, 100).await.unwrap();
    }
    assert_eq!(logged(&Scope::parse("org:acme").unwrap()).await.len(), 1);
    assert!(
        logged(&Scope::parse("org:other").unwrap()).await.is_empty(),
        "no setting, no log"
    );
}

/// A call whose markers can't be written isn't logged: a record that could never be found to be forgotten isn't
/// kept. Extraction goes on as if there were no log.
#[tokio::test]
async fn a_call_that_cannot_be_marked_is_not_logged() {
    if !crate::own_process("train_log::a_call_that_cannot_be_marked_is_not_logged") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let url = model().await;
    unsafe {
        std::env::set_var("MEMSTORE_TRAIN_LOG", &log);
        std::env::set_var("MEMSTORE_MODEL_CACHE", "off");
        std::env::set_var("MEMSTORE_EXTRACT", "openai");
        std::env::set_var("MEMSTORE_EXTRACT_URL", &url);
        std::env::set_var("MEMSTORE_EXTRACT_MODEL", "stand-in");
        std::env::set_var("MEMSTORE_EXTRACT_GOAL_BLIND", "1");
    }
    assert!(train_log::enabled());
    // Where the markers go there is a file, so none can be made.
    std::fs::create_dir_all(log.join("v1").join("default")).unwrap();
    std::fs::write(log.join("v1").join("default").join("_by"), b"in the way").unwrap();
    let store = crate::local_store(&dir.path().join("store")).await;
    let ex = extract::Extractor::from_env().unwrap().unwrap();
    store.ensure_scope(&Scope::Org).await.unwrap();
    let ep = Episode::new(
        "doc:a",
        b"Billing export moved to Postgres after the review.",
        &["*"],
    );
    store
        .commit(&Scope::Org, "alice", &[ep], &[])
        .await
        .unwrap();
    extract::run(&store, &Scope::Org, &ex, 100).await.unwrap();
    assert!(train_log::calls(&Scope::Org).await.unwrap().is_empty());
}
