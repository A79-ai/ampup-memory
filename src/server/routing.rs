//! Advice routed for an agent that won't wait long on it. An agent calling `start_session` itself (not a hook,
//! which waits in a copy no one waits on) gets its answer within [`wait`], and its own `get_advice` within
//! [`ask_wait`]: the advice step runs as a job of its own, keyed by its session, and one still routing at the end
//! of the wait goes on. The session's next advice call (any question) hands its outcome on, so the routing's pick
//! is never lost to a rephrased question.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::sessions::Served;

/// An advice step's outcome, once it ends: what it served, or why it failed.
type Outcome = Option<Result<Arc<Served>, String>>;

/// An advice step: when it started, and its outcome once it ends.
type Job = (Instant, tokio::sync::watch::Receiver<Outcome>);

/// Advice steps by (namespace, session id): those still routing, and those ended whose outcome no call has taken.
static JOBS: LazyLock<Mutex<HashMap<(String, String), Job>>> = LazyLock::new(Default::default);

/// How long an outcome no call takes is kept: an agent that never asks again leaves nothing behind.
const KEEP: Duration = Duration::from_secs(3_600);

/// How long an agent's `start_session` waits for its opening advice: `MEMSTORE_ADVICE_WAIT_MS` (default 1,000).
/// The agent needs its session id to go on; the advice can follow on its next `get_advice`.
pub fn wait() -> Duration {
    millis("MEMSTORE_ADVICE_WAIT_MS", 1_000)
}

/// How long an agent's own `get_advice`, all of it, waits for routing: `MEMSTORE_ADVICE_ASK_WAIT_MS` (default
/// 30,000). The agent asked for the advice before its work, and a host without hooks (ChatGPT, Codex, Claude
/// Desktop) has no other way to get it: a second's wait answered nearly every call "still being looked up", as a
/// route takes 1.5 to 12 s.
pub fn ask_wait() -> Duration {
    millis("MEMSTORE_ADVICE_ASK_WAIT_MS", 30_000)
}

fn millis(var: &str, default: u64) -> Duration {
    let ms = std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default);
    Duration::from_millis(ms)
}

/// Advice routes, any caller's, that took longer than [`ask_wait`]: an agent's `get_advice` routed so long ran
/// out of its wait and answered "still being looked up". Served as `memstore_advice_slow_routes_total`; the
/// `advice_slow` alert reads it.
pub static SLOW_ROUTES: LazyLock<prometheus::IntCounter> = LazyLock::new(|| {
    prometheus::IntCounter::new(
        "memstore_advice_slow_routes_total",
        "Advice routes that took longer than an agent's get_advice waits (MEMSTORE_ADVICE_ASK_WAIT_MS)",
    )
    .unwrap()
});

/// Counts an advice route in [`SLOW_ROUTES`] when it ends (done, failed or dropped) later than [`ask_wait`]
/// after it began.
pub struct Timed(Instant);

impl Timed {
    pub fn start() -> Self {
        Timed(Instant::now())
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        let took = self.0.elapsed();
        if took > ask_wait() {
            SLOW_ROUTES.inc();
            eprintln!(
                "advice: a route took {} ms, more than an agent's get_advice waits ({} ms)",
                took.as_millis(),
                ask_wait().as_millis()
            );
        }
    }
}

/// Runs `job`, session `key`'s advice step, on its own: its outcome if it ends by `until`, else
/// [`Served::Pending`] while it goes on (the session's next call takes its outcome, [`take`]).
pub async fn within(
    key: (String, String),
    job: impl Future<Output = anyhow::Result<Served>> + Send + 'static,
    until: Instant,
) -> anyhow::Result<Arc<Served>> {
    let (tx, rx) = tokio::sync::watch::channel(None);
    {
        let mut jobs = JOBS.lock().unwrap();
        jobs.retain(|_, (at, _)| at.elapsed() < KEEP);
        jobs.insert(key.clone(), (Instant::now(), rx));
    }
    // Run on its own task as part of the lookup it was started in (`crate::advice_lookups`).
    let job = crate::advice_lookups::carry(job);
    tokio::spawn(async move {
        let outcome = job.await.map(Arc::new).map_err(|e| format!("{e:#}"));
        if let Err(e) = &outcome {
            eprintln!("advice: a session's advice step failed after its caller went on: {e}");
        }
        let _ = tx.send(Some(outcome));
    });
    take(&key, until)
        .await
        .unwrap_or(Ok(Arc::new(Served::Pending)))
}

/// The outcome of session `key`'s advice step that no call has taken yet, waiting until `until` for one still
/// routing ([`Served::Pending`] if it goes on); none when the session has none.
pub async fn take(key: &(String, String), until: Instant) -> Option<anyhow::Result<Arc<Served>>> {
    let mut rx = JOBS.lock().unwrap().get(key)?.1.clone();
    let left = until.saturating_duration_since(Instant::now());
    let ended = tokio::time::timeout(left, rx.wait_for(Option::is_some))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|o| o.clone());
    Some(match ended {
        Some(outcome) => {
            JOBS.lock().unwrap().remove(key);
            outcome.map_err(anyhow::Error::msg)
        }
        None => Ok(Arc::new(Served::Pending)),
    })
}
