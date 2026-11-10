//! Alerts the engine raises on its own metrics: the telemetry contract's `alerts` channel.
//!
//! A server with telemetry on (an enrolled credential, `crate::telemetry::Outbox`) evaluates a fixed set
//! of rules every `AMPUP_MEMORY_ALERTS_EVAL_SECS` (60 by default, 10 at least). The rules read the engine's
//! own metrics and state (request counts and durations, the disks it writes, the directory, the
//! extraction round, store commits), never data. What leaves is which alert, how bad, since when and how
//! many evaluations it has held for: an [`AlertEvent`].
//!
//! | alert | condition | warning | critical |
//! |---|---|---|---|
//! | `http_5xx_rate` | share of 5xx answers over the last 5 min, with at least 20 requests (the probes' left out) | above 5% | above 20% |
//! | `p99_latency` | p99 duration of reads (every route but [`UNTIMED_ROUTES`] and the probes') over the last 5 min, with at least 100 of them | above 2 s | above 5 s |
//! | `disk_usage` | the fullest of the disks the server writes (warehouse, index, telemetry, temp) | above 85% | above 95% |
//! | `directory_blocked` | the directory log holds a change newer than this engine reads | | while it holds |
//! | `extraction_backlog` | episodes the extraction round finds waiting, above 100 for 15 min | while it holds | |
//! | `store_commit_failures` | store commits that failed over the last 10 min | 1 or more | more than 5 |
//! | `advice_slow` | advice routes that took longer than an agent's `get_advice` waits (`AMPUP_MEMORY_ADVICE_ASK_WAIT_MS`, 30 s) over the last 10 min | 1 or more | more than 5 |
//! | `key_expiry` | an API key used in the last 7 days expires within 3 days (one issued for 3 days or less is left out: it lapses as it was made to); requests refused for an expired API key over the last 10 min | a key expiring | a key refused |
//!
//! An alert fires once its condition held on [`FIRE_AFTER`] evaluations in a row, and resolves once it was
//! clear on [`RESOLVE_AFTER`] in a row. It is sent when it fires, on each change of severity, every
//! [`RESEND_EVERY`] evaluations while it fires (its count grown), and once when it resolves. Each alert's
//! state is also on `/metrics` as `memstore_alert_firing{name,severity}`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use chrono::{DateTime, SubsecRound, TimeDelta, Utc};

use crate::telemetry::{AlertEvent, AlertName, AlertState, Severity};

/// Evaluations in a row a condition must hold before its alert fires.
pub const FIRE_AFTER: u32 = 2;
/// Evaluations in a row a condition must be clear before its alert resolves.
pub const RESOLVE_AFTER: u32 = 2;
/// A firing alert is sent again after this many more evaluations held.
pub const RESEND_EVERY: u64 = 10;
/// Seconds between evaluations by default (`AMPUP_MEMORY_ALERTS_EVAL_SECS`), and the least it may be set to.
pub const EVAL_SECS: u64 = 60;
pub const MIN_EVAL_SECS: u64 = 10;

