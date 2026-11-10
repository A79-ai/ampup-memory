//! Offboarding: an org deleted with its memory (`memstore org delete`), a person forgotten
//! (`memstore users forget`), and an org's memory exported (`memstore org export`).
//!
//! Deleting means the bytes are gone from the customer's storage, not hidden. The Iceberg crate this
//! engine uses (iceberg-rust 0.10) can append and expire snapshots, but has no row delete, overwrite or
//! rewrite action, and no orphan-file removal. So:
//!
//! - An org's memory is dropped table by table with `purge_table` (its data files, manifests, manifest
//!   lists and metadata), after its data and episode-body directories are deleted, and then everything
//!   left under the table's location is deleted too (orphans of failed writes). Its search index goes
//!   with it.
//! - Forgetting a person rewrites each table that holds their rows ([`Store::rewrite_table`]) in place:
//!   the rows to keep are written to new data files and one compare-and-set commit replaces every data
//!   file of the table with them (rows other writers commit meanwhile are read and carried over before it
//!   lands); then every older snapshot is expired and every file only those referenced is deleted, so
//!   nothing of the removed rows is left in any snapshot or file. No catalog rename is needed (BigLake's
//!   Iceberg REST catalog has none). Their episodes' bodies are deleted, and the scope's search indexes
//!   are deleted (they are rebuilt from the tables).
//!
//! Every step can be run again: a delete or forget that stopped halfway is finished by running it again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{BooleanArray, RecordBatch};
use futures::TryStreamExt;
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::{NamespaceIdent, TableIdent};
use serde_json::{Value, json};

use crate::people::directory::{Actor, Directory, Event, Forgotten, Refused, user_id};
use crate::{
    AGG_REQUESTS, AGGREGATES, CLOSURES, DEFAULT_ORG, DIRECTORY, DIRECTORY_SNAPSHOT, EPISODES,
    EXTRACTIONS, FACTS, FEEDBACK, INSTALLATIONS, KEY_USAGE, LEDGER, Ledger, MAIN, QA_LOG, QUOTES,
    RETENTION, RETRACTIONS, SCHEMAS, SYSTEM_NS, Scope, Store, USAGE_HOURS, body_path,
    optional_strings, read_files, read_planned, strings, timestamps,
};

/// A table renamed aside while it is rewritten is `<name>__old_<id>`: no table name of the engine holds `__`.
pub(crate) const OLD_TAG: &str = "__old_";
/// A table rewritten by an older version's rename-based rewrite carries this property, naming the old table,
/// until all its rows are in...
pub(crate) const REWRITE_PROP: &str = "memstore.rewrite-of";
/// ...and this one once they are, so a recovery tells a finished rewrite from a table made by something else.
const REWRITTEN_PROP: &str = "memstore.rewritten-from";
/// A table rewritten in place carries this property, naming the rewrite's sweep file ([`SweepFile`]), until every
/// snapshot it replaced is expired and every file only they referenced is deleted.
const SWEEP_PROP: &str = "memstore.sweep-from";
/// The snapshot a rewrite commits carries this summary property (an id of the run), so a commit whose answer was
/// lost is recognised.
const REWRITE_ID_PROP: &str = "memstore.rewrite-id";
/// A rewrite pass names itself on its table with this property (`<prefix><id>` = when it started, ms) before it
/// writes anything, and its commit takes it off: one left behind is a pass that never landed.
const PENDING_PREFIX: &str = "memstore.rewrite-pending.";
/// A pass named for longer than this never landed and isn't running: a sweep deletes its files.
const PENDING_STALE_MS: i64 = 60 * 60 * 1000;
/// A table rewritten in place carries this property: when its last rewrite committed (ms since the epoch). Set by
/// the commit that swaps the files, so `serve`'s ledger compaction knows how long ago the last one was.
pub const COMPACTED_AT_PROP: &str = "memstore.compacted-at";
/// A scope's episodes table carries this property: when `serve`'s nightly maintenance window last finished with
/// the scope (ms since the epoch), so the next night takes the scopes least recently maintained first.
pub const MAINTAINED_AT_PROP: &str = "memstore.maintained-at";
/// Every table of a scope, in the order `serve`'s nightly window rewrites them: what cites an episode (facts and
/// what hangs off them, summaries, logged answers, feedback) before the episodes themselves, so a pass that stops
/// halfway finds the same episodes again the next night, as an erasure does ([`Store::erase_in`]).
pub const SCOPE_TABLES: [&str; 15] = [
    FACTS,
    QUOTES,
    CLOSURES,
    AGGREGATES,
    AGG_REQUESTS,
    QA_LOG,
    FEEDBACK,
    EXTRACTIONS,
    SCHEMAS,
    RETENTION,
    RETRACTIONS,
    crate::workspace::SNAPSHOTS,
    crate::pins::PINS,
    crate::governance::TABLE,
    EPISODES,
];
/// The system ledger's table name, in `memory_system`.
pub const LEDGER_TABLE: &str = LEDGER;
/// Every table of the system namespace (`memory_system`), the ledger first.
pub const SYSTEM_TABLES: [&str; 7] = [
    LEDGER,
    DIRECTORY,
    DIRECTORY_SNAPSHOT,
    INSTALLATIONS,
    KEY_USAGE,
    crate::audit::AUDIT,
    USAGE_HOURS,
];
/// How often a pass is started again from a fresh read after another rewrite of the table landed first (a forget
/// racing a compaction): each try either lands or gives up with nothing of it committed.
const MEANWHILE_TRIES: u32 = 5;

/// A rewrite pass gave up because another rewrite of the table committed after its read: nothing of the pass was
/// committed and its files are gone (or named for a later sweep), so a pass from a fresh read is safe.
#[derive(Debug)]
struct RewrittenMeanwhile(String);

impl std::fmt::Display for RewrittenMeanwhile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} was rewritten by something else meanwhile (run one rewrite at a time): nothing was changed",
            self.0
        )
    }
}

impl std::error::Error for RewrittenMeanwhile {}

/// Where a rewrite's progress goes: `compact-ledger` prints it, everything else drops it.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// A [`Progress`] that drops every step.
pub fn quiet(_: &str) {}

/// A table (the system ledger, a scope's summaries) as its current snapshot lists it
/// ([`Store::ledger_status`], [`Store::aggregates_status`]): counts only.
#[derive(Clone, Debug, Default)]
pub struct LedgerStatus {
    /// Live data files: what a cold read reads, one per ledger commit since the last compaction.
    pub data_files: u64,
    pub rows: u64,
    /// Bytes of those data files.
    pub bytes: u64,
    pub snapshots: usize,
    /// Manifests the current snapshot lists.
    pub manifests: usize,
    /// Metadata files the table's metadata names: the current one and its log.
    pub metadata_logs: usize,
    /// When the last compaction (or forget) of the ledger committed, if known.
    pub compacted_at_ms: Option<i64>,
    /// A rewrite committed and its sweep didn't finish (killed after its swap).
    pub unswept: bool,
    /// Rewrite passes named on the table that haven't landed: running, or killed before their swap...
    pub pending: usize,
    /// ...of which this many are over an hour old: a sweep deletes their files.
    pub stale_pending: usize,
}

impl LedgerStatus {
    /// Metadata files the table references: manifests, a manifest list per snapshot, and its metadata files.
    pub fn metadata_files(&self) -> usize {
        self.manifests + self.snapshots + self.metadata_logs
    }

    /// The data files a compaction leaves: the rows go to files of up to 512 MB each.
    pub fn data_files_after(&self) -> u64 {
        if self.rows == 0 {
            return 0;
        }
        self.bytes.div_ceil(512 * 1024 * 1024).max(1)
    }
}

/// How a rewrite's commit went ([`Store::replace_files`]).
enum Replaced {
    Landed,
    /// Refused for sure (409): nothing of that try is referenced. The table as it is now.
    Refused(iceberg::table::Table),
    /// Its answer was lost and a reload doesn't show it: it may still land. The table as it is now.
    Unsure(iceberg::table::Table),
}

/// What a rewrite's commit replaced, written next to the table's metadata before the commit and named by
/// [`SWEEP_PROP`]: the snapshots to expire and their manifest lists (the catalog may prune old metadata files).
///
/// The manifests and data files it replaced are listed too, not only found through the manifest lists: a trim of
/// the table's history ([`crate::expire_history`], on any later commit) may expire the replaced snapshots and delete
/// their manifest lists before a sweep stopped by a kill runs again.
#[derive(serde::Serialize, serde::Deserialize)]
struct SweepFile {
    format: u8,
    snapshots: Vec<i64>,
    lists: Vec<String>,
    #[serde(default)]
    manifests: Vec<String>,
    #[serde(default)]
    data: Vec<String>,
}

/// Where the rewrite pass `id` of `table` writes its data files.
fn rewrite_dir(table: &iceberg::table::Table, id: &str) -> String {
    format!(
        "{}/data/rewrite-{id}",
        table.metadata().location().trim_end_matches('/')
    )
}

/// What a rewrite did.
struct Rewritten {
    removed: usize,
    files_before: usize,
    snapshots_before: usize,
    files_after: usize,
    kept: Digest,
    late: Digest,
    after: Digest,
    /// Rows carried over as they were committed meanwhile that `keep` drops: another pass removes them.
    late_dropped: usize,
    /// Passes started again from a fresh read because another rewrite landed first.
    restarts: u32,
    /// Commit tries of the swap (1: the first landed; each more one lost to a writer's commit).
    swap_tries: u32,
}

/// What [`Store::compact_table`] did: the old table's data files and snapshots, the new table's data files,
/// and the rows read from the old table (`before`), committed to it while the new one was built (`late`),
/// and in the new one (`after`: always `before` plus `late`).
#[derive(Debug)]
pub struct Compacted {
    /// Rows left out (a summaries' compaction drops the runs no reader is served).
    pub removed: usize,
    pub files_before: usize,
    pub snapshots_before: usize,
    pub files_after: usize,
    pub before: Digest,
    pub late: Digest,
    pub after: Digest,
    /// Passes started again from a fresh read because another rewrite of the table landed first.
    pub restarts: u32,
    /// Commit tries of the swap: each one past the first lost to a writer's commit and caught up.
    pub swap_tries: u32,
}

/// A count of rows and a checksum of their contents that ignores their order and their files: the sum of a
/// hash of each row's values. A row lost, added, duplicated or changed changes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    pub rows: usize,
    pub sum: u128,
}

impl Digest {
    pub fn of<'a>(batches: impl IntoIterator<Item = &'a RecordBatch>) -> Result<Self> {
        use sha2::Digest as _;
        let mut d = Digest::default();
        for b in batches {
            let mut rows = vec![sha2::Sha256::new(); b.num_rows()];
            for (field, col) in b.schema().fields().iter().zip(b.columns()) {
                for (i, h) in rows.iter_mut().enumerate() {
                    h.update(field.name().as_bytes());
                    match cell(col.as_ref(), i)? {
                        None => h.update([0u8]),
                        Some(v) => {
                            h.update([1u8]);
                            h.update((v.len() as u64).to_le_bytes());
                            h.update(&v);
                        }
                    }
                }
            }
            for h in rows {
                let out = h.finalize();
                let mut first = [0u8; 16];
                first.copy_from_slice(&out[..16]);
                d.sum = d.sum.wrapping_add(u128::from_le_bytes(first));
                d.rows += 1;
            }
        }
        Ok(d)
    }

    pub fn plus(self, other: Digest) -> Digest {
        Digest {
            rows: self.rows + other.rows,
            sum: self.sum.wrapping_add(other.sum),
        }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} rows, checksum {:032x}", self.rows, self.sum)
    }
}

/// Row `i` of `col` as bytes; None if null.
fn cell(col: &dyn arrow_array::Array, i: usize) -> Result<Option<Vec<u8>>> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Float64Type, Int32Type, Int64Type, TimestampMicrosecondType,
    };
    use arrow_schema::{DataType, TimeUnit};
    if col.is_null(i) {
        return Ok(None);
    }
    Ok(Some(match col.data_type() {
        DataType::Utf8 => col.as_string::<i32>().value(i).as_bytes().to_vec(),
        DataType::LargeUtf8 => col.as_string::<i64>().value(i).as_bytes().to_vec(),
        DataType::Boolean => vec![col.as_boolean().value(i) as u8],
        DataType::Int32 => col
            .as_primitive::<Int32Type>()
            .value(i)
            .to_le_bytes()
            .to_vec(),
        DataType::Int64 => col
            .as_primitive::<Int64Type>()
            .value(i)
            .to_le_bytes()
            .to_vec(),
        DataType::Float64 => col
            .as_primitive::<Float64Type>()
            .value(i)
            .to_le_bytes()
            .to_vec(),
        DataType::Date32 => col
            .as_primitive::<Date32Type>()
            .value(i)
            .to_le_bytes()
            .to_vec(),
        DataType::Timestamp(TimeUnit::Microsecond, _) => col
            .as_primitive::<TimestampMicrosecondType>()
            .value(i)
            .to_le_bytes()
            .to_vec(),
        other => anyhow::bail!("no checksum for a column of type {other}"),
    }))
}

/// Every row.
fn keep_all(b: &RecordBatch) -> Result<BooleanArray> {
    Ok(BooleanArray::from(vec![true; b.num_rows()]))
}

fn refused(msg: String) -> anyhow::Error {
    Refused(msg).into()
}

/// Whether `actor` may delete, forget or export in `org`: the operator anywhere; an org's admins in their
/// own org, signed in as a person (never a service or file key).
fn may_manage(dir: &Directory, actor: &Actor, org: &str, what: &str) -> Result<()> {
    if actor.superuser {
        return Ok(());
    }
    let me = &actor.user;
    if actor.via.key.is_some() || actor.via.file_key.is_some() {
        return Err(refused(format!("{me} is a service key: it may not {what}")));
    }
    if !dir.is_admin(me, &actor.via, org) {
        return Err(refused(format!(
            "{me} may not {what} in org {org} (only its admins may)"
        )));
    }
    Ok(())
}

impl Store {
    /// The namespaces holding an org's memory, as the catalog lists them: its own, and its teams' (those
    /// of deleted teams too, whose memory stayed behind).
    pub async fn org_namespaces(&self, org: &str) -> Result<Vec<String>> {
        crate::check_name("org", org)?;
        // Another cell's org: refused, never listed as having nothing (a forget or a purge would find none).
        crate::cell::check_org(org)?;
        let (own, teams) = match org {
            DEFAULT_ORG => ("memory_org".to_string(), "memory_team_".to_string()),
            _ => (
                format!("memory_org_{org}"),
                format!("memory_org_{org}__team_"),
            ),
        };
        let mut out: Vec<String> = self
            .catalog()
            .list_namespaces(None)
            .await?
            .into_iter()
            .map(|n| n.to_url_string())
            .filter(|n| *n == own || n.starts_with(&teams))
            .collect();
        out.sort();
        Ok(out)
    }

