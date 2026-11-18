//! What each process counted, for the one that sends telemetry.
//!
//! Only the process holding the deployment's credential sends telemetry, but every process counts: a
//! worker's extraction tokens and commits, a cell's searches and ingests, each one's requests. Each writes
//! what it counted to `<memory_system>/_reports/<holder>.json` (beside the writer lease, reached the same
//! way) every minute (`AMPUP_MEMORY_USAGE_PERSIST_SECS`): its usage book and its alert readings. The sender adds
//! them up: usage from every report, a stopped process's too (its hours are still the deployment's), and
//! readings from those written in the last few minutes. A report is deleted once its newest hour is past
//! [`crate::usage::KEEP_DAYS`].
//!
//! Beside them, `_delivered.json`: each hour's usage as the control plane last took it, so the sender sends
//! an hour again only when it changed (a report written after the hour closed, a restart).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use super::State;
use crate::usage::{Bucket, Usage};

/// The directory beside the writer lease's.
const DIR: &str = "_reports";
const DELIVERED: &str = "_delivered.json";
/// A report written longer ago than this is a stopped process's: its usage counts, its readings don't.
const FRESH: TimeDelta = TimeDelta::minutes(3);
/// How often the place is found again (its credentials taken again; sooner where they run out sooner, as the
/// lease's do), and stopped processes' reports read again (they are kept otherwise: a stopped process never
/// writes again).
const AGAIN: Duration = Duration::from_secs(300);

/// One process's counts as it last wrote them.
#[derive(Serialize, Deserialize)]
pub struct Report {
    pub holder: String,
    pub cell: String,
    pub role: String,
    pub written_at: DateTime<Utc>,
    /// Totals since the process started.
    pub readings: crate::alerts::Readings,
    /// Its hours in the last [`crate::usage::KEEP_DAYS`].
    pub usage: Vec<(DateTime<Utc>, Bucket)>,
    /// The model tokens it spent for each org, this month and last ([`crate::limits`]): an org's month is every
    /// process's added up.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub org_tokens: crate::limits::Book,
}

impl Report {
    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        now - self.written_at < FRESH
    }
}

/// This process's name among the reports: its host's and a random one, never another's.
pub fn holder() -> &'static str {
    static HOLDER: LazyLock<String> = LazyLock::new(|| {
        let host: String = std::env::var("HOSTNAME")
            .unwrap_or_else(|_| "local".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        format!(
            "{host}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        )
    });
    &HOLDER
}

struct Place {
    op: opendal::Operator,
    /// When the place is found again: after [`AGAIN`], or before its credentials run out.
    until: Instant,
}

static PLACE: LazyLock<tokio::sync::Mutex<Option<Place>>> = LazyLock::new(Default::default);

/// The reports' place, found again every [`AGAIN`] and after a failure ([`forget`]); none when the catalog
/// can't keep it.
async fn place(state: &State) -> Result<Option<opendal::Operator>> {
    let store = state.store.read().unwrap().clone();
    place_in(&store).await
}

/// [`place`], for `store`'s catalog.
async fn place_in(store: &crate::Store) -> Result<Option<opendal::Operator>> {
    let mut p = PLACE.lock().await;
    if let Some(p) = p.as_ref().filter(|p| Instant::now() < p.until) {
        return Ok(Some(p.op.clone()));
    }
    let found = tokio::time::timeout(Duration::from_secs(30), super::writer::beside(store, DIR))
        .await
        .context("finding the reports' place took over 30s")??;
    // No place: no process reads another's counts, so each holds an org to its month's allowance by its own spend
    // alone, and N processes let it spend up to N times it. Said while any org has limits, once an hour.
    if found.is_none() && crate::limits::any() {
        static SAID: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
        let mut said = SAID.lock().unwrap();
        if said.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
            *said = Some(Instant::now());
            eprintln!(
                "ALERT reports: this catalog keeps no reports, so orgs' model tokens are counted per process: each process holds an org to its allowance by its own spend alone (with several, the org may spend that many times it)"
            );
        }
    }
    *p = found.clone().map(|(op, until)| Place {
        op,
        until: until.min(Instant::now() + AGAIN),
    });
    Ok(found.map(|(op, _)| op))
}

