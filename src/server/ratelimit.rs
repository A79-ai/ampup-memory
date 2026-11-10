//! Per-caller rate limits: a token bucket for each service key (by its hash) and each signed-in person
//! (by the identity the server verified), never for anything a caller merely says.
//!
//! `AMPUP_MEMORY_RATE_LIMIT` (e.g. `60/min`, or `60/min:120` for a burst of 120; unset or `off`: none) holds
//! every caller; ingest keys get `AMPUP_MEMORY_INGEST_RATE_LIMIT` instead (unset: ten times the first). A
//! bucket holds `burst` requests and refills at the rate; a request that finds it empty is answered 429
//! with `Retry-After`.
//!
//! A batch costs what its requests one at a time would: a `/search` of 100 rows takes 100 tokens (at most a
//! full bucket, so any batch the route takes can get through once the bucket refills).
//!
//! The buckets live in this server's memory: each replica limits on its own, so behind a load balancer
//! with N replicas a caller gets up to N times the limit.
//!
//! Apart from the rate, how much a server takes on at once (`off` lifts a cap):
//! - `AMPUP_MEMORY_MAX_CONNECTIONS` (default 4096): open connections per port. Past it a new connection is answered
//!   503 and closed.
//! - `AMPUP_MEMORY_MAX_IN_FLIGHT` (default 1024): requests being answered at once per port. Past it a request is
//!   answered 503 with `Retry-After`, before its body is read. The probes (`/ready`, `/health`, `/healthz`) are
//!   always answered.
//! - `AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER` (default 64): requests one caller (the same key or person as the rate)
//!   has under way at once. Past it a request is answered 429 with `Retry-After`, before its body is read.
//! - `AMPUP_MEMORY_HEADER_TIMEOUT_SECS` (default 660): how long a connection may take to send a request's headers,
//!   idle ones included. Longer than load balancers keep idle connections to a backend (600 s), so the balancer
//!   always closes first; a server reached directly can take less.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
    caps: Caps,
    /// Requests each caller has under way, by the same id as its bucket.
    under_way: Arc<Mutex<HashMap<String, usize>>>,
}

/// How much a server takes on at once: see the module's notes. None: no cap.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub connections: Option<usize>,
    pub in_flight: Option<usize>,
    pub per_caller: Option<usize>,
    pub header_timeout: Option<Duration>,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            connections: Some(4096),
            in_flight: Some(1024),
            per_caller: Some(64),
            header_timeout: Some(Duration::from_secs(660)),
        }
    }
}

impl Caps {
    /// From `AMPUP_MEMORY_MAX_CONNECTIONS`, `AMPUP_MEMORY_MAX_IN_FLIGHT`, `AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER` and
    /// `AMPUP_MEMORY_HEADER_TIMEOUT_SECS`, each a positive number (`off`: no cap), else the defaults.
    pub fn from_env() -> Result<Caps> {
        let d = Caps::default();
        let cap = |k: &str, default: Option<usize>| -> Result<Option<usize>> {
            match std::env::var(k).ok().map(|v| v.trim().to_string()) {
                None => Ok(default),
                Some(v) if v.is_empty() => Ok(default),
                Some(v) if v.eq_ignore_ascii_case("off") => Ok(None),
                Some(v) => match v.parse::<usize>() {
                    Ok(n) if n > 0 => Ok(Some(n)),
                    _ => anyhow::bail!("{k} is a positive number, or off"),
                },
            }
        };
        let secs = d.header_timeout.map(|t| t.as_secs() as usize);
        Ok(Caps {
            connections: cap("AMPUP_MEMORY_MAX_CONNECTIONS", d.connections)?,
            in_flight: cap("AMPUP_MEMORY_MAX_IN_FLIGHT", d.in_flight)?,
            per_caller: cap("AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER", d.per_caller)?,
            header_timeout: cap("AMPUP_MEMORY_HEADER_TIMEOUT_SECS", secs)?
                .map(|s| Duration::from_secs(s as u64)),
        })
    }
}

/// One of a caller's requests under way: counted until dropped.
pub struct UnderWay {
    who: String,
    under_way: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for UnderWay {
    fn drop(&mut self) {
        let mut m = self.under_way.lock().unwrap();
        if let Some(n) = m.get_mut(&self.who) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.who);
            }
        }
    }
}

impl Limiter {
    pub fn new(default: Option<Limit>, ingest: Option<Limit>) -> Self {
        Limiter {
            default,
            ingest,
            buckets: Mutex::new((HashMap::new(), PRUNE_AT)),
            caps: Caps::default(),
            under_way: Default::default(),
        }
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    /// Count one more request under way for `who` (the id its bucket has); None when it already has the most
    /// a caller may (`AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER`).
    pub fn begin(&self, who: &str) -> Option<UnderWay> {
        let mut m = self.under_way.lock().unwrap();
        let n = m.entry(who.to_string()).or_default();
        if self.caps.per_caller.is_some_and(|cap| *n >= cap) {
            return None;
        }
        *n += 1;
        Some(UnderWay {
            who: who.to_string(),
            under_way: self.under_way.clone(),
        })
    }

    /// From `AMPUP_MEMORY_RATE_LIMIT` and `AMPUP_MEMORY_INGEST_RATE_LIMIT` (or the specs given in their place).
    pub fn configured(default: Option<String>, ingest: Option<String>) -> Result<Self> {
        let var = |given: Option<String>, k: &str| given.or_else(|| std::env::var(k).ok());
        let default = Limit::parse(&var(default, "AMPUP_MEMORY_RATE_LIMIT").unwrap_or_default())
            .context("AMPUP_MEMORY_RATE_LIMIT")?;
        let ingest = match var(ingest, "AMPUP_MEMORY_INGEST_RATE_LIMIT") {
            Some(s) => Limit::parse(&s).context("AMPUP_MEMORY_INGEST_RATE_LIMIT")?,
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
        let caps = Caps::from_env()?;
        Ok(Limiter {
            caps,
            ..Limiter::new(default, ingest)
        })
    }

    fn limit(&self, class: Class) -> Option<Limit> {
        match class {
            Class::Default => self.default,
            Class::Ingest => self.ingest,
        }
    }

    /// Take `cost` requests (a batch's rows; at least one, at most a full bucket) from `who`'s bucket; if it
    /// holds fewer, how long until it holds them.
    pub fn take(&self, class: Class, who: &str, cost: usize) -> std::result::Result<(), Duration> {
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
        let cost = (cost.max(1) as f64).min(limit.burst);
        if b.tokens >= cost {
            b.tokens -= cost;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((cost - b.tokens) / limit.per_sec))
        }
    }
}
