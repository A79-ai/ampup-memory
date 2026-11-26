//! Answers shared between servers and kept across restarts: a worked-out answer (one of Home's parts, say) stored
//! under a key that names the memory state it was worked out at durably, so any server that finds the same state
//! may answer it at once instead of reading every table again.
//!
//! - **The store** ([`SharedAnswers`]): `get`, `put` with a time to live, and `purge` of a scope's answers. Which
//!   one a process uses is set by `AMPUP_MEMORY_SHARED_ANSWERS` ([`handle`]): `off` (the default: none), `memory`
//!   (this process alone, [`InProcess`]) or `object` (the object store memory is kept in; installed by the server
//!   that has one, [`install`]).
//! - **The key** ([`last_key`]): `v1/<namespace>/<part>/<class>/last`, one answer per part and class, overwritten
//!   by each put (one per memory state piled up on a busy memory, and a server whose state had moved since the last
//!   put never found it). `<class>` is a hash of who the answer is for ([`class_for`]): never a name in an object's
//!   path. The answer is stored with the state it was worked out at ([`Last`]): a label of every table the part
//!   reads ([`PART_TABLES`], [`Snapshots::read`]: its Iceberg snapshot id, or for the ledger, which every scope
//!   commits to, the scope's own rows in it) and the hour.
//! - **When a stored answer is answered** ([`Last::holds`]): only with proof, from the tables alone, that memory lost
//!   nothing between its state and the state the server reading it would read now, and never past its expiry or
//!   across an hour's turn. Fail-closed: what can't be read or told refuses, and the answer is worked out as on a
//!   miss.
//!   - Every table a visibility state is built from ([`VIS`]) and the scope's rows in the ledger must be as they
//!     were. A removal there is mostly an *append*: a retraction or a withdrawal is a row added to the
//!     retractions; a forget or a narrowed ACL writes the episode again (the same id, as its id hashes its source and
//!     body, not its ACL), through the same append a new episode takes, and so may a plain write from a connector;
//!     a new version of a document with a narrower ACL hides the old one under a new id; a forget of a sentence is a
//!     quote added; a merge adds only a ledger row, and can bring a branch's rows written long before. Which
//!     appended rows take something away is told only against the state before them ([`crate::Store::main_lost_nothing`]),
//!     which a server that hasn't built that state doesn't have. So any write to them refuses: a stored answer is
//!     answered only while the scope committed nothing since it but the answer log's commits. Every answer logged
//!     (and every session's step) commits one, so a busy memory's ledger moves every few seconds; each makes only
//!     its own answer-log rows readable, and the ledger's rows are compared with those commits left out
//!     ([`without_answers`]).
//!   - The directory (one table for every org) must be as stored too: a member removed, an admin demoted or an
//!     org's setting turned off is a row written there, and which of them narrows a part's answer isn't told apart
//!     here. It moves rarely beside commits.
//!   - Every other table it reads (the facts, the summaries, the extraction records, the answer log) may have moved
//!     only by appends: each snapshot from the stored one to the current one is an Iceberg `append`
//!     ([`appended_since`]). Those take nothing away: a fact is read only once its commit is committed (a commit
//!     moves the ledger, above), and the summaries and the records only grow between rewrites. The answer log grows
//!     too: an admin's clear of empty sessions, or a name or grade one person gives another's entry, is a row
//!     added, which a stored answer's activity may not show until the next is worked out; nothing a reader may no
//!     longer read is in it (an erasure rewrites it). A rewrite (a compaction, an erasure, an offboarding) refuses,
//!     as does a stored snapshot no longer in the table's history (`AMPUP_MEMORY_MAINTAIN_KEEP` keeps the newest 10
//!     to 20). The process-local loss signals ([`crate::Vis::removals`], the facts generation) are none of it: each
//!     is a counter of one process, and a first build takes a fresh one.
//!   - Answered while moved, the answer is the last one memory lost nothing since, as a kept answer is
//!     ([`crate::kept_answers`]): its numbers may miss what was added since, and the asker's next answer is worked out
//!     in the background.
//! - **Who shares an answer** ([`class_for`], [`sharing`]): one reader alone unless
//!   `AMPUP_MEMORY_SHARED_ANSWERS_CLASSES` is on ([`classes_on`]). Then a part that reads its reader only through what
//!   the ACLs let them read is shared by every reader who holds the same of the ACL entries the states it reads name
//!   (and one that doesn't read its reader at all by everyone): who reads the same, gets the same answer.
//! - **Read before** ([`Snapshots::read`]): the snapshot ids are read before the state the answer is worked out from
//!   is pinned, as [`crate::kept_answers::Mark`] is. A table only moves forward, so the answer then holds at least
//!   what its stored state names: ids older than the answer are safe (it may show a little more than its state), ids
//!   newer than the answer would be a leak (an answer from before a removal stored as of the state after it). Never
//!   store an answer with ids read after it was worked out.
//! - **Puts** ([`put_due`]): a part's answer is put when its bytes changed, and otherwise at most every
//!   [`PUT_SAME_EVERY`] (to move the state it is stored with on), so a memory that keeps changing writes one object
//!   per part and class, overwritten.
//! - **Aging out**: an episode past the scope's retention policy is gone with no table written. An answer is put
//!   with an expiry no later than its visibility state holds ([`expiry_for`]), worked out once and stored as it is
//!   (an absolute time, so a put made later doesn't push it on), and a store never answers one past its expiry.
//! - **Deletion**: a store holds copies of what memory showed. Every removal path purges the scope's answers
//!   ([`SharedAnswers::purge`], the prefix [`namespace_prefix`]), and every answer lives at most [`TTL`]. The
//!   purge hangs on the one place every removal is counted ([`crate::on_loss`], told by `memstore_core`'s loss
//!   count: a forget of an episode, a note's sentence or a fact, a retraction, a narrowed ACL, a version retired, a
//!   retention policy, an erasure, a purge of a namespace, an offboarding, an org deleted, the ledger rewritten,
//!   and every loss a build of main's state finds, aged out or made by another server): [`purge_on_loss`]. An
//!   answer worked out before a loss and put after its purge would be kept: [`put_unless_lost`] puts only while
//!   the scope's loss count hasn't moved since it was read, and purges again when it moved meanwhile. A server that
//!   hasn't read the loss yet may still put one after the purges (until its tables catch up): the loss wrote one of
//!   [`VIS`]'s tables, so [`Last::holds`] never answers it, and the delayed purge or [`TTL`] removes it.
//! - **The object store** ([`ObjectAnswers`]): one object per answer under `_answers/<key>` beside the catalog's
//!   system tables (where the writer lease, uploads and reports are kept: the bucket memory is already in, or the
//!   directory `serve --local` keeps it in), its expiry in its first line; a purge deletes the namespace's prefix,
//!   and [`ObjectAnswers::sweep`] deletes every answer older than [`TTL`] that no read came to.
//!
//! No caller reads or puts answers yet (Home's wiring does): the store a server installs ([`ObjectAnswers`]) and
//! the purges are in place.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use arrow_array::RecordBatch;
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Principal, Scope, Store};

/// The environment variable that picks the store: `off` (default), `memory` or `object`.
pub const ENV: &str = "AMPUP_MEMORY_SHARED_ANSWERS";

/// The longest an answer is kept: the backstop for a removal path that misses its purge.
pub const TTL: Duration = Duration::from_secs(24 * 3600);

/// The key's version: a change to what the key hashes, or to an answer's form, takes a new one.
pub const VERSION: &str = "v1";

/// A store of answers shared between servers.
#[async_trait::async_trait]
pub trait SharedAnswers: Send + Sync {
    /// The answer stored under `key`, unless there is none or it is past its time to live.
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    /// Stores `bytes` under `key` until `until` (µs since the epoch, [`expiry_for`]), and never past [`TTL`] from
    /// now: an expiry already past stores nothing. The expiry is stored as given, never worked out again from the
    /// time of the write.
    async fn put(&self, key: &str, bytes: Bytes, until: i64) -> Result<()>;
    /// Removes every answer of scope namespace `namespace` (every key under [`namespace_prefix`]).
    async fn purge(&self, namespace: &str) -> Result<()>;
    /// Removes every answer of each scope namespace `which` holds true of: an org's and its teams' (`<ns>__…`), or
    /// every namespace's.
    async fn purge_where(&self, which: &(dyn for<'n> Fn(&'n str) -> bool + Sync)) -> Result<()>;
}

