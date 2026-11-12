//! The nightly maintenance window: the only time `serve` rewrites storage.
//!
//! A rewrite (a compaction of a table, `compaction`) reads a whole table and writes it again. Run on
//! a timer at any hour, they landed in the working day (one org's summaries: 428 MB rewritten at 13:45 Pacific), and
//! right after a start every scope's ran at once, and object storage answered the burst with 429s. So every rewrite
//! runs in one announced window a night, `AMPUP_MEMORY_MAINTAIN_WINDOW` (default `02:00-06:00 America/Los_Angeles`;
//! `<HH:MM>-<HH:MM> [<IANA zone>]`, the zone America/Los_Angeles when not given; a window may cross midnight):
//!
//! - on the writer lease's holder only, through its fenced store (a server that lost the lease commits nothing);
//! - one scope at a time, a pause of `AMPUP_MEMORY_MAINTAIN_PACE` (default `2s`; `0` for none) between two, within a
//!   budget, `AMPUP_MEMORY_MAINTAIN_BUDGET` (default: until the window closes). Past it, or once the window closes,
//!   nothing more starts, and a scope not reached is logged as skipped and waits for the next night;
//! - least recently maintained first: when the window last finished with a scope is a property of its episodes
//!   table ([`crate::offboard::MAINTAINED_AT_PROP`]), so a scope the budget didn't reach goes first the next night,
//!   whatever restarts came between, and a scope finished earlier in the night isn't done twice;
//! - a throttled catalog or store (429, 503) is backed off, not counted a failure: the scope goes to the back of
//!   the night's queue, tried again after a longer pause (up to [`THROTTLE_TRIES`] times), and if it is still
//!   refused it goes first the next night.
//!
//! The system tables (the ledger, the directory, the audit log, ...) come first, by the default cell. Then, in each scope, in this order:
//!
//! 1. the dream step ([`dream_step`], `crate::dream`, on the scopes `AMPUP_MEMORY_DREAM_ON` names): a pass over the
//!    scope's memory before it is rewritten. It never holds back what follows: a failure, or a pass past its budget
//!    (`AMPUP_MEMORY_DREAM_BUDGET`, default `10m`, and never past the night's), is logged and the rewrite and the sweep
//!    run anyway; no model call starts past 85% of it, so what the pass did by then is committed (and a pass commits
//!    in batches as it goes, so one cut off keeps what it committed before). Deletion never
//!    waits on it either: forget and retract take effect on request, not here;
//! 2. the rewrites: the scope's tables listed once, and each past its threshold compacted (`compaction`), what
//!    cites episodes first and the episodes last; the files a rewrite replaced are deleted as it lands, and what an
//!    interrupted one left is finished. Where the scope's org turned on the scheduled purge (`Settings::purge`),
//!    what its sources deleted and what aged past its retention, hidden for longer than `AMPUP_MEMORY_PURGE_GRACE`
//!    (default `7d`), is left out of the same rewrites ([`crate::offboard::Erasure`], as `memstore org purge`
//!    deletes it), a table with rows to delete rewritten whether or not a compaction is due, and the org's audit
//!    says how much went. A rewrite of the facts or the episodes makes every file new to the scope's
//!    search indexes, which trust only the files they cover: they are brought up to date then and there
//!    (`"reindex"`, one index update at a time on a server, reusing the vectors they hold), not left to scan the
//!    whole table until the day's refresh. Then the indexes are tidied for the night (`"index_tidy"`,
//!    `search::tidy_night`): the deltas the day's updates left beside each index merged into it, which an update
//!    leaves alone (merging it every few updates rewrote one org's whole vector index, 1.2 GB, for a dozen rows),
//!    their files compacted once there are too many, and rows written twice dropped;
//! 3. the sweep: every table's snapshots past the newest `AMPUP_MEMORY_MAINTAIN_KEEP` expired, and the manifest lists
//!    and metadata files only they named deleted.
//!
//! Each scope's turn logs one line, counts only: `maintenance: {"night":...,"scope":...,"result":"done",
//! "dream":{...},"rewrites":{"facts":{"result":"ok","data_files_before":...,"data_files_after":...,"rows":...},
//! "episodes":{"result":"not_due","data_files":12},...},"reindex":{...},"index_tidy":{...},"sweep":{"tables":13,
//! "snapshots_before":...,"snapshots_after":...},"duration_ms":...}`,
//! a scope the budget didn't reach `{"result":"skipped_for_budget"}`, and the night one more
//! (`maintenance night: {...}`). The day's maintenance round (`AMPUP_MEMORY_MAINTAIN_SECS`) still expires snapshots of
//! busy tables, which every write needs, and keeps the ledger's metrics; it rewrites nothing.
//!
//! `AMPUP_MEMORY_MAINTAIN_WINDOW=always` keeps the window open: a pass every round, each one a "night" (to watch it
//! work); `off` rewrites nothing on a timer (`memstore compact-ledger` still does by hand).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Days, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use futures::FutureExt;
use serde_json::{Value, json};