/// `http_5xx_rate` and `p99_latency` look back this far; `http_5xx_rate` needs this many requests in it.
pub const HTTP_WINDOW: TimeDelta = TimeDelta::minutes(5);
pub const HTTP_MIN_REQUESTS: u64 = 20;
/// Timed reads `p99_latency` needs in its window: under 100, the p99 is the slowest one, and a single slow
/// call would page.
pub const P99_MIN_REQUESTS: u64 = 100;
/// Routes `p99_latency` doesn't time: writes, whose time is the object store's commit (seconds, more when a
/// bulk ingest queues them), the operator's console and the server's own plumbing. A slow write shows as
/// `extraction_backlog` or `store_commit_failures`, not as a slow read.
pub const UNTIMED_ROUTES: [&str; 10] = [
    "/ingest",
    "/admin",
    "/acl",
    "/retract",
    "/team",
    "/closure",
    "/acl_roles",
    "/metrics",
    "/telemetry",
    "/log_answer",
];
/// Shares of 5xx answers above which `http_5xx_rate` is a warning, and critical.
pub const HTTP_5XX_WARNING: f64 = 0.05;
pub const HTTP_5XX_CRITICAL: f64 = 0.20;
/// p99 request durations (seconds) above which `p99_latency` is a warning, and critical.
pub const P99_WARNING_SECS: f64 = 2.0;
pub const P99_CRITICAL_SECS: f64 = 5.0;
/// Shares of a disk used above which `disk_usage` is a warning, and critical.
pub const DISK_WARNING: f64 = 0.85;
pub const DISK_CRITICAL: f64 = 0.95;
/// `extraction_backlog`: more episodes than this waiting, for this long.
pub const EXTRACTION_BACKLOG_MAX: u64 = 100;
pub const EXTRACTION_BACKLOG_FOR: TimeDelta = TimeDelta::minutes(15);
/// `store_commit_failures` looks back this far; more failures than this in it is critical.
pub const COMMIT_WINDOW: TimeDelta = TimeDelta::minutes(10);
pub const COMMIT_FAILURES_CRITICAL: u64 = 5;
/// `advice_slow` looks back this far; more slow routes than this in it is critical.
pub const SLOW_ADVICE_WINDOW: TimeDelta = TimeDelta::minutes(10);
pub const SLOW_ADVICE_CRITICAL: u64 = 5;
/// `key_expiry`: a key used within [`KEY_USED_WITHIN`] that expires within [`KEY_EXPIRES_WITHIN`] is a
/// warning (a key nobody uses can lapse quietly), unless it was issued for [`KEY_EXPIRES_WITHIN`] or less:
/// such a key is inside the window from its first use, so the warning would hold its whole life and say
/// nothing its maker didn't choose. A request refused for an expired key within [`KEY_REFUSAL_WINDOW`] is
/// critical (a service still sending it fails every call), short-lived or not.
pub const KEY_USED_WITHIN: TimeDelta = TimeDelta::days(7);
pub const KEY_EXPIRES_WITHIN: TimeDelta = TimeDelta::days(3);
pub const KEY_REFUSAL_WINDOW: TimeDelta = TimeDelta::minutes(10);

/// What the rules read, at one evaluation. Counters are totals since the server started; the rules take
/// what they gained over their window.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Readings {
    /// Requests answered, but the probes' ([`crate::audit::PROBES`]): a warming server's readiness probe is
    /// answered 503 every few seconds, which no caller sees, and counted here it made each deploy's warm-ups
    /// a 5xx rate (and padded `p99_latency`'s minimum with answers that aren't reads).
    pub requests: u64,
    /// Of those, answered with a 5xx.
    pub requests_5xx: u64,
    /// The request-duration histogram over every route but [`UNTIMED_ROUTES`] and the probes': (upper bound
    /// in seconds, requests at or under it), bounds ascending, the last one infinite.
    #[serde(with = "bounds")]
    pub latency: Vec<(f64, u64)>,
    /// The fullest disk the server writes, as the share used (0 to 1); none when none could be read.
    pub disk_used: Option<f64>,
    /// The directory log holds a change newer than this engine reads.
    pub directory_blocked: bool,
    /// Episodes the last extraction round found waiting.
    pub extraction_backlog: u64,
    /// Store commits that failed.
    pub commit_failures: u64,
    /// Requests refused because their API key had expired.
    pub expired_key_refusals: u64,
    /// Advice routes slower than an agent's `get_advice` waits.
    pub slow_advice_routes: u64,
    /// API keys used within [`KEY_USED_WITHIN`] that expire within [`KEY_EXPIRES_WITHIN`] ([`keys_expiring`]).
    pub keys_expiring: u64,
}