/// Where a process keeps shared answers, from [`ENV`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Memory,
    Object,
}

impl Mode {
    /// The mode [`ENV`] names: off when unset; an unknown value is off too, said once in the log.
    pub fn from_env() -> Mode {
        match std::env::var(ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "off" => Mode::Off,
            "memory" => Mode::Memory,
            "object" => Mode::Object,
            other => {
                eprintln!("shared answers: {ENV}={other:?} is not off, memory or object: off");
                Mode::Off
            }
        }
    }
}

static HANDLE: OnceLock<Option<Arc<dyn SharedAnswers>>> = OnceLock::new();

/// This process's [`Mode`], read from [`ENV`] once.
pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(Mode::from_env)
}

/// This process's store of shared answers: none when [`ENV`] is off. `memory` is [`InProcess`]. `object` is the
/// store a server installs at start ([`install`], an [`ObjectAnswers`]); until one is, there is none (and a read
/// meanwhile doesn't settle that there never is).
pub fn handle() -> Option<Arc<dyn SharedAnswers>> {
    if mode() == Mode::Object {
        return HANDLE.get().cloned().flatten();
    }
    HANDLE
        .get_or_init(|| match mode() {
            Mode::Off | Mode::Object => None,
            Mode::Memory => {
                hold_runtime();
                crate::on_loss(purge_on_loss);
                Some(Arc::new(InProcess::new(IN_PROCESS_MAX)))
            }
        })
        .clone()
}

/// Installs this process's store (the object store, made where the memory's storage is known), before the first
/// [`handle`] in a mode other than `object`: from then on every loss counted purges it ([`purge_on_loss`]). An
/// error once a store is set.
pub fn install(store: Arc<dyn SharedAnswers>) -> Result<()> {
    HANDLE
        .set(Some(store))
        .map_err(|_| anyhow!("the shared answers store is set already"))?;
    hold_runtime();
    crate::on_loss(purge_on_loss);
    Ok(())
}

/// The runtime the store was set on: purges of losses counted off any runtime (a removal on a thread of its own)
/// run on it.
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Keeps the runtime this is called on as [`RUNTIME`], when there is one.
fn hold_runtime() {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let _ = RUNTIME.set(rt);
    }
}

/// Installs an [`ObjectAnswers`] over `store` (a command's, which isn't renewed) when [`mode`] is `object`: what
/// the command removes is purged from the answers servers share ([`settled`] waits for it). Said in the log when
/// it can't be (a store is set already).
pub fn install_object_for(store: &Store) {
    if mode() != Mode::Object {
        return;
    }
    let store = Arc::new(store.clone());
    if let Err(e) = install(Arc::new(ObjectAnswers::new(move || store.clone()))) {
        eprintln!("shared answers: {e:#}");
    }
}

