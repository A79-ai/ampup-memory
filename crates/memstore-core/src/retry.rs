//! Retrying a catalog or object-store call that failed transiently, the way cloud APIs ask callers to:
//! capped exponential backoff with full jitter, the server's `Retry-After` when it sends one, a rate limit
//! waited out in seconds rather than milliseconds, and a process-wide retry budget so that when a quota is
//! spent every caller doesn't keep spending it (the token bucket the AWS SDKs' standard retry mode uses).
//! A caller that already retries the whole operation runs the inner calls [`without_retries`], so retries
//! happen at one layer only.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Result;
use rand::Rng;

/// Retries after the first attempt.
const MAX_RETRIES: u32 = 5;
/// First wait after a 5xx or a dropped connection: a blip, usually over at once.
const BLIP_BASE: Duration = Duration::from_millis(50);
/// First wait after a rate limit or quota: those windows are a second to a minute.
const THROTTLE_BASE: Duration = Duration::from_secs(1);
/// No single wait is longer, whatever the attempt or the server's `Retry-After`.
const MAX_WAIT: Duration = Duration::from_secs(32);
/// A call's whole time, retries and waits included: a retry that would end past it isn't made. Reads sit
/// behind these calls and fall back to what is cached, so it stays near a blip's worst case; the server's
/// start waits out a longer outage in its own loop ([`backoff`]).
const DEADLINE: Duration = Duration::from_secs(3);

/// The retry budget: each retry takes [`RETRY_COST`], a first-try success returns one token and a success
/// after retries returns what they took. Empty, a failure is returned at once.
const BUDGET: u32 = 500;
const RETRY_COST: u32 = 5;
static TOKENS: AtomicU32 = AtomicU32::new(BUDGET);

tokio::task_local! {
    static NO_RETRY: ();
}

/// Object stores and catalogs answer some requests with transient 5xx / throttling errors and
/// expect a retry. Anything else (not found, denied, a commit conflict) is returned at once.
pub fn is_transient(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    is_throttled_text(&text)
        || [
            "status: 500",
            "status: 502",
            "status: 503",
            "status: 504",
            "InternalError",
            "ServiceUnavailable",
            "Connection reset",
            "connection error",
        ]
        .iter()
        .any(|m| text.contains(m))
}

/// A rate limit or spent quota (BigLake's catalog answers a burst of calls with 429 RESOURCE_EXHAUSTED).
pub fn is_throttled(e: &anyhow::Error) -> bool {
    is_throttled_text(&format!("{e:#}"))
}

fn is_throttled_text(text: &str) -> bool {
    [
        "status: 429",
        "Too Many Requests",
        "RESOURCE_EXHAUSTED",
        "SlowDown",
    ]
    .iter()
    .any(|m| text.contains(m))
}

/// How long to wait before retry number `retry` (0 for the first) after `e`: the server's `Retry-After`
/// when it sent one, else full jitter (anywhere from nothing to the step) over a step that doubles from
/// [`THROTTLE_BASE`] for a rate limit or [`BLIP_BASE`] otherwise, capped at [`MAX_WAIT`].
pub fn backoff(e: &anyhow::Error, retry: u32) -> Duration {
    let text = format!("{e:#}");
    if let Some(wait) = retry_after(&text) {
        return wait.min(MAX_WAIT);
    }
    let base = match is_throttled_text(&text) {
        true => THROTTLE_BASE,
        false => BLIP_BASE,
    };
    let step = base.saturating_mul(1 << retry.min(16)).min(MAX_WAIT);
    Duration::from_millis(rand::thread_rng().gen_range(0..=step.as_millis() as u64))
}

/// How long a failed background call holds its place before the next reader may start it again: a few
/// retries' worth of [`backoff`], so callers that each start one don't turn a throttled catalog's every
/// read into a call.
pub fn cooldown(e: &anyhow::Error) -> Duration {
    backoff(e, 3).max(match is_throttled(e) {
        true => THROTTLE_BASE,
        false => BLIP_BASE,
    })
}

/// `Retry-After` from the response headers an error quotes: delay-seconds or an HTTP date (RFC 9110).
fn retry_after(text: &str) -> Option<Duration> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("\"retry-after\": \"")? + "\"retry-after\": \"".len();
    let value = &text[at..at + text[at..].find('"')?];
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    (when.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

fn take(cost: u32) -> bool {
    let took = TOKENS
        .try_update(Ordering::AcqRel, Ordering::Acquire, |t| t.checked_sub(cost))
        .is_ok();
    if !took && !SPENT.swap(true, Ordering::AcqRel) {
        eprintln!(
            "memstore: retry budget spent: transient failures return at once until calls succeed again"
        );
    }
    took
}

fn give(n: u32) {
    let _ = TOKENS.try_update(Ordering::AcqRel, Ordering::Acquire, |t| {
        Some((t + n).min(BUDGET))
    });
    if TOKENS.load(Ordering::Acquire) >= RETRY_COST && SPENT.swap(false, Ordering::AcqRel) {
        eprintln!("memstore: retry budget refilling: transient failures are retried again");
    }
}

/// The budget ran out and that was said: said once each way, not once a call.
static SPENT: AtomicBool = AtomicBool::new(false);

/// `fut` with every [`with_retries`] inside it trying once: for a caller that retries the whole of it.
pub async fn without_retries<F: Future>(fut: F) -> F::Output {
    NO_RETRY.scope((), fut).await
}

/// `op`, tried again on a transient failure ([`is_transient`]) after [`backoff`], up to [`MAX_RETRIES`]
/// times, within [`DEADLINE`], and while the retry budget lasts.
pub(crate) async fn with_retries<T, F, Fut>(op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let once = NO_RETRY.try_with(|_| ()).is_ok();
    let started = std::time::Instant::now();
    let mut taken = 0;
    for retry in 0.. {
        match op().await {
            Err(e) if !once && retry < MAX_RETRIES && is_transient(&e) => {
                let wait = backoff(&e, retry);
                if started.elapsed() + wait > DEADLINE || !take(RETRY_COST) {
                    return Err(e);
                }
                taken += RETRY_COST;
                tokio::time::sleep(wait).await;
            }
            Ok(v) => {
                give(taken.max(1));
                return Ok(v);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
