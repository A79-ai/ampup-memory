//! Reusing a physical row across hybrid lists must save preparation without changing any list's input
//! to ranking. In particular, a fact ID is not a cache key and Lance's vector distance is not a keyword
//! score. Counts guard the repeated-work regression without a machine-dependent latency threshold.

use super::*;
use std::cell::Cell;

fn row(commit: &str, value: &str, vector: Option<Vec<f32>>) -> Arc<FactRow> {
    let mut fact = Fact::new("account:test", "status", value);
    fact.fact_id = "same-fact-id".into();
    Arc::new(FactRow {
        file: "current-file".into(),
        commit: commit.into(),
        fact,
        vec: vector,
    })
}

#[test]
fn repeated_lists_prepare_each_physical_row_once_and_keep_each_lists_distance() {
    let rows = [
        row("visible", "one two", Some(vec![3.0, 4.0])),
        row("hidden", "private", Some(vec![1.0, 1.0])),
        row("visible", "one two three", None),
        row("visible", "", Some(vec![2.0, 2.0])),
    ];
    // Try the denied row before and after an allowed row with the same ID. Neither visibility nor
    // contents may be borrowed from another physical row, even when their IDs are identical.
    for reverse in [false, true] {
        let mut rows = rows.to_vec();
        if reverse {
            rows.reverse();
        }
        let calls = Cell::new(0);
        let prepare = |f: &Fact, commit: &str| {
            calls.set(calls.get() + 1);
            (commit == "visible").then(|| f.value.split_whitespace().count())
        };
        let mut cache = PreparedCandidates::default();
        // Vector first, keyword/rescore lists, then a different vector distance and a missing one.
        // A keyword's BM25 score must never be mistaken for a distance (including a missing score).
        for (keyword, score) in [
            (false, Some(2.5)),
            (true, Some(99.0)),
            (true, None),
            (true, Some(101.0)),
            (false, Some(0.5)),
            (false, None),
        ] {
            for row in &rows {
                // Independent uncached reference: fixed 2-D query [1, 1].
                let expected = (row.commit == "visible").then(|| {
                    let distance = if keyword {
                        row.vec
                            .as_ref()
                            .map(|v| (v[0] - 1.0).powi(2) + (v[1] - 1.0).powi(2))
                    } else {
                        score
                    };
                    (row.fact.value.split_whitespace().count(), distance)
                });
                assert_eq!(
                    cache.for_list(row, &[1.0, 1.0], keyword, score, prepare),
                    expected,
                    "keyword={keyword}, score={score:?}, commit={}, value={}",
                    row.commit,
                    row.fact.value
                );
            }
        }
        assert_eq!(calls.get(), rows.len(), "preparation repeated across lists");
    }
}

#[test]
fn preparation_lives_for_one_pass_and_keeps_row_identity_alive() {
    let row = row("visible", "one two", Some(vec![3.0, 4.0]));
    let weak = Arc::downgrade(&row);
    let mut first = PreparedCandidates::default();
    assert_eq!(
        first.for_list(&row, &[1.0, 1.0], true, None, |_, _| Some(2)),
        Some((2, Some(13.0)))
    );
    // A later ranking pass has its own principal/filter/query. A warmed row must still be checked,
    // and its distance and term matches recomputed for that query.
    let mut second = PreparedCandidates::default();
    assert_eq!(
        second.for_list(&row, &[3.0, 4.0], true, None, |_, _| Some(1)),
        Some((1, Some(0.0)))
    );
    assert_eq!(
        PreparedCandidates::default().for_list(&row, &[3.0, 4.0], true, None, |_, _| None),
        None
    );
    drop(row);
    drop(second);
    assert!(weak.upgrade().is_some(), "a cached address could be reused");
    drop(first);
    assert!(
        weak.upgrade().is_none(),
        "the pass retained its rows after ending"
    );
}