/// How many purges are under way or waiting to start ([`purge_on_loss`]).
static PURGES_OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Waits, at most `within`, until no purge is under way or waiting: a command's before it exits. Whether none is.
pub async fn settled(within: Duration) -> bool {
    let until = Instant::now() + within;
    loop {
        if PURGES_OPEN.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return true;
        }
        if Instant::now() >= until {
            eprintln!(
                "shared answers: purges still under way after {}s: what they didn't reach goes within {}h",
                within.as_secs(),
                TTL.as_secs() / 3600
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The namespaces a purge is waiting to start for: a loss while one waits needs no other.
static PURGES_DUE: LazyLock<Mutex<HashSet<(String, bool)>>> = LazyLock::new(Default::default);

/// How many times a failed purge is tried again before the answers are left to their time to live ([`TTL`]).
const PURGE_TRIES: u32 = 4;

/// How long after the memory state it was worked out from was read an answer may still be put ([`put_unless_lost`]):
/// past it, nothing is put, and a put under way is let go.
pub const PUT_WITHIN: Duration = Duration::from_secs(30);

/// How long after a loss its namespace is purged again ([`purge_on_loss`]): another server may read the state from
/// before it for as long as its cached tables may lag (`AMPUP_MEMORY_HARD_STALENESS_MS`, as the store reads it), and
/// put what it worked out from it within [`PUT_WITHIN`] of that read, after the first purge. Twice that: a put let go
/// at its deadline may still land.
pub fn purge_again_after() -> Duration {
    let stale = std::env::var("AMPUP_MEMORY_HARD_STALENESS_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(60), Duration::from_millis);
    stale + 2 * PUT_WITHIN
}

/// The purges due again later ([`purge_again_after`]), by namespace and whether its teams too: when each is due,
/// the latest loss's.
static PURGES_LATER: LazyLock<Mutex<HashMap<(String, bool), Instant>>> =
    LazyLock::new(Default::default);

/// Told of every loss this process counts ([`crate::on_loss`]): purges the answers of namespace `ns` (and of every
/// namespace under it, when `under`; of every namespace, when `ns` is the system's, whose ledger an erasure
/// rewrote) from this process's store, in the background: on the runtime it is counted on, else the one the store
/// was set on ([`RUNTIME`]). A loss counted with neither leaves its answers to their time to live, said in the log.
/// Purged again [`purge_again_after`] later: a server that hasn't seen the loss yet may put an answer from before
/// it after the first purge, which no check on that server can refuse.
pub fn purge_on_loss(ns: &str, under: bool) {
    let Some(rt) = tokio::runtime::Handle::try_current()
        .ok()
        .or_else(|| RUNTIME.get().cloned())
    else {
        if HANDLE.get().cloned().flatten().is_some() {
            eprintln!(
                "shared answers: {ns} lost something off any runtime: its answers are not purged, and go within {}h",
                TTL.as_secs() / 3600
            );
        }
        return;
    };
    let team = format!("{ns}__");
    forget_puts(|n| ns == crate::SYSTEM_NS || n == ns || (under && n.starts_with(&team)));
    purge_now(&rt, ns, under);
    purge_later(&rt, ns, under);
}

/// [`purge_on_loss`]'s second purge, [`purge_again_after`] from the latest loss of `ns`: one pending for the
/// namespace is moved on rather than another started.
fn purge_later(rt: &tokio::runtime::Handle, ns: &str, under: bool) {
    if HANDLE.get().cloned().flatten().is_none() {
        return;
    }
    let due = (ns.to_string(), under);
    let at = Instant::now() + purge_again_after();
    if PURGES_LATER
        .lock()
        .unwrap()
        .insert(due.clone(), at)
        .is_some()
    {
        return;
    }
    let rt2 = rt.clone();
    rt.spawn(async move {
        loop {
            let wait = {
                let mut later = PURGES_LATER.lock().unwrap();
                match later.get(&due) {
                    None => return,
                    Some(at) if Instant::now() >= *at => {
                        later.remove(&due);
                        break;
                    }
                    Some(at) => *at,
                }
            };
            tokio::time::sleep_until(wait.into()).await;
        }
        purge_now(&rt2, &due.0, due.1);
    });
}

/// Purges `ns`'s answers (see [`purge_on_loss`]) now, in the background on `rt`.
fn purge_now(rt: &tokio::runtime::Handle, ns: &str, under: bool) {
    let Some(store) = HANDLE.get().cloned().flatten() else {
        return;
    };
    let due = (ns.to_string(), under);
    if !PURGES_DUE.lock().unwrap().insert(due.clone()) {
        return;
    }
    PURGES_OPEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    rt.spawn(async move {
        /// The purge counted open until it ends, however it ends.
        struct Open;
        impl Drop for Open {
            fn drop(&mut self) {
                PURGES_OPEN.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _open = Open;
        // Taken off before the purge lists anything: a loss from here on purges again.
        PURGES_DUE.lock().unwrap().remove(&due);
        let (ns, under) = due;
        let team = format!("{ns}__");
        let mut wait = Duration::from_millis(200);
        for attempt in 1..=PURGE_TRIES {
            let purged = if ns == crate::SYSTEM_NS {
                store.purge_where(&|_| true).await
            } else if under {
                store
                    .purge_where(&|n| n == ns || n.starts_with(&team))
                    .await
            } else {
                store.purge(&ns).await
            };
            match purged {
                Ok(()) => return,
                Err(e) if attempt == PURGE_TRIES => eprintln!(
                    "ALERT shared answers: {ns}'s answers not purged after a loss ({attempt} tries): they go within {}h: {e:#}",
                    TTL.as_secs() / 3600
                ),
                Err(_) => {
                    tokio::time::sleep(wait).await;
                    wait *= 4;
                }
            }
        }
    });
}

/// The loss counts an answer of a scope is put against ([`put_unless_lost`]): its own namespace's, and the system's,
/// where an org deleted or a person erased is counted (and every namespace's answers purged, [`purge_on_loss`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Losses {
    scope: u64,
    system: u64,
}

impl Losses {
    /// Scope namespace `ns`'s counts now ([`crate::losses`]).
    pub fn read(ns: &str) -> Losses {
        Losses {
            scope: crate::losses(ns),
            system: crate::losses(crate::SYSTEM_NS),
        }
    }
}

/// Puts `bytes` under `key` in `store` until `until` ([`expiry_for`]), an answer of scope namespace `ns` worked out
/// from a state read at `read_at` and pinned after its loss counts read `losses` ([`Losses::read`]): not at all once
/// either count moved (a loss since, whose purge may be done already) or past [`PUT_WITHIN`] from `read_at` (the
/// delayed purge, [`purge_again_after`], counts on it), and with the namespace purged again when a count moved while
/// the answer was put. An expiry already past puts nothing.
pub async fn put_unless_lost(
    store: &dyn SharedAnswers,
    ns: &str,
    losses: Losses,
    read_at: Instant,
    key: &str,
    bytes: Bytes,
    until: i64,
) -> Result<()> {
    let left = PUT_WITHIN.saturating_sub(read_at.elapsed());
    if until <= crate::now_micros() || Losses::read(ns) != losses || left.is_zero() {
        return Ok(());
    }
    tokio::time::timeout(left, store.put(key, bytes, until))
        .await
        .map_err(|_| {
            anyhow!(
                "not put within {}s of reading its state",
                PUT_WITHIN.as_secs()
            )
        })??;
    if Losses::read(ns) != losses {
        store.purge(ns).await?;
    }
    Ok(())
}

/// The answers [`InProcess`] keeps at most when [`handle`] makes it.
const IN_PROCESS_MAX: usize = 2_000;

/// Answers kept in this process alone: what a server without a shared store has, and a stand-in for one.
pub struct InProcess {
    /// Each answer and its expiry (µs since the epoch).
    held: Mutex<HashMap<String, (Bytes, i64)>>,
    max: usize,
}

impl InProcess {
    /// Keeping at most `max` answers: past it, those past their time to live go, then those nearest to it.
    pub fn new(max: usize) -> InProcess {
        InProcess {
            held: Mutex::new(HashMap::new()),
            max,
        }
    }
}

#[async_trait::async_trait]
impl SharedAnswers for InProcess {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let mut held = self.held.lock().unwrap();
        match held.get(key) {
            Some((b, until)) if crate::now_micros() < *until => Ok(Some(b.clone())),
            Some(_) => {
                held.remove(key);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn put(&self, key: &str, bytes: Bytes, until: i64) -> Result<()> {
        let now = crate::now_micros();
        let until = until.min(now + TTL.as_micros() as i64);
        if until <= now {
            return Ok(());
        }
        let mut held = self.held.lock().unwrap();
        if held.len() >= self.max && !held.contains_key(key) {
            held.retain(|_, (_, u)| now < *u);
            if held.len() >= self.max
                && let Some(first) = held
                    .iter()
                    .min_by_key(|(_, (_, u))| *u)
                    .map(|(k, _)| k.clone())
            {
                held.remove(&first);
            }
        }
        held.insert(key.to_string(), (bytes, until));
        Ok(())
    }

    async fn purge(&self, namespace: &str) -> Result<()> {
        let prefix = namespace_prefix(namespace);
        self.held
            .lock()
            .unwrap()
            .retain(|k, _| !k.starts_with(&prefix));
        Ok(())
    }

    async fn purge_where(&self, which: &(dyn for<'n> Fn(&'n str) -> bool + Sync)) -> Result<()> {
        self.held
            .lock()
            .unwrap()
            .retain(|k, _| namespace_of(k).is_none_or(|ns| !which(ns)));
        Ok(())
    }
}

/// The scope namespace of key `key` ([`last_key`]): none for one not of this version's form.
fn namespace_of(key: &str) -> Option<&str> {
    key.strip_prefix(VERSION)?
        .strip_prefix('/')?
        .split_once('/')
        .map(|(ns, _)| ns)
}

/// Where answers are kept, beside the catalog's system tables.
const ANSWERS_DIR: &str = "_answers";
/// How long the place answers are kept is used before it is found again (fresh credentials), at most.
const PLACE_FOR: Duration = Duration::from_secs(600);
/// What an answer's object starts with: this, its expiry (µs since the epoch) and a line's end, then the answer.
const OBJECT_HEAD: &str = "answer-until ";
/// How often [`ObjectAnswers::sweep_every`] deletes the answers past [`TTL`].
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// Answers kept in the object store memory is kept in: one object per answer, under `_answers/<key>` beside the
/// catalog's system tables ([`crate::server::writer::beside`]: the bucket the writer lease and reports are kept in,
/// or the local directory). Its first line is its expiry; a read past it finds none (and deletes it). A purge
/// deletes the namespace's prefix. Where the catalog keeps no such place, nothing is kept (and nothing found).
pub struct ObjectAnswers {
    /// The server's store now (it is renewed: after a deletion, on new credentials).
    store: Box<dyn Fn() -> Arc<Store> + Send + Sync>,
    /// The place found, for which store (by address), until when.
    place: tokio::sync::Mutex<Option<(usize, opendal::Operator, Instant)>>,
}

impl ObjectAnswers {
    /// Answers kept beside the catalog of whatever store `store` gives at the time.
    pub fn new(store: impl Fn() -> Arc<Store> + Send + Sync + 'static) -> ObjectAnswers {
        ObjectAnswers {
            store: Box::new(store),
            place: Default::default(),
        }
    }

    /// The place answers are kept, found again every [`PLACE_FOR`], before its credentials run out, for a store
    /// renewed, and after a failure; none where the catalog keeps no place beside its tables.
    async fn op(&self) -> Result<Option<opendal::Operator>> {
        let store = (self.store)();
        let id = Arc::as_ptr(&store) as usize;
        let mut p = self.place.lock().await;
        if let Some((i, op, until)) = p.as_ref()
            && *i == id
            && Instant::now() < *until
        {
            return Ok(Some(op.clone()));
        }
        let found = tokio::time::timeout(
            Duration::from_secs(30),
            crate::server::writer::beside(&store, ANSWERS_DIR),
        )
        .await
        .map_err(|_| anyhow!("finding where shared answers are kept took over 30s"))??;
        if found.is_none() {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                eprintln!(
                    "shared answers: this catalog keeps nothing beside its tables: no answer is shared"
                )
            });
        }
        *p = found
            .clone()
            .map(|(op, until)| (id, op, until.min(Instant::now() + PLACE_FOR)));
        Ok(found.map(|(op, _)| op))
    }

    /// After a failure: the place is found again (fresh credentials) next time.
    async fn lose_place(&self) {
        *self.place.lock().await = None;
    }

    /// `r`, the place forgotten when it failed.
    async fn checked<T>(&self, r: opendal::Result<T>) -> Result<T> {
        match r {
            Ok(t) => Ok(t),
            Err(e) => {
                self.lose_place().await;
                Err(e.into())
            }
        }
    }

    /// Deletes every answer older than [`TTL`]: those no read came to past their expiry. An answer's expiry is at
    /// most [`TTL`] after it was written, so none outlives its expiry by more than the sweeps' interval.
    pub async fn sweep(&self) -> Result<usize> {
        let Some(op) = self.op().await? else {
            return Ok(0);
        };
        let listed = op.list_with(&format!("{VERSION}/")).recursive(true).await;
        let entries = match listed {
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(0),
            r => self.checked(r).await?,
        };
        let before = opendal::raw::Timestamp::from_second(
            crate::now_micros() / 1_000_000 - TTL.as_secs() as i64,
        )?;
        let mut gone = 0;
        for e in entries {
            if e.path().ends_with('/') {
                continue;
            }
            let written = match e.metadata().last_modified() {
                Some(t) => Some(t),
                None => op.stat(e.path()).await.ok().and_then(|m| m.last_modified()),
            };
            if written.is_some_and(|t| t < before) {
                match op.delete(e.path()).await {
                    Ok(()) => gone += 1,
                    Err(err) if err.kind() == opendal::ErrorKind::NotFound => {}
                    Err(err) => eprintln!("shared answers: {} not swept: {err}", e.path()),
                }
            }
        }
        Ok(gone)
    }

    /// [`ObjectAnswers::sweep`] every `every`, for as long as the process runs.
    pub async fn sweep_every(self: Arc<Self>, every: Duration) {
        loop {
            match self.sweep().await {
                Ok(0) => {}
                Ok(n) => eprintln!("shared answers: swept {n} past their time to live"),
                Err(e) => eprintln!("shared answers: sweep failed: {e:#}"),
            }
            tokio::time::sleep(every).await;
        }
    }
}

/// An answer's object: its expiry, then the answer.
fn object_of(until: i64, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{OBJECT_HEAD}{until}\n").into_bytes();
    out.extend_from_slice(bytes);
    out
}

/// An object's expiry and answer: none for one not of [`object_of`]'s form.
fn parse_object(b: &Bytes) -> Option<(i64, Bytes)> {
    let end = b.iter().position(|c| *c == b'\n')?;
    let until = std::str::from_utf8(&b[..end])
        .ok()?
        .strip_prefix(OBJECT_HEAD)?
        .parse()
        .ok()?;
    Some((until, b.slice(end + 1..)))
}

#[async_trait::async_trait]
impl SharedAnswers for ObjectAnswers {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let Some(op) = self.op().await? else {
            return Ok(None);
        };
        let read = match op.read(key).await {
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
            r => self.checked(r).await?.to_bytes(),
        };
        match parse_object(&read) {
            Some((until, answer)) if crate::now_micros() < until => Ok(Some(answer)),
            _ => {
                // Past its time to live (or not an answer's form): never answered, and not kept either.
                if let Err(e) = op.delete(key).await
                    && e.kind() != opendal::ErrorKind::NotFound
                {
                    eprintln!("shared answers: {key} past its time not deleted: {e}");
                }
                Ok(None)
            }
        }
    }

    async fn put(&self, key: &str, bytes: Bytes, until: i64) -> Result<()> {
        let Some(op) = self.op().await? else {
            return Ok(());
        };
        // The cap on how long an answer lives is taken at the write; the expiry given is kept as it is.
        let now = crate::now_micros();
        let until = until.min(now + TTL.as_micros() as i64);
        if until <= now {
            return Ok(());
        }
        let r = op.write(key, object_of(until, &bytes)).await.map(|_| ());
        self.checked(r).await
    }

    async fn purge(&self, namespace: &str) -> Result<()> {
        let Some(op) = self.op().await? else {
            return Ok(());
        };
        match op
            .delete_with(&namespace_prefix(namespace))
            .recursive(true)
            .await
        {
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(()),
            r => self.checked(r).await,
        }
    }

    async fn purge_where(&self, which: &(dyn for<'n> Fn(&'n str) -> bool + Sync)) -> Result<()> {
        let Some(op) = self.op().await? else {
            return Ok(());
        };
        let root = format!("{VERSION}/");
        let entries = match op.list(&root).await {
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(()),
            r => self.checked(r).await?,
        };
        for e in entries {
            let Some(ns) = e
                .path()
                .strip_prefix(&root)
                .and_then(|r| r.strip_suffix('/'))
                .filter(|ns| !ns.is_empty() && !ns.contains('/'))
            else {
                continue;
            };
            if which(ns) {
                self.purge(ns).await?;
            }
        }
        Ok(())
    }
}

/// The prefix of every key of scope namespace `namespace`, ending in `/` so one namespace's prefix is never
/// another's (`memory_org_a/` is not a prefix of `memory_org_ab/...`).
pub fn namespace_prefix(namespace: &str) -> String {
    format!("{VERSION}/{namespace}/")
}

/// Where a table a part reads lives: the scope's own namespace, or the catalog's system namespace
/// ([`crate::SYSTEM_NS`]: the ledger and the directory, shared by every scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum In {
    Scope,
    System,
}

/// A table a part reads: where, and its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Table(pub In, pub &'static str);

impl Table {
    /// The table's namespace when `scope` is read.
    pub fn namespace(&self, scope: &Scope) -> String {
        match self.0 {
            In::Scope => scope.namespace(),
            In::System => crate::SYSTEM_NS.to_string(),
        }
    }

    /// The table's name when `scope` is read: `<namespace>.<table>`.
    pub fn name(&self, scope: &Scope) -> String {
        format!("{}.{}", self.namespace(scope), self.1)
    }
}

/// The tables every visibility state is built from ([`crate::Store::lagging_visibility`]): the ledger (which
/// commits are visible on the branch; labelled by the scope's own rows, as a state is kept while they are the same), the episodes (their ACLs, retention, documents), quotes, schemas, closures,
/// retractions (deletes, forgets, withdrawals), the retention policy and the governance decisions. Every part reads
/// memory through such a state, so every part's key holds them.
pub const VIS: [Table; 8] = [
    Table(In::System, crate::LEDGER),
    Table(In::Scope, crate::EPISODES),
    Table(In::Scope, crate::QUOTES),
    Table(In::Scope, crate::SCHEMAS),
    Table(In::Scope, crate::CLOSURES),
    Table(In::Scope, crate::RETRACTIONS),
    Table(In::Scope, crate::RETENTION),
    Table(In::Scope, crate::governance::TABLE),
];
const FACTS: Table = Table(In::Scope, crate::FACTS);
const AGGREGATES: Table = Table(In::Scope, crate::AGGREGATES);
const EXTRACTIONS: Table = Table(In::Scope, crate::EXTRACTIONS);
const QA_LOG: Table = Table(In::Scope, crate::QA_LOG);
const LEDGER: Table = Table(In::System, crate::LEDGER);
/// One table for every org, labelled by its snapshot id as this server's fold read it: it moves with any org's
/// membership or settings change, which is rare beside commits, and which org's rows a part's answer turns on (its
/// admins, who shares, a superuser's standing) isn't settled per part, so it isn't narrowed to the scope's org.
const DIRECTORY: Table = Table(In::System, crate::DIRECTORY);

/// [`VIS`] and every other table a part that reads facts may reach: the facts, the summaries (`aggregates`), the
/// extraction records, the answer log and the directory. One list for all of them: a table listed and not read
/// costs only how often an answer is found again (each is written through the ledger, already in every key); one
/// read and not listed is a leak.
const VIS_AND_READS: [Table; 13] = [
    VIS[0],
    VIS[1],
    VIS[2],
    VIS[3],
    VIS[4],
    VIS[5],
    VIS[6],
    VIS[7],
    FACTS,
    AGGREGATES,
    EXTRACTIONS,
    QA_LOG,
    DIRECTORY,
];

/// The tables each of Home's parts reads (`/browse {"op": "home"}`, [`crate::server`]'s `home`), by part name as
/// Home names them: what its key must hold. Derived by following each part's op to every table read beneath it
/// (`Store::rows`, `side_rows`, the facts plan, `governance_rows`, the answer log, the directory); where a path was
/// not followed to its end, the table is listed. A table missing here is a leak: the part's answer from before a
/// change to it would be found after.
///
/// Every part reads through a visibility state ([`VIS`]). Beyond it, what each was followed to:
/// - `counts` (`ls /`, the root's counts): the facts (entities, notes), the answer log (the advice listed, through
///   the sessions' evidence), the directory (an org's evidence bar);
/// - `week` (`facts` twice, `advice_usage`, `ls sources`): the facts, the answer log (advice served, sessions'
///   evidence), the directory;
/// - `lessons` (`lessons {count: true}`): the facts (the advice listed), the answer log (sessions, outcomes,
///   recalls), the directory;
/// - `activity` (`/admin activity`): the answer log, read through main's view (the facts), the directory (who is an
///   admin, whether the org shows its people's activity);
/// - `proposals` (`/admin list_proposals {count: true}`): the governance table and the ledger (in [`VIS`]), the facts
///   (each pending proposal's facts), those proposals' branches' states (the same tables), the directory;
/// - `conflicts` (`/admin list_conflicts {mine: true}`): the entities' owners (governance, in [`VIS`]), the facts,
///   the directory;
/// - `dream` (`/admin dream_decisions`): the dream's records and reviews (episodes, their bodies), the facts they
///   rest on, the answer log (each decision's reason), the directory (admins, how many people share);
/// - `branches` (`branches {build: false}`): the ledger's branch heads and each built branch's state, nothing else;
/// - `schema`: main's schema, from its state alone.
///
/// Those that read facts are keyed on [`VIS_AND_READS`]: no path followed reached the summaries or the extraction
/// records, but not every path beneath the fact reads was followed to its end, so both are listed. Listed by none:
/// `aggregate_requests`, `feedback` (search and cards count votes), `pins`, `workspace_snapshots`, and the system's
/// installations, key usage, usage hours and audit log. Every read is of the scope read, or of the system's tables:
/// no part reads another scope.
pub const PART_TABLES: [(&str, &[Table]); 9] = [
    ("counts", &VIS_AND_READS),
    ("week", &VIS_AND_READS),
    ("lessons", &VIS_AND_READS),
    ("branches", &VIS),
    ("activity", &VIS_AND_READS),
    ("proposals", &VIS_AND_READS),
    ("conflicts", &VIS_AND_READS),
    ("dream", &VIS_AND_READS),
    ("schema", &VIS),
];

/// The tables Home's part `part` reads ([`PART_TABLES`]); none for a part not listed (it has no key).
pub fn part_tables(part: &str) -> Option<&'static [Table]> {
    PART_TABLES
        .iter()
        .find(|(p, _)| *p == part)
        .map(|(_, t)| *t)
}