    /// Delete a namespace and everything in it for good: each table's data and episode bodies, then the
    /// table with purge (manifests, snapshots, metadata), then whatever is left under its location; then
    /// the namespace and its search indexes. Missing pieces are skipped, so it can be run again.
    pub async fn purge_namespace(&self, ns: &str) -> Result<usize> {
        let ident = NamespaceIdent::new(ns.into());
        let mut tables = 0;
        if self.catalog().namespace_exists(&ident).await? {
            for t in self.catalog().list_tables(&ident).await? {
                let table = match self.catalog().load_table(&t).await {
                    Ok(t) => t,
                    Err(_) if !self.catalog().table_exists(&t).await? => continue,
                    Err(e) => return Err(e).with_context(|| format!("load {ns}.{}", t.name())),
                };
                let io = table.file_io().clone();
                let loc = table
                    .metadata()
                    .location()
                    .trim_end_matches('/')
                    .to_string();
                // Data and bodies first: the manifests purge reads to find data files stay until it runs.
                for dir in ["data", "bodies"] {
                    io.delete_prefix(format!("{loc}/{dir}"))
                        .await
                        .with_context(|| format!("delete {loc}/{dir}"))?;
                }
                self.catalog()
                    .purge_table(&t)
                    .await
                    .with_context(|| format!("purge {ns}.{}", t.name()))?;
                io.delete_prefix(&loc)
                    .await
                    .with_context(|| format!("delete {loc}"))?;
                self.forget_cached(ns, t.name());
                tables += 1;
            }
            self.catalog()
                .drop_namespace(&ident)
                .await
                .with_context(|| format!("drop namespace {ns}"))?;
        }
        crate::search::drop_namespace_index(self, ns).await?;
        Ok(tables)
    }

    /// Remove every ledger row of `org`'s scopes from the system ledger (after its memory is purged): the
    /// ledger then holds nothing of it either. Returns how many rows went.
    pub async fn forget_ledger(&self, org: &str) -> Result<usize> {
        let own = Scope::org(org)?.namespace();
        let teams = format!("{own}__team_");
        let mut keep = |b: &arrow_array::RecordBatch| -> Result<arrow_array::BooleanArray> {
            Ok(strings(b, "scope")
                .iter()
                .map(|s| Some(*s != own && !s.starts_with(&teams)))
                .collect())
        };
        self.rewrite_table(SYSTEM_NS, crate::LEDGER, &mut keep)
            .await
    }

    /// Compact the system ledger ([`Store::compact_table`]): every ledger commit appends a data file and a
    /// manifest, and a cold read (each server's first, at boot) reads them all. One commit swaps the files, so
    /// writers and readers never wait: a ledger commit landing meanwhile is carried over (the compaction's
    /// commit is the one retried). None if there is no ledger.
    pub async fn compact_ledger(&self, progress: Progress<'_>) -> Result<Option<Compacted>> {
        self.compact_table(SYSTEM_NS, LEDGER, progress).await
    }

    /// Compact a scope's summaries ([`Store::compact_table`]), as [`Store::compact_ledger`] does the ledger: a
    /// summariser pass commits its runs to `aggregates` as it goes, and every read of a brief plans and loads all
    /// of the table's files (one production scope's had 4,800, and a cold `list_briefs` took 90 s). The older runs no reader
    /// can be served go on the way ([`Store::aggregates_keep`]). None if the scope has no summaries.
    pub async fn compact_aggregates(
        &self,
        scope: &Scope,
        progress: Progress<'_>,
    ) -> Result<Option<Compacted>> {
        let mut keep = self.aggregates_keep(scope).await?;
        self.compact_table_keeping(&scope.namespace(), AGGREGATES, &mut keep, progress)
            .await
    }