use super::{ScopeWalk, State, cell, compaction};
use crate::{Scope, Store};

/// The window when `AMPUP_MEMORY_MAINTAIN_WINDOW` is not set.
pub const DEFAULT_WINDOW: &str = "02:00-06:00 America/Los_Angeles";
/// The zone of a window that names none.
const DEFAULT_ZONE: Tz = chrono_tz::America::Los_Angeles;
/// A throttled scope is tried this many times in a night, the pause doubling each time.
pub const THROTTLE_TRIES: u32 = 3;
/// The pause between two reads of when a scope was last maintained, the first night after a start.
const MARKER_PACE: Duration = Duration::from_millis(100);

/// When the window is open (`AMPUP_MEMORY_MAINTAIN_WINDOW`).
#[derive(Clone, Debug, PartialEq)]
pub enum Window {
    /// Each night from `start` to `end`, local time in `zone`.
    Daily {
        start: NaiveTime,
        end: NaiveTime,
        zone: Tz,
    },
    /// Always open: a pass every round.
    Always,
    /// Never: nothing is rewritten on a timer.
    Off,
}

/// One night's opening of the window: which night (the local date it opened on, or the pass's start when the
/// window is always open), when it opened, and when it closes.
#[derive(Clone, Debug)]
pub struct Night {
    pub name: String,
    pub opened: DateTime<Utc>,
    pub closes: DateTime<Utc>,
}

impl Window {
    /// `02:00-06:00 America/Los_Angeles`, `22:30-01:00 UTC`, `always` or `off`.
    pub fn parse(s: &str) -> Result<Window> {
        let s = s.trim();
        match s {
            "always" => return Ok(Window::Always),
            "off" => return Ok(Window::Off),
            _ => {}
        }
        let bad = || {
            anyhow::anyhow!(
                "AMPUP_MEMORY_MAINTAIN_WINDOW is <HH:MM>-<HH:MM> [<IANA zone>] (like {DEFAULT_WINDOW:?}), always or \
                 off, got {s:?}"
            )
        };
        let (times, zone) = match s.split_once(char::is_whitespace) {
            Some((t, z)) => (
                t,
                z.trim().parse::<Tz>().map_err(|_| {
                    anyhow::anyhow!(
                        "AMPUP_MEMORY_MAINTAIN_WINDOW: {:?} is not an IANA time zone",
                        z.trim()
                    )
                })?,
            ),
            None => (s, DEFAULT_ZONE),
        };
        let (a, b) = times.split_once('-').ok_or_else(bad)?;
        let time = |t: &str| NaiveTime::parse_from_str(t.trim(), "%H:%M").map_err(|_| bad());
        let (start, end) = (time(a)?, time(b)?);
        anyhow::ensure!(
            start != end,
            "AMPUP_MEMORY_MAINTAIN_WINDOW opens and closes at the same time ({s:?}): use always or off"
        );
        Ok(Window::Daily { start, end, zone })
    }

    /// The window from the environment.
    pub fn from_env() -> Result<Window> {
        match std::env::var("AMPUP_MEMORY_MAINTAIN_WINDOW") {
            Ok(v) if !v.trim().is_empty() => Window::parse(&v),
            _ => Window::parse(DEFAULT_WINDOW),
        }
    }

