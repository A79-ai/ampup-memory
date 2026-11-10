//! Table compaction, in `serve`'s nightly maintenance window (`maintenance`): every table of every scope, and of
//! the system namespace, merged once it holds too many data files.
//!
//! Every commit appends a data file and a manifest to each table it writes, and a cold read reads them all: the
//! ledger at each server's start (thousands of files made a start take minutes), a scope's summaries at every brief
//! read (one org's grew to 4,800 files, and a cold `list_briefs` took 90 s), a scope's facts and episodes at every
//! first read of the scope. A compaction ([`crate::Store::compact_in`], as `memstore compact-ledger` does the
//! ledger) rewrites a table into as few files as it takes (512 MB each) in one compare-and-set swap, writers'
//! commits landing meanwhile carried over, the rows counted and checksummed as read, carried over and written,
//! then the replaced snapshots expired and their files deleted. It keeps every row, but a scope's summaries' older
//! runs no reader is served (a branch's newest, a workspace snapshot's and the last hour's, for cursors, are kept).
//!
//! - only the writer lease's holder, through its fenced store, so two servers never compact at once and one that
//!   lost the lease commits nothing; the system tables once a night by the default cell, a scope's in its turn;
//! - when a table holds more data files than its threshold, or (the ledger and the summaries) its last compaction
//!   is older than a day (read as of the window's close) and there are files to merge:
//!   - the ledger: `AMPUP_MEMORY_LEDGER_COMPACT_FILES` (500), `_EVERY` (24h), up to `_MAX_FILES` (100,000) and
//!     `_MAX_BYTES` (1 GiB);
//!   - each scope's summaries: `AMPUP_MEMORY_AGGREGATES_COMPACT_FILES` (200), `_EVERY` (24h), up to `_MAX_FILES` (20,000);
//!   - every other table (facts, episodes, quotes, closures, extractions, schemas, retention, retractions, the
//!     answer log, feedback, summary requests, workspace snapshots; the directory and its snapshots, installations,
//!     key usage, the audit log and the usage book): `AMPUP_MEMORY_COMPACT_FILES` (200), by files alone unless
//!     `AMPUP_MEMORY_COMPACT_EVERY` is set, up to `AMPUP_MEMORY_COMPACT_MAX_FILES` (20,000) and `AMPUP_MEMORY_COMPACT_MAX_BYTES`
//!     (1 GiB: a rewrite holds the table's rows in memory). The audit log's rows are kept whole: merged, never
//!     dropped;
//!   - past a ceiling it is left alone and logged (`"result":"needs_cli"`; the ledger's sets
//!     `memstore_ledger_compaction_cli_needed`): `memstore compact-ledger [--aggregates <scope>]` once;
//! - a run interrupted (the server killed, the lease lost) after its swap is finished by the next night's look at
//!   the table (`"result":"swept"`), and one killed before it committed nothing (its files go with a sweep an hour
//!   later);
//! - `AMPUP_MEMORY_LEDGER_COMPACT=off`, `AMPUP_MEMORY_AGGREGATES_COMPACT=off` and `AMPUP_MEMORY_COMPACT=off` turn each off (the
//!   ledger's metrics are still kept, every maintenance round).
//!
//! Each run logs one line, counts only (`ledger compaction: {...}`, `aggregates compaction: {...}` and `table
//! compaction: {...}`, the last two with the scope and the table), and appends an audit row of kind `maintenance`
//! (the operator's view).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::offboard::{Erasure, LedgerStatus};
use crate::{Scope, Store};

use super::State;

/// When the window compacts a table: `AMPUP_MEMORY_LEDGER_COMPACT*` (the ledger), `AMPUP_MEMORY_AGGREGATES_COMPACT*` (each
/// scope's summaries), `AMPUP_MEMORY_COMPACT*` (every other table).
#[derive(Clone, Debug)]
pub struct Config {
    pub on: bool,
    /// Compact once the table holds more data files than this...
    pub files: u64,
    /// ...or its last compaction is older than this (and there are files to merge). None: by files alone.
    pub every: Option<Duration>,
    /// Never in the window past this many data files: `memstore compact-ledger` once.
    pub max_files: u64,
    /// Nor past this many bytes of data files (a rewrite holds the table's rows in memory). None: no limit.
    pub max_bytes: Option<u64>,
}