/// A histogram as JSON holds it: JSON has no infinity, so the last bound is written as `f64::MAX`.
mod bounds {
    pub fn serialize<S: serde::Serializer>(v: &[(f64, u64)], s: S) -> Result<S::Ok, S::Error> {
        use serde::Serialize;
        let finite = |le: f64| if le.is_finite() { le } else { f64::MAX };
        v.iter()
            .map(|&(le, n)| (finite(le), n))
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<(f64, u64)>, D::Error> {
        use serde::Deserialize;
        let le = |le: f64| if le == f64::MAX { f64::INFINITY } else { le };
        Ok(Vec::<(f64, u64)>::deserialize(d)?
            .into_iter()
            .map(|(b, n)| (le(b), n))
            .collect())
    }
}

/// The deployment's readings, from each of its processes' own (`crate::server` reports: a server, a worker,
/// a cell, each counting what it did). A counter grows by what each process gained since it was last
/// seen; a process seen for the first time adds nothing, so neither a restarted sender nor a process just
/// heard from passes off a lifetime's totals as the last few minutes'. The disk is the fullest, the
/// directory blocked if any process finds it so, the backlog every cell's.
#[derive(Default)]
pub struct Combined {
    total: Readings,
    seen: BTreeMap<String, Readings>,
}

impl Combined {
    /// The readings now, from the processes reporting now (by name); one missing is forgotten.
    pub fn step<'a>(&mut self, now: impl IntoIterator<Item = (&'a str, &'a Readings)>) -> Readings {
        let t = &mut self.total;
        let mut seen = BTreeMap::new();
        let (mut disk, mut blocked, mut backlog, mut keys) = (None::<f64>, false, 0, 0);
        for (who, r) in now {
            if let Some(was) = self.seen.get(who) {
                t.requests += r.requests.saturating_sub(was.requests);
                t.requests_5xx += r.requests_5xx.saturating_sub(was.requests_5xx);
                t.commit_failures += r.commit_failures.saturating_sub(was.commit_failures);
                t.expired_key_refusals += r
                    .expired_key_refusals
                    .saturating_sub(was.expired_key_refusals);
                t.slow_advice_routes += r.slow_advice_routes.saturating_sub(was.slow_advice_routes);
                if t.latency.is_empty() {
                    t.latency = r.latency.iter().map(|&(le, _)| (le, 0)).collect();
                }
                let bounds = |h: &[(f64, u64)]| h.iter().map(|b| b.0).collect::<Vec<_>>();
                // Only histograms with the same bounds add up (an engine of another version may differ).
                if bounds(&t.latency) == bounds(&r.latency)
                    && bounds(&was.latency) == bounds(&r.latency)
                {
                    for (i, b) in t.latency.iter_mut().enumerate() {
                        b.1 += r.latency[i].1.saturating_sub(was.latency[i].1);
                    }
                }
            }
            disk = match (disk, r.disk_used) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            blocked |= r.directory_blocked;
            backlog += r.extraction_backlog;
            keys = keys.max(r.keys_expiring);
            seen.insert(who.to_string(), r.clone());
        }
        self.seen = seen;
        Readings {
            disk_used: disk,
            directory_blocked: blocked,
            extraction_backlog: backlog,
            keys_expiring: keys,
            ..t.clone()
        }
    }
}

impl Readings {
    /// The request counts from a server's metrics, over every route but the probes', and the duration
    /// histogram, over its reads.
    pub fn from_metrics(&mut self, m: &crate::metrics::Metrics) {
        use prometheus::core::Collector;
        let probe = |labels: &[prometheus::proto::LabelPair]| {
            labels
                .iter()
                .any(|l| l.name() == "route" && crate::audit::PROBES.contains(&l.value()))
        };
        let (mut all, mut bad) = (0, 0);
        for f in m.requests.collect() {
            for c in f.get_metric() {
                if probe(c.get_label()) {
                    continue;
                }
                let n = c.get_counter().get_value() as u64;
                all += n;
                if c.get_label()
                    .iter()
                    .any(|l| l.name() == "status" && l.value().starts_with('5'))
                {
                    bad += n;
                }
            }
        }
        let mut buckets: Vec<(f64, u64)> = vec![];
        let mut total = 0;
        for f in m.request_seconds.collect() {
            for h in f.get_metric() {
                if probe(h.get_label())
                    || h.get_label()
                        .iter()
                        .any(|l| l.name() == "route" && UNTIMED_ROUTES.contains(&l.value()))
                {
                    continue;
                }
                let h = h.get_histogram();
                total += h.get_sample_count();
                for (i, b) in h.get_bucket().iter().enumerate() {
                    match buckets.get_mut(i) {
                        Some(x) => x.1 += b.cumulative_count(),
                        None => buckets.push((b.upper_bound(), b.cumulative_count())),
                    }
                }
            }
        }
        buckets.push((f64::INFINITY, total));
        self.requests = all;
        self.requests_5xx = bad;
        self.latency = buckets;
    }
}