    /// The night's opening the window is in at `now`; None while it is closed (and always for `off`). When it
    /// is always open, the opening starts now and lasts `budget`.
    pub fn open_at(&self, now: DateTime<Utc>, budget: Option<Duration>) -> Option<Night> {
        match self {
            Window::Off => None,
            Window::Always => Some(Night {
                name: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                opened: now,
                closes: now + budget.unwrap_or(Duration::from_secs(3600)),
            }),
            Window::Daily { zone, .. } => {
                let today = now.with_timezone(zone).date_naive();
                // Opened yesterday (a window across midnight) or today.
                [today.pred_opt(), Some(today)]
                    .into_iter()
                    .flatten()
                    .filter_map(|d| self.opening(d))
                    .find(|n| n.opened <= now && now < n.closes)
            }
        }
    }

    /// When the window next opens after `now`, for the announcement.
    pub fn next_open(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let Window::Daily { zone, .. } = self else {
            return None;
        };
        let today = now.with_timezone(zone).date_naive();
        (0..3)
            .filter_map(|d| today.checked_add_days(Days::new(d)))
            .filter_map(|d| self.opening(d))
            .map(|n| n.opened)
            .find(|at| *at > now)
    }

    /// The opening of a daily window on the local date `day`.
    fn opening(&self, day: NaiveDate) -> Option<Night> {
        let Window::Daily { start, end, zone } = self else {
            return None;
        };
        let end_day = match end > start {
            true => day,
            false => day.checked_add_days(Days::new(1))?,
        };
        Some(Night {
            name: day.to_string(),
            opened: local(zone, day, *start)?,
            closes: local(zone, end_day, *end)?,
        })
    }

    fn describe(&self) -> String {
        match self {
            Window::Daily { start, end, zone } => {
                format!("{}-{} {zone}", start.format("%H:%M"), end.format("%H:%M"))
            }
            Window::Always => "always open (AMPUP_MEMORY_MAINTAIN_WINDOW=always)".into(),
            Window::Off => {
                "off (AMPUP_MEMORY_MAINTAIN_WINDOW=off): nothing is rewritten on a timer".into()
            }
        }
    }
}

