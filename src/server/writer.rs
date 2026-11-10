//! One server at a time does the background writing on a catalog: extraction, summarising, sharing,
//! compiling, learning advice, ending and judging sessions, and search index updates. None of these claims
//! its work in the store (each reads what is left to do and does it), so two servers running them at once
//! pay twice for the same model calls, commit the same facts twice under new ids, and can undo each other's
//! search index updates. Two servers on one catalog is what a rolling deploy is for a few minutes: the new
//! one warms up while the old one serves.
//!
//! The lease lives beside the system tables (`<memory_system>/_writer/`), as objects:
//!
//! - `epoch-<n>`: the n-th holder, created only if absent (a conditional write), so of two servers taking
//!   the lease at once exactly one gets each epoch;
//! - `beat-<n>`: the holder's heartbeat, rewritten every [`BEAT`] by the holder alone, and marked released
//!   when it stops.
//!
//! A server takes epoch n+1 only when epoch n's holder released it, or when epoch n's heartbeat hasn't
//! changed for [`TTL`] as the waiting server's own clock runs (it compares what it reads, never the holder's
//! clock with its own). A holder stops writing as soon as it sees a newer epoch, and once its last heartbeat
//! is [`FENCE`] old by its own clock (less than [`TTL`], so it has stopped before anyone may take over),
//! however long a heartbeat write hangs. Its background work goes through a store fenced by the lease
//! ([`crate::Store::fenced`]), so a commit is refused, not written, once the lease is lost, even mid-round.
//! On a rolling deploy the old server stops its background work and releases the lease on SIGTERM, and the
//! new one starts writing within a [`BEAT`] of that.
//!
//! The lease is reached with the first of these that can write beside the system tables: the ledger table's
//! own storage credentials (vended per table by Snowflake, Databricks and other REST catalogs, or the ones the
//! catalog was configured with), taken again every few minutes since they last about an hour; else, only where
//! the operator named the bucket for it (`MEMSTORE_WRITER_LEASE_IDENTITY_BUCKET`), the server's own identity,
//! built from the location alone (no endpoint or identity setting from the catalog, which could otherwise send
//! this server's tokens elsewhere). Where neither can (vended credentials and no identity of its own), the
//! lease lives in the ledger's own directory (`<ledger>/_writer/`). Every server tries the same places in the
//! same order, deciding by a probe write, and moves on only on a refusal (anything else is tried again), so
//! they all find the same one; each looks again every few minutes, and before taking an epoch at the ledger's
//! directory, and moves (letting go of what it holds) if an earlier place has become reachable. A location of a kind the lease can't be kept in
//! (anything but a local directory, `gs://`, `s3://` or an Azure `abfss://` path) is an error: logged,
//! `memstore_writer_lease_failing` is 1, and `/ready` answers 503 saying so, rather than every server
//! writing (or none). Until the lease's location is known (the catalog answers) a server doesn't write.
//! Only `MEMSTORE_WRITER_LEASE=off` makes a server write without it: for a catalog no other server shares.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Store;

/// How often the holder beats, and a waiting server looks.
pub const BEAT: Duration = Duration::from_secs(10);
/// A holder whose heartbeat hasn't changed for this long has lost the lease: another server may take it.
pub const TTL: Duration = Duration::from_secs(60);
/// A holder whose last heartbeat is this old stops writing, before anyone may take over.
pub const FENCE: Duration = Duration::from_secs(40);
/// How often the lease's storage credentials are taken again from the catalog (and after a failed beat), at the
/// longest: sooner, [`BEFORE_EXPIRY`] before they run out, where they say when ([`credentials_until`]).
const RECREDENTIAL: Duration = Duration::from_secs(300);
/// How long before the storage token it holds runs out the lease takes credentials again: after the catalog has
/// taken its next token (two minutes before, `platform`), well before a beat could be sent with this one.
const BEFORE_EXPIRY: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
struct Beat {
    holder: String,
    /// µs since the epoch, the holder's clock: for people reading the object, never compared.
    at: i64,
    #[serde(default)]
    released: bool,
}

/// Where the lease lives, and how it is reached now.
struct Place {
    /// `<location>/_writer`, for the logs and `/ready`.
    dir: String,
    op: opendal::Operator,
    /// When `op`'s credentials are to be taken again ([`credentials_until`]).
    until: Instant,
    /// Not the first place tried (the ledger's own directory): before taking an epoch here, the earlier
    /// places are tried again, so this server never takes one beside a peer that holds the lease there.
    fallback: bool,
}