    /// Which rows of a scope's summaries a reader may still be served, as a `keep` for [`Store::rewrite`]. A
    /// summary run (its rows share a commit) is read only while it is the newest run of its (goal, subject,
    /// audience) that a reader sees, so a run is kept when it is that for some reader: on any branch now; at any
    /// of the workspace's snapshots; through any live pin (`crate::pins`); or on a page a cursor will still fetch,
    /// which, as the index keeps its versions (`AMPUP_MEMORY_INDEX_KEEP_VERSIONS_SECS`), is honoured for
    /// `AMPUP_MEMORY_AGGREGATES_KEEP_SECS` (an hour): every run recorded since, and the newest before. A row of no commit yet (a pass still writing) is kept.
    /// The rest are older versions no read reaches.
    async fn aggregates_keep(
        &self,
        scope: &Scope,
    ) -> Result<impl FnMut(&RecordBatch) -> Result<BooleanArray> + Send + use<>> {
        // A run's (goal, subject, audience).
        type Run = (String, String, Option<String>);
        let ns = scope.namespace();
        let (ledger, rows) = tokio::try_join!(
            self.rows(SYSTEM_NS, LEDGER),
            self.side_rows(&ns, AGGREGATES)
        )?;
        let ledger = Ledger::from_batches(&ledger.rows, &ns);
        let mut views: Vec<HashSet<String>> = ledger
            .branches()
            .into_iter()
            .map(|b| ledger.visible(b))
            .collect();
        // A live pin (`crate::pins`), as a snapshot: its runs are kept while it is.
        for pin in self.live_pins(scope, crate::now_micros()).await? {
            views.push(ledger.reachable(&pin.heads));
        }
        if let Some(ws) = scope.org_name() {
            for snap in self.snapshots(ws).await? {
                for heads in snap
                    .heads
                    .get(&scope.to_string())
                    .into_iter()
                    .flat_map(|b| b.values())
                {
                    views.push(ledger.reachable(heads));
                }
            }
        }
        let secs = std::env::var("AMPUP_MEMORY_AGGREGATES_KEEP_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3_600i64);
        let cutoff = crate::now_micros() - secs * 1_000_000;
        let committed: HashSet<String> = views.iter().flatten().cloned().collect();
        let mut kept: HashSet<(String, Run)> = HashSet::new();
        for view in &views {
            // Per (goal, subject, audience): the newest run, and the newest before the cutoff.
            let mut newest: HashMap<Run, (i64, String)> = HashMap::new();
            let mut before: HashMap<Run, (i64, String)> = HashMap::new();
            for b in rows.iter() {
                let (commit, goal, subject) = (
                    strings(b, "commit_id"),
                    strings(b, "goal"),
                    strings(b, "subject"),
                );
                let (audience, at) = (
                    optional_strings(b, "audience"),
                    timestamps(b, "recorded_at"),
                );
                for i in 0..b.num_rows() {
                    if !view.contains(&commit[i]) {
                        continue;
                    }
                    let run = (goal[i].clone(), subject[i].clone(), audience[i].clone());
                    let this = (at[i].unwrap_or_default(), commit[i].clone());
                    let newer = |m: &mut HashMap<Run, (i64, String)>| {
                        let e = m.entry(run.clone()).or_insert_with(|| this.clone());
                        if this > *e {
                            *e = this.clone();
                        }
                    };
                    newer(&mut newest);
                    if this.0 < cutoff {
                        newer(&mut before);
                    }
                }
            }
            for (run, (_, c)) in newest.into_iter().chain(before) {
                kept.insert((c, run));
            }
        }
        Ok(move |b: &RecordBatch| {
            let (commit, goal, subject) = (
                strings(b, "commit_id"),
                strings(b, "goal"),
                strings(b, "subject"),
            );
            let (audience, at) = (
                optional_strings(b, "audience"),
                timestamps(b, "recorded_at"),
            );
            Ok((0..b.num_rows())
                .map(|i| {
                    at[i].unwrap_or_default() >= cutoff
                        || !committed.contains(&commit[i])
                        || kept.contains(&(
                            commit[i].clone(),
                            (goal[i].clone(), subject[i].clone(), audience[i].clone()),
                        ))
                })
                .map(Some)
                .collect())
        })
    }

    /// The system ledger's files as its current snapshot lists them ([`Store::table_status`]). None if there is
    /// no ledger.
    pub async fn ledger_status(&self) -> Result<Option<LedgerStatus>> {
        self.table_status(SYSTEM_NS, LEDGER).await
    }

    /// A scope's summaries' files as their current snapshot lists them ([`Store::table_status`]). None if the
    /// scope has no summaries.
    pub async fn aggregates_status(&self, scope: &Scope) -> Result<Option<LedgerStatus>> {
        self.table_status(&scope.namespace(), AGGREGATES).await
    }

    /// [`Store::sweep_ledger`] for any table of `scope` (a system table for None). Returns how many files were
    /// deleted; 0 if there is no such table.
    pub async fn sweep_table(&self, scope: Option<&Scope>, name: &str) -> Result<usize> {
        let ns = scope.map_or(SYSTEM_NS.to_string(), Scope::namespace);
        let nsi = NamespaceIdent::new(ns.clone());
        let ident = TableIdent::new(nsi.clone(), name.into());
        if !self.catalog().namespace_exists(&nsi).await?
            || !self.catalog().table_exists(&ident).await?
        {
            return Ok(0);
        }
        let n = self.sweep_rewrite(&ident).await?;
        self.forget_cached(&ns, name);
        Ok(n)
    }

    /// A table of `scope`'s (a system table for None) files as its current snapshot lists them
    /// ([`Store::table_status`]). None if there is no such table.
    pub async fn status_in(
        &self,
        scope: Option<&Scope>,
        name: &str,
    ) -> Result<Option<LedgerStatus>> {
        let ns = scope.map_or(SYSTEM_NS.to_string(), Scope::namespace);
        self.table_status(&ns, name).await
    }

    /// The tables of `scope` (the system's for None) by name, in one listing; none if its namespace doesn't exist.
    /// With it, [`Store::status_listed`] and [`Store::trim_listed`] don't ask the catalog whether each exists.
    pub async fn tables_in(&self, scope: Option<&Scope>) -> Result<HashSet<String>> {
        let nsi = NamespaceIdent::new(scope.map_or(SYSTEM_NS.to_string(), Scope::namespace));
        if !self.catalog().namespace_exists(&nsi).await? {
            return Ok(HashSet::new());
        }
        Ok(self
            .catalog()
            .list_tables(&nsi)
            .await?
            .into_iter()
            .map(|t| t.name().to_string())
            .collect())
    }

    /// [`Store::status_in`] of a table [`Store::tables_in`] listed: one load of its metadata and a read of its
    /// manifest list, nothing asked of the catalog besides.
    pub async fn status_listed(&self, scope: Option<&Scope>, name: &str) -> Result<LedgerStatus> {
        let ns = scope.map_or(SYSTEM_NS.to_string(), Scope::namespace);
        let ident = TableIdent::new(NamespaceIdent::new(ns), name.into());
        self.loaded_status(&ident).await
    }

    /// Expire the history of the tables of `scope` (the system's for None) `names` lists, as [`Store::maintain`]
    /// does, past the newest `keep` snapshots, for tables [`Store::tables_in`] listed: one load of each. Returns
    /// (table, snapshots before, after) per table.
    pub async fn trim_listed(
        &self,
        scope: Option<&Scope>,
        names: &[&str],
        keep: usize,
    ) -> Result<Vec<(String, usize, usize)>> {
        let ns = scope.map_or(SYSTEM_NS.to_string(), Scope::namespace);
        let mut out = vec![];
        for name in names {
            let ident = TableIdent::new(NamespaceIdent::new(ns.clone()), (*name).into());
            let table = self.load_fresh(&ident).await?;
            let (before, after) = crate::trim_to(&*self.catalog(), &table, keep).await?;
            self.invalidate(&ns, name);
            out.push((format!("{ns}.{name}"), before, after));
        }
        Ok(out)
    }

    /// `ns.name`'s files as its current snapshot lists them, from its metadata and manifest list alone (no data
    /// file is read): what a compaction would merge, and whether a rewrite of it was interrupted. None if there
    /// is no such table.
    async fn table_status(&self, ns: &str, name: &str) -> Result<Option<LedgerStatus>> {
        let nsi = NamespaceIdent::new(ns.into());
        let ident = TableIdent::new(nsi.clone(), name.into());
        if !self.catalog().namespace_exists(&nsi).await?
            || !self.catalog().table_exists(&ident).await?
        {
            return Ok(None);
        }
        self.loaded_status(&ident).await.map(Some)
    }

    /// [`Store::table_status`] of a table known to exist.
    async fn loaded_status(&self, ident: &TableIdent) -> Result<LedgerStatus> {
        let table = self.load_fresh(ident).await?;
        let meta = table.metadata();
        let props = meta.properties();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut status = LedgerStatus {
            snapshots: meta.snapshots().count(),
            metadata_logs: meta.metadata_log().len() + 1,
            compacted_at_ms: props
                .get(COMPACTED_AT_PROP)
                .and_then(|v| v.parse().ok())
                // A compaction made before the property was set: its snapshot, while it is kept.
                .or_else(|| {
                    meta.snapshots()
                        .filter(|s| {
                            s.summary()
                                .additional_properties
                                .contains_key(REWRITE_ID_PROP)
                        })
                        .map(|s| s.timestamp_ms())
                        .max()
                }),
            unswept: props.contains_key(SWEEP_PROP),
            pending: props
                .keys()
                .filter(|k| k.starts_with(PENDING_PREFIX))
                .count(),
            stale_pending: props
                .iter()
                .filter(|(k, v)| {
                    k.starts_with(PENDING_PREFIX)
                        && v.parse::<i64>()
                            .is_ok_and(|at| now_ms - at > PENDING_STALE_MS)
                })
                .count(),
            ..Default::default()
        };
        let Some(current) = meta.current_snapshot() else {
            return Ok(status);
        };
        let list = crate::manifest_list(&table, current)
            .await?
            .with_context(|| format!("{ident} is encrypted"))?;
        for m in list.entries() {
            status.manifests += 1;
            if m.content == iceberg::spec::ManifestContentType::Data {
                status.data_files += u64::from(m.added_files_count.unwrap_or(0))
                    + u64::from(m.existing_files_count.unwrap_or(0));
                status.rows += m.added_rows_count.unwrap_or(0) + m.existing_rows_count.unwrap_or(0);
            }
        }
        status.bytes = current
            .summary()
            .additional_properties
            .get("total-files-size")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(status)
    }

    /// Finish a rewrite of the system ledger that stopped after its commit (expire what it replaced, delete the
    /// files only that referenced), and delete the files of passes that never landed and are an hour old
    /// ([`Store::sweep_rewrite`]). Returns how many files were deleted; 0 if there is no ledger.
    pub async fn sweep_ledger(&self) -> Result<usize> {
        let ident = TableIdent::new(NamespaceIdent::new(SYSTEM_NS.into()), LEDGER.into());
        if !self.catalog().table_exists(&ident).await? {
            return Ok(0);
        }
        let n = self.sweep_rewrite(&ident).await?;
        self.forget_cached(SYSTEM_NS, LEDGER);
        Ok(n)
    }

    /// Drop everything this store holds in memory about a table.
    fn forget_cached(&self, ns: &str, name: &str) {
        self.invalidate(ns, name);
        self.cache
            .entries
            .lock()
            .unwrap()
            .remove(&format!("{ns}.{name}"));
        self.tail.lock().unwrap().remove(ns);
        self.vis.lock().unwrap().retain(|(n, _), _| n != ns);
    }

    /// Rewrite `ns.name` keeping only the rows `keep` marks: the rest are gone from every snapshot and
    /// file. Returns how many rows were removed (0: the table is left as it was).
    ///
    /// In place, with no rename: the rows to keep are written to new data files, and one commit replaces every
    /// data file of the table with them ([`Store::replace_files`]). The commit is a compare-and-set on the
    /// table's snapshot: if a writer committed since the read, it fails, the rows appended meanwhile are read,
    /// filtered and written too, and the commit is tried again, so nothing a writer committed is lost. Readers see
    /// the old table or the new one, never part of either. Then every snapshot the commit replaced is expired and
    /// every file only they referenced (data files, manifests, manifest lists) is deleted
    /// ([`Store::sweep_rewrite`]): the removed rows are gone from storage, not just from the current snapshot.
    pub async fn rewrite_table(
        &self,
        ns: &str,
        name: &str,
        keep: &mut (dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send),
    ) -> Result<usize> {
        Ok(self
            .rewrite(ns, name, keep, false, &quiet)
            .await?
            .map_or(0, |r| r.removed))
    }

    /// Compact `ns.name`: rewrite it ([`Store::rewrite_table`]) keeping every row, into as few data files as the
    /// writer's target size allows (512 MB each), and expire every older snapshot. The rows are counted and
    /// checksummed as read, as committed meanwhile and as written; nothing is committed unless the files written
    /// hold exactly the first two. None if there is no such table.
    async fn compact_table(
        &self,
        ns: &str,
        name: &str,
        progress: Progress<'_>,
    ) -> Result<Option<Compacted>> {
        self.compact_table_keeping(ns, name, &mut keep_all, progress)
            .await
    }

    /// [`Store::compact_table`], leaving out the rows `keep` drops.
    async fn compact_table_keeping(
        &self,
        ns: &str,
        name: &str,
        keep: &mut (dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send),
        progress: Progress<'_>,
    ) -> Result<Option<Compacted>> {
        let Some(r) = self.rewrite(ns, name, keep, true, progress).await? else {
            return Ok(None);
        };
        Ok(Some(Compacted {
            removed: r.removed,
            files_before: r.files_before,
            snapshots_before: r.snapshots_before,
            files_after: r.files_after,
            before: r.kept,
            late: r.late,
            after: r.after,
            restarts: r.restarts,
            swap_tries: r.swap_tries,
        }))
    }

    async fn load_fresh(&self, ident: &TableIdent) -> Result<iceberg::table::Table> {
        crate::with_retries(|| async { Ok(self.catalog().load_table(ident).await?) })
            .await
            .with_context(|| format!("load {ident}"))
    }

    /// [`Store::rewrite_table`] (`compact`: also when no row is removed). None when there was nothing to do.
    async fn rewrite(
        &self,
        ns: &str,
        name: &str,
        keep: &mut (dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send),
        compact: bool,
        progress: Progress<'_>,
    ) -> Result<Option<Rewritten>> {
        let nsi = NamespaceIdent::new(ns.into());
        let ident = TableIdent::new(nsi.clone(), name.into());
        if !self.catalog().namespace_exists(&nsi).await? {
            return Ok(None);
        }
        // A rename-based rewrite an older version stopped halfway.
        self.recover_rewrite(ns, name).await?;
        if !self.catalog().table_exists(&ident).await? {
            return Ok(None);
        }
        let started = std::time::Instant::now();
        let step = |what: &str| {
            progress(&format!(
                "[{:>6.1}s] {ns}.{name} {what}",
                started.elapsed().as_secs_f64()
            ))
        };
        let mut out: Option<Rewritten> = None;
        // Rows committed while a pass ran are carried over as they are; if `keep` drops any of them, another pass.
        for pass in 0u32.. {
            // A pass that lost to another rewrite of the table (a forget and a compaction at once) committed
            // nothing: it starts again from a fresh read, which holds what the other one committed.
            let mut restarts = 0u32;
            let r = loop {
                // A rewrite stopped after its commit, before its old files were deleted.
                self.sweep_rewrite(&ident).await?;
                match self
                    .rewrite_pass(&ident, keep, compact && pass == 0, &step)
                    .await
                {
                    Err(e) if e.is::<RewrittenMeanwhile>() && restarts + 1 < MEANWHILE_TRIES => {
                        restarts += 1;
                        step(&format!(
                            "read: another rewrite landed first, nothing of this pass was committed; reading \
                             again (try {})",
                            restarts + 1
                        ));
                        tokio::time::sleep(std::time::Duration::from_millis(
                            200 + (uuid::Uuid::new_v4().as_u128() % 800) as u64,
                        ))
                        .await;
                    }
                    r => break r?,
                }
            };
            let Some(mut r) = r else {
                break;
            };
            r.restarts += restarts;
            self.forget_cached(ns, name);
            // Tests: a run killed right after the commit, before the replaced files are deleted.
            if std::env::var("AMPUP_MEMORY_STOP_AFTER_REWRITE").is_ok_and(|t| t == name) {
                anyhow::bail!(
                    "stopped after committing the rewrite of {ns}.{name} (AMPUP_MEMORY_STOP_AFTER_REWRITE)"
                );
            }
            step("sweep: expiring the replaced snapshots, deleting the files only they referenced");
            let swept = self.sweep_rewrite(&ident).await?;
            self.forget_cached(ns, name);
            step(&format!("sweep: deleted {swept} files; done"));
            let again = r.late_dropped > 0;
            out = Some(match out {
                None => r,
                Some(o) => Rewritten {
                    removed: o.removed + r.removed,
                    restarts: o.restarts + r.restarts,
                    ..o
                },
            });
            if !again {
                break;
            }
            anyhow::ensure!(
                pass < 20,
                "{ns}.{name}: rows to remove kept arriving while it was rewritten (after {pass} passes)"
            );
        }
        Ok(out)
    }

    /// One pass of [`Store::rewrite`]: read every data file of the current snapshot, write the rows `keep` marks
    /// to new data files, read them back, and commit one snapshot that holds them in place of the files read, plus
    /// the manifests writers appended since the read, as they are ([`Store::replace_files`]). A lost race costs
    /// only a reload and another commit, so a busy writer can't keep it from landing. None when nothing is to be
    /// removed (and `compact` is off). The rows carried over are read after the commit: `late_dropped` counts those
    /// `keep` would drop, for another pass.
    ///
    /// The pass names itself on the table first (`memstore.rewrite-pending.<id>`, taken off by the commit that
    /// lands it) and writes its files under `data/rewrite-<id>/`: if it never lands, its files (copies of rows
    /// that were kept, which a later forget may have to remove) are deleted by it, or, if it can't tell whether
    /// its commit landed or it was killed, by a later sweep ([`Store::sweep_rewrite`]).
    async fn rewrite_pass(
        &self,
        ident: &TableIdent,
        keep: &mut (dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send),
        compact: bool,
        step: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Option<Rewritten>> {
        use iceberg::spec::{ManifestContentType, ManifestStatus};
        let table = self.load_fresh(ident).await?;
        let Some(read_at) = table.metadata().current_snapshot().cloned() else {
            return Ok(None);
        };
        let read: HashSet<String> = match crate::manifest_list(&table, &read_at).await? {
            Some(list) => list
                .entries()
                .iter()
                .map(|m| m.manifest_path.clone())
                .collect(),
            None => anyhow::bail!("{ident} is encrypted: it can't be rewritten"),
        };
        let (files, planned) = read_planned(&table, HashMap::new()).await?;
        let files_before = files.len();
        let snapshots_before = table.metadata().snapshots().count();
        step(&format!(
            "read: {files_before} data files, {snapshots_before} snapshots"
        ));
        let (rows, removed) = filter_files(files.values(), keep)?;
        // The files read (a key is a data file's path, then its delete files', `|`-separated).
        let read_data: Vec<String> = files
            .keys()
            .flat_map(|k| k.split('|').map(str::to_string))
            .collect();
        drop(files);
        if removed == 0 && !compact {
            return Ok(None);
        }
        let kept = Digest::of(&rows)?;
        let rewrite_id = uuid::Uuid::new_v4().simple().to_string();
        let pending = format!("{PENDING_PREFIX}{rewrite_id}");
        let dir = rewrite_dir(&table, &rewrite_id);
        self.update_props(
            ident,
            &[(
                pending.clone(),
                chrono::Utc::now().timestamp_millis().to_string(),
            )],
            &[],
        )
        .await?;
        let written = write_batches(&table, &rows, &dir).await?;
        drop(rows);
        let io = table.file_io().clone();
        // Give up on this pass for sure (nothing of it can land): its files and its name go.
        let abandon = async |why: anyhow::Error| -> anyhow::Error {
            let gone = async {
                io.delete_prefix(format!("{dir}/")).await?;
                self.update_props(ident, &[], std::slice::from_ref(&pending))
                    .await
            };
            match gone.await {
                Ok(()) => why,
                Err(e) => why.context(format!(
                    "(its new files weren't deleted: {e:#}; a later sweep deletes them)"
                )),
            }
        };
        // Read back: nothing is committed unless the new files hold exactly the rows to keep.
        let back =
            Digest::of(&read_data_files(&table, planned.template.as_ref(), &written).await?)?;
        if back != kept {
            return Err(abandon(anyhow::anyhow!(
                "the files written for {ident} hold {back}, not the {kept} to keep: nothing was changed"
            ))
            .await);
        }
        step(&format!(
            "write: {} rows in {} files, read back and checked; swapping",
            kept.rows,
            written.len()
        ));
        if !self.props_of(ident).await?.contains_key(&pending) {
            anyhow::bail!(
                "{ident}: this rewrite was taken for one that died (not refreshed for an hour) and its files may be \
                 gone: nothing was committed"
            );
        }
        let mut now = table;
        // Whether a try's answer was lost (it may land later): then this pass's files are never deleted here.
        let mut unsure = false;
        // Manifests appended since the read, loaded once each (they never change).
        let mut loaded: HashMap<String, iceberg::spec::Manifest> = HashMap::new();
        let mut carried: Vec<iceberg::spec::ManifestFile> = vec![];
        let mut swap_tries = 0u32;
        for attempt in 0u32.. {
            swap_tries = attempt + 1;
            // Another rewrite committed and isn't swept yet: its sweep first (its mark names what it replaced).
            if now.metadata().properties().contains_key(SWEEP_PROP) {
                self.sweep_rewrite_but(ident, Some(&rewrite_id)).await?;
            }
            // Refreshed before each try (a pass not refreshed for an hour is taken for one that died), and checked:
            // if a sweep took it for dead, its files may be gone, so nothing is committed.
            // Only while the name is still there: a pass a sweep took for dead never names itself again.
            let at = chrono::Utc::now().timestamp_millis().to_string();
            let refreshed = match self.refresh_pending(ident, &pending, &at).await {
                Ok(r) => r,
                // Fenced (the writer lease lost) or the catalog failing: nothing of this try was sent.
                Err(e) => {
                    return Err(match unsure {
                        true => e,
                        false => abandon(e).await,
                    });
                }
            };
            now = self.load_fresh(ident).await?;
            if !refreshed || now.metadata().properties().get(&pending) != Some(&at) {
                return Err(anyhow::anyhow!(
                    "{ident}: this rewrite was taken for one that died (not refreshed for an hour) and its files may \
                     be gone: nothing was committed"
                ));
            }
            let current = now
                .metadata()
                .current_snapshot()
                .context("the table lost its snapshots")?;
            let list = crate::manifest_list(&now, current)
                .await?
                .context("the table is encrypted")?;
            let paths: HashSet<&String> = list.entries().iter().map(|m| &m.manifest_path).collect();
            carried = list
                .entries()
                .iter()
                .filter(|m| !read.contains(&m.manifest_path))
                .cloned()
                .collect();
            // Only appends since the read: every manifest read is still there, and none since deletes a file.
            if !read.iter().all(|m| paths.contains(m))
                || carried
                    .iter()
                    .any(|m| m.content != ManifestContentType::Data || m.has_deleted_files())
            {
                // Started again from a fresh read only when nothing of this pass can land (its answer was never
                // lost): a typed error says so.
                return Err(match unsure {
                    true => anyhow::anyhow!(
                        "{ident} was rewritten by something else meanwhile, and whether this rewrite's own commit \
                         landed is unknown: its new files were kept for a later sweep"
                    ),
                    false => abandon(RewrittenMeanwhile(ident.to_string()).into()).await,
                });
            }
            for m in &carried {
                if !loaded.contains_key(&m.manifest_path) {
                    let manifest = m
                        .load_manifest(now.file_io())
                        .await
                        .with_context(|| format!("read {}", m.manifest_path))?;
                    loaded.insert(m.manifest_path.clone(), manifest);
                }
            }
            let late: Vec<&iceberg::spec::DataFile> = carried
                .iter()
                .flat_map(|m| loaded[&m.manifest_path].entries())
                .filter(|e| e.status() != ManifestStatus::Deleted)
                .map(|e| e.data_file())
                .collect();
            let totals = (
                late.iter().map(|f| f.record_count()).sum::<u64>(),
                late.len() as u64,
                late.iter().map(|f| f.file_size_in_bytes()).sum::<u64>(),
            );
            let replaced = (
                read.iter()
                    .chain(carried.iter().map(|m| &m.manifest_path))
                    .cloned()
                    .collect::<Vec<String>>(),
                read_data
                    .iter()
                    .cloned()
                    .chain(late.iter().map(|f| f.file_path().to_string()))
                    .collect::<Vec<String>>(),
            );
            // A server that lost the writer lease sends nothing; unless an earlier try may still land, this pass's
            // files and name go now rather than with a sweep an hour later.
            if let Err(e) = self.check_fence() {
                return Err(match unsure {
                    true => e,
                    false => abandon(e).await,
                });
            }
            let outcome = self
                .replace_files(
                    &now,
                    &carried,
                    totals,
                    &written,
                    removed,
                    &rewrite_id,
                    attempt,
                    replaced,
                )
                .await;
            let fresh = match outcome {
                Ok(Replaced::Landed) => break,
                Ok(Replaced::Refused(fresh)) => fresh,
                Ok(Replaced::Unsure(fresh)) => {
                    unsure = true;
                    fresh
                }
                // Anything else: whether it landed is told by a reload. Not there: it may still land (a request
                // that timed out), so its files stay, for a later sweep.
                Err(e) => match self.load_fresh(ident).await {
                    Ok(t) if has_rewrite(&t, &rewrite_id) => break,
                    _ => {
                        return Err(e.context(
                            "whether the rewrite's commit landed is unknown: its new files were kept for a later \
                             sweep",
                        ));
                    }
                },
            };
            if attempt >= 200 {
                let why = anyhow::anyhow!(
                    "{ident} kept changing while it was rewritten: gave up after {attempt} tries, nothing was \
                     changed"
                );
                return Err(match unsure {
                    true => why,
                    false => abandon(why).await,
                });
            }
            if attempt % 10 == 9 {
                step(&format!(
                    "catch-up: writers committed meanwhile; swap try {}",
                    attempt + 2
                ));
            }
            now = fresh;
        }
        // The rows carried over, as they are: counted, and checked against `keep`.
        let late: Vec<iceberg::spec::DataFile> = carried
            .iter()
            .flat_map(|m| loaded[&m.manifest_path].entries())
            .filter(|e| e.status() != ManifestStatus::Deleted)
            .map(|e| e.data_file().clone())
            .collect();
        let committed = self.load_fresh(ident).await?;
        let template = match planned.template {
            Some(t) => Some(t),
            None => read_planned(&committed, HashMap::new()).await?.1.template,
        };
        let late_rows = read_data_files(&committed, template.as_ref(), &late).await?;
        let late_digest = Digest::of(&late_rows)?;
        let late_dropped = filter_files(std::iter::once(&Arc::new(late_rows)), keep)?.1;
        // A compaction reads the snapshot it committed back whole: exactly the rows read plus those carried over.
        let after = kept.plus(late_digest);
        if compact {
            let ours = committed
                .metadata()
                .snapshots()
                .find(|s| {
                    s.summary().additional_properties.get(REWRITE_ID_PROP) == Some(&rewrite_id)
                })
                .context("the committed snapshot is gone")?;
            let batches: Vec<RecordBatch> = committed
                .scan()
                .snapshot_id(ours.snapshot_id())
                .select_all()
                .build()?
                .to_arrow()
                .await?
                .try_collect()
                .await?;
            let whole = Digest::of(&batches)?;
            if whole != after {
                // Its files stay: the replaced snapshots keep theirs until someone looks.
                self.update_props(ident, &[], &[SWEEP_PROP.to_string()])
                    .await?;
                anyhow::bail!(
                    "the rewritten {ident} holds {whole}, not the {kept} read plus the {late_digest} carried over: \
                     its replaced snapshots and files were kept, so it can be rolled back to them"
                );
            }
        }
        step(&format!(
            "catch-up: {} rows in {} files committed meanwhile, carried over as they are",
            late_digest.rows,
            late.len()
        ));
        step(&format!(
            "swap: committed {} rows plus {} carried over (swap tries: {})",
            kept.rows, late_digest.rows, swap_tries
        ));
        Ok(Some(Rewritten {
            removed,
            files_before,
            snapshots_before,
            files_after: written.len() + late.len(),
            kept,
            late: late_digest,
            after,
            late_dropped,
            restarts: 0,
            swap_tries,
        }))
    }

    /// Commit one snapshot of `table` that holds exactly `files` plus the manifests `carried` (whose data files add
    /// up to `totals`: rows, files, bytes) in place of everything the current one holds, if the table's snapshot is
    /// still `table`'s (a compare-and-set). The same commit takes the pass's pending name off and marks the table
    /// with a sweep file ([`SWEEP_PROP`]) naming every snapshot it replaces, for [`Store::sweep_rewrite`].
    ///
    /// iceberg-rust 0.10 has no overwrite action, and its commit types are private, so this is its fast append run
    /// on a copy of `table` whose current snapshot lists only `carried` (a manifest list written for it): the
    /// snapshot it makes lists `carried` and one new manifest of `files`. The commit's requirement is still the
    /// real snapshot id, which is what makes it a compare-and-set, and its parent and sequence number are the
    /// real ones.
    #[allow(clippy::too_many_arguments)]
    async fn replace_files(
        &self,
        table: &iceberg::table::Table,
        carried: &[iceberg::spec::ManifestFile],
        totals: (u64, u64, u64),
        files: &[iceberg::spec::DataFile],
        removed: usize,
        rewrite_id: &str,
        attempt: u32,
        (manifests, data): (Vec<String>, Vec<String>),
    ) -> Result<Replaced> {
        let io = table.file_io();
        let loc = table
            .metadata()
            .location()
            .trim_end_matches('/')
            .to_string();
        // What this commit replaces, in a file of ours (the catalog may prune its old metadata files).
        let sweep = format!("{loc}/metadata/sweep-{rewrite_id}-{attempt}.json");
        let replaced = SweepFile {
            format: match table.metadata().format_version() {
                iceberg::spec::FormatVersion::V1 => 1,
                _ => 2,
            },
            snapshots: table
                .metadata()
                .snapshots()
                .map(|s| s.snapshot_id())
                .collect(),
            lists: table
                .metadata()
                .snapshots()
                .map(|s| s.manifest_list().to_string())
                .collect(),
            manifests,
            data,
        };
        io.new_output(&sweep)?
            .write(serde_json::to_vec(&replaced)?.into())
            .await
            .with_context(|| format!("write {sweep}"))?;
        let list = format!("{loc}/metadata/{}-replace.avro", uuid::Uuid::new_v4());
        let bare = with_manifests(table, &list, carried, totals).await?;
        let commit_uuid = uuid::Uuid::now_v7();
        let tx = Transaction::new(&bare);
        let tx = tx
            .fast_append()
            .with_check_duplicate(false)
            .set_commit_uuid(commit_uuid)
            .add_data_files(files.to_vec())
            .set_snapshot_properties(HashMap::from([
                (REWRITE_ID_PROP.to_string(), rewrite_id.to_string()),
                ("memstore.rows-removed".to_string(), removed.to_string()),
            ]))
            .apply(tx)?;
        let tx = tx
            .update_table_properties()
            .set(SWEEP_PROP.into(), sweep.clone())
            .set(
                COMPACTED_AT_PROP.into(),
                chrono::Utc::now().timestamp_millis().to_string(),
            )
            .remove(format!("{PENDING_PREFIX}{rewrite_id}"))
            .apply(tx)?;
        let catalog = self.catalog();
        let pinned = crate::PinnedCatalog {
            inner: &*catalog,
            pinned: bare.clone(),
        };
        let result = tx.commit(&pinned).await;
        // Never referenced by any commit (the copy's snapshot isn't sent): it goes whatever happened.
        let _ = io.delete(&list).await;
        let e = match result {
            Ok(_) => return Ok(Replaced::Landed),
            Err(e) => anyhow::Error::from(e),
        };
        let refused = e.chain().any(|c| {
            c.downcast_ref::<iceberg::Error>()
                .is_some_and(|ie| ie.kind() == iceberg::ErrorKind::CatalogCommitConflicts)
        });
        let conflict = crate::is_commit_conflict(&e);
        if !(conflict || crate::is_transient(&e) || crate::commit_state_unknown(&e)) {
            return Err(e).context(format!("replace the files of {}", table.identifier()));
        }
        if !conflict {
            tokio::time::sleep(std::time::Duration::from_millis(
                50 + (uuid::Uuid::new_v4().as_u128() % 100) as u64,
            ))
            .await;
        }
        let now = self.load_fresh(table.identifier()).await?;
        // A commit whose answer was lost: the table has our snapshot.
        if has_rewrite(&now, rewrite_id) {
            return Ok(Replaced::Landed);
        }
        if !refused {
            return Ok(Replaced::Unsure(now));
        }
        // Refused for sure (no retry inside the transaction: the copy turns them off): what this try wrote goes,
        // its manifest too (its column bounds hold the rows' values).
        let _ = io.delete(&sweep).await;
        let _ = io
            .delete(format!("{loc}/metadata/{commit_uuid}-m0.avro"))
            .await;
        Ok(Replaced::Refused(now))
    }

    async fn props_of(&self, ident: &TableIdent) -> Result<HashMap<String, String>> {
        Ok(self
            .load_fresh(ident)
            .await?
            .metadata()
            .properties()
            .clone())
    }

    /// Set and remove properties of `ident` (a commit with no requirement on the snapshot), retried on conflicts.
    async fn update_props(
        &self,
        ident: &TableIdent,
        set: &[(String, String)],
        remove: &[String],
    ) -> Result<()> {
        for attempt in 0u32.. {
            // A fenced store (a server's background work) commits nothing once it lost the writer lease.
            self.check_fence()?;
            let table = self.load_fresh(ident).await?;
            let props = table.metadata().properties();
            let set: Vec<&(String, String)> = set
                .iter()
                .filter(|(k, v)| props.get(k) != Some(v))
                .collect();
            let remove: Vec<String> = remove
                .iter()
                .filter(|k| props.contains_key(*k))
                .cloned()
                .collect();
            if set.is_empty() && remove.is_empty() {
                break;
            }
            let tx = Transaction::new(&table);
            let mut action = tx.update_table_properties();
            for (k, v) in set {
                action = action.set(k.clone(), v.clone());
            }
            for k in remove {
                action = action.remove(k);
            }
            match action.apply(tx)?.commit(&*self.catalog()).await {
                Ok(_) => break,
                Err(e)
                    if attempt < 50
                        && (e.kind() == iceberg::ErrorKind::CatalogCommitConflicts
                            || e.retryable()) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("update the properties of {ident}"));
                }
            }
        }
        Ok(())
    }

