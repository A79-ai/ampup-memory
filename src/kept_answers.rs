//! Answers kept per reader and memory state, for the reads that go through every fact or record a reader may read
//! to show a page: the review lists (conflicts, proposals, the dream's decisions), the lessons and their count,
//! the entity graph and the entities listed by type.
//!
//! - At the same memory state ([`At`]), a reader's answer is the same: it is kept, and answered again at once.
//! - The state is main's visibility state itself (its build, [`crate::Vis::build`], beside its heads) and the facts
//!   table's snapshot as this server reads them (and the answer log's rows, for an answer that reads them: [`Log`]): an answer read before the facts table, or the tables the
//!   visibility state is built from (an episode's record, an ACL, a decision), caught up with a commit is worked
//!   out again once they have. Heads alone don't say it: a state built while the episodes table lagged the ledger
//!   has a commit's heads without its episodes.
//! - Once main moved on, a reader's last answer is answered at once while the new state's is worked out in the
//!   background for the next read, but only while memory lost nothing since it was worked out, and for at most
//!   [`STALE`], or [`SUMMARY_STALE`] for a page's summary ([`Kept::summary`]: a return visit within the hour is
//!   answered at once), or however long ago it was for a Studio page's read ([`Kept::however_old`]: what was only
//!   added since, episodes, facts, closures, quotes, decisions or sessions logged, never keeps its reader waiting,
//!   on a page visited every few hours as on one visited every minute). Lost nothing: main's [`crate::Vis::removals`] is the same, so is the scope's loss count
//!   ([`crate::losses`], counted by every forget, delete, retraction, narrowed ACL, retired version, retention
//!   policy, purge and offboarding made here, and by every loss a build of main's state finds, what aged out
//!   included), so is the facts table's generation (it moves when an erasure or a forget rewrites facts away),
//!   and so is everything else the answer pins. Otherwise the answer is worked out again on that read, so it
//!   never shows what was taken away.
//! - The loss count and the facts table are read before the state the answer reads is pinned ([`Mark`]): a
//!   removal that ends while an answer is worked out leaves that answer behind.
//! - An answer kept here is one reader's: its key names the reader exactly (who, and every group they read as), so
//!   no reader is ever answered what was worked out for another, wider or narrower. An answer shared between
//!   servers ([`crate::shared_answers`]) is too, unless `AMPUP_MEMORY_SHARED_ANSWERS_CLASSES` is on: then a part
//!   that reads its reader only through what the ACLs let them read is shared by the readers who hold the same of
//!   the ACL entries the states it reads name, who read exactly the same of them, and a part that reads nothing of
//!   its reader by every reader of the scope ([`crate::shared_answers::class_for`]). A part that reads who its reader
//!   is (their sessions, their own advice, what they own, their standing) stays one reader's.
//! - A change this server makes to what these read (a conflict resolved, a proposal decided, a dream decision
//!   undone or applied, an owner set) drops the scope's answers ([`changed`]): its maker's next read is worked out
//!   on it. Another server's change reaches this one's readers as any write does: within [`STALE`], or on the
//!   read after the next for an answer kept however old ([`Kept::however_old`]).
//! - An answer over a window that ends now (the last 30 days, [`At::windowed`]) is answered as it is for
//!   [`WINDOW_FRESH`] at the same state; past it, it is answered while the next is worked out in the background, as
//!   when main moved on: its window never ends more than its kind's [`STALE`] (or [`SUMMARY_STALE`]) before now.
//! - The answers worked out again in the background while an older one is answered take turns across the server,
//!   a few at a time behind the reads people wait on ([`REFRESHES_AT_ONCE`]).
//! - An answer is worked out once at a time: a read that finds the reader's being worked out waits for it rather
//!   than working it out again, if it is at the same state, or (a read that may lag) at one an answer kept from
//!   would be served at, as above. The work runs on its own, so a request that
//!   stops waiting (a page closed, a read answered without it) doesn't stop it: the next read finds it kept.
//! - An answer served from an earlier state than the one asked for (while the next is worked out) is noted
//!   ([`not_sharable`]): an answer built from it is not one to share with other servers under the asked state's key
//!   ([`crate::shared_answers`]), only to show.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::watch;

/// `work`, run on a task of its own, with the asker's reads of main served as theirs are past quotes, closures and
/// decisions ([`crate::lagging_past_endings`], Studio's Home), so the work doesn't wait for the build they skipped.
fn past_endings<F: Future>(
    work: F,
) -> tokio::task::futures::TaskLocalFuture<std::cell::Cell<bool>, F> {
    crate::lagging_past_endings(crate::lags_past_endings(), work)
}