/// What some tables of one scope hold, each named by a label, read at once: one read per request, for all its parts'
/// keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshots {
    /// By `<namespace>.<table>`: the table's label ([`Snapshots::read`]).
    ids: HashMap<String, String>,
    /// The ledger's rows its label was worked out from, kept to label them again without the answer log's commits
    /// ([`without_answers`]).
    ledger: LedgerRows,
}

/// The ledger's rows as one read of it holds them ([`Snapshots`]). Never what tells two reads apart: their labels
/// are.
#[derive(Clone, Default)]
struct LedgerRows(Option<Arc<Vec<RecordBatch>>>);

impl PartialEq for LedgerRows {
    fn eq(&self, _: &LedgerRows) -> bool {
        true
    }
}

impl Eq for LedgerRows {}

impl std::fmt::Debug for LedgerRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ledger rows")
    }
}

impl Snapshots {
    /// The labels of `tables` in `scope`, as this server's next read would read them: a table's snapshot id, as this
    /// server holds it or from the catalog's metadata where it holds none ([`Store::snapshot_id`]: no data file is
    /// read), or `-` where it doesn't exist or has none yet. The ledger, one table for every scope, is labelled by
    /// `scope`'s own rows in it alone ([`ledger_label`], as the ledger is read now): its snapshot id moves with any
    /// scope's commit, several times a minute. Read before the state an answer is worked out from is pinned (see the
    /// module's doc).
    pub async fn read(store: &Store, scope: &Scope, tables: &[Table]) -> Result<Snapshots> {
        let mut tables = tables.to_vec();
        tables.sort();
        tables.dedup();
        let ledger = match tables.contains(&LEDGER) {
            true => Some(store.rows(crate::SYSTEM_NS, crate::LEDGER).await?.rows),
            false => None,
        };
        let got = futures::future::try_join_all(tables.iter().map(|t| {
            let ledger = ledger.clone();
            async move {
                let ns = t.namespace(scope);
                let label = match (*t, ledger) {
                    (LEDGER, Some(rows)) => format!(
                        "rows of {}: {}",
                        scope.namespace(),
                        ledger_label(rows, &scope.namespace()).await?
                    ),
                    _ => store
                        .snapshot_id(&ns, t.1)
                        .await?
                        // A table with no snapshot has a marker no id can be.
                        .map_or_else(|| "-".to_string(), |id| id.to_string()),
                };
                anyhow::Ok((t.name(scope), label))
            }
        }))
        .await?;
        Ok(Snapshots {
            ids: got.into_iter().collect(),
            ledger: LedgerRows(ledger),
        })
    }