    /// Set a pass's pending name `key` to `at`, only if the table still holds it: false (nothing set) once a sweep
    /// took the pass for dead.
    async fn refresh_pending(&self, ident: &TableIdent, key: &str, at: &str) -> Result<bool> {
        for attempt in 0u32.. {
            self.check_fence()?;
            let table = self.load_fresh(ident).await?;
            if !table.metadata().properties().contains_key(key) {
                return Ok(false);
            }
            let tx = Transaction::new(&table);
            match tx
                .update_table_properties()
                .set(key.into(), at.into())
                .apply(tx)?
                .commit(&*self.catalog())
                .await
            {
                Ok(_) => return Ok(true),
                Err(e)
                    if attempt < 50
                        && (e.kind() == iceberg::ErrorKind::CatalogCommitConflicts
                            || e.retryable()) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("refresh the rewrite pass on {ident}"));
                }
            }
        }
        unreachable!()
    }

    /// Remove each property of `pairs` whose value is still the one given; returns the keys removed.
    async fn remove_if_unchanged(
        &self,
        ident: &TableIdent,
        pairs: &[(String, String)],
    ) -> Result<Vec<String>> {
        for attempt in 0u32.. {
            self.check_fence()?;
            let table = self.load_fresh(ident).await?;
            let props = table.metadata().properties();
            let gone: Vec<String> = pairs
                .iter()
                .filter(|(k, v)| props.get(k) == Some(v))
                .map(|(k, _)| k.clone())
                .collect();
            if gone.is_empty() {
                return Ok(gone);
            }
            let tx = Transaction::new(&table);
            let mut action = tx.update_table_properties();
            for k in &gone {
                action = action.remove(k.clone());
            }
            match action.apply(tx)?.commit(&*self.catalog()).await {
                Ok(_) => return Ok(gone),
                Err(e)
                    if attempt < 50
                        && (e.kind() == iceberg::ErrorKind::CatalogCommitConflicts
                            || e.retryable()) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("update the properties of {ident}"));
                }
            }
        }
        unreachable!()
    }

    /// Finish every rewrite of a table of `ns` that stopped after its commit, and delete the files of passes that
    /// never landed ([`Store::sweep_rewrite`]): run before anything else of a forget or a purge, which may find
    /// nothing left to remove in that table and so never rewrite it again.
    pub(crate) async fn sweep_rewrites(&self, ns: &str) -> Result<usize> {
        let nsi = NamespaceIdent::new(ns.into());
        let mut n = 0;
        if !self.catalog().namespace_exists(&nsi).await? {
            return Ok(n);
        }
        for t in self.catalog().list_tables(&nsi).await? {
            let marked = match self.catalog().load_table(&t).await {
                Ok(table) => table
                    .metadata()
                    .properties()
                    .keys()
                    .any(|k| k == SWEEP_PROP || k.starts_with(PENDING_PREFIX)),
                Err(_) if !self.catalog().table_exists(&t).await? => false,
                Err(e) => return Err(e).with_context(|| format!("load {t}")),
            };
            if marked {
                n += self.sweep_rewrite(&t).await?;
                self.forget_cached(ns, t.name());
            }
        }
        Ok(n)
    }

    /// When the nightly maintenance window last finished with `scope` (ms since the epoch), as the scope's
    /// episodes table records it ([`MAINTAINED_AT_PROP`]), so the window takes the least recently maintained
    /// scopes first across restarts. None if never, or if the scope has no episodes table.
    pub async fn maintained_at(&self, scope: &Scope) -> Result<Option<i64>> {
        let nsi = NamespaceIdent::new(scope.namespace());
        let ident = TableIdent::new(nsi.clone(), EPISODES.into());
        if !self.catalog().namespace_exists(&nsi).await?
            || !self.catalog().table_exists(&ident).await?
        {
            return Ok(None);
        }
        Ok(self
            .load_fresh(&ident)
            .await?
            .metadata()
            .properties()
            .get(MAINTAINED_AT_PROP)
            .and_then(|v| v.parse().ok()))
    }

    /// Record that the nightly maintenance window finished with `scope` at `at_ms` ([`Store::maintained_at`]): a
    /// property of its episodes table, no rows. Nothing if it has none.
    pub async fn set_maintained_at(&self, scope: &Scope, at_ms: i64) -> Result<()> {
        let nsi = NamespaceIdent::new(scope.namespace());
        let ident = TableIdent::new(nsi.clone(), EPISODES.into());
        if !self.catalog().namespace_exists(&nsi).await?
            || !self.catalog().table_exists(&ident).await?
        {
            return Ok(());
        }
        self.update_props(
            &ident,
            &[(MAINTAINED_AT_PROP.to_string(), at_ms.to_string())],
            &[],
        )
        .await
    }

    /// Finish a rewrite of `ident` that has committed ([`SWEEP_PROP`] names its sweep file, which lists the
    /// snapshots it replaced): expire them, delete every file they referenced that no snapshot left references (data
    /// files first, then manifests, then manifest lists, so a run stopped halfway is finished by the next one), then
    /// clear the mark. And delete the files of every pass that never landed, named on the table for longer than
    /// [`PENDING_STALE_MS`] (a pass still running is younger). Returns how many files were deleted.
    pub(crate) async fn sweep_rewrite(&self, ident: &TableIdent) -> Result<usize> {
        self.sweep_rewrite_but(ident, None).await
    }

    /// [`Store::sweep_rewrite`], leaving the pass `own` (the caller's, still running) alone.
    async fn sweep_rewrite_but(&self, ident: &TableIdent, own: Option<&str>) -> Result<usize> {
        let table = self.load_fresh(ident).await?;
        let io = table.file_io().clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let stale: Vec<(String, String)> = table
            .metadata()
            .properties()
            .iter()
            .filter(|(k, v)| {
                // A value that doesn't parse is never taken for stale: only a pass's files are at stake.
                k.starts_with(PENDING_PREFIX)
                    && Some(&k[PENDING_PREFIX.len()..]) != own
                    && v.parse::<i64>()
                        .is_ok_and(|at| now_ms - at > PENDING_STALE_MS)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // The name goes first, then the files: a pass checks its name before each commit try (and never names
        // itself again once it is gone), so one that was only slow stops instead of committing files deleted under
        // it. Only a name still as stale as it was read goes, and only its files: one the pass refreshed meanwhile
        // (or took off by landing) is left alone.
        let stale = self.remove_if_unchanged(ident, &stale).await?;
        for k in &stale {
            let id = &k[PENDING_PREFIX.len()..];
            io.delete_prefix(format!("{}/", rewrite_dir(&table, id)))
                .await
                .with_context(|| format!("delete the files of the rewrite {id} of {ident}"))?;
        }
        let Some(sweep) = table.metadata().properties().get(SWEEP_PROP).cloned() else {
            return Ok(0);
        };
        let bytes = io
            .new_input(&sweep)?
            .read()
            .await
            .with_context(|| format!("read {sweep}, what a rewrite of {ident} replaced"))?;
        let old: SweepFile = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            // A mark of the first version of this: the table metadata the rewrite replaced.
            Err(_) => {
                let m = iceberg::spec::TableMetadata::read_from(&io, &sweep)
                    .await
                    .with_context(|| format!("read {sweep}, what a rewrite of {ident} replaced"))?;
                SweepFile {
                    format: match m.format_version() {
                        iceberg::spec::FormatVersion::V1 => 1,
                        _ => 2,
                    },
                    snapshots: m.snapshots().map(|s| s.snapshot_id()).collect(),
                    lists: m
                        .snapshots()
                        .map(|s| s.manifest_list().to_string())
                        .collect(),
                    manifests: vec![],
                    data: vec![],
                }
            }
        };
        let replaced: HashSet<i64> = old.snapshots.iter().copied().collect();
        for attempt in 0u32.. {
            let table = self.load_fresh(ident).await?;
            let ids: Vec<i64> = table
                .metadata()
                .snapshots()
                .map(|s| s.snapshot_id())
                .filter(|id| replaced.contains(id))
                .collect();
            if ids.is_empty() {
                break;
            }
            self.check_fence()?;
            let tx = Transaction::new(&table);
            let result = tx
                .expire_snapshots()
                .expire_snapshot_ids(ids)
                .expire_older_than_ms(0)
                .apply(tx)?
                .commit(&*self.catalog())
                .await;
            match result {
                Ok(_) => break,
                Err(e)
                    if attempt < 50
                        && (e.kind() == iceberg::ErrorKind::CatalogCommitConflicts
                            || e.retryable()) => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("expire the replaced snapshots of {ident}"));
                }
            }
        }
        let table = self.load_fresh(ident).await?;
        let live_lists: Vec<String> = table
            .metadata()
            .snapshots()
            .map(|s| s.manifest_list().to_string())
            .collect();
        let live = referenced(&io, live_lists, table.metadata().format_version(), false).await?;
        let old_fv = match old.format {
            1 => iceberg::spec::FormatVersion::V1,
            _ => iceberg::spec::FormatVersion::V2,
        };
        let mut gone = referenced(&io, old.lists, old_fv, true).await?;
        gone.manifests.extend(old.manifests);
        gone.data.extend(old.data);
        let mut n = 0;
        for (was, is) in [
            (&gone.data, &live.data),
            (&gone.manifests, &live.manifests),
            (&gone.lists, &live.lists),
        ] {
            let paths: Vec<String> = was.difference(is).cloned().collect();
            n += paths.len();
            delete_files(&io, paths.iter().map(String::as_str)).await?;
        }
        let table = self.load_fresh(ident).await?;
        if table.metadata().properties().get(SWEEP_PROP) == Some(&sweep) {
            self.update_props(ident, &[], &[SWEEP_PROP.to_string()])
                .await?;
        }
        let _ = io.delete(&sweep).await;
        Ok(n)
    }

    /// Finish what a rename-based rewrite of `ns.name` (made by an older version) that stopped halfway left: a
    /// complete new table (marked as the
    /// rewrite of the old one) means the old one is only to be purged; an incomplete one (still marked as
    /// under way) is dropped and the old one put back, so the rewrite starts over from it; no table at all
    /// means the old one is put back. A table of that name that is neither (made by something else while the
    /// old one was aside) is refused and both are left as they are: purging the old one could lose every row.
    async fn recover_rewrite(&self, ns: &str, name: &str) -> Result<()> {
        let nsi = NamespaceIdent::new(ns.into());
        let ident = TableIdent::new(nsi.clone(), name.into());
        let prefix = format!("{name}{OLD_TAG}");
        let olds: Vec<TableIdent> = self
            .catalog()
            .list_tables(&nsi)
            .await?
            .into_iter()
            .filter(|t| t.name().starts_with(&prefix))
            .collect();
        for old in olds {
            if self.catalog().table_exists(&ident).await? {
                let current = self.catalog().load_table(&ident).await?;
                let props = current.metadata().properties();
                let names_old = |k: &str| props.get(k).map(String::as_str) == Some(old.name());
                if names_old(REWRITE_PROP) {
                    // Incomplete: its files are its own (the old table's are untouched), so purge it.
                    self.purge_old(ns, &ident).await?;
                    self.catalog().rename_table(&old, &ident).await?;
                } else if names_old(REWRITTEN_PROP) {
                    self.purge_old(ns, &old).await?;
                } else {
                    anyhow::bail!(
                        "{ns}.{} is aside from a rewrite, but {ns}.{name} is not its rewrite: left both as \
                         they are; compare them and drop the one not to keep",
                        old.name()
                    );
                }
            } else {
                self.catalog().rename_table(&old, &ident).await?;
            }
            self.forget_cached(ns, name);
        }
        Ok(())
    }

    /// Whether this catalog can rewrite a table ([`Store::rewrite_table`]) and drop one with purge, tried on a
    /// throwaway table in `memory_system` before forgetting someone or deleting an org changes anything: write two
    /// rows, rewrite the table keeping one (one commit replacing its data file, the old snapshot expired, the old
    /// file deleted), read it back, and drop it with purge. Refused, naming the step the catalog didn't take, if it
    /// can't: otherwise a forget or a delete would stop halfway, after the directory change.
    pub async fn check_rewrites(&self) -> Result<()> {
        let nsi = NamespaceIdent::new(SYSTEM_NS.into());
        let id = &uuid::Uuid::new_v4().simple().to_string()[..12];
        let ident = TableIdent::new(nsi, format!("rewrite_probe_{id}"));
        let result = self.probe_rewrite(&ident).await;
        // Whatever the probe left goes (best effort: a catalog that can't purge gets a plain drop).
        if let Ok(table) = self.catalog().load_table(&ident).await {
            let loc = table
                .metadata()
                .location()
                .trim_end_matches('/')
                .to_string();
            if self.catalog().purge_table(&ident).await.is_err() {
                let _ = self.catalog().drop_table(&ident).await;
            }
            let _ = table.file_io().delete_prefix(&loc).await;
        }
        result.map_err(|(step, e)| {
            refused(format!(
                "this catalog can't {step} ({e:#}): forgetting a person and deleting an org need it, so nothing \
                 was changed"
            ))
        })
    }

    async fn probe_rewrite(
        &self,
        ident: &TableIdent,
    ) -> std::result::Result<(), (&'static str, anyhow::Error)> {
        fn at<E: Into<anyhow::Error>>(
            step: &'static str,
        ) -> impl Fn(E) -> (&'static str, anyhow::Error) {
            move |e| (step, e.into())
        }
        let fields = crate::schema(&[("x", true, iceberg::spec::PrimitiveType::String)])
            .map_err(at("create a table"))?;
        self.ensure_table(SYSTEM_NS, ident.name(), fields)
            .await
            .map_err(at("create a table"))?;
        let table = self
            .catalog()
            .load_table(ident)
            .await
            .map_err(at("create a table"))?;
        let column: arrow_array::ArrayRef =
            Arc::new(arrow_array::StringArray::from(vec!["gone", "kept"]));
        crate::append(
            &*self.catalog(),
            &table,
            vec![("x", column)],
            crate::OnConflict::Recommit,
        )
        .await
        .map_err(at("append to a table"))?;
        let table = self
            .catalog()
            .load_table(ident)
            .await
            .map_err(at("append to a table"))?;
        let before: Vec<String> = read_files(&table, HashMap::new())
            .await
            .map_err(at("read a table"))?
            .into_keys()
            .collect();
        let removed = self
            .rewrite_table(SYSTEM_NS, ident.name(), &mut |b| {
                Ok(strings(b, "x").iter().map(|x| Some(x != "gone")).collect())
            })
            .await
            .map_err(at("rewrite a table in place"))?;
        let table = self
            .catalog()
            .load_table(ident)
            .await
            .map_err(at("rewrite a table in place"))?;
        let rows: Vec<String> = read_files(&table, HashMap::new())
            .await
            .map_err(at("read a rewritten table"))?
            .values()
            .flat_map(|v| v.iter())
            .flat_map(|b| strings(b, "x"))
            .collect();
        if removed != 1 || rows != ["kept"] || table.metadata().snapshots().count() != 1 {
            return Err((
                "rewrite a table in place",
                anyhow::anyhow!(
                    "removed {removed} rows, left {rows:?} in {} snapshots",
                    table.metadata().snapshots().count()
                ),
            ));
        }
        for f in &before {
            if table
                .file_io()
                .exists(f)
                .await
                .map_err(at("delete a rewritten table's old files"))?
            {
                return Err((
                    "delete a rewritten table's old files",
                    anyhow::anyhow!("{f} is still there"),
                ));
            }
        }
        self.catalog()
            .purge_table(ident)
            .await
            .map_err(at("drop a table with purge"))?;
        Ok(())
    }

    /// Purge a table a rewrite leaves behind (the old one renamed aside, or an unfinished new one). It shares
    /// its location with the other, so only the files it references go (`purge_table`), never the location.
    async fn purge_old(&self, ns: &str, old: &TableIdent) -> Result<()> {
        let table = self
            .catalog()
            .load_table(old)
            .await
            .with_context(|| format!("load {ns}.{}", old.name()))?;
        let Err(e) = self.catalog().purge_table(old).await else {
            return Ok(());
        };
        // A purge that stopped halfway (the process was killed) has deleted some of the table's files, and a
        // catalog's purge that reads them all first then fails on every later try: delete what is left here.
        delete_table_files(&table)
            .await
            .with_context(|| format!("purge {ns}.{} (after {e})", old.name()))?;
        if self.catalog().table_exists(old).await? {
            self.catalog()
                .drop_table(old)
                .await
                .with_context(|| format!("drop {ns}.{} once its files were deleted", old.name()))?;
        }
        Ok(())
    }

    /// Delete everything `user` wrote into one namespace (`remember_memory` episodes, whose source names
    /// their author), the facts citing any of those episodes, the backfilled quotes of those facts, the
    /// closures those episodes are evidence for, the episodes' bodies and the namespace's search indexes.
    pub async fn forget_in(&self, ns: &str, user: &str) -> Result<Forgotten> {
        self.sweep_rewrites(ns).await?;
        let ident = TableIdent::new(NamespaceIdent::new(ns.into()), EPISODES.into());
        if !self.catalog().table_exists(&ident).await? {
            return Ok(Forgotten::default());
        }
        // Summaries made for them (their audience names them), and their asking for one.
        // As readers' tokens hold them (`norm_id`), and as given.
        let theirs = [
            format!("user:{}", crate::auth::acl::norm_id(user.trim())),
            format!("user:{}", user.trim()),
        ];
        let names_them =
            |audience: &str| audience_tokens(audience).iter().any(|t| theirs.contains(t));
        for table in [AGG_REQUESTS, AGGREGATES] {
            self.rewrite_table(ns, table, &mut |b| {
                Ok(optional_strings(b, "audience")
                    .iter()
                    .map(|a| Some(!a.as_deref().is_some_and(names_them)))
                    .collect())
            })
            .await?;
        }
        // What they asked the memory is theirs too.
        let asked = self
            .rewrite_table(ns, QA_LOG, &mut |b| {
                Ok(strings(b, "asker")
                    .iter()
                    .map(|a| Some(!crate::people::directory::forgets(a, user)))
                    .collect())
            })
            .await?;
        // So is the feedback they gave.
        let voted = self
            .rewrite_table(ns, FEEDBACK, &mut |b| {
                Ok(strings(b, "giver")
                    .iter()
                    .map(|g| Some(!crate::people::directory::forgets(g, user)))
                    .collect())
            })
            .await?;
        // Ownership they held, the reviews they decided, and the proposals they made are theirs: an owned
        // entity is unowned afterwards (the admin assigns another), and a proposal of theirs goes with the
        // decisions on it.
        self.forget_governance(ns, user).await?;
        // Their own learned advice is theirs too: its source names them (`advice:mine:<id>`).
        // The id as the ACL names them (`norm_id`), compared exactly: ids that differ in case are two people.
        let mine = format!(
            "{}{}",
            crate::PERSONAL_ADVICE_SOURCE,
            crate::auth::acl::norm_id(user.trim())
        );
        let authored = |source: &str| crate::remembered_by(source, user) || source == mine;
        let episodes = self.table(ns, EPISODES).await?;
        let rows = read_files(&episodes, HashMap::new()).await?;
        let (mut theirs, mut others) = (HashSet::new(), HashSet::new());
        for b in rows.values().flat_map(|v| v.iter()) {
            for (id, source) in strings(b, "episode_id")
                .into_iter()
                .zip(strings(b, "source"))
            {
                match authored(&source) {
                    true => theirs.insert(id),
                    false => others.insert(id),
                };
            }
        }
        if theirs.is_empty() {
            return Ok(Forgotten {
                answers: asked,
                feedback: voted,
                ..Default::default()
            });
        }
        // An id another source shares (an episode written before `remember_memory` ids covered their
        // source, whose body someone else wrote too) stays, with its body; only the rows this person wrote go.
        let gone: HashSet<String> = theirs.difference(&others).cloned().collect();
        let authored_rows = |_: &str, source: &str| authored(source);
        let mut out = self
            .erase_in(ns, Erasure::rows(gone, &authored_rows))
            .await?;
        out.episodes = theirs.len();
        out.answers += asked;
        out.feedback += voted;
        Ok(out)
    }

    /// Every governance row of `ns` as (commit id, body), or none when the scope predates the table.
    async fn governance_bodies(&self, ns: &str) -> Result<Vec<(String, String)>> {
        let name = crate::governance::TABLE;
        let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
        if !self.catalog().table_exists(&ident).await? {
            return Ok(vec![]);
        }
        let files = read_files(&self.table(ns, name).await?, HashMap::new()).await?;
        Ok(files
            .values()
            .flat_map(|v| v.iter())
            .flat_map(|b| strings(b, "commit_id").into_iter().zip(strings(b, "body")))
            .collect())
    }

    /// The ids of the review proposals of `ns` citing any of the episodes `gone`.
    async fn proposals_citing(&self, ns: &str, gone: &HashSet<String>) -> Result<HashSet<String>> {
        Ok(self
            .governance_bodies(ns)
            .await?
            .iter()
            .filter_map(|(_, b)| crate::governance::names(b))
            .filter(|n| n.is_proposal && n.evidence.iter().any(|e| gone.contains(e)))
            .filter_map(|n| n.proposal)
            .collect())
    }

    /// Leave out of `ns`'s governance rows what names `user`: their decisions, the proposals they made with every
    /// decision on those, and the ownership they hold. An entity they own now loses its whole ownership history
    /// (every owner and removal row of it), not their row alone: with only theirs gone, the owner before them
    /// would be the latest row again and own it once more. The entity is unowned until an admin assigns another.
    /// A row of theirs on an entity since transferred on is just left out: the later row wins as before.
    /// Who owns what now is read as the engine reads it (`governance_state`): the rows whose commit is visible on
    /// main, in the ledger's order, not the table's own clock.
    async fn forget_governance(&self, ns: &str, user: &str) -> Result<()> {
        let them = |u: &str| crate::people::directory::forgets(u, user);
        let bodies = self.governance_bodies(ns).await?;
        if bodies.is_empty() {
            return Ok(());
        }
        let ledger = read_files(&self.table(SYSTEM_NS, LEDGER).await?, HashMap::new()).await?;
        let ledger: Vec<RecordBatch> = ledger.values().flat_map(|v| v.iter().cloned()).collect();
        let ledger = Ledger::from_batches(&ledger, ns);
        let (on_main, times) = (ledger.visible(MAIN), ledger.times());
        let named: Vec<_> = bodies
            .iter()
            .filter_map(|(commit, b)| crate::governance::names(b).map(|n| (commit, n)))
            .collect();
        let theirs: HashSet<String> = named
            .iter()
            .filter(|(_, n)| n.is_proposal && n.user.as_deref().is_some_and(them))
            .filter_map(|(_, n)| n.proposal.clone())
            .collect();
        // Each entity's latest ownership row on main: the entity is theirs when that row makes them its owner.
        let mut latest: HashMap<&str, (i64, bool)> = HashMap::new();
        for (commit, n) in &named {
            if let (Some(entity), Some(at)) = (n.entity.as_deref(), times.get(commit.as_str()))
                && on_main.contains(commit.as_str())
            {
                let e = latest.entry(entity).or_insert((i64::MIN, false));
                if *at >= e.0 {
                    *e = (*at, n.user.as_deref().is_some_and(them));
                }
            }
        }
        let unowned: HashSet<String> = latest
            .into_iter()
            .filter(|(_, (_, owned))| *owned)
            .map(|(e, _)| e.to_string())
            .collect();
        self.rewrite_table(ns, crate::governance::TABLE, &mut |b| {
            Ok(strings(b, "body")
                .iter()
                .map(|body| {
                    Some(!crate::governance::names(body).is_some_and(|n| {
                        n.user.as_deref().is_some_and(them)
                            || n.proposal.is_some_and(|p| theirs.contains(&p))
                            || n.entity.is_some_and(|e| unowned.contains(&e))
                    }))
                })
                .collect())
        })
        .await?;
        Ok(())
    }

    /// Delete for good the episodes retracted in one namespace (deleted at their source, `POST /retract`)
    /// or past its retention policy: their rows and bodies, the facts citing them, those facts' backfilled
    /// quotes, the closures they are evidence for, and the namespace's search indexes. The retraction rows
    /// stay (they hold only ids), so a late retry of a purged episode is still retracted. A version a newer one
    /// only superseded (`replaces_source`) isn't deleted and stays, with its facts, to come back if written
    /// again. Nothing to purge, nothing rewritten. The nightly window's scheduled purge goes the same way
    /// ([`Store::purgeable_in`], an [`Erasure`]), with a grace and inside its compactions.
    pub async fn purge_retracted_in(&self, ns: &str) -> Result<Forgotten> {
        self.sweep_rewrites(ns).await?;
        let gone = self.purgeable_in(ns, std::time::Duration::ZERO).await?;
        if gone.is_empty() {
            return Ok(Forgotten::default());
        }
        let n = gone.len();
        let mut out = self.erase_in(ns, Erasure::of(gone)).await?;
        out.episodes = n;
        Ok(out)
    }

    /// The episodes of `ns` a purge deletes, those still stored: the ones its sources deleted (a delete, not a
    /// version a newer one superseded) and the ones past its retention policy, each hidden for longer than `grace`.
    /// A deleted one counts once the commit that deleted it is `grace` old; one past retention, once it is past it
    /// `grace` ago both under the policy now and under the policy then (a policy tightened within the grace deletes
    /// nothing it newly hides until it has held that long). With no grace, everything hidden now.
    pub async fn purgeable_in(
        &self,
        ns: &str,
        grace: std::time::Duration,
    ) -> Result<HashSet<String>> {
        let episodes_ident = TableIdent::new(NamespaceIdent::new(ns.into()), EPISODES.into());
        if !self.catalog().table_exists(&episodes_ident).await? {
            return Ok(HashSet::new());
        }
        // Read fresh, not from a cached snapshot: a retraction committed just now is purged too.
        let ledger = read_files(&self.table(SYSTEM_NS, LEDGER).await?, HashMap::new()).await?;
        let ledger: Vec<RecordBatch> = ledger.values().flat_map(|v| v.iter().cloned()).collect();
        let ledger = Ledger::from_batches(&ledger, ns);
        let committed = ledger.visible(MAIN);
        let times = ledger.times();
        // A side table a scope predates holds nothing.
        let read = async |name: &str| -> Result<Vec<RecordBatch>> {
            let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
            if !self.catalog().table_exists(&ident).await? {
                return Ok(vec![]);
            }
            let files = read_files(&self.table(ns, name).await?, HashMap::new()).await?;
            Ok(files.values().flat_map(|v| v.iter().cloned()).collect())
        };
        let (retractions, episodes) = (read(RETRACTIONS).await?, read(EPISODES).await?);
        let now = chrono::Utc::now().timestamp_micros();
        let cutoff = now - grace.as_micros() as i64;
        let on_main = |c: &str| committed.contains(c);
        // What was committed at least `grace` ago (with none, everything committed).
        let settled =
            |c: &str| on_main(c) && (grace.is_zero() || times.get(c).is_some_and(|t| *t <= cutoff));
        // A version only a newer one superseded isn't deleted: it stays, to come back if written again.
        let crate::Retracted {
            hidden: mut gone,
            superseded,
            ..
        } = crate::retracted(&retractions, &episodes, &settled, &|c| {
            times.get(c).copied()
        });
        gone.retain(|id| !superseded.contains_key(id));
        let retention = read(RETENTION).await?;
        let (now_policy, then_policy) = (
            crate::newest_retention(&retention, &on_main)?,
            crate::newest_retention(&retention, &settled)?,
        );
        let expired_then = crate::expired(&episodes, &then_policy, cutoff).0;
        gone.extend(
            crate::expired(&episodes, &now_policy, cutoff)
                .0
                .into_iter()
                .filter(|id| expired_then.contains(id)),
        );
        // A person's profile version that names any of them goes with them (its lines hold their notes' words), as
        // does one an earlier purge should have taken and didn't finish.
        gone.extend(self.profiles_naming(ns, &episodes, &gone).await?);
        // Only those still stored: a purge run again finds the rest of what it left, nothing more.
        Ok(episodes
            .iter()
            .flat_map(|b| strings(b, "episode_id"))
            .filter(|id| gone.contains(id))
            .collect())
    }

    /// The versions of people's profiles (`crate::is_profile_source`) among `episodes` of `ns`, forgotten or not,
    /// that hold what a purge takes: a profile cites its notes by id, and a line holds the words of one. One goes
    /// when its body names an episode `gone` holds or cites one no longer stored at all (an earlier purge took it,
    /// and a write racing that purge put the version back), and when its body is gone. Only the body says what a
    /// version names, and a purge deletes the bodies before the rows (so a run stopped halfway finds the same
    /// episodes again): a version whose body is gone is one an erasure stopped before its rows went, and nothing
    /// else would ever find it.
    async fn profiles_naming(
        &self,
        ns: &str,
        episodes: &[RecordBatch],
        gone: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let (mut profiles, mut stored) = (HashSet::new(), HashSet::new());
        for b in episodes {
            for (id, source) in strings(b, "episode_id")
                .into_iter()
                .zip(strings(b, "source"))
            {
                if crate::is_profile_source(&source) && !gone.contains(&id) {
                    profiles.insert(id.clone());
                }
                stored.insert(id);
            }
        }
        let mut out = HashSet::new();
        if profiles.is_empty() {
            return Ok(out);
        }
        let table = self.table(ns, EPISODES).await?;
        let io = table.file_io();
        for id in profiles {
            let path = body_path(&table, &id);
            if !io.exists(&path).await? {
                out.insert(id);
                continue;
            }
            let body = io.new_input(path)?.read().await?;
            let named = String::from_utf8_lossy(&body)
                .split(|c: char| !c.is_ascii_hexdigit())
                .any(|w| w.len() == 64 && gone.contains(w))
                || profile_cites(&body).iter().any(|n| !stored.contains(n));
            if named {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// Delete the episodes `ids` of `scope` for good, and what was drawn from them (an [`Erasure`], as a forget
    /// does): for a row the engine derived that must not outlive what it was drawn from (a person's profile
    /// version committed just as a note it cites was forgotten or deleted).
    pub async fn erase_episodes(&self, scope: &Scope, ids: HashSet<String>) -> Result<Forgotten> {
        let ns = scope.namespace();
        self.sweep_rewrites(&ns).await?;
        let n = ids.len();
        let mut out = self.erase_in(&ns, Erasure::of(ids)).await?;
        out.episodes = n;
        Ok(out)
    }

    /// The nightly window's scheduled purge of `scope` ([`Store::purgeable_in`] with its grace), as an [`Erasure`]
    /// for the window's rewrites to leave out; None when there is nothing to purge.
    pub async fn purge_plan(
        &self,
        scope: &Scope,
        grace: std::time::Duration,
    ) -> Result<Option<Erasure<'static>>> {
        let gone = self.purgeable_in(&scope.namespace(), grace).await?;
        Ok((!gone.is_empty()).then(|| Erasure::of(gone)))
    }

    /// Delete what `erasure` names from one namespace ([`Erasure`]): a table at a time, each rewritten leaving its
    /// rows out, the facts first and the episodes last, so a run stopped halfway finds the same episodes again.
    /// `episodes` is left as the erasure holds it.
    async fn erase_in(&self, ns: &str, mut erasure: Erasure<'_>) -> Result<Forgotten> {
        for name in ERASED_TABLES {
            self.rewrite_ns(ns, None, name, false, Some(&mut erasure), &quiet)
                .await?;
        }
        Ok(erasure.counts)
    }

    /// One table of `scope` (a system table for None) rewritten in the nightly window: compacted when `compact`
    /// (every row kept, but a scope's summaries' older runs no reader is served, [`Store::compact_aggregates`]),
    /// and leaving out what `erasure` deletes. One rewrite does both. None when there was nothing to do: not
    /// compacted, and nothing of the table's to delete.
    pub async fn rewrite_in(
        &self,
        scope: Option<&Scope>,
        name: &str,
        compact: bool,
        erasure: Option<&mut Erasure<'_>>,
        progress: Progress<'_>,
    ) -> Result<Option<Compacted>> {
        let ns = scope.map_or(SYSTEM_NS.to_string(), Scope::namespace);
        self.rewrite_ns(&ns, scope, name, compact, erasure, progress)
            .await
    }

    /// Delete the bodies of the episodes `ids` of `ns`, those still there.
    async fn delete_bodies(&self, ns: &str, ids: &HashSet<String>) -> Result<()> {
        let episodes = self.table(ns, EPISODES).await?;
        let io = episodes.file_io();
        for id in ids {
            let path = body_path(&episodes, id);
            if io.exists(&path).await? {
                io.delete(&path)
                    .await
                    .with_context(|| format!("delete the body of episode {id}"))?;
            }
        }
        Ok(())
    }

    /// [`Store::rewrite_in`] of `ns.name`; `scope` (`ns`'s) only for a compaction of its summaries.
    async fn rewrite_ns(
        &self,
        ns: &str,
        scope: Option<&Scope>,
        name: &str,
        compact: bool,
        mut erasure: Option<&mut Erasure<'_>>,
        progress: Progress<'_>,
    ) -> Result<Option<Compacted>> {
        let erasing = erasure.is_some();
        if erasing && name == EPISODES {
            self.delete_bodies(ns, &erasure.as_ref().unwrap().gone)
                .await?;
            // The indexes hold fact and passage text: deleted before the episodes go (so an index update
            // between the two can't keep them) and after (in case one ran meanwhile).
            crate::search::drop_namespace_index(self, ns).await?;
        }
        // Tests: a run killed before this table's rewrite (the episodes': after their bodies went, before their rows).
        if erasing && std::env::var("AMPUP_MEMORY_STOP_BEFORE_REWRITE").is_ok_and(|t| t == name) {
            anyhow::bail!(
                "stopped before rewriting {ns}.{name} (AMPUP_MEMORY_STOP_BEFORE_REWRITE)"
            );
        }
        // A logged answer or feedback citing an episode no longer stored at all (logged while an earlier erasure
        // ran) goes too: what is stored is read once the tables before them are rewritten.
        if erasing
            && (name == QA_LOG || name == FEEDBACK)
            && let Some(e) = erasure.as_mut()
            && e.stored.is_none()
        {
            let episodes = self.table(ns, EPISODES).await?;
            e.stored = Some(
                read_files(&episodes, HashMap::new())
                    .await?
                    .values()
                    .flat_map(|v| v.iter())
                    .flat_map(|b| strings(b, "episode_id"))
                    .collect(),
            );
        }
        // The proposals citing what goes: read before the table is rewritten, so a decision's row goes with
        // its proposal's whatever file either is in.
        if erasing
            && name == crate::governance::TABLE
            && let Some(e) = erasure.as_mut()
        {
            e.gone_proposals = self.proposals_citing(ns, &e.gone).await?;
        }
        let mut older_runs = match (compact, scope) {
            (true, Some(s)) if name == AGGREGATES => Some(self.aggregates_keep(s).await?),
            _ => None,
        };
        let mut erased = erasure.as_mut().and_then(|e| e.keep(name));
        if !compact && erased.is_none() {
            return Ok(None);
        }
        let mut keep = |b: &RecordBatch| -> Result<BooleanArray> {
            let mut k = keep_all(b)?;
            if let Some(f) = older_runs.as_mut() {
                k = arrow_arith::boolean::and(&k, &f(b)?)?;
            }
            if let Some(f) = erased.as_mut() {
                k = arrow_arith::boolean::and(&k, &f(b)?)?;
            }
            Ok(k)
        };
        let r = self.rewrite(ns, name, &mut keep, compact, progress).await?;
        drop(erased);
        if erasing && name == EPISODES {
            crate::search::drop_namespace_index(self, ns).await?;
            // And the bodies, where every row of what it deletes went (not one person's rows of an id others share):
            // one written back meanwhile, by a write that read it before it went (an ACL change of a profile
            // version, say), belongs to no row left.
            if let Some(e) = erasure.as_ref().filter(|e| e.rows.is_none()) {
                self.delete_bodies(ns, &e.gone).await?;
            }
        }
        let Some(r) = r else {
            return Ok(None);
        };
        if let Some(e) = erasure {
            let counted = match name {
                FACTS => Some(&mut e.counts.facts),
                QUOTES => Some(&mut e.counts.quotes),
                CLOSURES => Some(&mut e.counts.closures),
                QA_LOG => Some(&mut e.counts.answers),
                _ => None,
            };
            if let Some(n) = counted {
                *n += r.removed;
            }
        }
        Ok(Some(Compacted {
            removed: r.removed,
            files_before: r.files_before,
            snapshots_before: r.snapshots_before,
            files_after: r.files_after,
            before: r.kept,
            late: r.late,
            after: r.after,
            restarts: r.restarts,
            swap_tries: r.swap_tries,
        }))
    }
}

/// Which episode rows go, by id and source.
type RowsGone<'a> = dyn Fn(&str, &str) -> bool + Send + Sync + 'a;

/// The tables an erasure rewrites, in its order: what cites an episode first, the episodes last.
pub const ERASED_TABLES: [&str; 8] = [
    FACTS,
    QUOTES,
    CLOSURES,
    AGGREGATES,
    QA_LOG,
    FEEDBACK,
    crate::governance::TABLE,
    EPISODES,
];

/// What a deletion takes out of one namespace: the episodes `gone` names and everything drawn from them. The facts
/// citing any of them, those facts' backfilled quotes, the closures they are evidence for, the summaries drawing on
/// any of them, the logged answers and the feedback citing any of them (or an episode no longer stored at all), the
/// review proposals citing any of them as evidence (their rows quote the proposed values) with the decisions on
/// them, the episodes' bodies and the namespace's search indexes, and the episode rows themselves (all of a gone
/// episode's,
/// or those `rows` marks by id and source). A forget, a purge on request and the nightly window's scheduled purge
/// each build one; each table's rewrite then leaves its rows out ([`Store::rewrite_in`]), in [`ERASED_TABLES`]'s
/// order. The retraction and extraction rows stay (they hold only ids).
pub struct Erasure<'a> {
    gone: HashSet<String>,
    rows: Option<&'a RowsGone<'a>>,
    /// Facts left out so far: their quotes go too.
    dropped_facts: HashSet<String>,
    /// The episode ids stored when the answer log is rewritten.
    stored: Option<HashSet<String>>,
    /// The review proposals citing a gone episode, read when the governance table is rewritten: they go, with
    /// the decisions on them.
    gone_proposals: HashSet<String>,
    /// What went, as the audit and `org purge` report it (`episodes` as the caller counts them).
    pub counts: Forgotten,
}

