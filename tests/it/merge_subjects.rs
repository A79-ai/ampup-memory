//! `merge_subjects`: two ids for one thing become one. What the going id held moves under the survivor with its
//! sources, a ref to it now names the survivor, its name becomes the survivor's alias, and the old rows are gone
//! for every reader, while the survivor's own facts and an unrelated subject's stay as they were. A closure
//! that ended a moved fact still ends it, and withdrawing the extractor that wrote it still takes it.

use memstore::search::{Col, Filter, Lit};
use memstore::{Episode, Fact, Principal, Scope, Store};

async fn facts_of(store: &Store, id: &str) -> Vec<(String, String, Vec<String>, bool)> {
    let session = store.pin(&Scope::Org, memstore::MAIN).await.unwrap();
    let who = Principal::new("alice", &["all"]);
    let filter = Filter::Or(vec![
        Filter::Eq(Col::Entity, Lit::Str(id.into())),
        Filter::Eq(Col::ValueRef, Lit::Str(id.into())),
        Filter::Eq(Col::Value, Lit::Str(id.into())),
    ]);
    let mut out: Vec<_> = session
        .read_where(store, &who, &filter, None)
        .await
        .unwrap()
        .into_iter()
        .map(|f| {
            (
                f.entity,
                f.predicate,
                f.source_episodes,
                f.valid_to.is_some(),
            )
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_merged_subject_keeps_every_fact_under_the_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::local_store(dir.path()).await;
    store.ensure_scope(&Scope::Org).await.unwrap();
    let thread = Episode::new("slack:thread:1", b"voice ui thread", &["group:all"]);
    let thread_id = thread.episode_id.clone();
    let standup = Episode::new("meeting:standup:1", b"standup notes", &["group:all"]);
    let from_thread = vec![
        Fact::new("topic:voice_ui", "_name", "Voice UI").from_episodes(&[&thread]),
        Fact::new("topic:voice_ui", "status", "in review").from_episodes(&[&thread]),
    ];
    let closed = from_thread[1].fact_id.clone();
    let facts = vec![
        Fact::new("topic:voice_selector", "_name", "Voice selector").from_episodes(&[&standup]),
        Fact::new("topic:voice_selector", "status", "shipped").from_episodes(&[&standup]),
        Fact::new("meeting:standup:1", "discussed", "topic:voice_ui").from_episodes(&[&standup]),
        Fact::new("topic:other", "status", "open").from_episodes(&[&standup]),
    ];
    store
        .commit(
            &Scope::Org,
            "extract:model@g1/schema:s@v1",
            std::slice::from_ref(&thread),
            &from_thread,
        )
        .await
        .unwrap();
    store
        .commit(&Scope::Org, "test", std::slice::from_ref(&standup), &facts)
        .await
        .unwrap();
    // The standup ended the thread's status.
    store
        .close_facts(
            &Scope::Org,
            memstore::MAIN,
            "test",
            &[(closed.as_str(), from_thread[1].valid_from + 1, &standup)],
        )
        .await
        .unwrap();

    let n = store
        .merge_subjects(
            &Scope::Org,
            "test",
            "topic:voice_ui",
            "topic:voice_selector",
        )
        .await
        .unwrap();
    assert_eq!(n, 3, "name, status and the ref move");

    assert!(
        facts_of(&store, "topic:voice_ui").await.is_empty(),
        "nothing left under the old id"
    );
    let survivor = facts_of(&store, "topic:voice_selector").await;
    let preds: Vec<String> = survivor
        .iter()
        .map(|(e, p, _, _)| format!("{e} {p}"))
        .collect();
    assert_eq!(
        preds,
        [
            "meeting:standup:1 discussed",
            "topic:voice_selector _alias",
            "topic:voice_selector _name",
            "topic:voice_selector status",
            "topic:voice_selector status",
        ]
    );
    // The moved status still cites the thread it came from, and is still ended by the standup.
    assert!(
        survivor.iter().any(|(_, p, eps, ended)| p == "status"
            && eps == std::slice::from_ref(&thread_id)
            && *ended),
        "a moved fact keeps its own source episode and its closure: {survivor:?}"
    );
    assert_eq!(
        facts_of(&store, "topic:other").await.len(),
        1,
        "an unrelated subject is untouched"
    );

    // Idempotent: merging again moves nothing and writes nothing new.
    let again = store
        .merge_subjects(
            &Scope::Org,
            "test",
            "topic:voice_ui",
            "topic:voice_selector",
        )
        .await
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(facts_of(&store, "topic:voice_selector").await.len(), 5);

    // Withdrawing the extractor that wrote the thread's facts takes them wherever they now sit.
    store
        .retract_facts(&Scope::Org, "test", None, Some("extract:model@g1"))
        .await
        .unwrap();
    let left = facts_of(&store, "topic:voice_selector").await;
    assert!(
        left.iter()
            .all(|(_, _, eps, _)| eps != std::slice::from_ref(&thread_id)),
        "the withdrawn extractor's moved facts are gone: {left:?}"
    );
}