/// How long an answer's work may take before [`timed_work`] says where it went.
const SLOW_WORK: Duration = Duration::from_secs(1);

/// `job`, an answer's work run in the background, with its steps timed ([`crate::phases`]): the reads that wait on it
/// see only how long they waited, so work taking [`SLOW_WORK`] or longer logs its own steps (a wait for a listing's
/// turn among them). `why`: a read waiting on it, or a refresh while a kept answer is served.
async fn timed_work<T>(ns: &str, why: &str, job: impl Future<Output = Result<T>>) -> Result<T> {
    let began = Instant::now();
    // Boxed: the work is large, and a spawned task's future is built on the stack before it is moved to the heap.
    let (got, timed) = crate::phases::timed(Box::pin(job)).await;
    let took = began.elapsed();
    if took >= SLOW_WORK {
        eprintln!(
            "kept answer in {ns} worked out in {} ms ({why}{}): {}",
            took.as_millis(),
            if got.is_err() { ", failed" } else { "" },
            timed.marks
        );
    }
    got
}

/// How many kept answers are worked out again in the background at once across the server, while an older one is
/// answered ([`Kept::get`]): a visit a while after the last finds every answer its pages read behind main, and
/// their refreshes, all started at once, held the workers that visit's own reads were waiting for (six at once, 1-6 s
/// each, on staging). Each waits its turn; the reads asked meanwhile are answered the older answer, as before.
const REFRESHES_AT_ONCE: usize = 2;

/// The turns of [`REFRESHES_AT_ONCE`].
static REFRESH_TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(REFRESHES_AT_ONCE);

/// How old a kept answer may be and still be answered while the memory moved on, its next worked out in the
/// background meanwhile (it never is past a loss): a review list's or a listing's ([`Kept::new`]).
pub const STALE: Duration = Duration::from_secs(600);

/// [`STALE`] for a page's summary ([`Kept::summary`]: Home's, Advice's and Learned's counts and lists), whose numbers
/// may lag what was added since: a return visit within the hour is answered at once.
pub const SUMMARY_STALE: Duration = Duration::from_secs(3600);

/// How old a kept answer over a window that ends now ([`At::windowed`]) may be and still be answered as it is at the
/// same state: past it, it is answered while the next is worked out in the background, up to its kind's [`STALE`].
pub const WINDOW_FRESH: Duration = Duration::from_secs(600);

tokio::task_local! {
    /// Set within [`sharable`]: whether what the task worked out may be shared ([`not_sharable`]).
    static SHARABLE: Arc<std::sync::atomic::AtomicBool>;
}

/// `fut`'s output, and whether it may be shared with other servers under the key of the state it was asked at
/// ([`crate::shared_answers`]): not when anything it read answered from an earlier state while the next was worked
/// out, or failed ([`not_sharable`]).
pub async fn sharable<F: Future>(fut: F) -> (F::Output, bool) {
    let ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let out = SHARABLE.scope(ok.clone(), fut).await;
    (out, ok.load(std::sync::atomic::Ordering::Relaxed))
}

/// What this task is working out is not to be shared ([`sharable`]): it read an answer from an earlier state than
/// the one asked for (a kept answer served while the next is worked out), or a read failed. Nothing outside
/// [`sharable`].
pub fn not_sharable() {
    let _ = SHARABLE.try_with(|ok| ok.store(false, std::sync::atomic::Ordering::Relaxed));
}

/// What is read of a scope before the state an answer reads is pinned: its loss count and its facts table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// [`crate::losses`] of the scope.
    losses: u64,
    /// The facts table's snapshot and generation ([`crate::Store::facts_mark`]).
    facts: (i64, u64),
}

impl Mark {
    /// Read before pinning the state: a removal that ends after it moves the loss count, so what is worked out
    /// from a state pinned before the removal ended is never answered after it.
    ///
    /// A process's first build of main's state counts a loss (it can't tell one from none), so a mark read before it
    /// would never match the count again: the first answer worked out on a fresh server, or once its state was let
    /// go, would not be kept, and every read stayed cold until a background refresh ran. That build comes first,
    /// then the mark: the answer reads the state it built, which holds whatever it counted.
    pub async fn read(store: &crate::Store, scope: &crate::Scope) -> Result<Mark> {
        if !store.holds_visibility(scope, crate::MAIN) {
            // A read that fails to build it fails on its own read of the state, as it would have.
            let _ = store.lagging_visibility(scope, crate::MAIN).await;
        }
        let losses = crate::losses(&scope.namespace());
        Ok(Mark {
            losses,
            facts: store.facts_mark(scope).await?,
        })
    }
}