pub struct Lease {
    /// No lease kept: this server writes alone (`MEMSTORE_WRITER_LEASE=off`).
    alone: bool,
    /// Never taken: this server only serves (`MEMSTORE_ROLE=serve`), and the background writing, the index
    /// builds among it, is a worker's.
    reader: bool,
    held: AtomicBool,
    /// When this server's last heartbeat as holder was taken (µs on this process's monotonic clock, [`mono`]),
    /// once it landed: it holds the lease only while that is under [`FENCE`] old.
    beat_ok: AtomicI64,
    /// Stopped for good: the server is stopping, and never writes or takes the lease again.
    stopped: AtomicBool,
    place: Mutex<Option<Place>>,
    /// Why the lease can't be kept where the catalog is, if so: `/ready` answers 503 with it.
    unplaceable: Mutex<Option<String>>,
    /// The last failure to beat or take the lease, until one succeeds.
    failing: Mutex<Option<String>>,
    holder: String,
    /// The epoch this server holds.
    mine: tokio::sync::Mutex<Option<u64>>,
    /// The newest epoch's heartbeat as this server last read it, and when it first read it so: a heartbeat
    /// unchanged for [`TTL`] has lapsed.
    seen: Mutex<Option<(u64, Vec<u8>, Instant)>>,
}

/// An error as it may be logged: its chain, with every URL's query string dropped. Storage errors (opendal's
/// `uri` context, a catalog's file IO) name the request's URL, whose query can hold a signed URL's signature or
/// a vended SAS token; the scheme, host and path stay, which is what a reader needs.
pub fn redacted(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    let mut out = String::with_capacity(text.len());
    let mut in_url = false;
    let mut dropping = false;
    let mut word_start = 0;
    for (i, c) in text.char_indices() {
        let ends_word = c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}' | ')' | ']' | '>');
        if ends_word {
            in_url = false;
            dropping = false;
            word_start = i + c.len_utf8();
            out.push(c);
            continue;
        }
        if !in_url && text[word_start..i].ends_with(":/") && c == '/' {
            in_url = true;
        }
        if in_url && c == '?' {
            dropping = true;
            out.push_str("?<redacted>");
        }
        if !dropping {
            out.push(c);
        }
    }
    out
}

/// µs since this process started, on a clock that never goes back.
fn mono() -> i64 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_micros() as i64
}

