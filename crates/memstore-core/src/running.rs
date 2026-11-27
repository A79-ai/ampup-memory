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

/// How many rows make a synchronous pass over them long enough for [`cpu`] to hand its worker's other tasks on:
/// a pass over 10,000 ledger rows or log rows takes milliseconds; staging's ampup ledger, hundreds of thousands of
/// commits, took 1.0-1.3 s a pass and held an async worker that long, about once a minute.
pub const CPU_ROWS: usize = 10_000;

/// Runs `f`, a synchronous pass that may take a while of CPU (a walk of a large ledger, a decode of a large log),
/// without holding an async worker when `big`: on a multi-threaded runtime the worker's other tasks, its timers
/// among them, are handed to another thread while it runs ([`tokio::task::block_in_place`]), so a stall watch beat,
/// a health probe or a read waiting behind it on that worker still runs. Otherwise (small, outside a runtime, or on
/// a single-threaded one, as tests are) it just runs.
pub fn cpu<T>(big: bool, f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match big
        && Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread)
    {
        true => tokio::task::block_in_place(f),
        false => f(),
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

/// When the async runtime last ran [`watch_stalls`]'s beat, in ms since [`BORN`]. A request waits for a worker while
/// none is free, before its own timing starts, so a stall shows in no request's phases, only in every caller's wait.
static BEAT: AtomicU64 = AtomicU64::new(0);
static BORN: LazyLock<Instant> = LazyLock::new(Instant::now);

fn ms() -> u64 {
    BORN.elapsed().as_millis() as u64
}

/// Watches the async runtime for stalls of `at_least` or longer: a task beats every 100 ms, and a thread of its own
/// (never one of the runtime's) notices when the beats stop, because no worker was free to run it. Each stall is one
/// line when it ends: how long, the process's CPU in it and the cgroup's throttling (CPU-bound, throttled, or
/// waiting), what the threads were doing and what was running when it began. One more line if it goes on past 10 s.
/// Call it from inside the runtime; it runs for the process's life.
pub fn watch_stalls(at_least: Duration) {
    LazyLock::force(&BORN);
    stacks::install();
    BEAT.store(ms(), Ordering::Relaxed);
    tokio::spawn(async {
        loop {
            BEAT.store(ms(), Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let at_least = at_least.as_millis() as u64;
    let spawned = std::thread::Builder::new()
        .name("stall-watch".into())
        .spawn(move || {
            // The CPU and throttling at the last tick the runtime was keeping up, and when: a stall's are counted
            // from there, not from when it was noticed, at least `at_least` into it.
            let mut healthy = (ms(), Usage::now());
            // The stall under way: the last beat before it, the reading before it, and what it found.
            let mut stall: Option<(u64, (u64, Usage), String, bool)> = None;
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let beat = BEAT.load(Ordering::Relaxed);
                let lag = ms().saturating_sub(beat);
                match &mut stall {
                    None if lag >= at_least => {
                        let found = format!(
                            "threads: {}; running: {}{}",
                            threads(),
                            now(12),
                            stacks::of_running_workers()
                        );
                        let before = std::mem::replace(&mut healthy, (ms(), Usage::now()));
                        stall = Some((beat, before, found, false));
                    }
                    None if lag < 250 => healthy = (ms(), Usage::now()),
                    Some((from, _, found, said)) if !*said && lag >= 10_000 => {
                        *said = true;
                        eprintln!(
                            "runtime stalled for {} ms so far: a task waits that long for an async worker ({found})",
                            ms().saturating_sub(*from)
                        );
                    }
                    Some((from, (read_at, usage), found, _)) if beat > *from => {
                        let took = beat.saturating_sub(*from);
                        eprintln!(
                            "runtime stalled {took} ms: a task waited that long for an async worker ({}; {found})",
                            usage.since(ms().saturating_sub(*read_at))
                        );
                        stall = None;
                        healthy = (ms(), Usage::now());
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("runtime stalls aren't watched: {e}");
    }
}

/// The process's CPU time and its cgroup's CFS throttling, read together.
struct Usage {
    cpu_us: Option<u64>,
    throttled: Option<(u64, u64)>,
}

impl Usage {
    fn now() -> Usage {
        Usage {
            cpu_us: cpu_us(),
            throttled: throttled(),
        }
    }

    /// What happened between this reading and now, `took` ms later.
    fn since(&self, took: u64) -> String {
        let later = Usage::now();
        let mut out = vec![];
        if let (Some(a), Some(b)) = (self.cpu_us, later.cpu_us) {
            let used = b.saturating_sub(a) as f64 / 1e6;
            out.push(format!(
                "cpu {used:.1} s over {:.1} s, {:.1} cores",
                took as f64 / 1e3,
                used / (took.max(1) as f64 / 1e3)
            ));
        }
        if let (Some((n0, us0)), Some((n1, us1))) = (self.throttled, later.throttled) {
            out.push(format!(
                "throttled {} ms in {} periods",
                us1.saturating_sub(us0) / 1000,
                n1.saturating_sub(n0)
            ));
        }
        match out.is_empty() {
            true => "cpu unknown".into(),
            false => out.join(", "),
        }
    }
}

/// The CPU time this process has used, user and system, in microseconds.
fn cpu_us() -> Option<u64> {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes only the struct it is given.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return None;
    }
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    Some(us(u.ru_utime) + us(u.ru_stime))
}

/// The cgroup's CFS throttling so far: periods throttled and microseconds throttled (cgroup v2, else v1).
fn throttled() -> Option<(u64, u64)> {
    for (file, time_key, ns) in [
        ("/sys/fs/cgroup/cpu.stat", "throttled_usec", false),
        (
            "/sys/fs/cgroup/cpu,cpuacct/cpu.stat",
            "throttled_time",
            true,
        ),
        ("/sys/fs/cgroup/cpu/cpu.stat", "throttled_time", true),
    ] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let get = |k: &str| {
            text.lines().find_map(|l| {
                l.strip_prefix(k)?
                    .strip_prefix(' ')?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        };
        if let (Some(n), Some(t)) = (get("nr_throttled"), get(time_key)) {
            return Some((n, if ns { t / 1000 } else { t }));
        }
    }
    None
}

/// The process's threads by name (digits dropped, so a pool's threads count together), each as how many are
/// running or runnable, and how many are waiting (`S`, or `D` in the kernel), the busiest pools first. Of a pool's
/// waiting threads, those blocked on one lock (two or more in a futex wait on the same address) are told apart:
/// an idle async worker parks on a condition of its own, so seven workers "waiting" may be idle or may all wait for
/// one lock that a running thread holds. Linux only.
fn threads() -> String {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return "unknown".into();
    };
    let mut by: HashMap<String, (usize, usize, usize)> = HashMap::new();
    // Each pool's threads in a futex wait, by the address they wait on.
    let mut futex: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for e in dir.flatten() {
        let p = e.path();
        let (Ok(comm), Ok(stat)) = (
            std::fs::read_to_string(p.join("comm")),
            std::fs::read_to_string(p.join("stat")),
        ) else {
            continue;
        };
        let name: String = comm
            .trim()
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '_')
            .to_string();
        let state = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].trim_start().chars().next());
        if !matches!(state, Some('R'))
            && let Some(addr) = futex_wait(&p)
        {
            *futex
                .entry(name.clone())
                .or_default()
                .entry(addr)
                .or_default() += 1;
        }
        let n = by.entry(name).or_default();
        match state {
            Some('R') => n.0 += 1,
            Some('D') => n.2 += 1,
            _ => n.1 += 1,
        }
    }
    let mut all: Vec<_> = by.into_iter().collect();
    all.sort_by_key(|(name, n)| (std::cmp::Reverse(n.0), name.clone()));
    all.iter()
        .map(|(name, (r, s, d))| {
            let mut out = format!("{name} {r} running {s} waiting");
            if *d > 0 {
                out += &format!(" {d} in io");
            }
            // The most waiting on one address, when two or more do.
            if let Some(n) = futex
                .get(name)
                .and_then(|a| a.values().max().copied())
                .filter(|n| *n >= 2)
            {
                out += &format!(" ({n} on one lock)");
            }
            out
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The address a thread (its `/proc/self/task/<tid>` directory) waits on in a futex call, if it is in one: its
/// `syscall` file names the call's number and its arguments, the first being that address.
fn futex_wait(task: &std::path::Path) -> Option<String> {
    // futex's number: 202 on x86-64, 98 on arm64.
    const FUTEX: &str = if cfg!(target_arch = "aarch64") {
        "98"
    } else {
        "202"
    };
    let text = std::fs::read_to_string(task.join("syscall")).ok()?;
    let mut fields = text.split_whitespace();
    (fields.next()? == FUTEX).then(|| fields.next().map(str::to_string))?
}

/// What the async workers are doing in a stall, as their stacks: a stall whose task holds a worker without listing
/// itself ([`running`]) is named by the code it is in, and workers blocked on a lock it holds by where they wait.
/// When a stall is noticed, each async worker (at most sixteen) is sent a signal whose handler records its stack's
/// return addresses, and the watch's own thread names them. Once a stall. Linux only; off with
/// `AMPUP_MEMORY_STALL_STACKS=0`.
mod stacks {
    /// `; stacks: running leaf <- caller <- ... | ×7 waiting ...` for the async workers, those on a CPU first, the
    /// same stack once with its count; an idle worker (parked in the runtime, nothing of ours on its stack) isn't
    /// shown. Nothing when no stack was taken.
    pub fn of_running_workers() -> String {
        #[cfg(target_os = "linux")]
        {
            if !linux::on() {
                return String::new();
            }
            let mut seen: Vec<(String, usize)> = vec![];
            for (tid, running) in linux::workers().into_iter().take(16) {
                let Some(ips) = linux::stack_of(tid) else {
                    continue;
                };
                let named = name(&ips);
                if named == "unnamed" {
                    continue;
                }
                let s = format!("{} {named}", if running { "running" } else { "waiting" });
                match seen.iter_mut().find(|(k, _)| *k == s) {
                    Some(e) => e.1 += 1,
                    None => seen.push((s, 1)),
                }
            }
            if !seen.is_empty() {
                let all: Vec<String> = seen
                    .into_iter()
                    .map(|(s, n)| match n {
                        1 => s,
                        n => format!("×{n} {s}"),
                    })
                    .collect();
                return format!("; stacks: {}", all.join(" | "));
            }
        }
        String::new()
    }

    pub fn install() {
        #[cfg(target_os = "linux")]
        linux::install();
    }

    /// The most frames a stack shows, and the most characters of each.
    const FRAMES: usize = 14;
    const NAME: usize = 100;

    /// A stack's frames by name, innermost first: the signal's own and the runtime's plumbing (std, core, tokio,
    /// futures) left out, so what shows is the code that held the worker.
    #[allow(dead_code)]
    fn name(ips: &[usize]) -> String {
        let mut out: Vec<String> = vec![];
        for ip in ips {
            let mut here: Vec<String> = vec![];
            backtrace::resolve((*ip).saturating_sub(1) as *mut std::ffi::c_void, |s| {
                if let Some(n) = s.name() {
                    here.push(format!("{n:#}"));
                }
            });
            for n in here {
                let plumbing = [
                    "std::",
                    "core::",
                    "alloc::",
                    "tokio::",
                    "futures",
                    "backtrace::",
                ]
                .iter()
                .any(|p| n.starts_with(p) || n.starts_with(&format!("<{p}")))
                    || n.contains("running::stacks")
                    || n.starts_with("__")
                    || n.starts_with("_start")
                    || n.starts_with("start_thread")
                    || n.starts_with("clone");
                if plumbing || out.last() == Some(&n) {
                    continue;
                }
                out.push(n.chars().take(NAME).collect());
            }
            if out.len() >= FRAMES {
                break;
            }
        }
        match out.is_empty() {
            true => "unnamed".into(),
            false => out.join(" <- "),
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        const MAX: usize = 64;
        static IPS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
        static DEPTH: AtomicUsize = AtomicUsize::new(0);
        /// 0 idle, 1 asked, 2 being taken, 3 taken.
        static STATE: AtomicU32 = AtomicU32::new(0);
        static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

        fn signal() -> libc::c_int {
            libc::SIGRTMIN() + 6
        }

        pub fn on() -> bool {
            *INSTALLED.get().unwrap_or(&false)
        }

        pub fn install() {
            INSTALLED.get_or_init(|| {
                if std::env::var("AMPUP_MEMORY_STALL_STACKS").is_ok_and(|v| v == "0") {
                    return false;
                }
                // SAFETY: a zeroed sigaction with a handler that only reads its own stack and stores into atomics.
                unsafe {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = on_signal as *const () as usize;
                    sa.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut sa.sa_mask);
                    libc::sigaction(signal(), &sa, std::ptr::null_mut()) == 0
                }
            });
        }

        extern "C" fn on_signal(_: libc::c_int) {
            if STATE
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let mut n = 0;
            // SAFETY: walks this thread's own stack; nothing allocates, and the watch waits for it before reading.
            unsafe {
                backtrace::trace_unsynchronized(|f| {
                    IPS[n].store(f.ip() as usize, Ordering::Relaxed);
                    n += 1;
                    n < MAX
                });
            }
            DEPTH.store(n, Ordering::Relaxed);
            STATE.store(3, Ordering::Release);
        }

        /// The async workers by thread id, and whether each is on a CPU now (state `R`): those that are first.
        pub fn workers() -> Vec<(i32, bool)> {
            let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
                return vec![];
            };
            let mut all: Vec<(i32, bool)> = dir
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let comm = std::fs::read_to_string(p.join("comm")).ok()?;
                    if !comm.starts_with("tokio-rt-worker") {
                        return None;
                    }
                    let stat = std::fs::read_to_string(p.join("stat")).ok()?;
                    let state = stat
                        .rfind(')')
                        .and_then(|i| stat[i + 1..].trim_start().chars().next());
                    Some((e.file_name().to_str()?.parse().ok()?, state == Some('R')))
                })
                .collect();
            all.sort_by_key(|(_, running)| !running);
            all
        }

        /// `tid`'s stack's return addresses, innermost first, if it answered the signal within 200 ms.
        pub fn stack_of(tid: i32) -> Option<Vec<usize>> {
            STATE
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .ok()?;
            // SAFETY: tgkill sends a signal to one thread of this process; the handler is installed.
            let sent =
                unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, signal()) == 0 };
            let until = Instant::now() + Duration::from_millis(200);
            loop {
                match STATE.load(Ordering::Acquire) {
                    3 => break,
                    // Not taken (no answer in time, or never sent): take back the ask, unless the handler just began.
                    1 if !sent || Instant::now() >= until => {
                        if STATE
                            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            return None;
                        }
                    }
                    _ => std::thread::sleep(Duration::from_millis(1)),
                }
            }
            let n = DEPTH.load(Ordering::Relaxed);
            let ips = IPS[..n].iter().map(|a| a.load(Ordering::Relaxed)).collect();
            STATE.store(0, Ordering::Release);
            Some(ips)
        }
    }
}