/// How many keys, each (created in µs since the epoch, expires_at in seconds since the epoch, last used in
/// µs since the epoch), were used within [`KEY_USED_WITHIN`] of `now` and expire within
/// [`KEY_EXPIRES_WITHIN`] of it, not yet expired, and were issued for longer than [`KEY_EXPIRES_WITHIN`].
pub fn keys_expiring(
    keys: impl IntoIterator<Item = (i64, Option<i64>, Option<i64>)>,
    now: DateTime<Utc>,
) -> u64 {
    let soon = (now + KEY_EXPIRES_WITHIN).timestamp();
    let used_since = (now - KEY_USED_WITHIN).timestamp_micros();
    keys.into_iter()
        .filter(|(created, expires, used)| {
            expires.is_some_and(|t| {
                t > now.timestamp()
                    && t <= soon
                    && t - created / 1_000_000 > KEY_EXPIRES_WITHIN.num_seconds()
            }) && used.is_some_and(|u| u >= used_since)
        })
        .count() as u64
}

/// The share used of the fullest disk holding one of `dirs` (each read where it is, or at its nearest
/// existing parent), as `df` counts it: used over used plus what is available to the server.
pub fn disk_used(dirs: &[PathBuf]) -> Option<f64> {
    dirs.iter()
        .filter_map(|d| d.ancestors().find(|a| a.exists()).and_then(statvfs_used))
        .reduce(f64::max)
}

fn statvfs_used(dir: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string and `s` a statvfs the call fills.
    if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let used = (s.f_blocks as f64) - (s.f_bfree as f64);
    let room = used + s.f_bavail as f64;
    (room > 0.0).then(|| used / room)
}

/// The dirs a server writes, for `disk_usage`: a local warehouse, a local index, the telemetry mirror's
/// and the temp dir.
pub fn written_dirs(mirror: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![std::env::temp_dir()];
    let local = |uri: String| -> Option<PathBuf> {
        match uri.strip_prefix("file://") {
            Some(p) => Some(p.into()),
            None if !uri.contains("://") => Some(uri.into()),
            None => None,
        }
    };
    if let Some(w) = std::env::var("AMPUP_MEMORY_WAREHOUSE").ok().and_then(local) {
        dirs.push(w);
    }
    if let Some(i) = local(crate::search::default_index_base()) {
        dirs.push(i);
    }
    if let Some(m) = mirror.and_then(Path::parent) {
        dirs.push(m.to_path_buf());
    }
    dirs
}

/// The `q` quantile of a cumulative histogram, interpolated within its bucket as Prometheus's
/// `histogram_quantile` does; in the last (infinite) bucket, the highest finite bound.
pub fn quantile(q: f64, buckets: &[(f64, u64)]) -> Option<f64> {
    let total = buckets.last()?.1;
    if total == 0 {
        return None;
    }
    let rank = q * total as f64;
    let (mut below, mut below_n) = (0.0, 0u64);
    for &(le, n) in buckets {
        if n as f64 >= rank {
            if le.is_infinite() {
                return Some(below);
            }
            let inside = n.saturating_sub(below_n).max(1) as f64;
            return Some(below + (le - below) * (rank - below_n as f64) / inside);
        }
        (below, below_n) = (le, n);
    }
    Some(below)
}

/// One alert's evaluation state.
#[derive(Debug, Default)]
struct Track {
    /// Evaluations in a row its condition held while not firing, and when the first of them was.
    held: u32,
    held_since: Option<DateTime<Utc>>,
    firing: Option<Firing>,
}

