//! An index update after a scope's fact files were all rewritten clears every indexed row and writes them again
//! with the vectors they had (`search::Reuse`), and the index still holds every fact with its vector.

use arrow_array::cast::AsArray;
use arrow_array::{BooleanArray, RecordBatch};
use memstore::{Episode, Fact, Principal, Scope};

#[tokio::test]
async fn a_rewritten_scope_is_indexed_again_with_its_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::local_store(dir.path()).await;
    let scope = Scope::Team("t".into());
    store.ensure_scope(&scope).await.unwrap();
    for v in ["shipped", "blocked", "renewed"] {
        let ep = Episode::new("note:x", v.as_bytes(), &["group:all"]);
        let f = Fact::new(&format!("deal:{v}"), "status", v).from_episodes(&[&ep]);
        store.commit(&scope, "test", &[ep], &[f]).await.unwrap();
    }
    assert_eq!(memstore::search::update(&store, &scope).await.unwrap().0, 3);

    // Every data file of the facts table replaced: the index's rows all belong to files no longer current.
    // (Dropping one fact: a rewrite that removes nothing is skipped.)
    let mut keep = |b: &RecordBatch| {
        let v = b.column_by_name("value").unwrap().as_string::<i32>();
        Ok((0..b.num_rows())
            .map(|i| Some(v.value(i) != "renewed"))
            .collect::<BooleanArray>())
    };
    store
        .rewrite_table(&scope.namespace(), "facts", &mut keep)
        .await
        .unwrap();
    // A store opened since, as a server restart would: the rewrite doesn't reach the old one's cached plan.
    drop(store);
    let store = crate::local_store(dir.path()).await;

    assert_eq!(memstore::search::update(&store, &scope).await.unwrap().0, 2);
    let who = Principal::new("alice", &["all"]);
    let hits = memstore::search::search(&store, &scope, memstore::MAIN, &who, "blocked", 5, None)
        .await
        .unwrap();
    let hit = hits
        .iter()
        .find(|h| h.fact.value == "blocked")
        .expect("the fact is indexed again");
    assert!(hit.distance.is_some(), "it has its vector");
}