impl Lease {
    fn new(alone: bool, reader: bool) -> Arc<Lease> {
        let host = std::env::var("HOSTNAME").unwrap_or_default();
        mono();
        Arc::new(Lease {
            alone,
            reader,
            held: AtomicBool::new(false),
            beat_ok: AtomicI64::new(i64::MIN / 2),
            stopped: AtomicBool::new(false),
            place: Mutex::new(None),
            unplaceable: Mutex::new(None),
            failing: Mutex::new(None),
            holder: format!(
                "{host}:{}:{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ),
            mine: Default::default(),
            seen: Mutex::new(None),
        })
    }

    /// No lease: this server is the only writer on its catalog.
    pub fn alone() -> Arc<Lease> {
        Lease::new(true, false)
    }

    /// A lease to be kept beside the catalog's system tables by [`Lease::run`]; not held until then.
    pub fn kept() -> Arc<Lease> {
        Lease::new(false, false)
    }

    /// No lease, ever: this server serves reads and request writes, and leaves the background writing to the
    /// server that holds the lease.
    pub fn reader() -> Arc<Lease> {
        Lease::new(false, true)
    }

    /// Whether this server does the background writing now. Checked before every round of it, every scope
    /// in a round, and (through a fenced store, [`Lease::fence`]) every commit.
    pub fn holds(&self) -> bool {
        !self.reader
            && !self.stopped.load(Ordering::Acquire)
            && (self.alone
                || (self.held.load(Ordering::Acquire)
                    && mono() - self.beat_ok.load(Ordering::Acquire) < FENCE.as_micros() as i64))
    }

    /// [`Lease::holds`], as a store's fence ([`crate::Store::fenced`]).
    pub fn fence(self: &Arc<Self>) -> crate::Fence {
        let me = self.clone();
        Arc::new(move || me.holds())
    }

    /// Why the lease can't be kept where this server's catalog is (a location of a kind that takes none): the
    /// server isn't ready while so.
    pub fn unplaceable(&self) -> Option<String> {
        self.unplaceable.lock().unwrap().clone()
    }

    /// For `/ready`, which answers without credentials: whether this server writes, and the lease's state
    /// (`off`, `locating`, `kept`, `failing` or `unplaceable`; the log says where it is and why).
    pub fn status(&self) -> Value {
        let lease = if self.alone {
            "off"
        } else if self.reader {
            "reader"
        } else if self.unplaceable.lock().unwrap().is_some() {
            "unplaceable"
        } else if self.place.lock().unwrap().is_none() {
            "locating"
        } else if self.failing.lock().unwrap().is_some() {
            "failing"
        } else {
            "kept"
        };
        json!({"writer": self.holds(), "lease": lease})
    }

    fn report(&self) {
        crate::metrics::WRITER_LEASE_HELD.set(i64::from(self.holds()));
        let failing =
            self.unplaceable.lock().unwrap().is_some() || self.failing.lock().unwrap().is_some();
        crate::metrics::WRITER_LEASE_FAILING.set(i64::from(failing));
    }

    /// Keep the lease for as long as the server runs: find where it lives, then take it when it's free and
    /// beat while holding it.
    pub async fn run(self: Arc<Self>, store: impl Fn() -> Store + Send + 'static) {
        if self.alone || self.reader {
            self.report();
            return;
        }
        let mut wait = Duration::from_secs(1);
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            // Bounded: a catalog or store that hangs is a failure to say (and retry), not a silent wait.
            let located =
                match tokio::time::timeout(3 * BEAT, locate(&store(), &super::cell::lease_dir()))
                    .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!(
                        "finding it took over {}s",
                        (3 * BEAT).as_secs()
                    )),
                };
            match located {
                Ok(Located::At(place)) => {
                    eprintln!("writer lease: kept at {}", place.dir);
                    *self.place.lock().unwrap() = Some(place);
                    break;
                }
                Ok(Located::Never(why)) => {
                    eprintln!(
                        "ERROR writer lease: {why}: nothing is written in the background, and /ready answers 503"
                    );
                    *self.unplaceable.lock().unwrap() = Some(why);
                    self.report();
                    return;
                }
                Err(e) => {
                    let e = redacted(&e);
                    eprintln!(
                        "writer lease: not known yet, nothing written in the background: {e}"
                    );
                    *self.failing.lock().unwrap() = Some(e);
                    self.report();
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(BEAT);
                }
            }
        }
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            // Found again every few minutes, after a failure, and before a server not holding the lease may
            // take an epoch at a fallback place (the earlier places may have become reachable).
            let stale = {
                let place = self.place.lock().unwrap();
                place.as_ref().is_none_or(|p| {
                    Instant::now() >= p.until || (p.fallback && !self.held.load(Ordering::Acquire))
                })
            } || self.failing.lock().unwrap().is_some();
            if stale {
                self.relocate(&store()).await;
            }
            if self.unplaceable.lock().unwrap().is_some() {
                self.report();
                return;
            }
            let mut r = self.beat_once().await;
            // A store that answered with a refusal (credentials that ran out before they were taken again: the
            // token cut short or revoked) is asked once more with credentials taken again now, rather than this
            // server missing a beat.
            if let Err(e) = &r
                && answered(e)
                && !self.stopped.load(Ordering::Acquire)
            {
                eprintln!(
                    "writer lease: a beat failed, tried again with fresh credentials: {}",
                    redacted(e)
                );
                self.relocate(&store()).await;
                r = self.beat_once().await;
            }
            match r {
                Ok(()) => *self.failing.lock().unwrap() = None,
                Err(e) => {
                    let e = redacted(&e);
                    *self.failing.lock().unwrap() = Some(e.clone());
                    let mut mine = self.mine.lock().await;
                    let age = mono() - self.beat_ok.load(Ordering::Acquire);
                    match *mine {
                        Some(n) if age >= TTL.as_micros() as i64 => {
                            // Someone else may hold it now: take it again only as a newcomer.
                            *mine = None;
                            self.held.store(false, Ordering::Release);
                            eprintln!(
                                "writer lease: epoch {n} lapsed, background writing stopped: {e}"
                            );
                        }
                        Some(n) => eprintln!("writer lease: heartbeat of epoch {n} failed: {e}"),
                        None => eprintln!("writer lease: {e}"),
                    }
                }
            }
            self.report();
            tokio::time::sleep(BEAT).await;
        }
    }

    /// One beat, bounded: a heartbeat that hangs is a failed one (the fence holds whatever the store does).
    async fn beat_once(&self) -> Result<()> {
        match tokio::time::timeout(BEAT, self.tick()).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("the lease took over {}s", BEAT.as_secs())),
        }
    }

    /// Find the lease's place again, with fresh credentials (vended ones last about an hour): every few minutes,
    /// after a failure, and before taking an epoch at a fallback place. Where it is now somewhere else (an
    /// earlier place became reachable, as it is for this server's peers), this server lets go of any epoch it
    /// holds at the old place and moves, so it never holds one apart from its peers for long. A failure keeps
    /// the place and credentials it had.
    async fn relocate(&self, store: &Store) {
        let found =
            match tokio::time::timeout(3 * BEAT, locate(store, &super::cell::lease_dir())).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "finding it took over {}s",
                    (3 * BEAT).as_secs()
                )),
            };
        let place = match found {
            Ok(Located::At(p)) => p,
            Ok(Located::Never(why)) => {
                eprintln!("ERROR writer lease: {why}");
                *self.unplaceable.lock().unwrap() = Some(why);
                self.leave().await;
                return;
            }
            Err(e) => {
                eprintln!(
                    "writer lease: not found again (kept where it was): {}",
                    redacted(&e)
                );
                return;
            }
        };
        let moved = self
            .place
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.dir != place.dir);
        if moved {
            eprintln!("writer lease: now kept at {}", place.dir);
            self.leave().await;
        }
        *self.place.lock().unwrap() = Some(place);
    }

    /// Let go of the epoch this server holds where the lease was kept (it is moving, or can't be kept).
    async fn leave(&self) {
        let mine = self.mine.lock().await.take();
        self.held.store(false, Ordering::Release);
        if let (Some(n), Ok(op)) = (mine, self.op()) {
            let _ = self.beat(&op, n, true).await;
            eprintln!("writer lease: epoch {n} let go: background writing stopped");
        }
    }

    fn op(&self) -> Result<opendal::Operator> {
        self.place
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| p.op.clone())
            .context("the writer lease's location isn't known yet")
    }

    async fn tick(&self) -> Result<()> {
        let op = self.op()?;
        let mut mine = self.mine.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let listed = op.list("/").await.context("list the writer lease")?;
        let number = |prefix: &str| -> Vec<u64> {
            listed
                .iter()
                .filter_map(|e| e.name().strip_prefix(prefix)?.parse::<u64>().ok())
                .collect()
        };
        let epochs = number("epoch-");
        let top = epochs.iter().copied().max();
        // The next epoch is past every one ever seen here, heartbeats included: an epoch object that went
        // missing (a cleanup of the directory it is in) never lets its number be taken again.
        let newest = top.into_iter().chain(number("beat-")).max();
        if let Some(n) = *mine {
            // Ours no longer: a newer epoch, or ours gone (its holder can't tell it still holds it).
            let lost = top.is_some_and(|t| t > n) || !epochs.contains(&n);
            // Past the fence (heartbeats failing for FENCE): a waiter may have judged it lapsed and taken the
            // next epoch after this listing. Not resumed: taken again as the next epoch, if still free.
            let fenced = mono() - self.beat_ok.load(Ordering::Acquire) >= FENCE.as_micros() as i64;
            if lost || fenced {
                *mine = None;
                if self.held.swap(false, Ordering::AcqRel) || lost {
                    eprintln!(
                        "writer lease: epoch {n} {}: background writing stopped",
                        if lost {
                            "was taken after ours, or is gone"
                        } else {
                            "is past its fence"
                        }
                    );
                }
                if lost {
                    return Ok(());
                }
                return self
                    .take(&op, &mut mine, newest.map_or(n + 1, |m| m.max(n) + 1))
                    .await;
            }
            self.beat(&op, n, false).await?;
            if !self.held.swap(true, Ordering::AcqRel) {
                eprintln!("writer lease: heartbeat back (epoch {n}): background writing resumed");
            }
            // Epochs long gone: only the newest few are ever read.
            for old in epochs.iter().filter(|&&e| e + 2 < n) {
                let _ = op.delete(&beat(*old)).await;
                let _ = op.delete(&epoch(*old)).await;
            }
            return Ok(());
        }
        let free = match top {
            None => true,
            Some(n) => self.lapsed(&op, n).await?,
        };
        if !free {
            return Ok(());
        }
        self.take(&op, &mut mine, newest.map_or(1, |n| n + 1)).await
    }

    /// Take epoch `next` if no one has: a conditional create, so of several servers exactly one gets it.
    async fn take(&self, op: &opendal::Operator, mine: &mut Option<u64>, next: u64) -> Result<()> {
        let body = serde_json::to_vec(&Beat {
            holder: self.holder.clone(),
            at: crate::now_micros(),
            released: false,
        })?;
        match op.write_with(&epoch(next), body).if_not_exists(true).await {
            Ok(_) => {}
            Err(e) if e.kind() == opendal::ErrorKind::ConditionNotMatch => return Ok(()),
            Err(e) => return Err(e).context("take the writer lease"),
        }
        *mine = Some(next);
        self.beat(op, next, false).await?;
        self.held.store(true, Ordering::Release);
        eprintln!(
            "writer lease: epoch {next} is ours ({}): this server does the background writing",
            self.holder
        );
        Ok(())
    }

    /// Whether epoch `n`'s holder let it go: released, or its heartbeat (or, before its first, the epoch
    /// itself) unchanged for [`TTL`] since this server first read it so. Only this server's clock is used:
    /// a holder's clock that runs ahead or behind never shortens the wait. One that can't be read is held:
    /// never two writers on a guess.
    async fn lapsed(&self, op: &opendal::Operator, n: u64) -> Result<bool> {
        let read = |path: String| async move {
            match op.read(&path).await {
                Ok(b) => Ok(Some(b.to_vec())),
                Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(anyhow::Error::from(e).context(format!("read {path}"))),
            }
        };
        let body = match read(beat(n)).await? {
            Some(b) => b,
            None => match read(epoch(n)).await? {
                Some(b) => b,
                // Listed a moment ago and gone now (cleared as long gone): the next look lists the newest.
                None => return Ok(false),
            },
        };
        let parsed = serde_json::from_slice::<Beat>(&body)
            .with_context(|| format!("epoch {n}'s heartbeat is unreadable: held"))?;
        if parsed.released {
            return Ok(true);
        }
        let mut seen = self.seen.lock().unwrap();
        match &*seen {
            Some((m, b, since)) if *m == n && *b == body => Ok(since.elapsed() >= TTL),
            _ => {
                *seen = Some((n, body, Instant::now()));
                Ok(false)
            }
        }
    }

    /// Write epoch `n`'s heartbeat. Its time counts toward [`Lease::holds`] only once it has landed, and is
    /// taken before writing: a slow write never extends the fence.
    async fn beat(&self, op: &opendal::Operator, n: u64, released: bool) -> Result<()> {
        let taken = mono();
        let body = serde_json::to_vec(&Beat {
            holder: self.holder.clone(),
            at: crate::now_micros(),
            released,
        })?;
        op.write(&beat(n), body)
            .await
            .context("write the writer heartbeat")?;
        if !released {
            self.beat_ok.fetch_max(taken, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Stop writing now and for good: [`Lease::holds`] is false from here on, so every fenced commit is
    /// refused. The server is stopping; [`Lease::release`] then lets the lease go.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.held.store(false, Ordering::Release);
        self.report();
    }

    /// Stop writing and let the lease go now (the server is stopping), so the next server need not wait
    /// out [`TTL`]. Called once the background work has stopped.
    pub async fn release(&self) {
        if self.alone {
            return;
        }
        self.stop();
        // Under the lock: a tick in flight finishes first, and none starts after.
        let mine = self.mine.lock().await.take();
        self.held.store(false, Ordering::Release);
        if let (Some(n), Ok(op)) = (mine, self.op()) {
            match self.beat(&op, n, true).await {
                Ok(()) => eprintln!("writer lease: epoch {n} released"),
                Err(e) => eprintln!(
                    "writer lease: epoch {n} not released ({}): the next server takes it in {}s",
                    redacted(&e),
                    TTL.as_secs()
                ),
            }
        }
    }
}

enum Located {
    At(Place),
    /// The lease can't be kept there, ever: why.
    Never(String),
}

/// Where `leaf` (a directory kept beside the lease's) lives on `store`'s catalog, and how it is reached
/// now: where the lease would be found, with `leaf` in place of the lease's own directory. An error: not
/// known yet (try again); none: it can't be kept on this catalog. With it, when its credentials are to be taken
/// again ([`credentials_until`]).
pub(super) async fn beside(
    store: &Store,
    leaf: &str,
) -> Result<Option<(opendal::Operator, Instant)>> {
    Ok(match locate(store, leaf).await? {
        Located::At(p) => Some((p.op, p.until)),
        Located::Never(_) => None,
    })
}

/// When credentials taken now from file IO properties `props` are to be taken again: after [`RECREDENTIAL`], or
/// [`BEFORE_EXPIRY`] before their token runs out where they say when (`platform::token_expires_at`; the metadata
/// server hands out its cached token with what life it has left, and the catalog takes its next one two minutes
/// before then, so held five minutes a token ran out under the lease and the reports about once an hour).
fn credentials_until(props: &HashMap<String, String>) -> Instant {
    let left = crate::platform::token_expires_at(props).map(|at| {
        (at - chrono::Utc::now())
            .to_std()
            .unwrap_or_default()
            .saturating_sub(BEFORE_EXPIRY)
    });
    Instant::now() + left.map_or(RECREDENTIAL, |t| t.min(RECREDENTIAL))
}

/// Whether `e` is the store's answer (an error from the object store's client), not a timeout of ours.
pub(super) fn answered(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<opendal::Error>().is_some())
}

