//! An index update with nothing new to write still drops the old versions of the index's parts (and the
//! files only they used): an index no longer written to otherwise kept every version made before that.

use memstore::{Episode, Fact, Principal, Scope};

fn versions(dir: &std::path::Path, part: &str) -> usize {
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            }
            out.push(p);
        }
    }
    let mut all = vec![];
    walk(dir, &mut all);
    let v = all
        .iter()
        .find(|p| p.ends_with(format!("{part}/_versions")))
        .unwrap_or_else(|| panic!("no {part}/_versions under {}", dir.display()));
    std::fs::read_dir(v).unwrap().count()
}

#[tokio::test]
async fn an_idle_index_update_drops_old_versions() {
    if !crate::own_process("index_old_versions::an_idle_index_update_drops_old_versions") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = crate::local_store(dir.path()).await;
    let scope = Scope::Team("t".into());
    store.ensure_scope(&scope).await.unwrap();
    for v in ["shipped", "blocked", "renewed", "lost"] {
        let ep = Episode::new("note:x", v.as_bytes(), &["group:all"]);
        let f = Fact::new(&format!("deal:{v}"), "status", v).from_episodes(&[&ep]);
        store.commit(&scope, "test", &[ep], &[f]).await.unwrap();
        // Each update a new version of the facts index, kept for the hour readers may still be on it.
        memstore::search::update(&store, &scope).await.unwrap();
    }
    let index = dir.path().join("index");
    let before = versions(&index, "open.lance");
    assert!(before > 4, "{before} versions");

    // SAFETY: this test runs alone in its process (`own_process`).
    unsafe { std::env::set_var("AMPUP_MEMORY_INDEX_KEEP_VERSIONS_SECS", "0") };
    // Nothing new: the update writes nothing, and the versions no reader is on go.
    assert_eq!(
        memstore::search::update(&store, &scope).await.unwrap(),
        (0, 0)
    );
    let after = versions(&index, "open.lance");
    assert!(after < before, "{before} -> {after} versions");

    // The index still answers.
    let who = Principal::new("alice", &["all"]);
    let hits = memstore::search::search(&store, &scope, memstore::MAIN, &who, "blocked", 5, None)
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.fact.value == "blocked"));
}