    /// The label of table `t` of `scope` as read: an error when it wasn't.
    fn label(&self, scope: &Scope, t: &Table) -> Result<&str> {
        let name = t.name(scope);
        self.ids
            .get(&name)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("no snapshot read for {name}"))
    }
}

/// The actor of every ledger row the answer log's commits make (`qa_log`'s `write_qa_rows`), before who logged.
const ANSWER_LOG_ACTOR: &str = "log_answer:";

/// A label of namespace `ns`'s own rows in the ledger `rows`: `<rows>:<sha256>`, the SHA-256 over each row's own
/// SHA-256 (every column, each length-prefixed, a null marked apart from any value), sorted. Rows whose commit is in
/// `skip` are left out when they are the answer log's ([`ANSWER_LOG_ACTOR`], on main). The same on every server for
/// the same rows. Worked out once per ledger read for the whole ledger ([`ledger_label`]).
fn rows_label(batches: &[RecordBatch], ns: &str, skip: &HashSet<String>) -> String {
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::TimestampMicrosecondType;
    let mut rows: Vec<[u8; 32]> = vec![];
    for batch in batches {
        let text = |c: &str| batch.column_by_name(c).map(|c| c.as_string::<i32>());
        let Some(scope) = text("scope") else {
            continue;
        };
        let columns = ["commit_id", "branch", "actor", "parents"].map(text);
        let at = batch
            .column_by_name("recorded_at")
            .map(|c| c.as_primitive::<TimestampMicrosecondType>());
        let value = |c: usize, i: usize| columns[c].filter(|c| !c.is_null(i)).map(|c| c.value(i));
        for i in 0..batch.num_rows() {
            if scope.is_null(i) || scope.value(i) != ns {
                continue;
            }
            if value(0, i).is_some_and(|id| skip.contains(id))
                && value(1, i) == Some(crate::MAIN)
                && value(2, i).is_some_and(|a| a.starts_with(ANSWER_LOG_ACTOR))
            {
                continue;
            }
            let mut h = Sha256::new();
            let mut field = |v: Option<&[u8]>| match v {
                None => h.update([0u8]),
                Some(v) => {
                    h.update([1u8]);
                    h.update((v.len() as u64).to_le_bytes());
                    h.update(v);
                }
            };
            for c in 0..columns.len() {
                field(value(c, i).map(str::as_bytes));
            }
            let at = at
                .filter(|a| !a.is_null(i))
                .map(|a| a.value(i).to_le_bytes());
            field(at.as_ref().map(|a| &a[..]));
            rows.push(h.finalize().into());
        }
    }
    rows.sort_unstable();
    let mut h = Sha256::new();
    for r in &rows {
        h.update(r);
    }
    format!("{}:{}", rows.len(), crate::deployment::hex(&h.finalize()))
}

/// [`rows_label`] of `ns`'s rows in the ledger `rows`, none left out: worked out once per read of the ledger.
async fn ledger_label(rows: Arc<Vec<RecordBatch>>, ns: &str) -> Result<String> {
    type Labels = HashMap<String, (std::sync::Weak<Vec<RecordBatch>>, String)>;
    static LABELS: LazyLock<Mutex<Labels>> = LazyLock::new(Default::default);
    if let Some((of, label)) = LABELS.lock().unwrap().get(ns)
        && of.upgrade().is_some_and(|of| Arc::ptr_eq(&of, &rows))
    {
        return Ok(label.clone());
    }
    let label = {
        let (rows, ns) = (rows.clone(), ns.to_string());
        tokio::task::spawn_blocking(move || rows_label(&rows, &ns, &HashSet::new())).await?
    };
    let mut labels = LABELS.lock().unwrap();
    labels.retain(|_, (of, _)| of.strong_count() > 0);
    labels.insert(ns.to_string(), (Arc::downgrade(&rows), label.clone()));
    Ok(label)
}