/// The memory state an answer was worked out at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct At {
    /// The heads of the branch read (sorted, as [`crate::Vis::heads`] has them).
    pub heads: Vec<String>,
    /// The visibility state read ([`crate::Vis::build`]): the same heads can be built into states that hold
    /// different things while the tables they are built from catch up with each other.
    vis: u64,
    mark: Mark,
    /// Main's [`crate::Vis::removals`]: an answer is never served past a change in it.
    pub removals: u64,
    /// Whatever else the answer read that must not have moved for its last answer to be served (other branches'
    /// heads, for a list of the proposals on them). Empty when nothing else.
    pub pinned: Vec<String>,
    /// The scope's answer log as read before the answer was worked out, for an answer that reads it ([`Log`]).
    log: Option<Log>,
    /// Other side tables the answer reads that are only added to except by a removal (the votes), as read before it
    /// was worked out ([`At::reading`]): a batch of one gone since is a loss, as for the answer log.
    tables: Vec<Log>,
    /// Whether the answer is over a window that ends when it is worked out ([`At::windowed`]).
    window: bool,
}

/// The rows of a scope's answer log (`qa_log`) as this server holds them, held so no batch of them is freed (and its
/// identity taken by another) while an answer worked out from them is kept. The log is only ever added to except by
/// a removal (an offboarding, a retention policy, an erasure, here or on another server), which rewrites the files
/// it removed rows from: a log that still holds every batch an answer's log held lost nothing since.
#[derive(Clone)]
pub struct Log(Arc<Vec<arrow_array::RecordBatch>>);

impl Log {
    /// The log of `scope` as the next read of it would have it.
    pub async fn read(store: &crate::Store, scope: &crate::Scope) -> Result<Log> {
        Self::read_table(store, scope, crate::QA_LOG).await
    }

    /// Side table `name` of `scope` (one only added to except by a removal, as the log is) as the next read of it
    /// would have it: for [`At::reading`].
    pub async fn read_table(store: &crate::Store, scope: &crate::Scope, name: &str) -> Result<Log> {
        Ok(Log(store.side_rows(&scope.namespace(), name).await?))
    }

    /// Each batch's identity: its first column, shared by every read of the same data file.
    fn ids(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = self
            .0
            .iter()
            .filter(|b| b.num_columns() > 0)
            .map(|b| Arc::as_ptr(b.column(0)) as *const () as usize)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Whether `now` still holds every batch this held.
    fn kept_in(&self, now: &Log) -> bool {
        if Arc::ptr_eq(&self.0, &now.0) {
            return true;
        }
        let now = now.ids();
        self.ids().iter().all(|b| now.binary_search(b).is_ok())
    }
}

/// The same rows: the same batches, though a refresh of the log may have read them into a new list.
impl PartialEq for Log {
    fn eq(&self, other: &Log) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.ids() == other.ids()
    }
}

impl Eq for Log {}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Log({} batches)", self.0.len())
    }
}

impl At {
    /// Main's state as `vis` has it, pinned after `mark` was read, nothing else pinned.
    pub fn of(mark: Mark, vis: &crate::Vis) -> At {
        At {
            heads: vis.heads().to_vec(),
            vis: vis.build(),
            mark,
            removals: vis.removals,
            pinned: vec![],
            log: None,
            tables: vec![],
            window: false,
        }
    }

    /// For an answer over a window that ends when it is worked out (the last 30 days): at the same state it is
    /// answered as it is for [`WINDOW_FRESH`], then while the next is worked out, up to its kind's [`STALE`].
    pub fn windowed(self) -> At {
        At {
            window: true,
            ..self
        }
    }

    /// The same state, for an answer that also reads side table `table` ([`Log::read_table`], read before the answer
    /// is worked out, after the mark): answered again at once only while it holds the same rows, and while the
    /// next is worked out only while it lost none of them. A removal another server made (a person forgotten there
    /// whose votes were all this scope held of them) counts no loss here, but takes its batches away.
    pub fn reading(mut self, table: Log) -> At {
        self.tables.push(table);
        self
    }

    /// Whether an answer worked out at `self` `age` ago is answered as it is at `now`: the same state, and for an
    /// answer over a window, young enough.
    fn current(&self, now: &At, age: Duration) -> bool {
        self == now && (!now.window || age < WINDOW_FRESH)
    }