impl Config {
    /// The ledger's: `AMPUP_MEMORY_LEDGER_COMPACT*`. Its ceiling is a day's commits and more: every write is a ledger
    /// commit and a file, a busy deployment's day is thousands (dev: 1,200 to 6,000), and a ceiling under that
    /// (2,000, set when it was compacted hourly) had the window leave it alone every night from then on, 37,000
    /// files that a server's start took minutes to read. A rewrite holds its rows in memory, so 1 GiB at most, as
    /// every other table (dev's 69,000 rows are 105 MB).
    pub fn from_env() -> Result<Self> {
        Self::read(
            "AMPUP_MEMORY_LEDGER_COMPACT",
            500,
            Some(24 * 3600),
            100_000,
            Some(1 << 30),
        )
    }

    /// Each scope's summaries': `AMPUP_MEMORY_AGGREGATES_COMPACT*`. Fewer files before a compaction than the
    /// ledger's (a brief read loads them all, and the table is small), and a higher ceiling: one org's had
    /// 4,800 files and 1.2 GB, which one run in the window merges in minutes.
    pub fn aggregates_from_env() -> Result<Self> {
        Self::read(
            "AMPUP_MEMORY_AGGREGATES_COMPACT",
            200,
            Some(24 * 3600),
            20_000,
            None,
        )
    }

    /// Every other table's, a scope's or the system's: `AMPUP_MEMORY_COMPACT*`. By files alone unless
    /// `AMPUP_MEMORY_COMPACT_EVERY` is set: a rewrite of a scope's facts or episodes makes every file new to its search
    /// indexes, which are then rebuilt, so it isn't done every night for a few files. At most
    /// `AMPUP_MEMORY_COMPACT_MAX_BYTES` (1 GiB) of data files: a rewrite holds the table's rows in memory.
    pub fn tables_from_env() -> Result<Self> {
        Self::read("AMPUP_MEMORY_COMPACT", 200, None, 20_000, Some(1 << 30))
    }

    fn read(
        name: &str,
        files: u64,
        every: Option<u64>,
        max_files: u64,
        max_bytes: Option<u64>,
    ) -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let count = |k: &str, default: u64| -> Result<u64> {
            var(k).map_or(Ok(default), |v| {
                v.trim()
                    .parse::<u64>()
                    .with_context(|| format!("{k} is a number, got {v:?}"))
            })
        };
        let on = match var(name).as_deref().map(str::trim) {
            None | Some("on") => true,
            Some("off") => false,
            Some(v) => anyhow::bail!("{name} is on or off, got {v:?}"),
        };
        let every =
            match var(&format!("{name}_EVERY")) {
                None => every.map(Duration::from_secs),
                Some(v) => Some(duration(&v).with_context(|| {
                    format!("{name}_EVERY is like 24h, 90m or 3600s, got {v:?}")
                })?),
            };
        let max_bytes = match var(&format!("{name}_MAX_BYTES")) {
            None => max_bytes,
            Some(_) => Some(count(&format!("{name}_MAX_BYTES"), 0)?),
        };
        let c = Config {
            on,
            files: count(&format!("{name}_FILES"), files)?,
            every,
            max_files: count(&format!("{name}_MAX_FILES"), max_files)?,
            max_bytes,
        };
        anyhow::ensure!(
            c.max_files >= c.files,
            "{name}_MAX_FILES ({}) is below {name}_FILES ({}): the window would never compact",
            c.max_files,
            c.files
        );
        Ok(c)
    }
}

/// `24h`, `90m`, `45s`, `2d`, or a number of seconds.
pub(super) fn duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (n, per) = match s.char_indices().last() {
        Some((i, 'd')) => (&s[..i], 86_400),
        Some((i, 'h')) => (&s[..i], 3_600),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 's')) => (&s[..i], 1),
        _ => (s, 1),
    };
    let n: u64 = n.trim().parse()?;
    anyhow::ensure!(n > 0, "a duration above 0");
    Ok(Duration::from_secs(n * per))
}