/// Forget the place after a failure: it is found again (fresh credentials) next time.
pub async fn forget() {
    *PLACE.lock().await = None;
}

/// Write this process's report every `every`, for as long as it runs, and read the others' model tokens per org
/// ([`crate::limits`]) as often: at once too, so a process just started counts what its predecessors spent.
pub async fn run(state: Arc<State>, every: Duration) {
    let dirs = crate::alerts::written_dirs(None);
    let mut failing = false;
    let mut first = true;
    let mut others_failing = false;
    loop {
        if !std::mem::take(&mut first) {
            tokio::time::sleep(every).await;
        }
        let store = state.store.read().unwrap().clone();
        match others_in(&store).await {
            Ok(rs) => {
                crate::limits::set_others(rs.iter().map(|r| r.org_tokens.clone()));
                others_failing = false;
            }
            Err(e) => {
                if !std::mem::replace(&mut others_failing, true) {
                    eprintln!(
                        "reports: other processes' model tokens not read (kept as last read; retrying): {}",
                        super::writer::redacted(&e)
                    );
                }
            }
        }
        let report = Report {
            holder: holder().to_string(),
            cell: super::cell::me().to_string(),
            role: format!("{:?}", state.role).to_ascii_lowercase(),
            written_at: Utc::now(),
            readings: super::alert_readings(&state, &dirs),
            usage: crate::usage::METER.book(Utc::now()),
            org_tokens: crate::limits::book(),
        };
        match write(&store, &report).await {
            Ok(()) => {
                if failing {
                    eprintln!("reports: written again");
                }
                failing = false;
            }
            Err(e) => {
                if !failing {
                    eprintln!(
                        "reports: this process's counts not written (retrying): {}",
                        super::writer::redacted(&e)
                    );
                }
                failing = true;
                forget().await;
            }
        }
    }
}

/// Write `report`; a store that refused it is asked once more with the place found again (fresh credentials).
async fn write(store: &crate::Store, report: &Report) -> Result<()> {
    match write_once(store, report).await {
        Err(e) if super::writer::answered(&e) => {
            forget().await;
            write_once(store, report).await
        }
        r => r,
    }
}

async fn write_once(store: &crate::Store, report: &Report) -> Result<()> {
    let Some(op) = place_in(store).await? else {
        return Ok(());
    };
    let body = serde_json::to_vec(report)?;
    op.write(&format!("{}.json", report.holder), body)
        .await
        .context("write the report")?;
    Ok(())
}

/// Every other process's report, as last written. Stopped processes' reports are read once and kept (and
/// read again every [`AGAIN`]); one whose newest hour is past [`crate::usage::KEEP_DAYS`] is deleted. A store
/// that refused a read is asked once more with the place found again (fresh credentials).
pub async fn others(state: &State) -> Result<Vec<Arc<Report>>> {
    let store = state.store.read().unwrap().clone();
    others_in(&store).await
}

/// [`others`], for `store`'s catalog.
async fn others_in(store: &crate::Store) -> Result<Vec<Arc<Report>>> {
    match others_once(store).await {
        Err(e) if super::writer::answered(&e) => {
            forget().await;
            others_once(store).await
        }
        r => r,
    }
}