/// Where the lease lives on `store`'s catalog: `<memory_system>/<leaf>`, or inside the ledger's own
/// directory where the catalog's credentials reach only that. An error: not known yet (try again).
async fn locate(store: &Store, leaf: &str) -> Result<Located> {
    let s = store.system_storage().await?;
    let scheme = s
        .system
        .split_once("://")
        .map_or("file", |(scheme, _)| scheme)
        .to_ascii_lowercase();
    let at = |location: &str| format!("{}/{leaf}", location.trim_end_matches('/'));
    let until = credentials_until(&s.props);
    let place = |dir: String, op, until| Place {
        dir,
        op,
        until,
        fallback: false,
    };
    match scheme.as_str() {
        "file" => {
            let dir = at(&s.system);
            Ok(Located::At(place(
                dir.clone(),
                operator(&dir, &s.props)?,
                until,
            )))
        }
        "gs" | "gcs" | "s3" | "s3a" | "s3n" | "abfs" | "abfss" => {
            // Decided by what the lease needs, a write and a list there, not a list alone: a place this
            // server can list but not write would leave every server failing to take it. Every server on
            // the catalog tries the same places in the same order, so they agree.
            let system = at(&s.system);
            // Only a refusal (PermissionDenied) moves on to the next place: any other failure (an outage, a
            // timeout, a missing credential) is an error, tried again later, so two servers never settle on
            // different places because they met different passing failures.
            let refused = |e: opendal::Error, what: &str, whose: &str| -> Result<()> {
                if e.kind() == opendal::ErrorKind::PermissionDenied {
                    eprintln!(
                        "writer lease: {whose} credentials are refused at {what}: {}",
                        redacted(&anyhow::Error::from(e))
                    );
                    Ok(())
                } else {
                    Err(anyhow::Error::from(e).context(format!("write and list {what}")))
                }
            };
            // 1. The system tables' directory, with the catalog's credentials for them.
            let op = operator(&system, &s.props)?;
            match probe(&op).await {
                Ok(()) => return Ok(Located::At(place(system, op, until))),
                Err(e) => refused(e, &system, "the catalog's")?,
            }
            // 2. The same, with this server's own identity (a GKE workload's), only where the operator named
            // that bucket for it (`MEMSTORE_WRITER_LEASE_IDENTITY_BUCKET`): built from the location alone, never
            // the catalog's endpoints or identity settings, so a catalog can't send this server's own tokens
            // anywhere but the bucket the operator named.
            // Its tokens are the client's own, which takes them again itself.
            if let Some(op) = identity_operator(&system)? {
                match probe(&op).await {
                    Ok(()) => {
                        return Ok(Located::At(place(
                            system,
                            op,
                            Instant::now() + RECREDENTIAL,
                        )));
                    }
                    Err(e) => refused(e, &system, "this server's own")?,
                }
            }
            // 3. Nothing reaches beside the tables (credentials vended per table, no identity of its own: a
            // Snowflake or Databricks app): inside the ledger's own directory.
            let ledger = at(&s.ledger);
            let op = operator(&ledger, &s.props)?;
            probe(&op).await.with_context(|| {
                format!("write and list {ledger} (nothing this server holds may write at {system})")
            })?;
            Ok(Located::At(Place {
                fallback: true,
                ..place(ledger, op, until)
            }))
        }
        other => Ok(Located::Never(format!(
            "the system tables are at {other}://, where the writer lease can't be kept (a local directory, gs://, \
             s3:// or abfss:// only); set MEMSTORE_WRITER_LEASE=off if this is the catalog's only server"
        ))),
    }
}