/// What the window does with a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Nothing,
    /// A rewrite committed and wasn't swept, or a pass that never landed is an hour old: sweep it.
    Sweep,
    /// Compact: past the file count, or the last compaction is too old.
    Compact(&'static str),
    /// Past the most the window compacts: the CLI, once.
    TooBig,
}

pub fn plan(c: &Config, s: &LedgerStatus, now_ms: i64) -> Plan {
    if s.unswept || s.stale_pending > 0 {
        return Plan::Sweep;
    }
    // Nothing to merge: no more files than a compaction would leave (one per 512 MB), however long ago. So a
    // compacted table is never rewritten again until commits have added files, whatever the settings.
    if s.data_files <= s.data_files_after().max(1) {
        return Plan::Nothing;
    }
    let by_files = s.data_files > c.files;
    let by_age = c.every.is_some_and(|every| {
        s.compacted_at_ms
            .is_none_or(|at| now_ms - at > every.as_millis() as i64)
    });
    if !(by_files || by_age) {
        return Plan::Nothing;
    }
    if s.data_files > c.max_files || c.max_bytes.is_some_and(|max| s.bytes > max) {
        return Plan::TooBig;
    }
    Plan::Compact(if by_files { "files" } else { "age" })
}

/// One maintenance round's look at the ledger, on every server: its status read for the metrics, nothing
/// written. A compaction runs only in the nightly window ([`run`]).
pub(super) async fn observe(st: &Arc<State>, c: &Config) {
    let store = st.store.read().unwrap().clone();
    let status = match store.ledger_status().await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            eprintln!(
                "ledger compaction: the ledger's status is unreadable: {}",
                super::writer::redacted(&e)
            );
            return;
        }
    };
    crate::metrics::LEDGER_DATA_FILES.set(status.data_files as i64);
    if let Some(at) = status.compacted_at_ms {
        crate::metrics::LEDGER_COMPACTED_AT.set(at / 1000);
    }
    let plan = plan(c, &status, chrono::Utc::now().timestamp_millis());
    crate::metrics::LEDGER_COMPACTION_CLI_NEEDED.set(i64::from(plan == Plan::TooBig));
}

/// A table the window compacts: a scope's (`scope`), or the system's (None).
#[derive(Clone, Copy, Debug)]
pub(super) struct Target<'a> {
    pub scope: Option<&'a Scope>,
    pub table: &'a str,
}

impl Target<'_> {
    fn is_ledger(&self) -> bool {
        self.scope.is_none() && self.table == crate::offboard::LEDGER_TABLE
    }

    fn is_aggregates(&self) -> bool {
        self.scope.is_some() && self.table == crate::AGGREGATES
    }

    /// What its log lines start with and its audit rows are of: the ledger's and the summaries' as before.
    fn kind(&self) -> (&'static str, &'static str) {
        if self.is_ledger() {
            ("ledger compaction", "ledger_compaction")
        } else if self.is_aggregates() {
            ("aggregates compaction", "aggregates_compaction")
        } else {
            ("table compaction", "table_compaction")
        }
    }

    /// Name it in a log line: the scope and the table (`<namespace>.<table>`), none for the ledger (as before).
    fn label(&self, line: &mut Value) {
        if let Some(s) = self.scope {
            line["scope"] = json!(s.to_string());
        }
        if !self.is_ledger() {
            line["table"] = json!(self.name());
        }
    }

    /// `<namespace>.<table>`.
    fn name(&self) -> String {
        let ns = self
            .scope
            .map_or(crate::SYSTEM_NS.to_string(), Scope::namespace);
        format!("{ns}.{}", self.table)
    }

    /// What `memstore` command compacts it by hand, past what the window takes.
    fn by_hand(&self) -> String {
        match (self.is_ledger(), self.scope) {
            (true, _) => {
                "run `ampup-memory compact-ledger` once; the window keeps it small from then on"
                    .into()
            }
            (false, Some(s)) if self.is_aggregates() => {
                format!("run `ampup-memory compact-ledger --aggregates {s}` once")
            }
            _ => format!(
                "left as it is: over AMPUP_MEMORY_COMPACT_MAX_FILES or AMPUP_MEMORY_COMPACT_MAX_BYTES (raise them for a \
                 night to compact {} once)",
                self.name()
            ),
        }
    }
}