async fn others_once(store: &crate::Store) -> Result<Vec<Arc<Report>>> {
    /// Stopped processes' reports by name, and since when they are kept.
    type Kept = (Instant, HashMap<String, Arc<Report>>);
    static KEPT: LazyLock<tokio::sync::Mutex<Kept>> =
        LazyLock::new(|| tokio::sync::Mutex::new((Instant::now(), HashMap::new())));
    let Some(op) = place_in(store).await? else {
        return Ok(vec![]);
    };
    let mut kept = KEPT.lock().await;
    if kept.0.elapsed() >= AGAIN {
        *kept = (Instant::now(), HashMap::new());
    }
    let now = Utc::now();
    let mine = format!("{}.json", holder());
    let mut out = vec![];
    let mut listed = std::collections::HashSet::new();
    for entry in op.list("/").await.context("list the reports")? {
        let name = entry.name().to_string();
        if !name.ends_with(".json") || name.starts_with('_') || name == mine {
            continue;
        }
        listed.insert(name.clone());
        if let Some(r) = kept.1.get(&name) {
            out.push(r.clone());
            continue;
        }
        let body = match op.read(&name).await {
            Ok(b) => b.to_vec(),
            // Deleted since it was listed.
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow::Error::from(e).context(format!("read report {name}"))),
        };
        let r: Report = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("reports: {name} doesn't read, left out: {e}");
                continue;
            }
        };
        if now - r.written_at > TimeDelta::days(crate::usage::KEEP_DAYS) + TimeDelta::hours(1) {
            if let Err(e) = op.delete(&name).await {
                eprintln!(
                    "reports: {name} past keeping, not deleted: {}",
                    super::writer::redacted(&anyhow::Error::from(e))
                );
            }
            continue;
        }
        let r = Arc::new(r);
        if !r.fresh(now) {
            kept.1.insert(name, r.clone());
        }
        out.push(r);
    }
    kept.1.retain(|name, _| listed.contains(name));
    Ok(out)
}

/// Each hour's usage as the control plane last took it.
pub async fn delivered(state: &State) -> Result<BTreeMap<DateTime<Utc>, Usage>> {
    let Some(op) = place(state).await? else {
        return Ok(BTreeMap::new());
    };
    match op.read(DELIVERED).await {
        Ok(b) => {
            let sent: Vec<Usage> =
                serde_json::from_slice(&b.to_vec()).with_context(|| format!("read {DELIVERED}"))?;
            Ok(sent.into_iter().map(|u| (u.hour, u)).collect())
        }
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("read {DELIVERED}"))),
    }
}

/// Keep what the control plane took.
pub async fn keep_delivered(state: &State, sent: &BTreeMap<DateTime<Utc>, Usage>) -> Result<()> {
    let Some(op) = place(state).await? else {
        return Ok(());
    };
    let body = serde_json::to_vec(&sent.values().collect::<Vec<_>>())?;
    op.write(DELIVERED, body)
        .await
        .with_context(|| format!("write {DELIVERED}"))?;
    Ok(())
}

/// Run `work`, in a process that isn't a server (a dream job's worker), for `org`: its model calls count against
/// the org's month with every server's and are held to the org's limits as a server's are ([`crate::limits`]).
/// Before it, the org's limits are folded from the directory and every process's month read from their reports;
/// while it runs, this process's own counts are written beside theirs (as `role`) every `every`, and the others'
/// read again; and once more when it ends, however it ends. Nothing is written while it spent nothing.
pub async fn standalone<T>(
    store: &crate::Store,
    org: Option<&str>,
    role: &str,
    every: Duration,
    work: impl std::future::Future<Output = T>,
) -> T {
    if let Err(e) = store.directory().await {
        eprintln!(
            "limits: the directory didn't read, so no org's limits are known here: {}",
            super::writer::redacted(&e)
        );
    }
    let round = || async {
        match others_in(store).await {
            Ok(rs) => crate::limits::set_others(rs.iter().map(|r| r.org_tokens.clone())),
            Err(e) => eprintln!(
                "reports: other processes' model tokens not read (kept as last read): {}",
                super::writer::redacted(&e)
            ),
        }
        let org_tokens = crate::limits::book();
        if org_tokens.is_empty() {
            return;
        }
        let report = Report {
            holder: holder().to_string(),
            cell: super::cell::me().to_string(),
            role: role.to_string(),
            written_at: Utc::now(),
            readings: Default::default(),
            usage: vec![],
            org_tokens,
        };
        if let Err(e) = write(store, &report).await {
            eprintln!(
                "reports: this process's model tokens not written: {}",
                super::writer::redacted(&e)
            );
            forget().await;
        }
    };
    round().await;
    // The reports are written beside the work, not in a branch that would stop polling it (it may hold a catalog
    // lock a write waits for).
    let reporting = async {
        loop {
            tokio::time::sleep(every).await;
            round().await;
        }
    };
    let out = tokio::select! {
        out = crate::log_org::with(org, work) => out,
        _ = reporting => unreachable!("reporting runs until the work ends"),
    };
    round().await;
    out
}