impl<'a> Erasure<'a> {
    /// Every row of the episodes `gone` names.
    pub fn of(gone: HashSet<String>) -> Self {
        Erasure {
            gone,
            rows: None,
            dropped_facts: HashSet::new(),
            stored: None,
            gone_proposals: HashSet::new(),
            counts: Forgotten::default(),
        }
    }

    /// Only the rows of the episodes table `rows` marks (by id and source) go; what cites `gone` goes whole.
    fn rows(gone: HashSet<String>, rows: &'a RowsGone<'a>) -> Self {
        Erasure {
            rows: Some(rows),
            ..Erasure::of(gone)
        }
    }

    /// How many episodes it deletes.
    pub fn episodes(&self) -> usize {
        self.gone.len()
    }

    /// Which rows of `table` stay, as a `keep` for [`Store::rewrite`]; None if it takes nothing from the table.
    #[allow(clippy::type_complexity)]
    fn keep(
        &mut self,
        table: &str,
    ) -> Option<Box<dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send + '_>> {
        let gone = &self.gone;
        let cites = move |eps: &str, stored: Option<&HashSet<String>>| {
            let mut of = eps.split(',').map(str::trim).filter(|e| !e.is_empty());
            of.any(|e| gone.contains(e) || stored.is_some_and(|s| !s.contains(e)))
        };
        match table {
            FACTS => {
                let dropped = &mut self.dropped_facts;
                Some(Box::new(move |b| {
                    let ids = strings(b, "fact_id");
                    Ok(strings(b, "source_episodes")
                        .iter()
                        .zip(ids)
                        .map(|(src, id)| {
                            let cited = src.split(',').any(|e| gone.contains(e.trim()));
                            if cited {
                                dropped.insert(id);
                            }
                            Some(!cited)
                        })
                        .collect())
                }))
            }
            QUOTES => {
                let dropped = &self.dropped_facts;
                Some(Box::new(move |b| {
                    Ok(strings(b, "fact_id")
                        .iter()
                        .map(|f| Some(!dropped.contains(f)))
                        .collect())
                }))
            }
            CLOSURES => Some(Box::new(move |b| {
                Ok(strings(b, "episode_id")
                    .iter()
                    .map(|e| Some(!gone.contains(e)))
                    .collect())
            })),
            // Summaries paraphrase every episode of their batch: a row drawn from one of them goes too (the
            // subject's next summary rebuilds what is left).
            AGGREGATES => Some(Box::new(move |b| {
                Ok(strings(b, "episodes")
                    .iter()
                    .map(|eps| Some(!eps.split(',').any(|e| gone.contains(e.trim()))))
                    .collect())
            })),
            // A logged answer cited facts of its episodes, and feedback is on a fact of its episodes: each goes
            // with them, as does one citing an episode no longer stored at all.
            QA_LOG | FEEDBACK => {
                let stored = self.stored.as_ref();
                Some(Box::new(move |b| {
                    Ok(strings(b, "episodes")
                        .iter()
                        .map(|eps| Some(!cites(eps, stored)))
                        .collect())
                }))
            }
            // A proposal citing a gone episode goes (its rows quote what was proposed from it), and so does
            // every decision on it.
            crate::governance::TABLE => {
                let gone_proposals = &self.gone_proposals;
                Some(Box::new(move |b| {
                    Ok(strings(b, "body")
                        .iter()
                        .map(|body| {
                            Some(!crate::governance::names(body).is_some_and(|n| {
                                n.proposal.is_some_and(|p| gone_proposals.contains(&p))
                            }))
                        })
                        .collect())
                }))
            }
            EPISODES => {
                let rows = self.rows;
                Some(Box::new(move |b| {
                    Ok(strings(b, "episode_id")
                        .iter()
                        .zip(strings(b, "source"))
                        .map(|(id, source)| {
                            Some(!match rows {
                                Some(f) => f(id, &source),
                                None => gone.contains(id),
                            })
                        })
                        .collect())
                }))
            }
            _ => None,
        }
    }
}

