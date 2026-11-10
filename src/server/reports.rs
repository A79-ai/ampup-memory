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
    let mut p = PLACE.lock().await;
    if let Some(p) = p.as_ref().filter(|p| Instant::now() < p.until) {
        return Ok(Some(p.op.clone()));
    }
    let store = state.store.read().unwrap().clone();
    let found = tokio::time::timeout(Duration::from_secs(30), super::writer::beside(&store, DIR))
        .await
        .context("finding the reports' place took over 30s")??;
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

/// Write this process's report every `every`, for as long as it runs.
pub async fn run(state: Arc<State>, every: Duration) {
    let dirs = crate::alerts::written_dirs(None);
    let mut failing = false;
    loop {
        tokio::time::sleep(every).await;
        let report = Report {
            holder: holder().to_string(),
            cell: super::cell::me().to_string(),
            role: format!("{:?}", state.role).to_ascii_lowercase(),
            written_at: Utc::now(),
            readings: super::alert_readings(&state, &dirs),
            usage: crate::usage::METER.book(Utc::now()),
        };
        match write(&state, &report).await {
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
async fn write(state: &State, report: &Report) -> Result<()> {
    match write_once(state, report).await {
        Err(e) if super::writer::answered(&e) => {
            forget().await;
            write_once(state, report).await
        }
        r => r,
    }
}

async fn write_once(state: &State, report: &Report) -> Result<()> {
    let Some(op) = place(state).await? else {
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
    match others_once(state).await {
        Err(e) if super::writer::answered(&e) => {
            forget().await;
            others_once(state).await
        }
        r => r,
    }
}

async fn others_once(state: &State) -> Result<Vec<Arc<Report>>> {
    /// Stopped processes' reports by name, and since when they are kept.
    type Kept = (Instant, HashMap<String, Arc<Report>>);
    static KEPT: LazyLock<tokio::sync::Mutex<Kept>> =
        LazyLock::new(|| tokio::sync::Mutex::new((Instant::now(), HashMap::new())));
    let Some(op) = place(state).await? else {
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
