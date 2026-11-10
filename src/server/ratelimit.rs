//! Per-caller rate limits: a token bucket for each service key (by its hash) and each signed-in person
//! (by the identity the server verified), never for anything a caller merely says.
//!
//! `MEMSTORE_RATE_LIMIT` (e.g. `60/min`, or `60/min:120` for a burst of 120; unset or `off`: none) holds
//! every caller; ingest keys get `MEMSTORE_INGEST_RATE_LIMIT` instead (unset: ten times the first). A
//! bucket holds `burst` requests and refills at the rate; a request that finds it empty is answered 429
//! with `Retry-After`.
//!
//! The buckets live in this server's memory: each replica limits on its own, so behind a load balancer
//! with N replicas a caller gets up to N times the limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Requests per second, and how many may come at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limit {
    pub per_sec: f64,
    pub burst: f64,
}

impl Limit {
    /// `<n>/<s|sec|second|m|min|minute|h|hour>[:<burst>]`; `off` (or empty): none. The burst defaults to n.
    pub fn parse(spec: &str) -> Result<Option<Limit>> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let bad = || format!("rate limit {spec:?}: like 60/min, 5/s or 60/min:120 (burst), or off");
        let (rate, burst) = match spec.split_once(':') {
            Some((r, b)) => (r, Some(b.trim().parse::<u32>().ok().with_context(bad)?)),
            None => (spec, None),
        };
        let (n, unit) = rate.split_once('/').with_context(bad)?;
        let n: u32 = n.trim().parse().ok().with_context(bad)?;
        let secs = match unit.trim() {
            "s" | "sec" | "second" => 1.0,
            "m" | "min" | "minute" => 60.0,
            "h" | "hour" => 3600.0,
            _ => anyhow::bail!(bad()),
        };
        let burst = burst.unwrap_or(n);
        anyhow::ensure!(n > 0 && burst > 0, bad());
        Ok(Some(Limit {
            per_sec: f64::from(n) / secs,
            burst: f64::from(burst),
        }))
    }

    fn times(self, k: f64) -> Limit {
        Limit {
            per_sec: self.per_sec * k,
            burst: self.burst * k,
        }
    }
}

/// Which limit a caller is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Default,
    Ingest,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Default => "default",
            Class::Ingest => "ingest",
        }
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Buckets kept before idle (full) ones are dropped: a dropped bucket comes back full, so dropping a
/// full one changes nothing.
const PRUNE_AT: usize = 10_000;

/// The buckets, by limit and caller, and the size at which idle ones are next dropped.
type Buckets = (HashMap<(Class, String), Bucket>, usize);

pub struct Limiter {
    default: Option<Limit>,
    ingest: Option<Limit>,
    buckets: Mutex<Buckets>,
}

impl Limiter {
    pub fn new(default: Option<Limit>, ingest: Option<Limit>) -> Self {
        Limiter {
            default,
            ingest,
            buckets: Mutex::new((HashMap::new(), PRUNE_AT)),
        }
    }

    /// From `MEMSTORE_RATE_LIMIT` and `MEMSTORE_INGEST_RATE_LIMIT` (or the specs given in their place).
    pub fn configured(default: Option<String>, ingest: Option<String>) -> Result<Self> {
        let var = |given: Option<String>, k: &str| given.or_else(|| std::env::var(k).ok());
        let default = Limit::parse(&var(default, "MEMSTORE_RATE_LIMIT").unwrap_or_default())
            .context("MEMSTORE_RATE_LIMIT")?;
        let ingest = match var(ingest, "MEMSTORE_INGEST_RATE_LIMIT") {
            Some(s) => Limit::parse(&s).context("MEMSTORE_INGEST_RATE_LIMIT")?,
            None => default.map(|d| d.times(10.0)),
        };
        if let Some(d) = default {
            eprintln!(
                "rate limit per key and per person, on this replica: {:.2}/s, burst {} (ingest keys: {})",
                d.per_sec,
                d.burst,
                ingest.map_or("none".into(), |i| format!(
                    "{:.2}/s, burst {}",
                    i.per_sec, i.burst
                ))
            );
        }
        Ok(Limiter::new(default, ingest))
    }

    fn limit(&self, class: Class) -> Option<Limit> {
        match class {
            Class::Default => self.default,
            Class::Ingest => self.ingest,
        }
    }

    /// Take one request from `who`'s bucket; if it is empty, how long until it holds one.
    pub fn take(&self, class: Class, who: &str) -> std::result::Result<(), Duration> {
        let Some(limit) = self.limit(class) else {
            return Ok(());
        };
        let now = Instant::now();
        let mut guard = self.buckets.lock().unwrap();
        let (buckets, prune_at) = &mut *guard;
        if buckets.len() >= *prune_at {
            buckets.retain(|(c, _), b| {
                self.limit(*c).is_some_and(|l| {
                    b.tokens + now.duration_since(b.at).as_secs_f64() * l.per_sec < l.burst
                })
            });
            *prune_at = (buckets.len() * 2).max(PRUNE_AT);
        }
        let b = buckets.entry((class, who.to_string())).or_insert(Bucket {
            tokens: limit.burst,
            at: now,
        });
        b.tokens =
            (b.tokens + now.duration_since(b.at).as_secs_f64() * limit.per_sec).min(limit.burst);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - b.tokens) / limit.per_sec))
        }
    }
}