/// `day` at `time` in `zone`, in UTC. A time the clocks skip (02:30 as summer time starts) is taken an hour
/// later; one they pass twice, the first time.
fn local(zone: &Tz, day: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let at = day.and_time(time);
    zone.from_local_datetime(&at)
        .earliest()
        .or_else(|| {
            zone.from_local_datetime(&(at + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
}

/// The window and what it does: `AMPUP_MEMORY_MAINTAIN_*`, `AMPUP_MEMORY_DREAM_BUDGET` and the compaction settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub window: Window,
    /// The most one night's pass may take (`AMPUP_MEMORY_MAINTAIN_BUDGET`); None: until the window closes.
    pub budget: Option<Duration>,
    /// The pause between two scopes (`AMPUP_MEMORY_MAINTAIN_PACE`).
    pub pace: Duration,
    /// The most the dream step may take in one scope (`AMPUP_MEMORY_DREAM_BUDGET`).
    pub dream: Duration,
    /// Snapshots kept of each table by the sweep (`AMPUP_MEMORY_MAINTAIN_KEEP`).
    pub keep: usize,
    pub ledger: compaction::Config,
    pub aggregates: compaction::Config,
    /// Every other table's (`AMPUP_MEMORY_COMPACT*`).
    pub tables: compaction::Config,
    /// How long what a scheduled purge deletes must have been hidden first (`AMPUP_MEMORY_PURGE_GRACE`).
    pub purge_grace: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let span = |k: &str| -> Result<Option<Duration>> {
            var(k)
                .map(|v| match v.trim() {
                    "0" => Ok(Duration::ZERO),
                    t => compaction::duration(t)
                        .with_context(|| format!("{k} is like 4h, 90m or 30s, got {v:?}")),
                })
                .transpose()
        };
        Ok(Config {
            window: Window::from_env()?,
            budget: span("AMPUP_MEMORY_MAINTAIN_BUDGET")?.filter(|d| !d.is_zero()),
            pace: span("AMPUP_MEMORY_MAINTAIN_PACE")?.unwrap_or(Duration::from_secs(2)),
            dream: span("AMPUP_MEMORY_DREAM_BUDGET")?.unwrap_or(Duration::from_secs(600)),
            keep: var("AMPUP_MEMORY_MAINTAIN_KEEP")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(10usize)
                .max(1),
            ledger: compaction::Config::from_env()?,
            aggregates: compaction::Config::aggregates_from_env()?,
            tables: compaction::Config::tables_from_env()?,
            purge_grace: span("AMPUP_MEMORY_PURGE_GRACE")?
                .unwrap_or(Duration::from_secs(7 * 86_400)),
        })
    }

    /// When a night's pass must stop starting work: the window's close, or the budget from when the pass
    /// started, whichever is first.
    fn deadline(&self, night: &Night, started: DateTime<Utc>) -> DateTime<Utc> {
        match self.budget {
            Some(b) => night.closes.min(started + b),
            None => night.closes,
        }
    }

    fn announce(&self, now: DateTime<Utc>) -> String {
        let mut line = format!("maintenance window: {}", self.window.describe());
        if let Some(n) = self.window.open_at(now, self.budget) {
            line.push_str(&format!(
                ", open now until {}",
                n.closes.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ));
        } else if let Some(at) = self.window.next_open(now) {
            line.push_str(&format!(
                ", next opening {}",
                at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ));
        }
        if self.window != Window::Off {
            line.push_str(&format!(
                "; one scope at a time, {}s apart, budget {}, dream step up to {}s a scope; rewrites (table \
                 compaction) run only then",
                self.pace.as_secs_f64(),
                self.budget
                    .map_or("until the window closes".into(), |b| format!("{}s", b.as_secs())),
                self.dream.as_secs()
            ));
        }
        line
    }
}

/// The pre-rewrite dream step: a pass over a scope's memory before the night's rewrite of it, within `budget`
/// ([`run_dream`] bounds it). It is the dream's own gated step (`crate::dream::tick`: releases, and a pass once a
/// day when enough new facts wait), only on the scopes `AMPUP_MEMORY_DREAM_ON` names, so off by default; it answers
/// what it did as counts for the night's line. Whatever it returns, or however long it takes, the rewrite and the
/// sweep run after it ([`run_dream`] catches its failure).
pub async fn dream_step(store: &Store, scope: &Scope, budget: Duration) -> Result<Value> {
    if !crate::dream::internal_scheduler()? {
        return Ok(json!({"result":"external_scheduler"}));
    }
    let on = crate::dream::On::from_env().is_some_and(|on| on.covers(scope));
    let ex = crate::extract::Extractor::dream_from_env()?;
    match (on, ex) {
        (true, Some(ex)) => {
            // No call starts past 85% of the budget, so the calls in flight finish and what the pass did is committed
            // before [`run_dream`] gives up on it. A pass cut off there still keeps the batches it committed before
            // (`AMPUP_MEMORY_DREAM_COMMIT_EVERY`, `AMPUP_MEMORY_DREAM_COMMIT_SECS`), and the next goes on after them.
            let mut gate = crate::dream::Gate::from_env()?;
            gate.budget.until = Some(Instant::now() + budget.mul_f64(0.85));
            Ok(crate::dream::tick(store, scope, &ex, &gate).await?.counts())
        }
        _ => Ok(json!({"result": "none"})),
    }
}

/// [`dream_step`], bounded by `budget` and never failing: what it did, or why it didn't, for the night's line.
async fn run_dream(store: &Store, scope: &Scope, budget: Duration) -> Value {
    let started = Instant::now();
    let mut out = match tokio::time::timeout(budget, dream_step(store, scope, budget)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({"result": "failed", "error": super::writer::redacted(&e)}),
        Err(_) => json!({"result": "timed_out", "budget_ms": budget.as_millis() as u64}),
    };
    if let Some(o) = out.as_object_mut() {
        o.insert(
            "duration_ms".into(),
            json!(started.elapsed().as_millis() as u64),
        );
    }
    out
}

/// How a scope's turn in the window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    /// Every rewrite due ran (or wasn't due), and the sweep: recorded as maintained.
    Done,
    /// The budget ran out before a rewrite: the rest waits for the next night.
    OutOfBudget,
    /// A catalog or store refused for load (429, 503): tried again later.
    Throttled,
    /// Something failed: logged; the next night tries again first.
    Failed,
}

impl Ended {
    fn name(self) -> &'static str {
        match self {
            Ended::Done => "done",
            Ended::OutOfBudget => "skipped_for_budget",
            Ended::Throttled => "throttled",
            Ended::Failed => "failed",
        }
    }
}