/// The nightly window's look at a table that exists (`maintenance`): swept or compacted as [`plan`] says, with the age trigger
/// read as of `as_of_ms` (the window's close, so a table compacted at this hour last night is due again tonight).
/// Each run logs its own line (`ledger compaction: {...}`, `aggregates compaction: {...}` with the scope, `table
/// compaction: {...}` with the table) and appends an audit row; this returns what the window's line says of it. An
/// error is a status that couldn't be read or a run that failed.
pub(super) async fn run(
    store: &Store,
    t: Target<'_>,
    c: &Config,
    as_of_ms: i64,
    erasure: Option<&mut Erasure<'_>>,
) -> Result<Value> {
    // The window lists a scope's tables once ([`crate::Store::tables_in`]) and runs this only for those there.
    let status = store.status_listed(t.scope, t.table).await?;
    let plan = plan(c, &status, as_of_ms);
    if t.is_ledger() {
        crate::metrics::LEDGER_COMPACTION_CLI_NEEDED.set(i64::from(plan == Plan::TooBig));
    }
    // A scheduled purge leaves its rows out of this table's rewrite: the compaction's when one is due, else one of
    // its own (as `memstore org purge` would), whatever the thresholds say.
    let erasure =
        erasure.filter(|_| t.scope.is_some() && crate::offboard::ERASED_TABLES.contains(&t.table));
    if let Some(e) = erasure {
        let due = c.on && matches!(plan, Plan::Compact(_));
        let trigger = match plan {
            Plan::Compact(trigger) if c.on => trigger,
            _ => "purge",
        };
        return compact(store, t, trigger, &status, due, Some(e)).await;
    }
    if !c.on {
        return Ok(json!({"result": "off", "data_files": status.data_files}));
    }
    match plan {
        Plan::Nothing => Ok(json!({"result": "not_due", "data_files": status.data_files})),
        Plan::TooBig => {
            let mut line = json!({"result": "needs_cli", "data_files": status.data_files,
                                  "bytes": status.bytes, "max_files": c.max_files, "max_bytes": c.max_bytes,
                                  "action": t.by_hand()});
            t.label(&mut line);
            eprintln!("{}: {line}", t.kind().0);
            Ok(line)
        }
        Plan::Sweep => sweep(store, t, &status).await,
        Plan::Compact(trigger) => compact(store, t, trigger, &status, true, None).await,
    }
}

async fn sweep(store: &Store, t: Target<'_>, before: &LedgerStatus) -> Result<Value> {
    let started = Instant::now();
    let result = store.sweep_table(t.scope, t.table).await;
    let ms = started.elapsed().as_millis() as u64;
    let (outcome, mut line) = match &result {
        Ok(n) => (
            "swept",
            json!({"result": "swept", "files_deleted": n, "duration_ms": ms,
                   "unswept_compaction": before.unswept, "stale_passes": before.stale_pending}),
        ),
        Err(e) => {
            if t.is_ledger() {
                crate::metrics::LEDGER_COMPACTION_FAILURES.inc();
            }
            (
                "sweep_failed",
                json!({"result": "sweep_failed", "duration_ms": ms, "error": super::writer::redacted(e)}),
            )
        }
    };
    t.label(&mut line);
    let counts = [
        ("files_deleted", *result.as_ref().unwrap_or(&0) as u64),
        ("duration_ms", ms),
    ];
    audit(store, t.kind().1, outcome, &counts, &mut line).await;
    eprintln!("{}: {line}", t.kind().0);
    result.map(|_| line)
}