/// [`rows_label`] of `scope`'s rows in the ledger `snapshots` read, without the answer log's commits: each commit
/// some row of `scope`'s answer log holds, as this server reads that log now (after the ledger, which a commit
/// reaches only once its rows are written). Only `qa_log`'s `write_qa_rows` writes that table, each time under a
/// commit id of its own, and that commit makes only those rows readable: a ledger whose rows, these left out, are
/// as they were has had no commit since but the answer log's. A log read behind the ledger leaves a commit in, and
/// the labels differ: never the other way.
pub async fn without_answers(
    store: &Store,
    scope: &Scope,
    snapshots: &Snapshots,
) -> Result<String> {
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    let rows = snapshots
        .ledger
        .0
        .clone()
        .ok_or_else(|| anyhow!("the ledger wasn't read"))?;
    let log = store.side_rows(&scope.namespace(), crate::QA_LOG).await?;
    let ns = scope.namespace();
    Ok(tokio::task::spawn_blocking(move || {
        let mut answers = HashSet::new();
        for b in log.iter() {
            let Some(ids) = b.column_by_name("commit_id") else {
                continue;
            };
            let ids = ids.as_string::<i32>();
            answers.extend(
                (0..ids.len())
                    .filter(|i| !ids.is_null(*i))
                    .map(|i| ids.value(i).to_string()),
            );
        }
        rows_label(&rows, &ns, &answers)
    })
    .await?)
}

/// The settings that change what a reader may read or how Home's parts are worked out for them: who a caller is and
/// which groups and org they hold, who reads everything, who a dream's records are for, what people share, and how
/// answers are shared. Read into [`made_by`].
const VISIBILITY_SETTINGS: [&str; 10] = [
    "AMPUP_MEMORY_CALLER_IDENTITY",
    "AMPUP_MEMORY_JWT_GROUPS_CLAIM",
    "AMPUP_MEMORY_JWT_ORG_CLAIM",
    "AMPUP_MEMORY_SUPER_ADMINS",
    "AMPUP_MEMORY_DREAM_AUDIENCES",
    "AMPUP_MEMORY_SHARE_PEOPLE",
    "AMPUP_MEMORY_SHARE_END_ON_BASIS_LOSS",
    "AMPUP_MEMORY_SHARED_ANSWERS_CLASSES",
    "AMPUP_MEMORY_VIEW_FACTS",
    "AMPUP_MEMORY_EVAL_GROUPS",
];

/// What made this process's answers: the SHA-256 of its build (`AMPUP_MEMORY_BUILD_ID`, a hash of the sources) and of
/// each of [`VISIBILITY_SETTINGS`] as set here. A stored answer is read only by a process with the same: a deploy that
/// changes what readers may see, or a setting that does, never serves the answers made before it.
pub fn made_by() -> &'static str {
    static MADE_BY: LazyLock<String> = LazyLock::new(|| {
        let mut h = Sha256::new();
        h.update(env!("AMPUP_MEMORY_BUILD_ID").as_bytes());
        for name in VISIBILITY_SETTINGS {
            h.update([0u8]);
            h.update(name.as_bytes());
            match std::env::var(name) {
                Ok(v) => {
                    h.update([1u8]);
                    h.update(v.as_bytes());
                }
                Err(_) => h.update([2u8]),
            }
        }
        crate::deployment::hex(&h.finalize())
    });
    &MADE_BY
}

/// The SHA-256 of reader class `class`, as a stored answer holds it: never the class itself.
fn class_sum(class: &str) -> String {
    crate::deployment::hex(&Sha256::digest(class.as_bytes()))
}

/// The hour `at` (µs since the epoch) falls in: a stored answer's, as Home's week is counted back from its start.
pub fn hour_of(at: i64) -> i64 {
    at.div_euclid(3_600_000_000)
}

/// The last segment of every answer's key ([`last_key`]).
const LAST: &str = "last";

/// The key of part `part`'s last answer for reader class `class` in `scope`: `v1/<namespace>/<part>/<class>/last`,
/// `<class>` the SHA-256 of `class`. One per part and class, overwritten by each put. An error for a part not
/// listed in [`PART_TABLES`].
pub fn last_key(scope: &Scope, part: &str, class: &str) -> Result<String> {
    if part_tables(part).is_none() {
        return Err(anyhow!("no tables listed for part {part:?}"));
    }
    Ok(format!(
        "{}{part}/{}/{LAST}",
        namespace_prefix(&scope.namespace()),
        crate::deployment::hex(&Sha256::digest(class.as_bytes())),
    ))
}

/// A part's last answer as stored under [`last_key`]: the answer, the hour its state was read in, and that state.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Last {
    /// The build and the settings that made it ([`made_by`]): another build's or settings' answer is never read.
    made_by: String,
    /// The namespace it answers in, and the SHA-256 of its reader class: one read under another key isn't read.
    ns: String,
    class: String,
    /// The part it answers: an answer is never read as another part's.
    part: String,
    /// The hour its state was read in ([`hour_of`]): Home's week is counted back from its start.
    pub hour: i64,
    /// Each table the part reads (`<namespace>.<table>`) and its label as read before the answer was worked out
    /// ([`Snapshots::read`]).
    state: BTreeMap<String, String>,
    /// The ledger's label as read then, without the answer log's commits ([`without_answers`]): none when it wasn't
    /// worked out, and then any commit since refuses it.
    #[serde(default)]
    ledger_without_answers: Option<String>,
    /// The part's answer.
    pub answer: Value,
}

/// Why a stored answer may not answer ([`Last::holds`]): what refused it, and the table that did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub why: &'static str,
    pub table: Option<&'static str>,
}

impl Refused {
    fn of(why: &'static str) -> Refused {
        Refused { why, table: None }
    }

    fn at(why: &'static str, t: &Table) -> Refused {
        Refused {
            why,
            table: Some(t.1),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.table {
            Some(t) => write!(f, "{}: {}", t, self.why),
            None => f.write_str(self.why),
        }
    }
}

/// How a stored answer holds at the state a server reads now ([`Last::holds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holds {
    /// Every table the part reads is as it was: the answer at that state.
    Exact,
    /// Only rows that take nothing away were added since: the last answer memory lost nothing since, which may miss
    /// what was added.
    Since,
}

impl Last {
    /// Part `part`'s answer `answer` in `scope`, worked out from a state pinned after `snapshots` were read in hour
    /// `hour`, the ledger's label then without the answer log's commits `ledger_without_answers`
    /// ([`without_answers`] of `snapshots`): an error for a part not listed, or one of its tables not read.
    pub fn new(
        scope: &Scope,
        part: &str,
        class: &str,
        hour: i64,
        snapshots: &Snapshots,
        ledger_without_answers: Option<String>,
        answer: Value,
    ) -> Result<Last> {
        let tables =
            part_tables(part).ok_or_else(|| anyhow!("no tables listed for part {part:?}"))?;
        let mut state = BTreeMap::new();
        for t in tables {
            state.insert(t.name(scope), snapshots.label(scope, t)?.to_string());
        }
        Ok(Last {
            made_by: made_by().to_string(),
            ns: scope.namespace(),
            class: class_sum(class),
            part: part.to_string(),
            hour,
            state,
            ledger_without_answers,
            answer,
        })
    }

    /// As stored.
    pub fn to_bytes(&self) -> Result<Bytes> {
        Ok(serde_json::to_vec(self)?.into())
    }

    /// What [`Last::to_bytes`] stored; none for anything else.
    pub fn from_bytes(bytes: &[u8]) -> Option<Last> {
        serde_json::from_slice(bytes).ok()
    }