/// What a step's failure makes of the turn: throttled for a refusal for load, failed otherwise.
fn failed(ended: &mut Ended, e: &anyhow::Error) -> Value {
    let throttled = crate::is_transient(e);
    if *ended != Ended::Failed {
        *ended = if throttled {
            Ended::Throttled
        } else {
            Ended::Failed
        };
    }
    json!({"result": if throttled { "throttled" } else { "failed" }, "error": super::writer::redacted(e)})
}

/// Where a scope's org asked for the scheduled purge (`Settings::purge`): the org's incarnation, for its audit row.
#[derive(Clone, Copy, Debug)]
struct Purges {
    incarnation: i64,
}

/// A scope's (or, for `None`, the system tables') turn in the night: the dream step, the rewrites due (with the
/// scheduled purge's rows left out where the org asked for it), the sweep.
async fn turn(
    store: &Store,
    scope: Option<&Scope>,
    c: &Config,
    night: &Night,
    deadline: DateTime<Utc>,
    purges: Option<Purges>,
) -> (Ended, Value) {
    let started = Instant::now();
    let mut ended = Ended::Done;
    let mut line = json!({"night": night.name});
    let o = line.as_object_mut().unwrap();
    o.insert(
        "scope".into(),
        json!(scope.map_or("memory_system".into(), |s| s.to_string())),
    );
    let left = || (deadline - Utc::now()).to_std().unwrap_or_default();
    // 1. The dream step: never in the way of what follows.
    if let Some(scope) = scope {
        o.insert(
            "dream".into(),
            run_dream(store, scope, c.dream.min(left())).await,
        );
    }
    // 2. The rewrites, each table past its threshold, each only with budget left. The age triggers are read as of
    // the window's close: a table compacted at this hour last night is due again tonight.
    let as_of = night.closes.timestamp_millis();
    let mut rewrites = serde_json::Map::new();
    let mut indexed_files_rewritten = false;
    // Which tables there are, in one listing: each is then read once for its files, never asked about first.
    let there = match store.tables_in(scope).await {
        Ok(t) => t,
        Err(e) => {
            let v = failed(&mut ended, &e);
            o.insert("tables".into(), v);
            o.insert("result".into(), json!(ended.name()));
            return (ended, line);
        }
    };
    let tables: Vec<&str> = match scope {
        None => &crate::offboard::SYSTEM_TABLES[..],
        Some(_) => &crate::offboard::SCOPE_TABLES[..],
    }
    .iter()
    .copied()
    .filter(|t| there.contains(*t))
    .collect();
    // What the scheduled purge deletes, where the org asked for it: what the scope's sources deleted and what aged
    // past its retention, hidden for longer than the grace. Left out of the same rewrites that compact the tables,
    // after the dream step, which it never waits for beyond its budget.
    let mut erasure = None;
    if let (Some(scope), Some(_)) = (scope, purges) {
        match store.purge_plan(scope, c.purge_grace).await {
            Ok(e) => erasure = e,
            Err(e) => {
                let v = failed(&mut ended, &e);
                o.insert("purge".into(), v);
            }
        }
    }
    let mut purged_episodes = false;
    for &table in &tables {
        if left().is_zero() {
            ended = Ended::OutOfBudget;
            rewrites.insert(table.into(), json!({"result": "skipped_for_budget"}));
            continue;
        }
        let t = compaction::Target { scope, table };
        let config = match (scope, table) {
            (None, crate::offboard::LEDGER_TABLE) => &c.ledger,
            (Some(_), crate::AGGREGATES) => &c.aggregates,
            _ => &c.tables,
        };
        let v = match compaction::run(store, t, config, as_of, erasure.as_mut()).await {
            Ok(v) => summary(&v),
            Err(e) => failed(&mut ended, &e),
        };
        if table == crate::EPISODES && erasure.is_some() && v["result"] == "ok" {
            purged_episodes = true;
        }
        // A rewritten file is new to the search indexes, whose rows of the files it replaced are no longer trusted.
        if scope.is_some()
            && [crate::FACTS, crate::EPISODES].contains(&table)
            && v["result"] == "ok"
        {
            indexed_files_rewritten = true;
        }
        rewrites.insert(table.into(), v);
    }
    o.insert("rewrites".into(), Value::Object(rewrites));
    // What the purge took, in the org's audit as `org purge` records it: counts only, never what went.
    if let (Some(scope), Some(p), Some(e)) = (scope, purges, erasure.as_mut()) {
        if purged_episodes {
            e.counts.episodes = e.episodes();
        }
        let mut v = json!(e.counts);
        let took = e.counts.episodes
            + e.counts.facts
            + e.counts.quotes
            + e.counts.closures
            + e.counts.answers;
        if took > 0 {
            let detail = format!(
                "scheduled purge (deleted at the source or past retention for over {}s): {}",
                c.purge_grace.as_secs(),
                serde_json::to_string(&e.counts).unwrap_or_default()
            );
            if let Err(err) = audit_purge(store, scope, p, detail).await {
                v["audit_error"] = json!(super::writer::redacted(&err));
            }
        }
        if !purged_episodes {
            v["episodes_left_for_next_night"] = json!(e.episodes());
        }
        o.insert("purge".into(), v);
    }
    // Its facts or episodes rewritten, the scope's search indexes cover none of their files: searches would scan
    // them whole (keyword only past a few hundred facts) until the next index update. That runs now, in the
    // window, reusing the vectors the indexes hold. Then the indexes are tidied for the night: what the day's
    // updates left beside each index merged into it.
    if let Some(scope) = scope {
        let (reindexed, tidied) = reindex(store, scope, deadline, indexed_files_rewritten).await;
        if let Some(r) = reindexed {
            o.insert("reindex".into(), r);
        }
        o.insert("index_tidy".into(), tidied);
    }
    // 3. The sweep: every table's snapshots past the newest `keep` expired, and the manifest lists and metadata files
    // only they named deleted. (What an interrupted rewrite left was finished in step 2: a table marked so is
    // planned as a sweep.)
    let sweep = async {
        let trimmed = store.trim_listed(scope, &tables, c.keep).await?;
        let (before, after) = trimmed
            .iter()
            .fold((0, 0), |(b, a), (_, x, y)| (b + x, a + y));
        anyhow::Ok(
            json!({"tables": trimmed.len(), "snapshots_before": before, "snapshots_after": after}),
        )
    };
    let v = match sweep.await {
        Ok(v) => v,
        Err(e) => failed(&mut ended, &e),
    };
    o.insert("sweep".into(), v);
    o.insert(
        "duration_ms".into(),
        json!(started.elapsed().as_millis() as u64),
    );
    o.insert("result".into(), json!(ended.name()));
    (ended, line)
}

