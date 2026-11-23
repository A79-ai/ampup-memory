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
//!   [`STALE`]. Lost nothing: main's [`crate::Vis::removals`] is the same, so is the scope's loss count
//!   ([`crate::losses`], counted by every forget, delete, retraction, narrowed ACL, retired version, retention
//!   policy, purge and offboarding made here, and by every loss a build of main's state finds, what aged out
//!   included), so is the facts table's generation (it moves when an erasure or a forget rewrites facts away),
//!   and so is everything else the answer pins. Otherwise the answer is worked out again on that read, so it
//!   never shows what was taken away.
//! - The loss count and the facts table are read before the state the answer reads is pinned ([`Mark`]): a
//!   removal that ends while an answer is worked out leaves that answer behind.
//! - An answer is one reader's: its key names the reader exactly (who, and every group they read as), so no reader
//!   is ever answered what was worked out for another, wider or narrower.
//! - A change this server makes to what these read (a conflict resolved, a proposal decided, a dream decision
//!   undone or applied, an owner set) drops the scope's answers ([`changed`]): its maker's next read is worked out
//!   on it. Another server's change reaches this one's readers as any write does, within [`STALE`].
//! - An answer is worked out once at a time: a read that finds the reader's being worked out waits for it rather
//!   than working it out again, if it is at the same state, or (a read that may lag) at one an answer kept from
//!   would be served at, as above. The work runs on its own, so a request that
//!   stops waiting (a page closed, a read answered without it) doesn't stop it: the next read finds it kept.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::watch;

/// How old a kept answer may be and still be answered while the memory moved on (it never is past a loss).
pub const STALE: Duration = Duration::from_secs(600);

tokio::task_local! {
    /// Set while an answer is worked out ahead of its reads ([`exact`]).
    static EXACT: ();
}

/// `fut`, an answer worked out ahead of the reads that want it (and kept as at the state it read): the kept answers
/// it reads are each worked out at that state, never one from before it answered while the next is worked out
/// ([`Kept::get`]'s `lag`), or what it keeps would be one state behind and taken for current.
pub async fn exact<F: Future>(fut: F) -> F::Output {
    EXACT.scope((), fut).await
}

/// The [`STALE`]-long span of time now falls in: in the key of an answer over a window that ends now (the last 30 days),
/// so a kept one is never answered for a window that ended more than [`STALE`] ago.
pub fn span() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / STALE.as_secs()
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
    pub async fn read(store: &crate::Store, scope: &crate::Scope) -> Result<Mark> {
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
        Ok(Log(store
            .side_rows(&scope.namespace(), crate::QA_LOG)
            .await?))
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
        }
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
    fn lost_nothing(&self, now: &At) -> bool {
        self.removals == now.removals
            && self.mark.losses == now.mark.losses
            && self.mark.facts.1 == now.mark.facts.1
            && self.pinned == now.pinned
            && match (&self.log, &now.log) {
                (None, None) => true,
                (Some(was), Some(now)) => was.kept_in(now),
                _ => false,
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

fn generation(ns: &str) -> u64 {
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
    /// Answers kept at most; past it, those older than [`STALE`] go, then the oldest.
    max: usize,
    /// Whether an answer may be kept: one that isn't is still answered to the reads that waited for it.
    keepable: fn(&T) -> bool,
}

impl<T: Send + Sync + 'static> Kept<T> {
    pub fn new(max: usize) -> Self {
        Self::new_if(max, |_| true)
    }

    /// [`Kept::new`], keeping only the answers `keepable` passes (a search ranked without its vectors, say).
    pub fn new_if(max: usize, keepable: fn(&T) -> bool) -> Self {
        Kept {
            entries: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            max,
            keepable,
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
        let lag = lag && EXACT.try_with(|_| ()).is_err();
        {
            let mut kept = self.entries.lock().unwrap();
            if let Some(e) = kept.get_mut(&key)
                && e.generation == generation
            {
                if e.at == at {
                    return Ok(e.answer.clone());
                }
                if lag && e.at.lost_nothing(&at) && e.made.elapsed() < STALE {
                    if !e.refreshing {
                        e.refreshing = true;
                        let (job, ns) = (work(), ns.to_string());
                        tokio::spawn(async move {
                            match job.await {
                                Ok(answer) => self.keep(&ns, key, at, generation, Arc::new(answer)),
                                Err(err) => {
                                    eprintln!("kept answer in {ns} not refreshed: {err:#}");
                                    if let Some(e) = self.entries.lock().unwrap().get_mut(&key) {
                                        e.refreshing = false;
                                    }
                                }
                            }
                        });
                    }
                    return Ok(e.answer.clone());
                }
            }
        }
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
                    let (job, ns) = (work(), ns.to_string());
                    tokio::spawn(async move {
                        let got = job.await.map(Arc::new);
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
        let key = format!("{ns}\n{reader}");
        let kept = self.entries.lock().unwrap();
        let e = kept.get(&key).filter(|e| e.generation == generation(ns))?;
        let exact = e.at == *at;
        (exact || e.at.lost_nothing(at) && e.made.elapsed() < STALE)
            .then(|| (e.answer.clone(), exact, e.made.elapsed()))
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
            kept.retain(|_, e| e.made.elapsed() < STALE);
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
