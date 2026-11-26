//! Reads someone is waiting for go first: a server's background loops (keep-warm, dreaming, table maintenance)
//! step aside while an interactive read is under way, and their table loads take turns from a small pool.
//!
//! On a busy org the loops and the reads share one process's catalog quota, object-store connections and CPU.
//! Keep-warm alone ran 11–21 s a cycle on staging (Nov 2026), its passage and index reads beside a Studio page's
//! and a hook's `get_advice`, and those reads waited on the same resources.
//!
//! - An interactive read (a Studio page's request, `get_advice`, `search_memory`) holds an [`Interactive`] while it
//!   runs.
//! - A loop calls [`pause_for_reads`] between its steps: while an interactive read is under way it waits, at most
//!   `AMPUP_MEMORY_BACKGROUND_YIELD_MS` (default 1500) a pause, so a loop is delayed, never stopped (hooks on a busy
//!   org are almost never all done, and keep-warm that never ran would leave every read cold). A pause is taken
//!   only where the loop holds nothing a read could wait on (a table's load lock, a visibility build), so a read is
//!   never queued behind a paused loop.
//! - A loop's table load takes a permit from a pool of `AMPUP_MEMORY_BACKGROUND_LOADS` (default 4, 0 no cap) for as
//!   long as it loads ([`crate::loads::turn`]), so a keep-warm touch or a refresh no one waits for never has more
//!   than that in flight beside the reads. A request's loads never take one.
//!
//! What a loop does is unchanged: only when.

use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore, SemaphorePermit};

/// Interactive reads under way in this process.
static UNDER_WAY: AtomicUsize = AtomicUsize::new(0);

/// Woken when the last interactive read under way ends.
static QUIET: Notify = Notify::const_new();

/// An interactive read under way, while held: loops pause for it ([`pause_for_reads`]).
#[must_use]
pub struct Interactive(());

impl Interactive {
    pub fn begin() -> Interactive {
        UNDER_WAY.fetch_add(1, Ordering::AcqRel);
        Interactive(())
    }
}

impl Drop for Interactive {
    fn drop(&mut self) {
        if UNDER_WAY.fetch_sub(1, Ordering::AcqRel) == 1 {
            QUIET.notify_waiters();
        }
    }
}

/// How many interactive reads are under way now.
pub fn under_way() -> usize {
    UNDER_WAY.load(Ordering::Acquire)
}

/// The most a loop waits at one pause (`AMPUP_MEMORY_BACKGROUND_YIELD_MS`, default 1500; 0 never pauses).
fn most_wait() -> Duration {
    static MOST: LazyLock<Duration> = LazyLock::new(|| {
        Duration::from_millis(
            std::env::var("AMPUP_MEMORY_BACKGROUND_YIELD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1500),
        )
    });
    *MOST
}

/// Pauses since the last [`report`]: how many waited, and how long in all.
static PAUSED: Mutex<(u64, Duration)> = Mutex::new((0, Duration::ZERO));

/// A loop's pause between its steps: while an interactive read is under way, wait until none is, at most
/// [`most_wait`]. Returns at once when none is, or when the caller is not a background loop
/// ([`crate::loads::is_loop`]): a request, or a starting server's warm-up, never pauses here. Never called while
/// holding a lock or a shared build a read could be waiting on.
pub async fn pause_for_reads() {
    if under_way() == 0 || !crate::loads::is_loop() {
        return;
    }
    let most = most_wait();
    if most.is_zero() {
        return;
    }
    let started = Instant::now();
    let until = tokio::time::Instant::now() + most;
    loop {
        let quiet = QUIET.notified();
        tokio::pin!(quiet);
        // Registered before the count is read, so an end between the two still wakes it.
        quiet.as_mut().enable();
        if under_way() == 0 {
            break;
        }
        if tokio::time::timeout_at(until, quiet).await.is_err() {
            break;
        }
    }
    let mut p = PAUSED.lock().unwrap();
    p.0 += 1;
    p.1 += started.elapsed();
}

/// Loops' table loads at once (`AMPUP_MEMORY_BACKGROUND_LOADS`, default 4; 0 no cap).
static LOADS: LazyLock<Option<Semaphore>> = LazyLock::new(|| {
    let n = std::env::var("AMPUP_MEMORY_BACKGROUND_LOADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4);
    (n > 0).then(|| Semaphore::new(n))
});

/// A loop's turn at a table load from the pool, held while it loads: none for a request's (or a start's) load, or
/// with no cap.
pub async fn load_permit() -> Option<SemaphorePermit<'static>> {
    if !crate::loads::is_loop() {
        return None;
    }
    // Never closed, so acquire can't fail.
    Some(LOADS.as_ref()?.acquire().await.unwrap())
}

/// The pauses since the last report, as one line (none when there were none). Counting starts again.
pub fn report() -> Option<String> {
    let (n, total) = std::mem::take(&mut *PAUSED.lock().unwrap());
    (n > 0).then(|| {
        format!(
            "background work paused {n} times for interactive reads, {} ms in all",
            total.as_millis()
        )
    })
}