    /// As [`At::of`], for an answer that also reads the scope's answer log, `log` as read before it is worked out
    /// (after `mark`): it is answered again at once only at the same log, and while it is worked out again only
    /// when the log lost nothing since ([`Log`]).
    pub fn with_log(mark: Mark, vis: &crate::Vis, log: Log) -> At {
        At {
            log: Some(log),
            ..At::of(mark, vis)
        }
    }

    /// Whether memory lost nothing between `self` and `now` (the same removals, loss count, facts generation and
    /// pins): only then may an answer worked out at `self` be answered at `now` while `now`'s is worked out.
    pub fn lost_nothing(&self, now: &At) -> bool {
        self.lost(now).is_none()
    }

    /// What [`At::lost_nothing`] found moved between `self` and `now`, as a read's step names it ([`Kept::get`]):
    /// none when nothing did.
    fn lost(&self, now: &At) -> Option<&'static str> {
        if self.removals != now.removals {
            return Some("kept answer not served: main lost something");
        }
        if self.mark.losses != now.mark.losses {
            return Some("kept answer not served: a loss counted");
        }
        if self.mark.facts.1 != now.mark.facts.1 {
            return Some("kept answer not served: facts rewritten");
        }
        if self.pinned != now.pinned {
            return Some("kept answer not served: its pins moved");
        }
        if self.tables.len() != now.tables.len()
            || self
                .tables
                .iter()
                .zip(&now.tables)
                .any(|(was, now)| !was.kept_in(now))
        {
            return Some("kept answer not served: a table it read lost rows");
        }
        match (&self.log, &now.log) {
            (None, None) => None,
            (Some(was), Some(now)) if was.kept_in(now) => None,
            _ => Some("kept answer not served: the answer log lost rows"),
        }
    }
}

struct Entry<T> {
    at: At,
    made: Instant,
    /// The scope's [`changed`] count it was worked out under.
    generation: u64,
    answer: Arc<T>,
    refreshing: bool,
}

/// How many times this server changed each scope's reviewed memory: an answer worked out before the last is not
/// answered again.
static GENERATIONS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);

/// How many times this server changed scope namespace `ns`'s reviewed memory ([`changed`]): read before an answer
/// kept with [`Kept::put`] is worked out.
pub fn generation(ns: &str) -> u64 {
    GENERATIONS.lock().unwrap().get(ns).copied().unwrap_or(0)
}

/// This server changed what the kept answers of scope namespace `ns` read: each is worked out again on its next read.
pub fn changed(ns: &str) {
    *GENERATIONS
        .lock()
        .unwrap()
        .entry(ns.to_string())
        .or_default() += 1;
}

/// An answer being worked out, for the reads that want the same.
struct Running<T> {
    at: At,
    generation: u64,
    /// Its outcome once worked out (a failure, said).
    done: watch::Receiver<Option<Result<Arc<T>, String>>>,
}

/// One kind of kept answer, by scope and reader.
pub struct Kept<T> {
    entries: Mutex<HashMap<String, Entry<T>>>,
    running: Mutex<HashMap<String, Running<T>>>,
    /// Answers kept at most; past it, those older than `stale` go, then the oldest.
    max: usize,
    /// How old an answer may be and still be answered while the memory moved on: [`STALE`] or [`SUMMARY_STALE`].
    stale: Duration,
    /// Whether an answer may be kept: one that isn't is still answered to the reads that waited for it.
    keepable: fn(&T) -> bool,
    /// Whether [`Kept::get`] answers a kept answer that lost nothing however old it is ([`Kept::however_old`]).
    any_age: bool,
}

