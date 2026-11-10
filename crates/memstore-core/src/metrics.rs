//! A server's Prometheus metrics (`/metrics`).
//!
//! Every label takes its values from a fixed, small set the server owns, never from a caller's text:
//! a route template ([`crate::audit::route`]), an HTTP status, a refusal reason code
//! ([`crate::audit::Reason`]), a key role, and an org name only when the directory holds that org
//! (else `-`). No user, key name, token, raw path or query ever becomes a label or a value, so a scrape
//! leaks nothing an operator's dashboard shouldn't hold and the series count stays bounded (by the
//! routes times the statuses, the reasons, and the directory's orgs times four roles).
//!
//! `/metrics` is served on its own port (`MEMSTORE_METRICS_PORT`, bound like the public port), or on
//! the public port to the operator's admin key only: never to anyone else.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

/// The directory's own metrics: kept with the directory a server holds (so they outlive a store
/// rebuilt on refresh), and registered with the server's metrics.
pub struct DirMetrics {
    /// Seconds a directory load from the catalog took (reading the log's new rows and folding them).
    pub refresh_seconds: Histogram,
    /// Directory commits that lost a compare-and-set to another server and were re-planned.
    pub commit_retries: IntCounter,
    /// 1 while the directory log holds a change this engine can't read, of a newer version or an op it
    /// doesn't know (the fold stops before it, and no one is signed in); 0 otherwise.
    pub blocked: IntGauge,
    /// The version of that change (0: none).
    pub blocked_version: IntGauge,
    /// Loads that met a change this engine can't read.
    pub unsupported_events: IntCounter,
    /// First loads whose newest snapshot was not what the log folds to (so not loaded: the log was
    /// replayed). Someone who can write the catalog wrote it, or a bug did.
    pub snapshot_refused: IntCounter,
}

impl Default for DirMetrics {
    fn default() -> Self {
        DirMetrics {
            refresh_seconds: Histogram::with_opts(
                HistogramOpts::new(
                    "memstore_directory_refresh_seconds",
                    "Time to load the directory's new changes from the catalog and fold them",
                )
                .buckets(vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
            )
            .unwrap(),
            commit_retries: IntCounter::new(
                "memstore_directory_commit_retries_total",
                "Directory commits re-planned after another server committed first",
            )
            .unwrap(),
            blocked: IntGauge::new(
                "memstore_directory_blocked",
                "1 while the directory log holds a change this server can't read (a newer version, or an op it doesn't know): it signs no one in and refuses directory writes",
            )
            .unwrap(),
            blocked_version: IntGauge::new(
                "memstore_directory_blocked_event_version",
                "The version of the change this server can't read (0: none)",
            )
            .unwrap(),
            unsupported_events: IntCounter::new(
                "memstore_directory_unsupported_events_total",
                "Directory loads that met a change this server can't read",
            )
            .unwrap(),
            snapshot_refused: IntCounter::new(
                "memstore_directory_snapshot_refused_total",
                "First directory loads whose snapshot was not what the log folds to: it was ignored and the log replayed",
            )
            .unwrap(),
        }
    }
}

impl DirMetrics {
    /// A fold was loaded: whether it stopped before a change it can't read.
    pub fn loaded(&self, blocked: Option<(i64, i64)>) {
        match blocked {
            Some((version, at)) => {
                self.unsupported_events.inc();
                if self.blocked.get() == 0 || self.blocked_version.get() != version {
                    eprintln!(
                        "ALERT directory: the log has {} at {at}: this server signs no one in, refuses directory writes and reports not ready until it is upgraded",
                        crate::people::directory::blocked_why(version)
                    );
                }
                self.blocked.set(1);
                self.blocked_version.set(version);
            }
            None => {
                if self.blocked.get() == 1 {
                    eprintln!("directory: every change folds again");
                }
                self.blocked.set(0);
                self.blocked_version.set(0);
            }
        }
    }
}

/// The server's metrics.
pub struct Metrics {
    registry: Registry,
    /// Requests by route template and status.
    pub requests: IntCounterVec,
    /// Seconds from a request's headers being read to its response, by route template.
    pub request_seconds: prometheus::HistogramVec,
    /// 401s, 403s and 429s by reason code.
    pub refusals: IntCounterVec,
    /// Service keys' authenticated uses by their org (`-` unless the directory holds it) and role.
    pub key_uses: IntCounterVec,
    /// 429s by which limit (`default` or `ingest`).
    pub rate_limited: IntCounterVec,
    /// Seconds since the start of the last index update that brought every served scope's index up to
    /// date (since the server started, until one has). Set when scraped.
    pub index_lag: prometheus::Gauge,
    /// Index updates that failed (search keeps the previous index).
    pub index_failures: IntCounter,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let vec = |name: &str, help: &str, labels: &[&str]| {
            let c = IntCounterVec::new(Opts::new(name, help), labels).unwrap();
            registry.register(Box::new(c.clone())).unwrap();
            c
        };
        let requests = vec(
            "memstore_http_requests_total",
            "Requests by route template and HTTP status",
            &["route", "status"],
        );
        let request_seconds = prometheus::HistogramVec::new(
            HistogramOpts::new(
                "memstore_http_request_seconds",
                "Seconds from a request's headers being read to its response, by route template",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["route"],
        )
        .unwrap();
        registry
            .register(Box::new(request_seconds.clone()))
            .unwrap();
        let refusals = vec(
            "memstore_auth_refusals_total",
            "Refused requests (401, 403, 429) by reason code",
            &["reason"],
        );
        let key_uses = vec(
            "memstore_key_uses_total",
            "Service key uses by org (- unless the directory holds it) and role",
            &["org", "role"],
        );
        let rate_limited = vec(
            "memstore_rate_limited_total",
            "Requests answered 429 by which limit (default, ingest)",
            &["limit"],
        );
        let index_lag = prometheus::Gauge::new(
            "memstore_index_freshness_lag_seconds",
            "Seconds since the start of the last index update that brought every served scope up to date",
        )
        .unwrap();
        registry.register(Box::new(index_lag.clone())).unwrap();
        let index_failures = IntCounter::new(
            "memstore_index_update_failures_total",
            "Index updates that failed (search keeps the previous index)",
        )
        .unwrap();
        registry.register(Box::new(index_failures.clone())).unwrap();
        Metrics {
            registry,
            requests,
            request_seconds,
            refusals,
            key_uses,
            rate_limited,
            index_lag,
            index_failures,
        }
    }