#[derive(Debug)]
struct Firing {
    started_at: DateTime<Utc>,
    severity: Severity,
    count: u64,
    /// `count` when it was last sent.
    sent: u64,
    /// Evaluations in a row it was clear, and when the first of them was.
    clear: u32,
    clear_since: Option<DateTime<Utc>>,
}

/// Evaluates the rules, one [`Evaluator::step`] per evaluation, and says what to send.
#[derive(Debug, Default)]
pub struct Evaluator {
    /// Past readings, to take counters' gains over a window: as far back as the longest window.
    history: VecDeque<(DateTime<Utc>, Readings)>,
    /// Since when the extraction backlog has been above [`EXTRACTION_BACKLOG_MAX`].
    backlog_since: Option<DateTime<Utc>>,
    tracks: BTreeMap<AlertName, Track>,
}

impl Evaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The readings `window` before `now`: the newest kept at or before then; none (the server's start,
    /// whose counters were all 0) while the server is younger than the window.
    fn before(&self, now: DateTime<Utc>, window: TimeDelta) -> Option<&Readings> {
        self.history
            .iter()
            .rev()
            .find(|(t, _)| *t <= now - window)
            .map(|(_, r)| r)
    }

    /// Each rule's severity on these readings: none where its condition doesn't hold.
    pub fn conditions(
        &mut self,
        now: DateTime<Utc>,
        r: &Readings,
    ) -> BTreeMap<AlertName, Option<Severity>> {
        let zero = Readings::default();
        let http = self.before(now, HTTP_WINDOW).unwrap_or(&zero);
        let requests = r.requests.saturating_sub(http.requests);
        let bad = r.requests_5xx.saturating_sub(http.requests_5xx);
        let rate = (requests >= HTTP_MIN_REQUESTS).then(|| bad as f64 / requests as f64);
        let latency: Vec<(f64, u64)> = r
            .latency
            .iter()
            .enumerate()
            .map(|(i, &(le, n))| {
                let was = http.latency.get(i).filter(|b| b.0 == le).map_or(0, |b| b.1);
                (le, n.saturating_sub(was))
            })
            .collect();
        let timed = latency.last().map_or(0, |b| b.1);
        let p99 = (timed >= P99_MIN_REQUESTS)
            .then(|| quantile(0.99, &latency))
            .flatten();
        let commits = self.before(now, COMMIT_WINDOW).unwrap_or(&zero);
        let failed = r.commit_failures.saturating_sub(commits.commit_failures);
        let keys = self.before(now, KEY_REFUSAL_WINDOW).unwrap_or(&zero);
        let expired = r
            .expired_key_refusals
            .saturating_sub(keys.expired_key_refusals);
        let advice = self.before(now, SLOW_ADVICE_WINDOW).unwrap_or(&zero);
        let slow = r
            .slow_advice_routes
            .saturating_sub(advice.slow_advice_routes);
        if r.extraction_backlog > EXTRACTION_BACKLOG_MAX {
            self.backlog_since.get_or_insert(now);
        } else {
            self.backlog_since = None;
        }
        let above = |v: Option<f64>, warning: f64, critical: f64| match v {
            Some(v) if v > critical => Some(Severity::Critical),
            Some(v) if v > warning => Some(Severity::Warning),
            _ => None,
        };
        BTreeMap::from([
            (
                AlertName::Http5xxRate,
                above(rate, HTTP_5XX_WARNING, HTTP_5XX_CRITICAL),
            ),
            (
                AlertName::P99Latency,
                above(p99, P99_WARNING_SECS, P99_CRITICAL_SECS),
            ),
            (
                AlertName::DiskUsage,
                above(r.disk_used, DISK_WARNING, DISK_CRITICAL),
            ),
            (
                AlertName::DirectoryBlocked,
                r.directory_blocked.then_some(Severity::Critical),
            ),
            (
                AlertName::ExtractionBacklog,
                self.backlog_since
                    .is_some_and(|s| now - s >= EXTRACTION_BACKLOG_FOR)
                    .then_some(Severity::Warning),
            ),
            (
                AlertName::StoreCommitFailures,
                match failed {
                    0 => None,
                    n if n > COMMIT_FAILURES_CRITICAL => Some(Severity::Critical),
                    _ => Some(Severity::Warning),
                },
            ),
            (
                AlertName::AdviceSlow,
                match slow {
                    0 => None,
                    n if n > SLOW_ADVICE_CRITICAL => Some(Severity::Critical),
                    _ => Some(Severity::Warning),
                },
            ),
            (
                AlertName::KeyExpiry,
                if expired > 0 {
                    Some(Severity::Critical)
                } else {
                    (r.keys_expiring > 0).then_some(Severity::Warning)
                },
            ),
        ])
    }

    /// Evaluate the rules on `r`, read at `now`: the events to send (none when nothing changed).
    pub fn step(&mut self, now: DateTime<Utc>, r: &Readings) -> Vec<AlertEvent> {
        let conditions = self.conditions(now, r);
        self.history.push_back((now, r.clone()));
        let longest = HTTP_WINDOW
            .max(COMMIT_WINDOW)
            .max(KEY_REFUSAL_WINDOW)
            .max(SLOW_ADVICE_WINDOW);
        while self.history.len() > 1 && self.history[1].0 <= now - longest {
            self.history.pop_front();
        }
        let mut events = vec![];
        for (name, held) in conditions {
            let t = self.tracks.entry(name).or_default();
            match (t.firing.as_mut(), held) {
                (None, None) => {
                    t.held = 0;
                    t.held_since = None;
                }
                (None, Some(severity)) => {
                    t.held += 1;
                    let since = *t.held_since.get_or_insert(now);
                    if t.held >= FIRE_AFTER {
                        let f = Firing {
                            started_at: since.trunc_subsecs(0),
                            severity,
                            count: t.held.into(),
                            sent: t.held.into(),
                            clear: 0,
                            clear_since: None,
                        };
                        events.push(event(name, &f, None));
                        t.firing = Some(f);
                        t.held = 0;
                        t.held_since = None;
                    }
                }
                (Some(f), Some(severity)) => {
                    f.clear = 0;
                    f.clear_since = None;
                    f.count += 1;
                    let changed = severity != f.severity;
                    f.severity = severity;
                    if changed || f.count - f.sent >= RESEND_EVERY {
                        f.sent = f.count;
                        events.push(event(name, f, None));
                    }
                }
                (Some(f), None) => {
                    f.clear += 1;
                    let since = *f.clear_since.get_or_insert(now);
                    if f.clear >= RESOLVE_AFTER {
                        events.push(event(name, f, Some(since.trunc_subsecs(0))));
                        t.firing = None;
                    }
                }
            }
        }
        events
    }

    /// The alerts firing now, and how bad.
    pub fn firing(&self) -> Vec<(AlertName, Severity)> {
        self.tracks
            .iter()
            .filter_map(|(n, t)| t.firing.as_ref().map(|f| (*n, f.severity)))
            .collect()
    }

    /// Set `memstore_alert_firing` ([`firing_gauge`]) to what fires now: 1 for each firing alert at its
    /// severity, 0 for every other alert and severity.
    pub fn export(&self, gauge: &prometheus::IntGaugeVec) {
        let firing = self.firing();
        for name in AlertName::ALL {
            for sev in Severity::ALL {
                gauge
                    .with_label_values(&[name.as_str(), sev.as_str()])
                    .set(firing.contains(&(name, sev)) as i64);
            }
        }
    }
}

fn event(name: AlertName, f: &Firing, ended_at: Option<DateTime<Utc>>) -> AlertEvent {
    AlertEvent {
        name,
        severity: f.severity,
        state: if ended_at.is_some() {
            AlertState::Resolved
        } else {
            AlertState::Firing
        },
        started_at: f.started_at,
        ended_at,
        count: f.count,
    }
}

/// `memstore_alert_firing{name,severity}`: 1 while that alert fires at that severity.
pub fn firing_gauge() -> prometheus::IntGaugeVec {
    prometheus::IntGaugeVec::new(
        prometheus::Opts::new(
            "memstore_alert_firing",
            "1 while the alert fires at this severity, else 0 (the telemetry alerts channel's rules)",
        ),
        &["name", "severity"],
    )
    .unwrap()
}