/// The notes a profile version's body cites: every string of its JSON that is an episode id (64 hex digits), but in
/// its lines' words (`text`), which may quote anything. Nothing where the body isn't JSON.
fn profile_cites(body: &[u8]) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()) => {
                out.push(s.clone())
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Value::Object(o) => o
                .iter()
                .filter(|(k, _)| k.as_str() != "text")
                .for_each(|(_, v)| walk(v, out)),
            _ => {}
        }
    }
    let mut out = vec![];
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        walk(&v, &mut out);
    }
    out
}

/// The rows of `files` `keep` marks, and how many it didn't.
fn filter_files<'a>(
    files: impl Iterator<Item = &'a Arc<Vec<RecordBatch>>>,
    keep: &mut (dyn FnMut(&RecordBatch) -> Result<BooleanArray> + Send),
) -> Result<(Vec<RecordBatch>, usize)> {
    let (mut kept, mut removed) = (vec![], 0);
    for b in files.flat_map(|v| v.iter()) {
        let mask = keep(b)?;
        let k = arrow_select::filter::filter_record_batch(b, &mask)?;
        removed += b.num_rows() - k.num_rows();
        if k.num_rows() > 0 {
            kept.push(k);
        }
    }
    Ok((kept, removed))
}

/// Delete every file `table`'s metadata references (its data files, manifests, manifest lists and metadata
/// files), as a purge does, skipping what is already gone. A purge deletes data files before the manifests that
/// list them, and manifests before the manifest lists, so a manifest list or manifest gone means what it listed
/// is gone too.
async fn delete_table_files(table: &iceberg::table::Table) -> Result<()> {
    let io = table.file_io();
    let gone = |path: String| async move {
        if io.exists(&path).await? {
            io.delete(&path)
                .await
                .with_context(|| format!("delete {path}"))?;
        }
        anyhow::Ok(())
    };
    let meta = table.metadata();
    let mut manifests = HashSet::new();
    let mut lists = vec![];
    for snapshot in meta.snapshots() {
        lists.push(snapshot.manifest_list().to_string());
        if !io.exists(snapshot.manifest_list()).await? {
            continue;
        }
        if let Some(list) = crate::manifest_list(table, snapshot).await? {
            manifests.extend(list.entries().iter().cloned());
        }
    }
    let mut paths = HashSet::new();
    for m in &manifests {
        if !io.exists(&m.manifest_path).await? {
            continue;
        }
        for e in m.load_manifest(io).await?.entries() {
            paths.insert(e.data_file().file_path().to_string());
        }
    }
    for p in paths {
        gone(p).await?;
    }
    for m in manifests {
        gone(m.manifest_path).await?;
    }
    for l in lists {
        gone(l).await?;
    }
    for m in meta.metadata_log() {
        gone(m.metadata_file.clone()).await?;
    }
    if let Some(m) = table.metadata_location() {
        gone(m.to_string()).await?;
    }
    Ok(())
}