/// The scheduled purge's row in the org's audit (`memstore audit`), through the fenced store: as `org purge`'s,
/// with the server as its actor.
async fn audit_purge(store: &Store, scope: &Scope, p: Purges, detail: String) -> Result<()> {
    store.check_fence()?;
    store
        .append_audit(&[crate::audit::Entry {
            at: Utc::now().timestamp_micros(),
            kind: crate::audit::Kind::Change,
            org: scope.org_name().map(str::to_string),
            incarnation: Some(p.incarnation),
            actor: "server".into(),
            scope: Some(scope.to_string()),
            path: None,
            detail: Some(detail),
            status: None,
            count: 1,
            reads: 0,
            writes: 0,
        }])
        .await
}

/// A rewrite's line, as the night's line carries it: its outcome and counts, not its scope again.
fn summary(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("scope");
        o.remove("action");
    }
    v
}

/// Bring `scope`'s fact and passage indexes up to date after its facts or episodes were rewritten (`rewritten`),
/// then tidy them for the night (`search::tidy_night`): every index merged into one, which updates leave to the
/// night, and rows written twice dropped. One index update at a time on a server (two at once undo each other's
/// work): this waits for the refresh's to finish, up to the night's deadline, and otherwise leaves the update to
/// the refresh's next round and the tidy to the next night. Returns the reindex's line (when `rewritten`) and the
/// tidy's.
async fn reindex(
    store: &Store,
    scope: &Scope,
    deadline: DateTime<Utc>,
    rewritten: bool,
) -> (Option<Value>, Value) {
    let started = Instant::now();
    let Some(_turn) = super::Indexing::wait(deadline).await else {
        return (
            rewritten.then(|| json!({"result": "left_to_the_index_refresh"})),
            json!({"result": "left_for_the_next_night"}),
        );
    };
    let turn = match super::writer::Turn::acquire(store, &format!("index:{scope}"), deadline).await
    {
        Ok(turn) => turn,
        Err(e) => {
            return (
                None,
                json!({"result":"left_for_the_next_night", "error":super::writer::redacted(&e)}),
            );
        }
    };
    let indexed = turn.store(store);
    let store = &indexed;
    let reindexed = if rewritten {
        let r = async {
            let (facts, _) = crate::search::update(store, scope).await?;
            let (passages, _, _) = crate::search::passages::update(store, scope).await?;
            anyhow::Ok((facts, passages))
        }
        .await;
        Some(match r {
            Ok((facts, passages)) => {
                json!({"result": "ok", "fact_rows": facts, "passages": passages,
                                           "duration_ms": started.elapsed().as_millis() as u64})
            }
            Err(e) => json!({"result": "failed", "error": super::writer::redacted(&e),
                             "duration_ms": started.elapsed().as_millis() as u64}),
        })
    } else {
        None
    };
    let started = Instant::now();
    if reindexed.as_ref().is_some_and(|r| r["result"] != "ok") {
        turn.finish(store).await;
        return (reindexed, json!({"result":"left_for_the_next_night"}));
    }
    let r = async {
        let facts = crate::search::tidy_night(store, scope).await?;
        let passages = crate::search::passages::tidy_night(store, scope).await?;
        anyhow::Ok((facts, passages))
    }
    .await;
    let counts = |(merged, compacted, duplicates): (usize, usize, usize)| json!({"merged": merged, "compacted": compacted, "duplicates_dropped": duplicates});
    let tidied = match r {
        Ok((facts, passages)) => json!({"result": "ok", "facts": counts(facts),
                                       "passages": counts(passages),
                                       "duration_ms": started.elapsed().as_millis() as u64}),
        Err(e) => json!({"result": "failed", "error": super::writer::redacted(&e),
                         "duration_ms": started.elapsed().as_millis() as u64}),
    };
    turn.finish(store).await;
    (reindexed, tidied)
}

