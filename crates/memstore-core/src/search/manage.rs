//! Looking after a scope's indexes (`memstore index status|rebuild|verify|list|drop`, `index --wait`).
//!
//! Each index (facts, passages) records in its manifest the ledger heads every update covered and how it
//! was built ([`super::Coverage`]). So an index can say what it covers and what it doesn't yet (`status`),
//! be rebuilt from the tables into an empty location and compared with the incremental one (`rebuild`,
//! `verify`), and live beside an index of another configuration until that one is dropped (`list`, `drop`).

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use arrow_array::{Array, RecordBatch, cast::AsArray};
use futures::TryStreamExt;
use lance::Dataset;
use serde_json::{Value, json};

use super::{Coverage, IndexConfig, Part};
use crate::{Scope, Store};

/// Whether every head of `branch` (every branch, when none) is reachable from what `c` covers.
async fn covers(
    store: &Store,
    scope: &Scope,
    c: Option<&Coverage>,
    heads: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, bool>> {
    let reach = match c {
        Some(c) => {
            let had: Vec<String> = c.heads.values().flatten().cloned().collect();
            store.reachable(scope, &had).await?
        }
        None => HashSet::new(),
    };
    Ok(heads
        .iter()
        .map(|(b, hs)| (b.clone(), hs.iter().all(|h| reach.contains(h))))
        .collect())
}

/// `memstore index status`: for the facts and the passage index, the configuration, the newest update (the
/// heads it covers), whether it covers each branch's head now, and the tail it doesn't hold yet.
pub async fn status(store: &Store, scope: &Scope) -> Result<Value> {
    let heads = store.branch_heads(scope).await?;
    let (fdir, pdir) = (
        super::dir_uri(store, scope),
        super::passages::dir_uri(store, scope),
    );
    let (fcov, pcov) = (super::coverage(&fdir).await?, super::coverage(&pdir).await?);

    let held = match super::open_part(store.index_base(), scope, Part::Files).await? {
        Some(f) => super::covered(&f).await?,
        None => HashSet::new(),
    };
    let (_, tail) = store.fact_files(scope, &held).await?;
    let fact_rows: usize = tail
        .iter()
        .map(|(_, b)| b.iter().map(RecordBatch::num_rows).sum::<usize>())
        .sum();

    let pheld = match crate::search::open_ds(&format!("{pdir}/files.lance")).await {
        Ok(f) => super::covered(&f).await?,
        Err(_) => HashSet::new(),
    };
    let episodes = store.episode_files(scope).await?;
    let ptail: Vec<_> = episodes
        .iter()
        .filter(|(k, _)| !pheld.contains(*k))
        .collect();
    let episode_rows: usize = ptail
        .iter()
        .map(|(_, b)| b.iter().map(RecordBatch::num_rows).sum::<usize>())
        .sum();

    let part = |config: IndexConfig,
                dir: &str,
                last: Option<&Coverage>,
                covers: BTreeMap<String, bool>,
                unindexed: Value| {
        json!({
            "location": dir,
            "config": config,
            "updated_at": last.map(|c| c.at),
            "covered_heads": last.map(|c| &c.heads),
            "covers": covers,
            "unindexed": unindexed,
        })
    };
    let fc = covers(store, scope, fcov.last(), &heads).await?;
    let pc = covers(store, scope, pcov.last(), &heads).await?;
    let up = fc.values().chain(pc.values()).all(|b| *b) && fact_rows == 0 && episode_rows == 0;
    Ok(json!({
        "scope": scope.to_string(),
        "heads": heads,
        "up_to_date": up,
        "facts": part(super::config(), &fdir, fcov.last(), fc,
                      json!({"files": tail.len(), "rows": fact_rows})),
        "passages": part(super::passages::config(store), &pdir, pcov.last(), pc,
                         json!({"files": ptail.len(), "episodes": episode_rows})),
    }))
}

/// Whether a [`status`] says both indexes cover `branch`'s head (every branch's, when none).
pub fn covered(status: &Value, branch: Option<&str>) -> bool {
    ["facts", "passages"].iter().all(|k| {
        let c = &status[*k]["covers"];
        match branch {
            Some(b) => c[b].as_bool().unwrap_or(false),
            None => c
                .as_object()
                .is_some_and(|m| m.values().all(|v| v.as_bool() == Some(true))),
        }
    })
}

/// `memstore index --wait`: return the status once both indexes cover the head of `branch` (every
/// branch), as another process (a server) updates them; refused after `timeout`.
pub async fn wait(
    store: &Store,
    scope: &Scope,
    branch: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Value> {
    let start = std::time::Instant::now();
    loop {
        let s = status(store, scope).await?;
        if covered(&s, branch) {
            return Ok(s);
        }
        anyhow::ensure!(
            start.elapsed() < timeout,
            "{scope}'s indexes don't cover {} after {}s: {}",
            branch.map_or("its heads".to_string(), |b| format!("branch {b}'s head")),
            timeout.as_secs(),
            json!({"facts": s["facts"]["covers"], "passages": s["passages"]["covers"]})
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// `memstore index rebuild --to`: build the scope's indexes from its tables into `fresh`'s index location,
/// which must hold none of them yet.
pub async fn rebuild(fresh: &Store, scope: &Scope) -> Result<String> {
    for dir in [
        super::dir_uri(fresh, scope),
        super::passages::dir_uri(fresh, scope),
    ] {
        anyhow::ensure!(
            super::bytes_under(&dir).await? == 0,
            "{dir} isn't empty: a rebuild goes into an empty location"
        );
    }
    let t = std::time::Instant::now();
    let (rows, files) = super::update(fresh, scope).await?;
    let (passages, pfiles, _) = super::passages::update(fresh, scope).await?;
    Ok(format!(
        "rebuilt {scope} into {}: {rows} fact rows from {files} files, {passages} passages from {pfiles} files, {:.1}s",
        fresh.index_base(),
        t.elapsed().as_secs_f64()
    ))
}

/// A row of an index part as `verify` compares it: everything but the vector, and the vector.
type Rows = HashMap<String, Vec<(String, Vec<f32>)>>;

/// Every row of `ds`, by the data file it came from: the `cols` joined, and the vector.
async fn rows_by_file(ds: Option<&Dataset>, tag: &str, cols: &[&str]) -> Result<Rows> {
    let mut out: Rows = HashMap::new();
    let Some(ds) = ds else {
        return Ok(out);
    };
    let mut project = vec!["data_file", "vec"];
    project.extend_from_slice(cols);
    let batches: Vec<RecordBatch> = ds
        .scan()
        .project(&project)?
        .try_into_stream()
        .await?
        .try_collect()
        .await?;
    for b in &batches {
        let file = crate::strings(b, "data_file");
        let v = b.column_by_name("vec").context("vec")?.as_fixed_size_list();
        for (i, file) in file.iter().enumerate() {
            let key: Vec<String> = cols
                .iter()
                .map(|c| cell(b.column_by_name(c).unwrap().as_ref(), i))
                .collect();
            let row = v.value(i);
            let vec = row
                .as_primitive::<arrow_array::types::Float32Type>()
                .values()
                .to_vec();
            out.entry(file.clone())
                .or_default()
                .push((format!("{tag}|{}", key.join("|")), vec));
        }
    }
    Ok(out)
}

/// One value of a column `verify` compares, as text.
fn cell(a: &dyn Array, i: usize) -> String {
    use arrow_array::types::{Float64Type, Int32Type, Int64Type};
    use arrow_schema::DataType;
    if a.is_null(i) {
        return "∅".into();
    }
    match a.data_type() {
        DataType::Utf8 => a.as_string::<i32>().value(i).to_string(),
        DataType::Int64 => a.as_primitive::<Int64Type>().value(i).to_string(),
        DataType::Int32 => a.as_primitive::<Int32Type>().value(i).to_string(),
        DataType::Float64 => a.as_primitive::<Float64Type>().value(i).to_string(),
        t => format!("<{t}>"),
    }
}

/// The differences between the rows of files `inc` holds and a rebuild's rows of the same files.
fn compare(what: &str, inc: &Rows, full: &Rows, held: &HashSet<String>, out: &mut Vec<String>) {
    for f in inc.keys().filter(|f| !held.contains(*f)) {
        out.push(format!(
            "{what}: rows of {f}, which the index doesn't mark as held"
        ));
    }
    for f in held {
        let mut a = inc.get(f).cloned().unwrap_or_default();
        let mut b = full.get(f).cloned().unwrap_or_default();
        a.sort_by(|x, y| x.0.cmp(&y.0));
        b.sort_by(|x, y| x.0.cmp(&y.0));
        let (ka, kb): (Vec<&String>, Vec<&String>) = (
            a.iter().map(|r| &r.0).collect(),
            b.iter().map(|r| &r.0).collect(),
        );
        if ka != kb {
            out.push(format!(
                "{what}: {f} has {} rows incrementally and {} rebuilt, or different ones",
                a.len(),
                b.len()
            ));
            continue;
        }
        let worst = a
            .iter()
            .zip(&b)
            .flat_map(|(x, y)| x.1.iter().zip(&y.1).map(|(p, q)| (p - q).abs()))
            .fold(0f32, f32::max);
        if worst > 1e-4 {
            out.push(format!(
                "{what}: {f}'s vectors differ by up to {worst} from a rebuild's"
            ));
        }
    }
}

/// `memstore index verify`: compare the incremental indexes of `inc` with a rebuild of the same scope in
/// `full`'s location (built first if it isn't there). Every file the incremental index holds must still be
/// in the table and have exactly the rows (and vectors) a rebuild gives it; files it doesn't hold yet are
/// its unindexed tail, reported but no difference. Refused, listing them, on any difference.
pub async fn verify(inc: &Store, full: &Store, scope: &Scope) -> Result<String> {
    if super::bytes_under(&super::dir_uri(full, scope)).await? == 0 {
        rebuild(full, scope).await?;
    }
    let mut diffs = vec![];
    let fact_cols = [
        "fact_id",
        "commit_id",
        "text",
        "valid_from",
        "valid_to",
        "sources",
        "quote",
        "entity_type",
        "value_type",
        "value_num",
        "value_ts",
        "value_ref",
        "unit",
        "cite",
        "time_basis",
    ];
    let (mut held_n, mut tail_n) = (0, 0);
    // Facts: open and closed parts, by file.
    {
        let (a, b) = (inc.index_base(), full.index_base());
        let held = match super::open_part(a, scope, Part::Files).await? {
            Some(f) => super::covered(&f).await?,
            None => HashSet::new(),
        };
        let (current, _) = inc.fact_files(scope, &held).await?;
        for f in held.difference(&current) {
            diffs.push(format!("facts: holds {f}, which the table no longer has"));
        }
        let mut ia: Rows = HashMap::new();
        let mut fa: Rows = HashMap::new();
        for (part, tag) in [(Part::Open, "open"), (Part::Closed, "closed")] {
            for (k, v) in rows_by_file(
                super::open_part(a, scope, part).await?.as_ref(),
                tag,
                &fact_cols,
            )
            .await?
            {
                ia.entry(k).or_default().extend(v);
            }
            for (k, v) in rows_by_file(
                super::open_part(b, scope, part).await?.as_ref(),
                tag,
                &fact_cols,
            )
            .await?
            {
                fa.entry(k).or_default().extend(v);
            }
        }
        let held_now: HashSet<String> = held.intersection(&current).cloned().collect();
        compare("facts", &ia, &fa, &held, &mut diffs);
        held_n += held_now.len();
        tail_n += current.len() - held_now.len();
    }
    // Passages, by file.
    {
        let (da, db) = (
            super::passages::dir_uri(inc, scope),
            super::passages::dir_uri(full, scope),
        );
        let open = |u: String| async move {
            match crate::search::open_ds(&u).await {
                Ok(d) => Ok::<_, anyhow::Error>(Some(d)),
                Err(lance::Error::DatasetNotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        let held = match open(format!("{da}/files.lance")).await? {
            Some(f) => super::covered(&f).await?,
            None => HashSet::new(),
        };
        let current: HashSet<String> = inc.episode_files(scope).await?.into_keys().collect();
        for f in held.difference(&current) {
            diffs.push(format!(
                "passages: holds {f}, which the table no longer has"
            ));
        }
        let cols = ["episode_id", "chunk", "commit_id", "text"];
        let pa = rows_by_file(
            open(format!("{da}/passages.lance")).await?.as_ref(),
            "p",
            &cols,
        )
        .await?;
        let pb = rows_by_file(
            open(format!("{db}/passages.lance")).await?.as_ref(),
            "p",
            &cols,
        )
        .await?;
        compare("passages", &pa, &pb, &held, &mut diffs);
        let held_now = held.intersection(&current).count();
        held_n += held_now;
        tail_n += current.len() - held_now;
    }
    anyhow::ensure!(
        diffs.is_empty(),
        "{scope}'s incremental indexes differ from a rebuild in {}:\n{}",
        full.index_base(),
        diffs.join("\n")
    );
    Ok(format!(
        "ok: {scope}'s indexes equal a rebuild from the tables in {} ({held_n} data files compared, {tail_n} not indexed yet)",
        full.index_base()
    ))
}

/// `memstore index list`: every index directory of the scope in the store's location, with its
/// configuration (from its manifest), size, newest update, and whether this store reads it.
pub async fn list(store: &Store, scope: &Scope) -> Result<Vec<Value>> {
    let root = format!(
        "{}/{}",
        store.index_base().trim_end_matches('/'),
        scope.namespace()
    );
    let current = [super::dir_name(), super::passages::dir_name(store)];
    let mut out = vec![];
    for name in super::subdirs(&root).await? {
        let dir = format!("{root}/{name}");
        let cov = super::coverage(&dir).await?;
        out.push(json!({
            "dir": name,
            "read_here": current.contains(&name),
            "config": cov.last().map(|c| &c.config),
            "updated_at": cov.last().map(|c| c.at),
            "bytes": super::bytes_under(&dir).await?,
        }));
    }
    Ok(out)
}

/// `memstore index drop`: delete one of the scope's index directories. Never one this store reads: that
/// is the index of its configuration (change the configuration first).
pub async fn drop_dir(store: &Store, scope: &Scope, name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && !name.contains('/') && !name.contains(".."),
        "bad index directory {name:?}"
    );
    anyhow::ensure!(
        name != super::dir_name() && name != super::passages::dir_name(store),
        "{name} is the index this configuration reads: change the configuration before dropping it"
    );
    let root = format!(
        "{}/{}",
        store.index_base().trim_end_matches('/'),
        scope.namespace()
    );
    anyhow::ensure!(
        super::subdirs(&root).await?.iter().any(|d| d == name),
        "{scope} has no index {name}"
    );
    super::remove_dir(&format!("{root}/{name}")).await
}