    /// Add a collector kept elsewhere (the directory's, the audit recorder's).
    pub fn register(&self, c: Box<dyn prometheus::core::Collector>) {
        if let Err(e) = self.registry.register(c) {
            eprintln!("metrics: {e}");
        }
    }

    /// Register the directory's metrics.
    pub fn register_dir(&self, d: &DirMetrics) {
        self.register(Box::new(d.refresh_seconds.clone()));
        self.register(Box::new(d.commit_retries.clone()));
        self.register(Box::new(d.blocked.clone()));
        self.register(Box::new(d.blocked_version.clone()));
        self.register(Box::new(d.unsupported_events.clone()));
        self.register(Box::new(d.snapshot_refused.clone()));
    }

    /// The Prometheus text format.
    pub fn render(&self) -> String {
        let mut out = vec![];
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut out)
            .unwrap_or_default();
        String::from_utf8(out).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// The audit recorder's metrics.
pub struct AuditMetrics {
    /// Buffered audit writes that failed (their rows are kept and retried).
    pub flush_failures: IntCounter,
    /// Audit events dropped because the buffer was full.
    pub dropped: IntCounter,
}

impl Default for AuditMetrics {
    fn default() -> Self {
        AuditMetrics {
            flush_failures: IntCounter::new(
                "memstore_audit_flush_failures_total",
                "Writes of buffered audit events that failed (kept and retried)",
            )
            .unwrap(),
            dropped: IntCounter::new(
                "memstore_audit_dropped_events_total",
                "Audit events dropped because the buffer was full",
            )
            .unwrap(),
        }
    }
}

impl Metrics {
    /// Register the audit recorder's metrics.
    pub fn register_audit(&self, a: &AuditMetrics) {
        self.register(Box::new(a.flush_failures.clone()));
        self.register(Box::new(a.dropped.clone()));
    }
}

/// Store commits that failed to land (each caller's commit in a failed group commit counts once): the
/// data or ledger append gave up. Served as `memstore_store_commit_failures_total`; the
/// `store_commit_failures` alert reads it.
pub static STORE_COMMIT_FAILURES: std::sync::LazyLock<IntCounter> =
    std::sync::LazyLock::new(|| {
        IntCounter::new(
            "memstore_store_commit_failures_total",
            "Store commits (episodes, facts, extractions) that failed to land",
        )
        .unwrap()
    });

/// Requests whose API key matched one past its expiry. A person's expired sign-in isn't one: it refreshes,
/// where a service still sending an expired key is failing every request. Served as
/// `memstore_expired_key_refusals_total`; the `key_expiry` alert reads it.
pub static EXPIRED_KEY_REFUSALS: std::sync::LazyLock<IntCounter> = std::sync::LazyLock::new(|| {
    IntCounter::new(
        "memstore_expired_key_refusals_total",
        "Requests refused because their API key had expired",
    )
    .unwrap()
});

/// 1 while the keys file (`MEMSTORE_API_KEYS`) as it is now can't be read or doesn't parse: the server keeps
/// the keys it last read, so a key removed from the file since still works here. 0 once the file reads again.
pub static KEYS_FILE_FAILING: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_keys_file_failing",
        "1 while the keys file can't be read or parsed (the keys last read are kept)",
    )
    .unwrap()
});