/// A rewrite of the table: compacted when `due`, and leaving out what `erasure` deletes from it.
async fn compact(
    store: &Store,
    t: Target<'_>,
    trigger: &'static str,
    before: &LedgerStatus,
    due: bool,
    erasure: Option<&mut Erasure<'_>>,
) -> Result<Value> {
    let started = Instant::now();
    if t.is_ledger() {
        crate::take_ledger_wait();
    }
    let purging = erasure.is_some();
    let result = store
        .rewrite_in(t.scope, t.table, due, erasure, &crate::offboard::quiet)
        .await;
    // A purge that found nothing of its own here, with no compaction due: nothing was written.
    if !due && matches!(result, Ok(None)) {
        return Ok(json!({"result": "not_due", "data_files": before.data_files}));
    }
    let took = started.elapsed();
    let after = store.status_listed(t.scope, t.table).await.ok();
    let mut line = json!({
        "trigger": trigger,
        "data_files_before": before.data_files,
        "metadata_files_before": before.metadata_files(),
        "bytes_before": before.bytes,
        "duration_ms": took.as_millis() as u64,
    });
    t.label(&mut line);
    let o = line.as_object_mut().unwrap();
    if purging {
        o.insert("purge".into(), json!(true));
    }
    if t.is_ledger() {
        let wait = crate::take_ledger_wait();
        crate::metrics::LEDGER_COMPACTION_SECONDS.observe(took.as_secs_f64());
        o.insert("writer_wait_max_ms".into(), json!(wait.as_millis() as u64));
    }
    if let Some(a) = &after {
        o.insert("data_files_after".into(), json!(a.data_files));
        o.insert("metadata_files_after".into(), json!(a.metadata_files()));
        o.insert("bytes_after".into(), json!(a.bytes));
        if t.is_ledger() {
            crate::metrics::LEDGER_DATA_FILES.set(a.data_files as i64);
        }
    }
    let outcome = match &result {
        Ok(Some(c)) => {
            o.insert("rows".into(), json!(c.after.rows));
            o.insert("rows_dropped".into(), json!(c.removed));
            o.insert("rows_carried_over".into(), json!(c.late.rows));
            o.insert("swap_tries".into(), json!(c.swap_tries));
            o.insert("restarts".into(), json!(c.restarts));
            if t.is_ledger()
                && let Some(at) = after.as_ref().and_then(|a| a.compacted_at_ms)
            {
                crate::metrics::LEDGER_COMPACTED_AT.set(at / 1000);
            }
            "ok"
        }
        Ok(None) => "no_table",
        Err(e) => {
            if t.is_ledger() {
                crate::metrics::LEDGER_COMPACTION_FAILURES.inc();
            }
            o.insert("error".into(), json!(super::writer::redacted(e)));
            "failed"
        }
    };
    o.insert("result".into(), json!(outcome));
    let n = |k: &str| line.get(k).and_then(Value::as_u64).unwrap_or(0);
    let counts = [
        ("rows", n("rows")),
        ("rows_dropped", n("rows_dropped")),
        ("rows_carried_over", n("rows_carried_over")),
        ("data_files_before", n("data_files_before")),
        ("data_files_after", n("data_files_after")),
        ("metadata_files_before", n("metadata_files_before")),
        ("metadata_files_after", n("metadata_files_after")),
        ("bytes_before", n("bytes_before")),
        ("bytes_after", n("bytes_after")),
        ("duration_ms", n("duration_ms")),
        ("writer_wait_max_ms", n("writer_wait_max_ms")),
    ];
    audit(store, t.kind().1, outcome, &counts, &mut line).await;
    eprintln!("{}: {line}", t.kind().0);
    result.map(|_| line)
}

/// The run's audit row (the operator's view), through the fenced store: a server that lost the lease writes none.
async fn audit(
    store: &Store,
    kind: &'static str,
    outcome: &'static str,
    counts: &[(&'static str, u64)],
    line: &mut Value,
) {
    let row = crate::audit::maintenance(kind, outcome, counts);
    let written = match store.check_fence() {
        Ok(()) => store.append_audit(&[row]).await,
        Err(e) => Err(e),
    };
    if let Err(e) = written {
        line.as_object_mut()
            .unwrap()
            .insert("audit_error".into(), json!(super::writer::redacted(&e)));
    }
}