impl<T: Send + Sync + 'static> Kept<T> {
    pub fn new(max: usize) -> Self {
        Self::new_if(max, |_| true)
    }

    /// [`Kept::new`], for a page's summary: answered while the memory moved on for up to [`SUMMARY_STALE`].
    pub fn summary(max: usize) -> Self {
        Kept {
            stale: SUMMARY_STALE,
            ..Self::new(max)
        }
    }

    /// [`Kept::new`], keeping only the answers `keepable` passes (a search ranked without its vectors, say).
    pub fn new_if(max: usize, keepable: fn(&T) -> bool) -> Self {
        Kept {
            entries: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            max,
            stale: STALE,
            keepable,
            any_age: false,
        }
    }

    /// These answers, answered by [`Kept::get`] once main moved on while memory lost nothing since, however old,
    /// the next worked out in the background meanwhile: a Studio page's read, visited every few hours, never waits
    /// on what was only added since. A loss makes the next read work its answer out, as for any kept answer;
    /// `stale` still bounds [`Kept::peek`] and says which answers go first when the kept answers are full.
    pub fn however_old(self) -> Self {
        Kept {
            any_age: true,
            ..self
        }
    }

    /// The answer for `reader` (a key naming the reader exactly, and whatever else changes the answer) in scope
    /// namespace `ns` at `at`: kept, or worked out by `work`. `lag`: a lagging answer may be served while the next
    /// is worked out (a read of main as it is now; never a page pinned to a state).
    pub async fn get<F, Fut>(
        &'static self,
        ns: &str,
        reader: &str,
        at: At,
        lag: bool,
        work: F,
    ) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let key = format!("{ns}\n{reader}");
        let generation = generation(ns);
        // Why the answer is worked out on this read, when it is: a step of the read ([`crate::phases`]).
        let why = {
            let mut kept = self.entries.lock().unwrap();
            match kept.get_mut(&key) {
                None => "kept answer not served: none kept",
                Some(e) if e.generation != generation => {
                    "kept answer not served: changed here since"
                }
                Some(e) => {
                    let age = e.made.elapsed();
                    if e.at.current(&at, age) {
                        crate::metrics::KEPT_ANSWERS_SERVED.inc();
                        return Ok(e.answer.clone());
                    }
                    let lost = match e.at == at {
                        true => None,
                        false if !lag => Some("kept answer not served: a pinned state moved"),
                        false => e.at.lost(&at),
                    };
                    match lost {
                        Some(why) => why,
                        None if !self.any_age && age >= self.stale => {
                            "kept answer not served: too old"
                        }
                        None => {
                            // Not this state's answer as it is now: shown while the next is worked out, never shared.
                            not_sharable();
                            if !e.refreshing {
                                e.refreshing = true;
                                let (job, ns) = (past_endings(work()), ns.to_string());
                                let queued = Instant::now();
                                // Behind the reads people wait on, [`REFRESHES_AT_ONCE`] at a time.
                                tokio::spawn(crate::loads::later(async move {
                                    let _turn = REFRESH_TURNS.acquire().await;
                                    // Worked out meanwhile (by a read that couldn't be answered the older one): not
                                    // again.
                                    if let Some(e) = self.entries.lock().unwrap().get_mut(&key)
                                        && (e.made > queued || e.at == at)
                                    {
                                        e.refreshing = false;
                                        return;
                                    }
                                    match timed_work(&ns, "refresh", job).await {
                                        Ok(answer) => {
                                            self.keep(&ns, key, at, generation, Arc::new(answer))
                                        }
                                        Err(err) => {
                                            eprintln!("kept answer in {ns} not refreshed: {err:#}");
                                            if let Some(e) =
                                                self.entries.lock().unwrap().get_mut(&key)
                                            {
                                                e.refreshing = false;
                                            }
                                        }
                                    }
                                }));
                            }
                            crate::metrics::KEPT_ANSWERS_SERVED.inc();
                            return Ok(e.answer.clone());
                        }
                    }
                }
            }
        };
        crate::phases::mark(why);
        self.worked_out(ns, key, at, generation, lag, work).await
    }

    /// The answer for `key` at `at` under `generation`, from the work already under way for it (at `at`, or with `lag`
    /// at a state it may be answered at), else from `work`, started now and kept once done.
    async fn worked_out<F, Fut>(
        &'static self,
        ns: &str,
        key: String,
        at: At,
        generation: u64,
        lag: bool,
        work: F,
    ) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let mut done = {
            let mut running = self.running.lock().unwrap();
            match running.get(&key) {
                Some(r)
                    if r.generation == generation
                        && (r.at == at || lag && r.at.lost_nothing(&at)) =>
                {
                    if r.at != at {
                        not_sharable();
                    }
                    crate::phases::mark("kept answer being worked out: waited for it");
                    r.done.clone()
                }
                _ => {
                    let (tx, rx) = watch::channel(None);
                    running.insert(
                        key.clone(),
                        Running {
                            at: at.clone(),
                            generation,
                            done: rx.clone(),
                        },
                    );
                    let (job, ns) = (past_endings(work()), ns.to_string());
                    tokio::spawn(async move {
                        let got = timed_work(&ns, "read", job).await.map(Arc::new);
                        if let Ok(answer) = &got {
                            self.keep(&ns, key.clone(), at.clone(), generation, answer.clone());
                        }
                        let mut running = self.running.lock().unwrap();
                        if running
                            .get(&key)
                            .is_some_and(|r| r.at == at && r.generation == generation)
                        {
                            running.remove(&key);
                        }
                        drop(running);
                        let _ = tx.send(Some(got.map_err(|e| format!("{e:#}"))));
                    });
                    rx
                }
            }
        };
        let got = done
            .wait_for(Option::is_some)
            .await
            .map_err(|_| anyhow!("a kept answer's work stopped before it was worked out"))?;
        match got.as_ref() {
            Some(Ok(answer)) => Ok(answer.clone()),
            Some(Err(e)) => Err(anyhow!("{e}")),
            None => unreachable!("waited for an outcome"),
        }
    }

    /// The answer [`Kept::get`] with `lag` would answer at once for `reader` at `at`, if one is kept: nothing is
    /// worked out or started here. With it, whether it was worked out at `at` itself (not only at a state it may be
    /// answered at while the next is worked out) and how old it is.
    pub fn peek(&self, ns: &str, reader: &str, at: &At) -> Option<(Arc<T>, bool, Duration)> {
        self.peek_within(ns, reader, at, self.stale)
    }

    /// [`Kept::peek`], for an answer that may be up to `age` old while the memory moved on (a summary whose numbers
    /// may lag): never past a loss, as any kept answer.
    pub fn peek_within(
        &self,
        ns: &str,
        reader: &str,
        at: &At,
        age: Duration,
    ) -> Option<(Arc<T>, bool, Duration)> {
        let key = format!("{ns}\n{reader}");
        let kept = self.entries.lock().unwrap();
        let e = kept.get(&key).filter(|e| e.generation == generation(ns))?;
        let made = e.made.elapsed();
        let exact = e.at.current(at, made);
        let found = exact || e.at.lost_nothing(at) && made < age;
        if found && !exact {
            not_sharable();
        }
        found.then(|| (e.answer.clone(), exact, made))
    }

    /// Keeps `answer` for `reader`, worked out at `at` (its mark read before the state it read was pinned) under the
    /// scope's [`generation`] read before it was worked out: as an answer [`Kept::get`] worked out is kept (not when
    /// the scope changed here or lost something since).
    pub fn put(&self, ns: &str, reader: &str, at: At, generation: u64, answer: T) {
        self.keep(
            ns,
            format!("{ns}\n{reader}"),
            at,
            generation,
            Arc::new(answer),
        );
    }

    /// The answer for `reader` at `at` worked out again by `work`, whatever is kept (one already being worked out at
    /// `at` is waited for instead), and kept as [`Kept::get`] keeps one: for a loop that works answers out ahead of
    /// the reads that want them.
    pub async fn renew<F, Fut>(
        &'static self,
        ns: &str,
        reader: &str,
        at: At,
        work: F,
    ) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let key = format!("{ns}\n{reader}");
        let generation = generation(ns);
        self.worked_out(ns, key, at, generation, false, work).await
    }

    /// Keeps `answer`, worked out at `at` under `generation`: unless the scope changed here since (the answer may
    /// not hold the change), or lost something since `at` was marked (it would never be answered again).
    fn keep(&self, ns: &str, key: String, at: At, generation: u64, answer: Arc<T>) {
        if self::generation(ns) != generation
            || crate::losses(ns) != at.mark.losses
            || !(self.keepable)(&answer)
        {
            if let Some(e) = self.entries.lock().unwrap().get_mut(&key) {
                e.refreshing = false;
            }
            return;
        }
        let mut kept = self.entries.lock().unwrap();
        if kept.len() >= self.max && !kept.contains_key(&key) {
            kept.retain(|_, e| e.made.elapsed() < self.stale);
            if kept.len() >= self.max
                && let Some(oldest) = kept
                    .iter()
                    .min_by_key(|(_, e)| e.made)
                    .map(|(k, _)| k.clone())
            {
                kept.remove(&oldest);
            }
        }
        kept.insert(
            key,
            Entry {
                at,
                made: Instant::now(),
                generation,
                answer,
                refreshing: false,
            },
        );
    }
}

/// A reader as a key: who they are and every group they read as, so two readers share an answer only when they
/// read exactly the same.
pub fn reader_key(who: &crate::Principal) -> String {
    let mut tokens = who.tokens().to_vec();
    tokens.sort();
    format!("{}\n{}", who.user(), tokens.join(","))
}