/// Start the window's loop: every `tick` (the maintenance round), on the writer lease's holder, a night's pass
/// once the window is open and tonight's isn't finished. As background writing, stopped at shutdown before the
/// lease is released.
pub(super) fn start(st: &Arc<State>, c: Config, tick: Duration) {
    eprintln!("{}", c.announce(Utc::now()));
    if c.window == Window::Off {
        return;
    }
    let st2 = st.clone();
    st.background(async move {
        let st = st2;
        let mut walk = ScopeWalk::default();
        // When the window last finished with each scope, as far as this server knows (read from the scope's
        // episodes table the first time).
        let mut maintained: HashMap<Scope, i64> = HashMap::new();
        let mut finished: Option<String> = None;
        loop {
            tokio::time::sleep(tick).await;
            if !st.writer.holds() {
                continue;
            }
            let now = Utc::now();
            let Some(night) = c.window.open_at(now, c.budget) else {
                continue;
            };
            if finished.as_ref() == Some(&night.name) {
                continue;
            }
            if pass(&st, &c, &night, &mut walk, &mut maintained).await {
                finished = Some(night.name);
            }
        }
    });
}

/// One night's pass. True once it is over for the night: every scope had its turn or was skipped for the
/// budget. False if the lease was lost (a later round of the same night goes on where it stopped).
async fn pass(
    st: &Arc<State>,
    c: &Config,
    night: &Night,
    walk: &mut ScopeWalk,
    maintained: &mut HashMap<Scope, i64>,
) -> bool {
    let started = Utc::now();
    let clock = Instant::now();
    let deadline = c.deadline(night, started);
    let store = st.writing_store();
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut line = |ended: Ended, l: &Value| {
        *counts.entry(ended.name()).or_default() += 1;
        eprintln!("maintenance: {l}");
    };
    // The system tables first, once a night, by the default cell.
    let mut first = true;
    if cell::is_default() {
        let (ended, l) = turn(&store, None, c, night, deadline, None).await;
        line(ended, &l);
        first = false;
    }
    // This cell's scopes, least recently maintained first; those finished earlier tonight (before a restart, or a
    // lost lease) are not done again.
    let scopes = walk.next(st, &store).await;
    // Which orgs asked for the scheduled purge: none tonight if the directory can't be read.
    let dir = match crate::people::directory::Directory::load(&store).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!(
                "maintenance night: the directory is unreadable, so no scheduled purge runs tonight: {}",
                super::writer::redacted(&e)
            );
            None
        }
    };
    let purges = |scope: &Scope| -> Option<Purges> {
        let dir = dir.as_ref()?;
        let org = dir.orgs.get(scope.org_name()?)?;
        dir.purges(scope).then_some(Purges {
            incarnation: org.created_at,
        })
    };
    let mut queue = vec![];
    for scope in scopes {
        let at = match maintained.get(&scope) {
            Some(at) => *at,
            // Read once per server, spaced out: a first night's reads of every scope must not be a burst either.
            None => match tokio::time::sleep(MARKER_PACE)
                .then(|_| store.maintained_at(&scope))
                .await
            {
                Ok(at) => {
                    let at = at.unwrap_or(0);
                    maintained.insert(scope.clone(), at);
                    at
                }
                // Unknown: taken as never, so it isn't left behind.
                Err(_) => 0,
            },
        };
        if at < night.opened.timestamp_millis() {
            queue.push((at, scope));
        }
    }
    queue.sort();
    let mut queue: VecDeque<(Scope, u32)> = queue.into_iter().map(|(_, s)| (s, 0)).collect();
    let mut pause = c.pace;
    while let Some((scope, tries)) = queue.pop_front() {
        // A pause between two turns, so a night's reads and commits never come as a burst.
        if !first && !pause.is_zero() && Utc::now() < deadline {
            tokio::time::sleep(pause).await;
        }
        first = false;
        if !st.writer.holds() {
            eprintln!(
                "maintenance night: {}",
                json!({"night": night.name, "result": "lease_lost", "scopes_left": queue.len() + 1,
                       "duration_ms": clock.elapsed().as_millis() as u64})
            );
            return false;
        }
        if Utc::now() >= deadline {
            let l = json!({"night": night.name, "scope": scope.to_string(), "result": "skipped_for_budget"});
            line(Ended::OutOfBudget, &l);
            continue;
        }
        let (ended, mut l) = crate::log_org::with(
            scope.org_name(),
            turn(&store, Some(&scope), c, night, deadline, purges(&scope)),
        )
        .await;
        match ended {
            Ended::Done => {
                let at = Utc::now().timestamp_millis();
                if let Err(e) = store.set_maintained_at(&scope, at).await {
                    l["mark_error"] = json!(super::writer::redacted(&e));
                }
                maintained.insert(scope.clone(), at);
                pause = c.pace;
            }
            // Backed off: the pause doubles, and the scope goes to the back of the queue.
            Ended::Throttled if tries + 1 < THROTTLE_TRIES => {
                pause = (pause.max(Duration::from_secs(1)) * 2).min(Duration::from_secs(300));
                l["retry_in_ms"] = json!(pause.as_millis() as u64);
                queue.push_back((scope.clone(), tries + 1));
            }
            _ => {}
        }
        line(ended, &l);
    }
    let mut summary = json!({"night": night.name, "result": "over",
                             "duration_ms": clock.elapsed().as_millis() as u64,
                             "deadline": deadline.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
    for (k, n) in counts {
        summary[k] = json!(n);
    }
    eprintln!("maintenance night: {summary}");
    true
}