    /// Whether this answer may answer part `part` of `scope` in hour `hour` at the state `now` names (read before
    /// anything the reader is answered from is pinned): how, or why not. Fail-closed: a table not stored, not read or
    /// whose history can't be read refuses.
    ///
    /// - Each of [`VIS`]'s tables (the ledger by the scope's own rows) must be as stored: a removal there is mostly
    ///   an append (a retraction, an episode written again under a narrower ACL or forgotten, a narrower new version,
    ///   a forget's quote, a merge), told apart from an addition only against the state before it. The ledger alone
    ///   may have gained the answer log's commits ([`without_answers`]): each makes only its own answer-log rows
    ///   readable, and every answer logged commits one, so a busy org's ledger moves every few seconds.
    /// - The directory must be as stored: a member removed, an admin demoted or a setting turned off is a row written
    ///   there.
    /// - Each other table the part reads may have moved only by appends from the stored snapshot ([`appended_since`],
    ///   in `histories` as the catalog has them now): a fact is read only once its commit is committed (a commit
    ///   moves the ledger), and the answer log, the summaries and the extraction records only grow between rewrites.
    ///
    /// Its expiry (retention ageing writes no table) is the store's to keep: a store never answers past it.
    #[allow(clippy::too_many_arguments)]
    pub async fn holds(
        &self,
        store: &Store,
        scope: &Scope,
        part: &str,
        class: &str,
        hour: i64,
        now: &Snapshots,
        histories: &Histories,
    ) -> std::result::Result<Holds, Refused> {
        if self.made_by != made_by() {
            return Err(Refused::of("made by another build or settings"));
        }
        if self.ns != scope.namespace() || self.class != class_sum(class) {
            return Err(Refused::of("stored for another reader"));
        }
        if self.part != part {
            return Err(Refused::of("stored for another part"));
        }
        if self.hour != hour {
            return Err(Refused::of("from another hour"));
        }
        let tables = part_tables(part).ok_or(Refused::of("no tables listed"))?;
        if self.state.len() != tables.len() {
            return Err(Refused::of("stored for other tables"));
        }
        let mut moved = vec![];
        let mut ledger_moved = false;
        for t in tables {
            let was = self
                .state
                .get(&t.name(scope))
                .ok_or(Refused::at("not stored", t))?;
            let is = now
                .label(scope, t)
                .map_err(|_| Refused::at("not read", t))?;
            if was == is {
                continue;
            }
            if *t == LEDGER {
                ledger_moved = true;
                continue;
            }
            if VIS.contains(t) || *t == DIRECTORY {
                return Err(Refused::at("written since", t));
            }
            moved.push((t, was));
        }
        if ledger_moved {
            let was = self
                .ledger_without_answers
                .as_deref()
                .ok_or(Refused::at("written since", &LEDGER))?;
            let is = histories
                .without_answers(store, scope, now)
                .await
                .map_err(|_| Refused::at("answer log not read", &LEDGER))?;
            if was != is {
                return Err(Refused::at("written since", &LEDGER));
            }
        }
        if moved.is_empty() && !ledger_moved {
            return Ok(Holds::Exact);
        }
        let checked = futures::future::join_all(moved.into_iter().map(|(t, was)| async move {
            let from = match was.as_str() {
                "-" => None,
                id => Some(
                    id.parse::<i64>()
                        .map_err(|_| Refused::at("stored as no snapshot", t))?,
                ),
            };
            let meta = histories.of(store, scope, t).await.map_err(|e| {
                eprintln!("shared answers: {e}");
                Refused::at("history not read", t)
            })?;
            appended_since(&meta, from).map_err(|why| Refused::at(why, t))
        }))
        .await;
        checked
            .into_iter()
            .collect::<std::result::Result<(), Refused>>()?;
        Ok(Holds::Since)
    }
}

/// Tables' metadata as the catalog has them now, each loaded once (for one ask's parts): their histories, for
/// [`appended_since`].
#[derive(Default)]
pub struct Histories(
    tokio::sync::Mutex<HashMap<String, Arc<History>>>,
    tokio::sync::OnceCell<std::result::Result<String, String>>,
);

/// One table's metadata, once loaded ([`Histories`]): what loading it failed with, else.
type History = tokio::sync::OnceCell<std::result::Result<iceberg::spec::TableMetadataRef, String>>;

impl Histories {
    /// [`without_answers`] of `now`, worked out once.
    async fn without_answers(
        &self,
        store: &Store,
        scope: &Scope,
        now: &Snapshots,
    ) -> std::result::Result<String, String> {
        self.1
            .get_or_init(|| async {
                without_answers(store, scope, now).await.map_err(|e| {
                    eprintln!("shared answers: the ledger without answers not read: {e:#}");
                    format!("{e:#}")
                })
            })
            .await
            .clone()
    }

    /// Table `t` of `scope` as the catalog has it now.
    async fn of(
        &self,
        store: &Store,
        scope: &Scope,
        t: &Table,
    ) -> std::result::Result<iceberg::spec::TableMetadataRef, String> {
        let name = t.name(scope);
        let cell = self.0.lock().await.entry(name.clone()).or_default().clone();
        cell.get_or_init(|| async {
            let ident = iceberg::TableIdent::new(
                iceberg::NamespaceIdent::new(t.namespace(scope)),
                t.1.to_string(),
            );
            store
                .catalog()
                .load_table(&ident)
                .await
                .map(|t| t.metadata_ref())
                .map_err(|e| format!("{name} not loaded: {e}"))
        })
        .await
        .clone()
    }
}

/// The summary property memstore's own appends carry (`memstore_core`'s `APPEND_ID_PROP`).
const APPEND_ID_PROP: &str = "memstore.append-id";
/// The summary property memstore's rewrites carry (`memstore_core::offboard`'s `REWRITE_ID_PROP`).
const REWRITE_ID_PROP: &str = "memstore.rewrite-id";

/// Whether every snapshot of the table `meta` describes, after snapshot `from` (none: from the table's first) up to
/// its current one, only added rows: Err(why) when one may have taken rows away, or `from` isn't on the current
/// snapshot's line in the history the table keeps (`AMPUP_MEMORY_MAINTAIN_KEEP`: the newest 10 to 20 snapshots).
///
/// The operation alone doesn't tell: iceberg-rust 0.10 has no overwrite, so memstore commits its rewrites (an
/// erasure, an offboarding, a purge, a compaction) as a fast append too, of a snapshot that lists only what it kept.
/// A snapshot counts as added rows only when its operation is `append`, it carries the property memstore's own
/// appends carry and not the one its rewrites carry, and its summary counts nothing deleted. Anything else refuses,
/// a rewrite that kept every row too (no proof of that is written yet).
pub fn appended_since(
    meta: &iceberg::spec::TableMetadata,
    from: Option<i64>,
) -> std::result::Result<(), &'static str> {
    let mut at = meta.current_snapshot().cloned();
    loop {
        let Some(s) = at else {
            // The start of the table's line: only a state before any snapshot is on it here.
            return match from {
                None => Ok(()),
                Some(_) => Err("not on the table's line"),
            };
        };
        if Some(s.snapshot_id()) == from {
            return Ok(());
        }
        let summary = s.summary();
        let props = &summary.additional_properties;
        let deleted = [
            "deleted-data-files",
            "deleted-records",
            "removed-delete-files",
            "added-delete-files",
            "added-position-delete-files",
            "added-equality-delete-files",
            "added-position-deletes",
            "added-equality-deletes",
        ]
        .iter()
        .any(|k| props.get(*k).is_some_and(|v| v != "0"));
        if summary.operation != iceberg::spec::Operation::Append
            || !props.contains_key(APPEND_ID_PROP)
            || props.contains_key(REWRITE_ID_PROP)
            || deleted
        {
            return Err("rewritten since");
        }
        at = match s.parent_snapshot_id() {
            None => None,
            Some(p) => Some(
                meta.snapshot_by_id(p)
                    .cloned()
                    .ok_or("its history expired")?,
            ),
        };
    }
}

/// How often a part's answer is put again when its bytes are the same as the last put here ([`put_due`]): it moves
/// the state it is stored with on, so a server reading it finds fewer tables moved since.
pub const PUT_SAME_EVERY: Duration = Duration::from_secs(10);

/// What a put put ([`PUT`]): the SHA-256 of the answer, and when.
type Put = ([u8; 32], Instant);

/// The puts this process made lately, by key.
static PUT: LazyLock<Mutex<HashMap<String, Put>>> = LazyLock::new(Default::default);

