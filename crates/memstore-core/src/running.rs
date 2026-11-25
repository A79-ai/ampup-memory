//! What this process is doing now, for the memory log: a request, a tool call, a warm-up step, a background
//! build each hold a [`Running`] while they work, and a jump in the memory allocated ([`watch_jumps`]) is logged
//! with everything running at that moment, so the line says what took the memory without a profiler.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
/// A running thing: what it is, the last phase it reached ([`at`]), and since when.
type Entry = (String, &'static str, Instant);
static NOW: LazyLock<Mutex<HashMap<u64, Entry>>> = LazyLock::new(Default::default);

tokio::task_local! {
    /// The running thing a task's [`crate::phases::mark`]s belong to ([`within`]).
    static CURRENT: u64;
}

/// Runs `f` as `r`'s: the phases it marks show beside `r` in the list.
pub async fn within<F: std::future::Future>(r: &Running, f: F) -> F::Output {
    CURRENT.scope(r.0, f).await
}

/// `f`, run as the running thing this task works for (if any): a task spawned for a request's work (its commit)
/// keeps marking that request's phases.
pub fn carry<F: std::future::Future>(f: F) -> impl std::future::Future<Output = F::Output> {
    let id = CURRENT.try_with(|id| *id).ok();
    async move {
        match id {
            Some(id) => CURRENT.scope(id, f).await,
            None => f.await,
        }
    }
}

/// Notes that the running thing this task works for (if any) reached `phase`.
pub fn at(phase: &'static str) {
    let _ = CURRENT.try_with(|id| {
        if let Some(e) = NOW.lock().unwrap().get_mut(id) {
            e.1 = phase;
        }
    });
}

/// Held while `what` runs; it leaves the list when dropped.
pub struct Running(u64);

/// Notes that `what` runs until the returned guard is dropped.
pub fn running(what: impl Into<String>) -> Running {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    NOW.lock()
        .unwrap()
        .insert(id, (label(what.into()), "", Instant::now()));
    Running(id)
}

/// `what` as the log shows it: a caller's text in it (a tool's name) can't break the line or run on, so escaped
/// and cut to [`MAX_LABEL`] characters.
fn label(what: String) -> String {
    what.chars()
        .take(MAX_LABEL)
        .collect::<String>()
        .escape_debug()
        .to_string()
}

/// The most characters of a label kept.
const MAX_LABEL: usize = 120;

impl Running {
    /// Says it has moved on to `what`, timed from now.
    pub fn step(&self, what: impl Into<String>) {
        if let Some(e) = NOW.lock().unwrap().get_mut(&self.0) {
            *e = (label(what.into()), "", Instant::now());
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        NOW.lock().unwrap().remove(&self.0);
    }
}

/// Everything running now, the longest running first, as `what (12 s)`, at most `most` of them.
fn now(most: usize) -> String {
    let mut all: Vec<(String, Duration)> = NOW
        .lock()
        .unwrap()
        .values()
        .map(|(w, phase, at)| match *phase {
            "" => (w.clone(), at.elapsed()),
            p => (format!("{w} at {p}"), at.elapsed()),
        })
        .collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.1));
    let n = all.len();
    let mut out: Vec<String> = all
        .into_iter()
        .take(most)
        .map(|(w, d)| format!("{w} ({:.1} s)", d.as_secs_f64()))
        .collect();
    if n > most {
        out.push(format!("and {} more", n - most));
    }
    out.join("; ")
}

/// Every quarter second, the memory allocated (`allocated`, in bytes); when it rose by at least `jump` within the
/// last 5 s, a line with what ran then, at most one each 10 s. Runs for the process's life.
pub async fn watch_jumps(allocated: fn() -> Option<usize>, jump: usize) {
    const EVERY: Duration = Duration::from_millis(250);
    const WINDOW: Duration = Duration::from_secs(5);
    let mut seen: VecDeque<(Instant, usize)> = VecDeque::new();
    let mut said = Instant::now() - Duration::from_secs(60);
    loop {
        tokio::time::sleep(EVERY).await;
        let Some(bytes) = allocated() else { return };
        let at = Instant::now();
        while seen.front().is_some_and(|(t, _)| at - *t > WINDOW) {
            seen.pop_front();
        }
        seen.push_back((at, bytes));
        let (from_at, from) = seen
            .iter()
            .min_by_key(|(_, b)| *b)
            .copied()
            .unwrap_or((at, bytes));
        if bytes >= from + jump && said.elapsed() >= Duration::from_secs(10) {
            said = at;
            eprintln!(
                "memory jump: allocated {} MB -> {} MB in {:.1} s; running: {}",
                from >> 20,
                bytes >> 20,
                (at - from_at).as_secs_f64(),
                now(12)
            );
        }
    }
}