/// `table` as a commit base whose current snapshot lists only `manifests` (a manifest list written for it at `path`),
/// with totals of `totals` (rows, data files, bytes), so a snapshot built on it counts what it holds. Automatic
/// commit retry is off for it: a retry would rebase onto the catalog's table and keep every file.
async fn with_manifests(
    table: &iceberg::table::Table,
    path: &str,
    manifests: &[iceberg::spec::ManifestFile],
    totals: (u64, u64, u64),
) -> Result<iceberg::table::Table> {
    use iceberg::spec::{FormatVersion, ManifestListWriter, TableMetadata, TableProperties};
    let meta = table.metadata();
    let current = meta
        .current_snapshot()
        .context("a table with no snapshot has no files to replace")?;
    let out = table.file_io().new_output(path)?.writer().await?;
    let (id, parent, seq) = (
        current.snapshot_id(),
        current.parent_snapshot_id(),
        current.sequence_number(),
    );
    let mut writer = match meta.format_version() {
        FormatVersion::V1 => ManifestListWriter::v1(out, id, parent),
        FormatVersion::V2 => ManifestListWriter::v2(out, id, parent, seq),
        FormatVersion::V3 => anyhow::bail!(
            "{} is a v3 table: rewrites support v1 and v2",
            table.identifier()
        ),
    };
    writer.add_manifests(manifests.iter().cloned())?;
    writer.close().await?;
    let mut json = serde_json::to_value(meta)?;
    for s in json["snapshots"].as_array_mut().into_iter().flatten() {
        if s["snapshot-id"].as_i64() == Some(id) {
            s["manifest-list"] = json!(path);
            if let Some(summary) = s["summary"].as_object_mut() {
                summary.retain(|k, _| !k.starts_with("total-"));
                for (k, v) in [
                    ("total-records", totals.0),
                    ("total-data-files", totals.1),
                    ("total-files-size", totals.2),
                    ("total-delete-files", 0),
                    ("total-position-deletes", 0),
                    ("total-equality-deletes", 0),
                ] {
                    summary.insert(k.into(), json!(v.to_string()));
                }
            }
        }
    }
    json["properties"][TableProperties::PROPERTY_COMMIT_NUM_RETRIES] = json!("0");
    let bare: TableMetadata = serde_json::from_value(json)?;
    let mut builder = iceberg::table::Table::builder()
        .metadata(bare)
        .identifier(table.identifier().clone())
        .file_io(table.file_io().clone())
        .runtime(iceberg::Runtime::try_current()?);
    if let Some(loc) = table.metadata_location() {
        builder = builder.metadata_location(loc);
    }
    Ok(builder.build()?)
}

/// Whether `table` has the snapshot of the rewrite `rewrite_id` ([`REWRITE_ID_PROP`]).
fn has_rewrite(table: &iceberg::table::Table, rewrite_id: &str) -> bool {
    table.metadata().snapshots().any(|s| {
        s.summary()
            .additional_properties
            .get(REWRITE_ID_PROP)
            .is_some_and(|id| id == rewrite_id)
    })
}

/// The files some snapshots reference: their manifest lists, the manifests those list, the data (and delete) files
/// those name. `lenient`: a manifest list or manifest that is gone is skipped (a sweep stopped halfway deleted it,
/// after everything it named); else it is an error.
#[derive(Default)]
struct Referenced {
    lists: HashSet<String>,
    manifests: HashSet<String>,
    data: HashSet<String>,
}

async fn referenced(
    io: &iceberg::io::FileIO,
    lists: Vec<String>,
    fv: iceberg::spec::FormatVersion,
    lenient: bool,
) -> Result<Referenced> {
    use futures::StreamExt;
    let mut out = Referenced::default();
    let mut manifests = HashMap::new();
    out.lists = lists.into_iter().collect();
    let reads = futures::stream::iter(out.lists.iter().cloned().map(|list| {
        let io = io.clone();
        async move {
            if lenient && !io.exists(&list).await? {
                return anyhow::Ok(vec![]);
            }
            let bytes = io
                .new_input(&list)?
                .read()
                .await
                .with_context(|| format!("read {list}"))?;
            Ok(iceberg::spec::ManifestList::parse_with_version(&bytes, fv)?
                .consume_entries()
                .into_iter()
                .collect())
        }
    }))
    .buffer_unordered(32);
    let read: Vec<Vec<iceberg::spec::ManifestFile>> = reads.try_collect().await?;
    for m in read.into_iter().flatten() {
        manifests.entry(m.manifest_path.clone()).or_insert(m);
    }
    out.manifests = manifests.keys().cloned().collect();
    let loads = futures::stream::iter(manifests.into_values().map(|m| {
        let io = io.clone();
        async move {
            let io = &io;
            if lenient && !io.exists(&m.manifest_path).await? {
                return anyhow::Ok(vec![]);
            }
            let manifest = m
                .load_manifest(io)
                .await
                .with_context(|| format!("read {}", m.manifest_path))?;
            Ok(manifest
                .entries()
                .iter()
                .map(|e| e.data_file().file_path().to_string())
                .collect())
        }
    }))
    .buffer_unordered(32);
    let paths: Vec<Vec<String>> = loads.try_collect().await?;
    out.data = paths.into_iter().flatten().collect();
    Ok(out)
}

/// Delete `paths`, 32 at a time; one already gone is skipped.
async fn delete_files<'a>(
    io: &iceberg::io::FileIO,
    paths: impl Iterator<Item = &'a str>,
) -> Result<()> {
    use futures::StreamExt;
    let paths: Vec<String> = paths.map(String::from).collect();
    futures::stream::iter(paths.into_iter().map(|p| {
        let io = io.clone();
        async move {
            if let Err(e) = io.delete(&p).await
                && io.exists(&p).await?
            {
                return Err(anyhow::Error::from(e).context(format!("delete {p}")));
            }
            anyhow::Ok(())
        }
    }))
    .buffer_unordered(32)
    .try_collect::<Vec<()>>()
    .await?;
    Ok(())
}

/// Write `batches` (rows read from `table`) as new data files of it, uncommitted.
async fn write_batches(
    table: &iceberg::table::Table,
    batches: &[RecordBatch],
    dir: &str,
) -> Result<Vec<iceberg::spec::DataFile>> {
    let Some(first) = batches.first() else {
        return Ok(vec![]);
    };
    let all = arrow_select::concat::concat_batches(&first.schema(), batches)?;
    let schema = all.schema();
    let columns = schema
        .fields()
        .iter()
        .zip(all.columns())
        .map(|(f, c)| (f.name().as_str(), c.clone()))
        .collect();
    crate::write_data_files(table, columns, Some(dir)).await
}

/// The rows of data files of `table` (none committed yet), read as a planned scan task of it (`template`) reads.
async fn read_data_files(
    table: &iceberg::table::Table,
    template: Option<&iceberg::scan::FileScanTask>,
    files: &[iceberg::spec::DataFile],
) -> Result<Vec<RecordBatch>> {
    if files.is_empty() {
        return Ok(vec![]);
    }
    let template = template.context("no scan task of the table to read its new files with")?;
    let tasks = files
        .iter()
        .map(|f| iceberg::scan::FileScanTask {
            file_size_in_bytes: f.file_size_in_bytes(),
            start: 0,
            length: f.file_size_in_bytes(),
            record_count: Some(f.record_count()),
            data_file_path: f.file_path().to_string(),
            data_file_format: f.file_format(),
            deletes: vec![],
            partition: Some(f.partition().clone()),
            ..template.clone()
        })
        .collect();
    let read = crate::read_tasks(table, tasks, HashMap::new()).await?;
    Ok(read
        .into_values()
        .flat_map(|v| v.iter().cloned().collect::<Vec<_>>())
        .collect())
}

/// Purge every namespace of `org` (its own and its teams') with its search indexes: (namespaces,
/// tables) purged. Never the default org's.
pub async fn purge_memory(store: &Store, org: &str) -> Result<(usize, usize)> {
    anyhow::ensure!(org != DEFAULT_ORG, "the default org can't be deleted");
    let mut tables = 0;
    let namespaces = store.org_namespaces(org).await?;
    for ns in &namespaces {
        tables += store.purge_namespace(ns).await?;
    }
    Ok((namespaces.len(), tables))
}