/// Times the keys file was read again because it changed, and its new keys took effect.
pub static KEYS_FILE_RELOADS: std::sync::LazyLock<IntCounter> = std::sync::LazyLock::new(|| {
    IntCounter::new(
        "memstore_keys_file_reloads_total",
        "Times a changed keys file was read and its keys took effect",
    )
    .unwrap()
});

/// 1 while this server holds the writer lease (does the background writing: extraction, summarising, index
/// updates), else 0; 1 too with `MEMSTORE_WRITER_LEASE=off`. Summed over a catalog's servers it is 1: 0 for
/// longer than a minute means nothing is extracted or indexed anywhere.
pub static WRITER_LEASE_HELD: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_writer_lease_held",
        "1 while this server holds the writer lease and does the background writing",
    )
    .unwrap()
});

/// 1 while the writer lease can't be kept from this server: its location takes no lease (`/ready` says why
/// and answers 503), or its last attempt to beat or take it failed.
pub static WRITER_LEASE_FAILING: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_writer_lease_failing",
        "1 while the writer lease can't be placed, taken or kept from this server",
    )
    .unwrap()
});

/// Episodes the last extraction round found waiting to be read, summed over the scopes it visited (each
/// scope's round reads at most 200, so this is at most 200 per scope). 0 without an extraction model.
/// The `extraction_backlog` alert reads it.
pub static EXTRACTION_BACKLOG: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_extraction_backlog_episodes",
        "Episodes the last extraction round found waiting to be read (at most 200 per scope)",
    )
    .unwrap()
});

/// Live data files of the system ledger (`memory_system.ledger`): one per ledger commit since its last compaction,
/// and what a cold read (each server's first, at start) reads. Set on every maintenance round, by every server.
pub static LEDGER_DATA_FILES: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_ledger_data_files",
        "Live data files of the system ledger (one per commit since its last compaction)",
    )
    .unwrap()
});

/// When the ledger's last compaction committed (unix seconds; 0 while unknown), as the ledger records it: the same
/// on every server, whichever ran it (`serve`'s timer or `memstore compact-ledger`).
pub static LEDGER_COMPACTED_AT: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_ledger_compaction_last_success_timestamp_seconds",
        "Unix time the system ledger's last compaction committed (0: unknown)",
    )
    .unwrap()
});

/// Seconds a ledger compaction run by this server's timer took, read to sweep.
pub static LEDGER_COMPACTION_SECONDS: std::sync::LazyLock<Histogram> =
    std::sync::LazyLock::new(|| {
        Histogram::with_opts(
            HistogramOpts::new(
                "memstore_ledger_compaction_duration_seconds",
                "Seconds a ledger compaction run by this server's timer took, read to sweep",
            )
            .buckets(vec![
                0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
            ]),
        )
        .unwrap()
    });