/// The puts [`PUT`] remembers at most.
const PUT_HELD: usize = 10_000;

/// Whether `answer` (a part's answer, without its state) is put under `key` now: when it differs from what this
/// process last put there, or that was [`PUT_SAME_EVERY`] ago. Noted as put.
pub fn put_due(key: &str, answer: &[u8]) -> bool {
    let sum: [u8; 32] = Sha256::digest(answer).into();
    let mut put = PUT.lock().unwrap();
    if let Some((was, at)) = put.get(key)
        && *was == sum
        && at.elapsed() < PUT_SAME_EVERY
    {
        return false;
    }
    if put.len() >= PUT_HELD && !put.contains_key(key) {
        put.retain(|_, (_, at)| at.elapsed() < PUT_SAME_EVERY);
        if put.len() >= PUT_HELD {
            put.clear();
        }
    }
    put.insert(key.to_string(), (sum, Instant::now()));
    true
}

/// The puts noted of the namespaces `which` holds true of are forgotten ([`put_due`]): their answers were purged,
/// and the next answer worked out is put whatever it is.
fn forget_puts(which: impl Fn(&str) -> bool) {
    PUT.lock()
        .unwrap()
        .retain(|k, _| namespace_of(k).is_none_or(|ns| !which(ns)));
}

/// Who an answer is for, as a string the key hashes: the reader exactly, as a kept answer's key names them
/// ([`crate::kept_answers::reader_key`]: who, and every token they read with), so no answer is shared between
/// readers. Written as JSON: a reader's groups come from their IdP and aren't checked as an ACL's entries are, so no
/// separator could tell a group `a,group:b` from groups `a` and `b`. A caller whose answer also turns on how the
/// reader signed in or what the directory lets them read (Home's admin parts) appends that, as Home's own key does.
/// [`class_for`] is the class of one of Home's parts, which may be wider.
pub fn reader_class(who: &Principal) -> String {
    let mut tokens = who.tokens().to_vec();
    tokens.sort();
    json!([who.user(), tokens]).to_string()
}

/// The environment variable that lets readers share answers ([`class_for`]): `on`, or `off` (the default: every
/// answer is one reader's).
pub const CLASSES_ENV: &str = "AMPUP_MEMORY_SHARED_ANSWERS_CLASSES";

/// Whether readers may share answers ([`CLASSES_ENV`]), read once: off unless it says `on` (or `true`, `1`).
pub fn classes_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var(CLASSES_ENV).unwrap_or_default();
        match v.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => true,
            "" | "off" | "false" | "0" => false,
            other => {
                eprintln!("shared answers: {CLASSES_ENV}={other:?} is not on or off: off");
                false
            }
        }
    })
}

/// Who may share a part's answer ([`sharing`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// Every reader of the scope: the answer reads nothing that turns on its reader.
    Everyone,
    /// Every reader who reads the same of each visibility state the answer reads ([`crate::Vis::reader_entries`]).
    Visible,
    /// The reader alone ([`reader_class`]).
    Reader,
}

/// Who may share the answer of Home's part `part`, found by following each part's op to everything it reads of its
/// reader. The proof for [`Sharing::Visible`]: a part's answer is a function of the states it reads and of its
/// reader, and the reader is read only through the episodes each state lets them read ([`crate::Vis`]'s `seen`, the
/// OR of the ACL index's bitmaps of their tokens). A token no episode of a state names adds nothing to that OR, so
/// the reader's tokens that some episode names ([`crate::auth::AclIndex::matched`], with the token of a reader of
/// everything) decide it: two readers with the same ones read the same episodes, facts and documents of the state,
/// and get the same answer. In particular `*` and `user:<id>`, in every reader's tokens, count only where an ACL of
/// the state names them: a reader no ACL names by their id shares with their colleagues of the same groups, one an
/// ACL names has a class of their own.
///
/// - `schema`: how many types main's schema declares, the same for every reader of the scope ([`Sharing::Everyone`]).
/// - `branches` (`browse {op: "branches"}`): each branch's compiled skills the reader may read, through
///   `Session::documents` (`Vis::version_at`, `Vis::can_see_episode`), only `seen` ([`Sharing::Visible`], of main's
///   state and every other branch's it reads: [`class_for`]).
/// - Every other part reads its reader by who they are, beyond what the ACLs grant ([`Sharing::Reader`]):
///   - `counts`: the sessions folder counts the sessions whose source names the reader as author (`browse::filed`),
///     whatever their ACL (one shared with a team is the author's alone there), and the advice their own space's
///     (`advice::Space::mine`);
///   - `week`: the advice served counts the sessions the reader asked (`advice::usage`, the answer log's `asker`,
///     which no ACL names), and their own notes;
///   - `lessons`: the reader's own lessons and sessions (`lessons::run`, `me`);
///   - `conflicts`: those on the entities the reader owns (`conflict_ops`, the owners against `me`);
///   - `proposals` (those waiting on the reader), `dream` and `activity` (an org's admins): who they are and their
///     standing, as Home's own key has them.
pub fn sharing(part: &str) -> Sharing {
    match part {
        "schema" => Sharing::Everyone,
        "branches" => Sharing::Visible,
        _ => Sharing::Reader,
    }
}

/// The class of Home's part `part` for reader `who` in `scope`, as the key ([`last_key`]) takes it: with
/// [`classes_on`], who may share it ([`sharing`]); else, and for a part each reader has their own of,
/// [`reader_class`] followed by `standing` (what else the answer turns on of the reader: how they signed in,
/// whether they read everyone's sessions, what the directory lets them read, as Home's key has them).
///
/// A [`Sharing::Visible`] class is of the states the answer reads: main's as it is now and every other branch's
/// built here, as `branches` pins them, written as JSON. An ACL entry holds no `,` (`validate_acl_entry`) and a
/// branch's name no `=` or newline (`Store::make_branch`), but JSON keeps the class unambiguous whatever they allow.
/// Worked out before the answer (for the get) and again after it (for the put): put only when both are the same
/// and name every branch the answer counts skills on, so the class is of every state the answer read (a branch's
/// state built while it was worked out moves it). A branch not built here is left out, so a server holding other
/// branches' states than the answer's has another class and shares nothing. One holding an older state of the same
/// branches may count fewer of a reader's tokens, never more: their tokens are then a superset of the answer's
/// reader's on every state it read, so the answer may show them less than they read, never more.
pub async fn class_for(
    store: &Store,
    scope: &Scope,
    part: &str,
    who: &Principal,
    standing: &str,
) -> Result<String> {
    let sharing = match classes_on() {
        true => sharing(part),
        false => Sharing::Reader,
    };
    Ok(match sharing {
        Sharing::Reader => format!("reader\n{}\n{standing}", reader_class(who)),
        Sharing::Everyone => "everyone".to_string(),
        Sharing::Visible => {
            let mut branches: Vec<String> =
                store.branch_heads_held(scope).await?.into_keys().collect();
            branches.retain(|b| b != crate::MAIN);
            branches.sort();
            let main = store.lagging_visibility(scope, crate::MAIN).await?;
            let others = futures::future::try_join_all(
                branches.iter().map(|b| store.pin_if_built(scope, b)),
            )
            .await?;
            let mut states = vec![json!([crate::MAIN, main.reader_entries(who)])];
            for (b, s) in branches.iter().zip(others) {
                // A branch not built is read by no answer: none of its tokens counts.
                if let Some(s) = s {
                    states.push(json!([b, s.vis().reader_entries(who)]));
                }
            }
            format!("visible\n{}", Value::Array(states))
        }
    })
}

/// Until when an answer worked out from visibility state `vis` may be kept (µs since the epoch): at most [`TTL`]
/// from now, and never past when the next of its episodes ages out ([`crate::Vis::fresh_until`]), which writes no
/// table to move the key. Worked out once for every part of one answer and stored as it is: an expiry worked out
/// again at each part's write would slide past the episode's ageing out by however long the earlier writes took.
/// Not later than now: keep none.
pub fn expiry_for(vis: &crate::Vis) -> i64 {
    let cap = crate::now_micros() + TTL.as_micros() as i64;
    vis.fresh_until().map_or(cap, |until| until.min(cap))
}