/// `memstore org delete`, after the directory change: purge the org's namespaces and its search indexes,
/// remove its rows from the ledger, the audit table and the key usage table, and record the delete in the audit log
/// under no org (the org is gone; only the operator sees it). Can be run again.
pub async fn purge_org(store: &Store, actor: &str, org: &str) -> Result<usize> {
    let (namespaces, tables) = purge_memory(store, org).await?;
    let ledger_rows = store.forget_ledger(org).await?;
    // The org's rows in the system tables: removed, not anonymised (an org's audit is the org's).
    let mut not_org = |b: &RecordBatch| -> Result<BooleanArray> {
        Ok(optional_strings(b, "org")
            .iter()
            .map(|o| Some(o.as_deref() != Some(org)))
            .collect())
    };
    let audit_rows = store
        .rewrite_table(SYSTEM_NS, crate::audit::AUDIT, &mut not_org)
        .await?;
    let usage_rows = store
        .rewrite_table(SYSTEM_NS, KEY_USAGE, &mut not_org)
        .await?;
    let mut e = crate::audit::Entry {
        at: chrono::Utc::now().timestamp_micros(),
        kind: crate::audit::Kind::Change,
        org: None,
        actor: crate::audit::clean(actor),
        scope: None,
        path: None,
        detail: None,
        status: None,
        count: 1,
        reads: 0,
        writes: 0,
        incarnation: None,
    };
    e.detail = Some(format!(
        "delete org {org}: {namespaces} namespaces and {tables} tables purged; {ledger_rows} ledger, {audit_rows} audit and {usage_rows} key usage rows removed"
    ));
    store.append_audit(&[e]).await?;
    Ok(tables)
}

/// `memstore users forget`: take `user` out of the org (or, for the operator with no org named, every
/// org) and its teams, delete what they wrote there, and leave a tombstone in the directory (who forgot
/// whom, when, and how much went; never what). Returns the tombstones' descriptions and the counts.
pub async fn forget_user(
    store: &Store,
    actor: &Actor,
    user: &str,
    org: Option<&str>,
) -> Result<Value> {
    let user = user_id(user);
    if user.is_empty()
        || user
            .chars()
            .any(|c| c == ':' || c == ',' || c.is_whitespace() || c.is_control())
    {
        return Err(refused(format!("{user:?} is not a user id")));
    }
    if !actor.superuser && user == user_id(&actor.user) {
        return Err(refused(format!(
            "{user} can't forget themselves (an admin key or another admin can)"
        )));
    }
    let dir = Directory::load(store).await?;
    let orgs: Vec<String> = match org {
        Some(o) => vec![o.to_string()],
        None if actor.superuser => dir.orgs.keys().cloned().collect(),
        None => return Err(refused("say which org: --org <org>".into())),
    };
    for o in &orgs {
        if crate::check_name("org", o).is_err() {
            return Err(refused(format!("invalid org name {o:?}")));
        }
        may_manage(&dir, actor, o, &format!("forget {user}"))?;
        // Another cell's org: refused before anything changes in any org, never forgotten halfway.
        crate::cell::check_org(o)?;
        if !dir.orgs.contains_key(o) {
            return Err(refused(format!("no org {o}")));
        }
    }
    // Before anything changes: a catalog that can't rewrite a table would leave the forget halfway.
    store.check_rewrites().await?;
    let me = actor.user.clone();
    let mut changes = vec![];
    let mut counts = BTreeMap::new();
    for o in &orgs {
        // Out of the org (and so its teams) first: they write nothing more here. Checked again against
        // the directory as it is when the change commits.
        let removal = store
            .append_directory_with(|dir| {
                may_manage(dir, actor, o, &format!("forget {user}"))?;
                let Some(org) = dir.orgs.get(o) else {
                    return Err(refused(format!("no org {o}")));
                };
                let joins = user
                    .rsplit_once('@')
                    .is_some_and(|(_, d)| org.domains.contains_key(d))
                    || !org.groups.is_empty();
                let remove = Event::new(&me, "member.remove", o).user(&user);
                // Keys imported in their name go with them, member or not (one imported before their
                // removal revoked nothing).
                let revokes = crate::people::directory::import_revokes(
                    dir,
                    &me,
                    std::slice::from_ref(&remove),
                );
                let mut out = match org.members.contains_key(&user) || joins {
                    true => vec![remove],
                    false => vec![],
                };
                out.extend(revokes);
                Ok(out)
            })
            .await?;
        changes.extend(removal.iter().map(|e| e.describe()));
        // The rights again before each step that follows, against the directory as it is then: an admin
        // demoted after the removal deletes nothing more and leaves no tombstone. What is left is safe (the
        // person is out of the org, nothing of theirs is half-deleted in a scope: each scope's forget is whole)
        // and the forget can be run again by someone who may.
        let still = |dir: &Directory| {
            may_manage(dir, actor, o, &format!("forget {user}")).map_err(|e| {
                refused(format!(
                    "{e:#}: {user} is out of org {o}, but what they wrote there is not all deleted yet; \
                     an admin of {o} can run the forget again to finish it"
                ))
            })
        };
        let mut total = Forgotten::default();
        for ns in store.org_namespaces(o).await? {
            still(&Directory::fresh(store).await?)?;
            total.add(&store.forget_in(&ns, &user).await?);
        }
        let detail = serde_json::to_string(&total)?;
        let tomb = store
            .append_directory_with(|dir| {
                still(dir)?;
                Ok(vec![
                    Event::new(&me, "member.forget", o)
                        .user(&user)
                        .detail(&detail),
                ])
            })
            .await?;
        changes.extend(tomb.iter().map(|e| e.describe()));
        counts.insert(o.clone(), total);
    }
    Ok(json!({"changes": changes, "forgotten": counts}))
}

/// `memstore org purge`: delete for good what was retracted in the org (its scope and every team's, deleted
/// teams' left-over memory included), as [`Store::purge_retracted_in`] does, and record in the org's audit
/// how much went (never what). Retraction already hid it from every reader; this takes it out of storage, a
/// table rewrite per table holding such rows, so it runs in batches rather than once per delete. Can be run
/// again. Returns the counts.
pub async fn purge_retracted(store: &Store, actor: &Actor, org: &str) -> Result<Value> {
    crate::cell::check_org(org)?;
    if crate::check_name("org", org).is_err() {
        return Err(refused(format!("invalid org name {org:?}")));
    }
    let dir = Directory::load(store).await?;
    may_manage(&dir, actor, org, "purge memory")?;
    if org != DEFAULT_ORG && !dir.orgs.contains_key(org) {
        return Err(refused(format!("no org {org}")));
    }
    let mut total = Forgotten::default();
    for ns in store.org_namespaces(org).await? {
        total.add(&store.purge_retracted_in(&ns).await?);
    }
    let detail = serde_json::to_string(&total)?;
    store
        .append_audit(&[crate::audit::Entry {
            at: chrono::Utc::now().timestamp_micros(),
            kind: crate::audit::Kind::Change,
            org: Some(org.to_string()),
            incarnation: Some(dir.orgs.get(org).map_or(0, |o| o.created_at)),
            actor: crate::audit::clean(&actor.user),
            scope: None,
            path: None,
            detail: Some(format!("purge retracted memory: {detail}")),
            status: None,
            count: 1,
            reads: 0,
            writes: 0,
        }])
        .await?;
    Ok(json!({"org": org, "purged": total}))
}

/// `memstore org retention`: show an org's (or one of its teams') retention policy, or with `rules` set a
/// new one ([`crate::retention::Retention::parse`]; none keeps everything) and record it in the org's
/// audit. Episodes past the policy are gone for every reader at once; `org purge` removes them from
/// storage. Org admins in their org, or the operator.
pub async fn retention(
    store: &Store,
    actor: &Actor,
    org: &str,
    team: Option<&str>,
    rules: Option<&[String]>,
) -> Result<Value> {
    crate::cell::check_org(org)?;
    if crate::check_name("org", org).is_err() {
        return Err(refused(format!("invalid org name {org:?}")));
    }
    let dir = Directory::load(store).await?;
    let what = match rules {
        Some(_) => "set retention",
        None => "see retention",
    };
    may_manage(&dir, actor, org, what)?;
    let known = dir.orgs.get(org);
    if org != DEFAULT_ORG && known.is_none() {
        return Err(refused(format!("no org {org}")));
    }
    let scope = match team {
        None => Scope::org(org)?,
        Some(t) => {
            if !known.is_some_and(|o| o.teams.contains_key(t)) {
                return Err(refused(format!("no team {t} in org {org}")));
            }
            Scope::team(org, t)?
        }
    };
    if let Some(rules) = rules {
        let policy = crate::retention::Retention::parse(rules)?;
        store.set_retention(&scope, &actor.user, &policy).await?;
        store
            .append_audit(&[crate::audit::Entry {
                at: chrono::Utc::now().timestamp_micros(),
                kind: crate::audit::Kind::Change,
                org: Some(org.to_string()),
                incarnation: Some(known.map_or(0, |o| o.created_at)),
                actor: crate::audit::clean(&actor.user),
                scope: Some(scope.to_string()),
                path: None,
                detail: Some(format!("retention: {}", policy.describe().join(" "))),
                status: None,
                count: 1,
                reads: 0,
                writes: 0,
            }])
            .await?;
    }
    let policy = store.retention(&scope).await?;
    Ok(json!({"scope": scope.to_string(), "retention": policy.describe()}))
}

/// One file of an export: its path under the export directory and its JSON lines.
pub type ExportFile = (String, Vec<Value>);

/// `memstore org export`: the org's memory as committed facts, the entities they describe, and the
/// episodes with their text. The operator (`reader` unused) takes all of it: the org's scope and every
/// team's, whatever the episodes' ACLs. An org admin (`reader`: who they are signed in as) takes what
/// they may read: the org's scope and the teams they are members of (a team is isolated from the rest of
/// its org, admins included), and only the episodes their ACLs let them see and the facts all of whose
/// sources they can see, as every read path gives it to them. Never a secret, a key or a key's hash:
/// those live in the directory, which isn't exported.
pub async fn export_org(
    store: &Store,
    actor: &Actor,
    reader: Option<&crate::Principal>,
    org: &str,
) -> Result<Vec<ExportFile>> {
    crate::cell::check_org(org)?;
    if crate::check_name("org", org).is_err() {
        return Err(refused(format!("invalid org name {org:?}")));
    }
    let dir = Directory::load(store).await?;
    // Rights first: whether an org exists is no business of someone who may not manage it.
    may_manage(&dir, actor, org, "export memory")?;
    let Some(o) = dir.orgs.get(org) else {
        return Err(refused(format!("no org {org}")));
    };
    // Fail closed: only the operator exports without a reader.
    let reader = match (actor.superuser, reader) {
        (true, _) => None,
        (false, Some(r)) => Some(r),
        (false, None) => return Err(refused(format!("{} may not export org {org}", actor.user))),
    };
    let teams: Vec<String> = match reader {
        None => o.teams.keys().cloned().collect(),
        Some(_) => dir
            .access(&actor.user, &actor.via)
            .get(org)
            .map(|a| a.teams.iter().cloned().collect())
            .unwrap_or_default(),
    };
    let mut scopes = vec![("org".to_string(), Scope::org(org)?)];
    for t in teams {
        scopes.push((format!("team-{t}"), Scope::team(org, &t)?));
    }
    let mut out = vec![];
    for (name, scope) in scopes {
        let ns = scope.namespace();
        let facts_ident = TableIdent::new(NamespaceIdent::new(ns.clone()), FACTS.into());
        if !store.catalog().table_exists(&facts_ident).await? {
            continue;
        }
        let vis = store.visibility(&scope, MAIN).await?;
        let (facts, entities) = export_facts(store, &ns, &vis, reader).await?;
        let episodes = export_episodes(store, &ns, &vis, reader).await?;
        out.push((format!("{name}/facts.jsonl"), facts));
        out.push((format!("{name}/entities.jsonl"), entities));
        out.push((format!("{name}/episodes.jsonl"), episodes));
    }
    Ok(out)
}

async fn export_facts(
    store: &Store,
    ns: &str,
    vis: &crate::Vis,
    reader: Option<&crate::Principal>,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let table = store.table(ns, FACTS).await?;
    let rows = read_files(&table, HashMap::new()).await?;
    let mut facts = vec![];
    let mut entities: BTreeMap<String, (Option<String>, usize)> = BTreeMap::new();
    for b in rows.values().flat_map(|v| v.iter()) {
        for (commit, f) in crate::decode_facts(b, "source_episodes") {
            let shown = match reader {
                None => {
                    vis.committed.contains(&commit)
                        && !vis.retracted_facts.contains(&f.fact_id)
                        && !f.source_episodes.iter().any(|e| vis.is_retracted(e))
                }
                Some(who) => vis.allows(&commit, &f, who, None),
            };
            if !shown {
                continue;
            }
            let e = entities.entry(f.entity.clone()).or_default();
            e.1 += 1;
            if e.0.is_none() {
                e.0 = f.entity_type.clone();
            }
            facts.push(json!({
                "fact_id": f.fact_id, "entity": f.entity, "predicate": f.predicate, "value": f.value,
                "valid_from": f.valid_from, "valid_to": f.valid_to, "source_episodes": f.source_episodes,
                "quote": f.quote, "entity_type": f.entity_type, "value_type": f.value_type,
                "value_num": f.value_num, "value_ts": f.value_ts, "value_ref": f.value_ref,
                "unit": f.unit, "cite": crate::cite_json(&f), "time_basis": f.time_basis,
                "commit_id": commit,
            }));
        }
    }
    let entities = entities
        .into_iter()
        .map(|(id, (kind, n))| json!({"entity": id, "entity_type": kind, "facts": n}))
        .collect();
    Ok((facts, entities))
}

async fn export_episodes(
    store: &Store,
    ns: &str,
    vis: &crate::Vis,
    reader: Option<&crate::Principal>,
) -> Result<Vec<Value>> {
    let table = store.table(ns, EPISODES).await?;
    let rows = read_files(&table, HashMap::new()).await?;
    // The newest committed row of each episode (an ACL change adds a row).
    let mut newest: BTreeMap<String, (i64, Value)> = BTreeMap::new();
    for b in rows.values().flat_map(|v| v.iter()) {
        let (commit, id, source) = (
            strings(b, "commit_id"),
            strings(b, "episode_id"),
            strings(b, "source"),
        );
        let (occurred, recorded) = (timestamps(b, "occurred_at"), timestamps(b, "recorded_at"));
        let (acl, about) = (strings(b, "acl"), optional_strings(b, "about"));
        for i in 0..b.num_rows() {
            if !vis.committed.contains(&commit[i])
                || vis.is_retracted(&id[i])
                || reader.is_some_and(|who| !vis.can_see_episode(&id[i], who))
            {
                continue;
            }
            let at = recorded[i].unwrap_or_default();
            if newest.get(&id[i]).is_some_and(|(t, _)| *t > at) {
                continue;
            }
            let v = json!({
                "episode_id": id[i], "source": source[i], "occurred_at": occurred[i],
                "recorded_at": recorded[i], "acl": crate::split_list(&acl[i]),
                "about": about[i].as_deref().map(crate::split_list).unwrap_or_default(),
            });
            newest.insert(id[i].clone(), (at, v));
        }
    }
    let mut out = vec![];
    for (id, (_, mut v)) in newest {
        let body = table
            .file_io()
            .new_input(body_path(&table, &id))?
            .read()
            .await
            .with_context(|| format!("read the body of episode {id}"))?;
        match String::from_utf8(body.to_vec()) {
            Ok(text) => v["body"] = json!(text),
            Err(_) => {
                use base64::Engine;
                v["body_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&body));
            }
        }
        out.push(v);
    }
    Ok(out)
}

/// The ACL tokens an audience names: a request's `for:[...]`, or a single entry (a per-entry run's). An audience
/// that doesn't parse names no one.
pub fn audience_tokens(audience: &str) -> Vec<String> {
    match audience.strip_prefix("for:") {
        Some(json) => serde_json::from_str(json).unwrap_or_default(),
        None => vec![audience.to_string()],
    }
}