/// Ledger compactions (or sweeps of an interrupted one) by this server's timer that failed.
pub static LEDGER_COMPACTION_FAILURES: std::sync::LazyLock<IntCounter> = std::sync::LazyLock::new(
    || {
        IntCounter::new(
            "memstore_ledger_compaction_failures_total",
            "Ledger compactions (or sweeps of an interrupted one) by this server's timer that failed",
        )
        .unwrap()
    },
);

/// 1 while the ledger holds more data files than the timer compacts in one run
/// (`MEMSTORE_LEDGER_COMPACT_MAX_FILES`): an operator runs `memstore compact-ledger` once.
pub static LEDGER_COMPACTION_CLI_NEEDED: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(
    || {
        IntGauge::new(
            "memstore_ledger_compaction_cli_needed",
            "1 while the ledger holds more data files than the timer compacts (run memstore compact-ledger once)",
        )
        .unwrap()
    },
);

/// Bytes of decoded table rows the store's cache holds, the system tables aside: what `MEMSTORE_CACHE_BYTES`
/// caps. Set after each cold load of a table.
pub static CACHE_BYTES: std::sync::LazyLock<IntGauge> = std::sync::LazyLock::new(|| {
    IntGauge::new(
        "memstore_cache_bytes",
        "Bytes of decoded table rows the cache holds (MEMSTORE_CACHE_BYTES caps it)",
    )
    .unwrap()
});

/// The memory this process may use, in bytes: `MEMSTORE_MEMORY_BYTES`, else its container's limit (cgroup v2
/// `memory.max`, v1 `memory.limit_in_bytes`); none where neither sets one.
pub fn memory_limit() -> Option<u64> {
    static LIMIT: std::sync::LazyLock<Option<u64>> = std::sync::LazyLock::new(|| {
        if let Some(n) = std::env::var("MEMSTORE_MEMORY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return Some(n);
        }
        [
            "/sys/fs/cgroup/memory.max",
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
        ]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok()?.trim().parse::<u64>().ok())
        // cgroup v1 writes "no limit" as a number near 2^63.
        .filter(|n| *n < 1 << 50)
    });
    *LIMIT
}

/// A memory budget's default: `default`, or `share` of [`memory_limit`] where that is less. The defaults were
/// set for a large host; on an 8 GiB pod the table cache (3 GiB), the index caches (1.25 GiB) and an
/// update's known vectors (1 GiB) together left too little for the server's own working memory and an
/// index update, and it was OOM-killed.
pub fn sized(default: usize, share: f64) -> usize {
    match memory_limit() {
        Some(limit) => default.min((limit as f64 * share) as usize),
        None => default,
    }
}

/// Hand the memory glibc's allocator holds free back to the system. A no-op elsewhere.
pub fn trim_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only returns the allocator's free memory to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// The process's memory, for the server's memory log: resident and peak resident (Linux), and what glibc's
/// allocator holds against what is in use, so memory freed but kept by the allocator shows apart from
/// memory still live.
pub fn process_memory() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .map_or("?".to_string(), str::to_string)
    };
    let mb = |kb: String| kb.parse::<u64>().map_or(kb, |k| (k >> 10).to_string());
    format!(
        "rss {} MB, peak {} MB, threads {}{}",
        mb(field("VmRSS:")),
        mb(field("VmHWM:")),
        field("Threads:"),
        ALLOCATOR_STATS
            .get()
            .map_or_else(malloc_held, |stats| stats())
    )
}

/// What the binary's global allocator holds, for [`process_memory`], when it isn't glibc's malloc (the server
/// sets it for jemalloc). Unset, the log reads glibc's counters.
pub static ALLOCATOR_STATS: std::sync::OnceLock<fn() -> String> = std::sync::OnceLock::new();

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn malloc_held() -> String {
    // SAFETY: mallinfo2 only reads the allocator's own counters.
    let m = unsafe { libc::mallinfo2() };
    format!(
        ", malloc in use {} MB, free but held {} MB, mmapped {} MB, arenas {} MB",
        m.uordblks >> 20,
        m.fordblks >> 20,
        m.hblkhd >> 20,
        m.arena >> 20
    )
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn malloc_held() -> String {
    String::new()
}
