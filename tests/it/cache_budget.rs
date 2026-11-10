//! The cache budget (`MEMSTORE_CACHE_BYTES`): past it, the least recently read scope is dropped from memory as a
//! whole, and reads of it still answer, loading it again. Two scopes on a local catalog, a budget of one byte:
//! reading the second drops the first, reading the first again gives the same facts, and a write to the dropped
//! scope is seen by the next read.

use memstore::search::{Col, Filter, Lit};
use memstore::{Episode, Fact, Principal, Scope, Store};

async fn values(store: &Store, scope: &Scope) -> Vec<String> {
    let session = store.pin(scope, memstore::MAIN).await.unwrap();
    let who = Principal::new("alice", &["all"]);
    let filter = Filter::Eq(Col::Predicate, Lit::Str("status".into()));
    let mut v: Vec<String> = session
        .read_where(store, &who, &filter, None)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.value)
        .collect();
    v.sort();
    v
}

async fn write(store: &Store, scope: &Scope, value: &str) {
    let ep = Episode::new("note:x", value.as_bytes(), &["group:all"]);
    let f = Fact::new("topic:x", "status", value).from_episodes(&[&ep]);
    store.commit(scope, "test", &[ep], &[f]).await.unwrap();
}

#[tokio::test]
async fn the_least_recently_read_scope_is_dropped_and_loaded_again() {
    if !crate::own_process(
        "cache_budget::the_least_recently_read_scope_is_dropped_and_loaded_again",
    ) {
        return;
    }
    let (a, b) = (Scope::Team("a".into()), Scope::Team("b".into()));
    // The same two scopes, each in a store of its own.
    let seeded = async |dir: &std::path::Path| {
        let store = crate::local_store(dir).await;
        for (s, v) in [(&a, "open"), (&b, "shipped")] {
            store.ensure_scope(s).await.unwrap();
            write(&store, s, v).await;
        }
        store
    };
    // No limit: a store that reads both scopes holds both.
    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe { std::env::set_var("MEMSTORE_CACHE_BYTES", "0") };
    let dir0 = tempfile::tempdir().unwrap();
    let unbounded = seeded(dir0.path()).await;
    assert_eq!(values(&unbounded, &a).await, ["open"]);
    assert_eq!(values(&unbounded, &b).await, ["shipped"]);
    let both = memstore::metrics::CACHE_BYTES.get();
    assert!(both > 0, "reading the scopes fills the cache");

    // A budget of one byte: a store that reads a, then b, keeps only b.
    unsafe { std::env::set_var("MEMSTORE_CACHE_BYTES", "1") };
    let dir = tempfile::tempdir().unwrap();
    let store = seeded(dir.path()).await;
    assert_eq!(values(&store, &a).await, ["open"]);
    assert_eq!(values(&store, &b).await, ["shipped"]);
    let after_b = memstore::metrics::CACHE_BYTES.get();
    assert!(
        after_b > 0 && after_b < both * 3 / 4,
        "a was not dropped: {after_b} bytes cached after reading b, {both} with both scopes"
    );

    // Read again after being dropped: the same answer, from a fresh load.
    assert_eq!(values(&store, &a).await, ["open"]);
    // A write to a scope that was dropped is seen by its next read.
    values(&store, &b).await;
    write(&store, &a, "blocked").await;
    assert_eq!(values(&store, &a).await, ["blocked", "open"]);
}