/// The lease's store at `dir` reached with this server's own identity (application default credentials, the
/// instance's or workload's), where the operator allows it: `MEMSTORE_WRITER_LEASE_IDENTITY_BUCKET` names, exactly,
/// `dir`'s whole authority: a valid gs/s3 bucket name, or `<filesystem>@<account>.dfs.core.windows.net` over
/// `abfss` (never plain-http `abfs`). None otherwise. Nothing comes from the catalog: no
/// endpoint, region or identity-provider setting, so the server's own tokens go only to the store's own
/// endpoint for that bucket.
fn identity_operator(dir: &str) -> Result<Option<opendal::Operator>> {
    let Ok(allowed) = std::env::var("MEMSTORE_WRITER_LEASE_IDENTITY_BUCKET") else {
        return Ok(None);
    };
    let allowed = allowed.trim();
    let Some((scheme, rest)) = dir.split_once("://") else {
        return Ok(None);
    };
    // The whole authority, exactly as `operator` takes it (everything before the first '/'): compared as is,
    // never a part of it, so no spelling the operator reads one way and the store another gets through.
    let authority = rest.split('/').next().unwrap_or_default();
    let ok = match scheme.to_ascii_lowercase().as_str() {
        "gs" | "gcs" | "s3" | "s3a" | "s3n" => bucket_name(authority),
        // `<filesystem>@<account>.dfs.core.windows.net`, named whole; over https only (abfs is plain http).
        "abfss" => authority.split_once('@').is_some_and(|(fs, host)| {
            bucket_name(fs)
                && host
                    .strip_suffix(".dfs.core.windows.net")
                    .is_some_and(|account| {
                        (3..=24).contains(&account.len())
                            && account
                                .chars()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    })
        }),
        _ => false,
    };
    if !ok || allowed.is_empty() || authority != allowed {
        eprintln!(
            "writer lease: not trying this server's own identity at {scheme}://{authority}: \
             MEMSTORE_WRITER_LEASE_IDENTITY_BUCKET doesn't name exactly that (a bucket, or <filesystem>@<account>.dfs.core.windows.net over abfss)"
        );
        return Ok(None);
    }
    operator(dir, &HashMap::new()).map(Some)
}

/// A bucket (or ADLS filesystem) name as the stores allow it: 3 to 63 lower-case letters, digits, '-', '_' and
/// '.', starting and ending with a letter or digit. Nothing else ('@', ':', '%', upper case) passes.
fn bucket_name(name: &str) -> bool {
    let ends = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    (3..=63).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && ends(name.chars().next())
        && ends(name.chars().last())
}

/// Whether the lease can be kept through `op`: an object written, listed and deleted.
async fn probe(op: &opendal::Operator) -> opendal::Result<()> {
    let name = format!("probe-{}", uuid::Uuid::new_v4().simple());
    op.write(&name, "probe").await?;
    op.list("/").await?;
    op.delete(&name).await
}

/// The object store at `dir` (`<location>/_writer`), reached with `props`, the catalog's file IO properties
/// for the system tables (as iceberg reads them: `gcs.*`, `s3.*`, `client.*`, `adls.*`). Without
/// credentials there, the environment's (application default credentials, the instance's role).
fn operator(dir: &str, props: &HashMap<String, String>) -> Result<opendal::Operator> {
    opendal::init_default_registry();
    let prop = |k: &str| props.get(k).filter(|v| !v.is_empty()).cloned();
    let truthy = |k: &str| {
        prop(k)
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "t" | "1" | "on"))
    };
    let mut config: Vec<(String, String)> = vec![];
    let mut set = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            config.push((k.to_string(), v));
        }
    };
    let (scheme, rest) = dir.split_once("://").unwrap_or(("file", dir));
    let scheme = scheme.to_ascii_lowercase();
    let bucket_root = |rest: &str| {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        (bucket.to_string(), format!("/{}", prefix.trim_matches('/')))
    };
    let service = match scheme.as_str() {
        "file" => {
            // A plain directory: the conditional create is an exclusive open (the default local operator's
            // write-then-rename would replace an epoch another server just made).
            std::fs::create_dir_all(rest).with_context(|| format!("create {rest}"))?;
            set("root", Some(rest.to_string()));
            "fs"
        }
        "gs" | "gcs" => {
            let (bucket, root) = bucket_root(rest);
            set("bucket", Some(bucket));
            set("root", Some(root));
            set("token", prop("gcs.oauth2.token"));
            set("credential", prop("gcs.credentials-json"));
            set("endpoint", prop("gcs.service.path"));
            // As iceberg reads them (an emulator, a public bucket).
            if props.contains_key("gcs.no-auth") {
                set("skip_signature", Some("true".into()));
                set("disable_vm_metadata", Some("true".into()));
                set("disable_config_load", Some("true".into()));
            }
            if truthy("gcs.allow-anonymous") {
                set("skip_signature", Some("true".into()));
            }
            "gcs"
        }
        "s3" | "s3a" | "s3n" => {
            let (bucket, root) = bucket_root(rest);
            set("bucket", Some(bucket));
            set("root", Some(root));
            set("endpoint", prop("s3.endpoint"));
            set("access_key_id", prop("s3.access-key-id"));
            set("secret_access_key", prop("s3.secret-access-key"));
            set("session_token", prop("s3.session-token"));
            set(
                "region",
                prop("client.region").or_else(|| prop("s3.region")),
            );
            set("role_arn", prop("client.assume-role.arn"));
            set("external_id", prop("client.assume-role.external-id"));
            set("role_session_name", prop("client.assume-role.session-name"));
            // Iceberg's default: virtual-host style, unless the catalog asks for path style.
            set(
                "enable_virtual_host_style",
                Some((!truthy("s3.path-style-access")).to_string()),
            );
            // Server-side encryption as the catalog asks for it (a bucket policy may refuse writes without).
            match prop("s3.sse.type")
                .map(|t| t.to_ascii_lowercase())
                .as_deref()
            {
                None | Some("none") => {}
                Some("s3") => set("server_side_encryption", Some("AES256".into())),
                Some("kms") => {
                    set("server_side_encryption", Some("aws:kms".into()));
                    set("server_side_encryption_aws_kms_key_id", prop("s3.sse.key"));
                }
                Some("custom") => {
                    set(
                        "server_side_encryption_customer_algorithm",
                        Some("AES256".into()),
                    );
                    set("server_side_encryption_customer_key", prop("s3.sse.key"));
                    set(
                        "server_side_encryption_customer_key_md5",
                        prop("s3.sse.md5"),
                    );
                }
                Some(other) => {
                    anyhow::bail!("s3.sse.type {other}: expected none, s3, kms or custom")
                }
            }
            "s3"
        }
        "abfs" | "abfss" => {
            // abfss://<filesystem>@<account>.dfs.core.windows.net/<path>
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let (filesystem, host) = authority
                .split_once('@')
                .with_context(|| format!("{dir}: no <filesystem>@<account> in it"))?;
            let account = host.split('.').next().unwrap_or_default().to_string();
            let http = if scheme == "abfss" { "https" } else { "http" };
            set("filesystem", Some(filesystem.to_string()));
            set("endpoint", Some(format!("{http}://{host}")));
            set("root", Some(format!("/{}", path.trim_matches('/'))));
            set(
                "account_name",
                prop("adls.account-name").or(Some(account.clone())),
            );
            set("account_key", prop("adls.account-key"));
            // A vended SAS token is keyed by the account's host (`adls.sas-token.<account>.dfs.core.windows.net`).
            let sas = prop("adls.sas-token").or_else(|| {
                props
                    .iter()
                    .find(|(k, v)| {
                        !v.is_empty()
                            && k.strip_prefix("adls.sas-token.")
                                .is_some_and(|h| h == host || h.split('.').next() == Some(&account))
                    })
                    .map(|(_, v)| v.clone())
            });
            set("sas_token", sas);
            set("tenant_id", prop("adls.tenant-id"));
            set("client_id", prop("adls.client-id"));
            set("client_secret", prop("adls.client-secret"));
            set("authority_host", prop("adls.authority-host"));
            "azdls"
        }
        other => anyhow::bail!("{other}:// can't hold the writer lease"),
    };
    // Bounded: a request that hangs fails (the lease's own loop retries; a conditional create is never retried
    // blind, which could take an epoch twice over).
    Ok(opendal::Operator::via_iter(service, config)
        .with_context(|| format!("the writer lease's object store {dir}"))?
        .layer(opendal::layers::TimeoutLayer::new().with_timeout(BEAT)))
}

fn epoch(n: u64) -> String {
    format!("epoch-{n:020}")
}

fn beat(n: u64) -> String {
    format!("beat-{n:020}")
}
