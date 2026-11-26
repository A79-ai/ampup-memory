//! `/browse {"op": "home"}`: what Studio's Home shows, from one request on the memory the caller selected, instead of
//! a request per number. Each part is read as the op it stands for would read it for this caller, with the same
//! checks: a part the caller may not read (an admin's, where they aren't one), or one that fails, is null, and Home
//! leaves its numbers out, as it does when that op is refused. Only counts and short lists, never a page of facts:
//!
//! - `counts`: the root folders' counts (`ls /`): sources, entities, notes, advice, skills, sessions;
//! - `sources`: the newest source folders (`ls sources`), each with its count and when it last brought something;
//! - `week`: the facts that took effect in the last seven days: how many, and of the newest of them how many were
//!   read (`sample`), the entities they are about and where they came from (`by_source`, by a source's first
//!   segment);
//! - `month` (with `week`): the facts that took effect on each of the last 30 days (UTC), for Home's chart;
//! - `lessons`: how many lessons were learned in the week (`lessons {count: true}`);
//! - `advice`: the team's notes served in the week (`advice_usage`): how often, how many notes, and how many of
//!   those served wait for ratifying;
//! - `activity` (an org's admins, where it shows its people's activity): who asked or ran something in the week,
//!   how many sessions, how many of them were given advice, and the sessions, runs and answers begun on each of the
//!   last 30 days (`by_day`);
//! - `proposals`: those that wait on the caller, and all that wait for review; `conflicts`: those on what the
//!   caller owns;
//! - `dream` (an org's admins): its last pass, what waits and what it applied (as `dream_decisions` answers, with
//!   no decisions);
//! - `branches`: the memory's branches with their skills (`branches`), a branch whose state isn't built with none
//!   known; `schema`: how many types it declares.
//!
//! Home builds no branch's visibility state: each is a build of the whole memory, seconds on a large one. The
//! proposals are counted, not listed ([`crate::Store::proposals_waiting`]): where each stands is read from the ledger
//! and the governance records alone, and only those waiting for review are checked for whether the reader may see
//! them, from main's state. The few main's state can't tell wait for their branch's state, built meanwhile in the
//! background one at a time, and no longer than the other parts asked for.
//!
//! The parts that read every fact or record are kept per reader by memory state ([`crate::kept_answers`]): the root
//! counts, the review lists and the lessons are kept by their own ops; the week's facts, the advice served and the
//! sources here, for the hour the week is counted back from. The advice served and the activity are counted from the
//! answer log as kept with them, which may be up to [`crate::kept_answers::SUMMARY_STALE`] behind it: a session logged since
//! counts once they are worked out again.
//!
//! A first ask waits [`FIRST`] for its parts, counted from when the request began (signing in and pinning the
//! memory included): those not worked out by then (a large memory's first read since a restart, or since memory was
//! taken away) are named in `pending` and left out, and go on being worked out. An ask naming them (`"parts":
//! [...]`) waits up to [`AGAIN`] for them, joining that work rather than starting it again; the root's counts,
//! counted in the background, are waited for as they land.
//!
//! Answered at once: an ask on main is answered before the request pins anything (a pin may wait seconds for a build
//! of main) from the asker's last whole answer ([`AHEAD`], kept by the reader key below), marked `kept` with its age,
//! when it is younger than [`KEPT_FOR`] and memory lost nothing since it was worked out, as a kept answer is checked
//! ([`crate::kept_answers`]): the scope's loss count (every forget, delete, retraction, narrowed ACL, retention,
//! purge and offboarding made here, and every loss a build of main found), main's removals, the facts table's
//! generation and the answer log, with main's last built state checked against the tables as read now, never built
//! for it ([`crate::Store::main_lost_nothing`]). Its numbers may then lag what was added since: the asker's next answer is
//! worked out in the background meanwhile once it is old enough ([`due`]), for the next open. Where it can't be told without a build,
//! the ask goes on as below. An ask that works its parts out itself keeps its whole answer the same way, or, with
//! parts pending, has the whole worked out in the background for its ask again.
//!
//! Worked out ahead: whoever asks for Home is watched for [`WATCHED_FOR`] after, and their whole answer is worked out
//! again in the background once main's state moved (the answer log growing alone doesn't count: the parts that read
//! it lag it as their kept answers do), and every [`RENEW`] while it doesn't. A run works out at most [`PER_RUN`]
//! readers' answers, those worked out longest ago first, and an org has at most one run every
//! `AMPUP_MEMORY_HOME_AHEAD_SECS` ([`run_every`], 60 s by default), with [`AT_ONCE`] memories worked out at a time across
//! the server. Its parts are read as an ask reads them: a kept part from an earlier state memory lost nothing since
//! may answer while that part's next answer is worked out in the background ([`crate::kept_answers`]), so an answer
//! composed of such parts is worked out once more at the next run, from the parts worked out meanwhile. An ask
//! answers with the last answer worked out ([`AHEAD`]) when it was worked out at the state the ask finds, or at one
//! before it that memory lost nothing since ([`crate::kept_answers`]: no forget, delete, retraction, narrowed ACL,
//! offboarding or rewrite of the facts or answer log between them, and no change here to the review lists) and
//! that is younger than [`KEPT_FOR`] (its numbers then miss only what was added since, until its next answer is
//! worked out), and works its parts out itself (as above) only when there is none. Each answer is one reader's,
//! never another's:
//! kept by who they are and every group they read as, how they signed in, and whether the directory lets them read
//! the admins' parts (an admin of the memory's org, an org that shows its people's activity), as the ask finds
//! them. Not per memory and shared between readers: every part reads only what its reader may read, so the same
//! memory's numbers differ between two readers.
//!
//! Shared between servers ([`crate::shared_answers`], when `AMPUP_MEMORY_SHARED_ANSWERS` names a store): each part's
//! last answer on main is also kept there, one per part and who it is for ([`classes_of`]), overwritten by each put,
//! with the memory state it was worked out at named durably (the snapshot id of every table the part reads, read
//! before the state it is worked out from is pinned, and the hour). An ask looks for its parts there after the
//! asker's own answer kept here ([`at_once`]: before anything is pinned, every part asked for or none; then each part
//! on its own as it is worked out, [`answer`]), and takes one only with proof from the tables that memory lost nothing
//! since its state ([`crate::shared_answers::Last::holds`]): every table a visibility state is built from, and the
//! scope's ledger rows, as they were (any commit to the scope since refuses), and the others it reads moved by
//! memstore's own appends alone. One whose tables moved so is the last answer memory lost nothing since, as a kept
//! answer served while the next is worked out: the ask that takes one has the asker's answer worked out in the
//! background ([`refresh`]) rather than kept as this state's. A part worked out is put there once its work is shown
//! to be at the state it is stored with: no part of it answered from an earlier state
//! ([`crate::kept_answers::sharable`]) or failed, the tables' ids the same after it as before, and main's state the
//! one built from them ([`crate::Store::pin_if_built`]); only when its answer changed, or to move its state on
//! ([`crate::shared_answers::put_due`]). A part found is used only while the scope lost nothing here and its reviewed
//! memory didn't change here since the ids were read (the loss count and the review changes, as a kept answer is
//! checked). The background work of a reader's answer looks there only for its first answer on this server, and
//! takes only a part stored at the state it reads: after that it works each part out and puts it again.

use std::future::Future;

use super::*;

/// The days Home counts back over.
const WEEK_DAYS: i64 = 7;
/// The days Home's chart shows, one bar each (the activity's `by_day` counts as many).
const MONTH_DAYS: i64 = crate::activity::BY_DAY_DAYS;
const DAY_MICROS: i64 = 86_400_000_000;
/// The days Home counts back over, in µs.
const WEEK_MICROS: i64 = WEEK_DAYS * DAY_MICROS;
/// The newest of the week's facts read to say where they came from (`count` is the whole week's).
const FACT_SAMPLE: usize = 200;
/// Source folders named, the newest first.
const SOURCES_SHOWN: usize = 8;

/// The week's facts, the advice served and the sources, as one reader reads them at one memory state.
static WEEK: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::summary(500));

/// The parts of the answer, by name (`week` also answers `sources` and `advice`).
const PARTS: [&str; 9] = [
    "counts",
    "week",
    "lessons",
    "branches",
    "activity",
    "proposals",
    "conflicts",
    "dream",
    "schema",
];
/// How long a first ask waits for its parts: what isn't worked out by then is named `pending`, and goes on being
/// worked out (the parts read from kept answers, [`crate::kept_answers`]) for the ask again.
const FIRST: Duration = Duration::from_secs(1);
/// How long an ask for named parts (those a first ask said were pending) waits for them: well within a gateway's
/// usual 30 s for a request.
const AGAIN: Duration = Duration::from_secs(15);
/// How often a wait for the root's counts looks again (they are counted in the background, by [`crate::browse`]),
/// and a wait for the branches of the proposals main's state can't tell to be built.
const COUNTS_EVERY: Duration = Duration::from_millis(250);
/// How long the proposals are waited for once every other part asked for has answered.
const BESIDE: Duration = Duration::from_millis(300);
/// How long a reader is watched (their Home worked out ahead) after they last asked for it.
const WATCHED_FOR: Duration = Duration::from_secs(1800);
/// How often a watched memory is looked at: whether a run is due, and whose answer it works out.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// How often a reader's answer is worked out again while the memory's state doesn't move: the activity and the week
/// are counted back from when they are worked out, and the parts read from the answer log lag it until then.
const RENEW: Duration = Duration::from_secs(120);
/// How many watched readers' answers one run works out. One: each answer reads every part again for its reader
/// (the root's counts, the dream's records, the answer log, ...), so a run's cost grows with the readers it works
/// out, while under steady writes main's state moves every few seconds and every answer is always due. Taking the
/// one worked out longest ago (or never) each run, an org's readers take turns: with the default
/// [`run_every`] and fewer than ten of them watched, each is worked out again within ten minutes, and an ask with none
/// kept young enough works out its own parts.
const PER_RUN: usize = 1;
/// How long an answer worked out ahead waits for its parts (the proposals main's state can't tell, for their
/// branches' states): one not [`settled`] by then is worked out again at the next run, so a memory whose branches
/// are still being built doesn't hold the server's turn ([`AT_ONCE`]).
const AHEAD_WAIT: Duration = Duration::from_secs(5);
/// How many memories' answers are worked out at once, across the server.
const AT_ONCE: usize = 1;
/// How old a reader's kept answer may be and still answer an ask at once, while memory lost nothing since it was
/// worked out: Home's numbers are a summary, and the ask has the next answer worked out meanwhile ([`refresh`]).
const KEPT_FOR: Duration = Duration::from_secs(3600);
/// How long an answer an ask has worked out in the background ([`refresh`]) waits for its parts: as long as an ask
/// again waits, so the proposals main's state can't tell are in it.
const REFRESH_WAIT: Duration = AGAIN;
/// How old a kept answer served at once is before an ask has it worked out again ([`due`]), when main's state moved
/// since.
const REFRESH_AFTER: Duration = Duration::from_secs(30);
/// How many answers asks have worked out in the background at once, across the server (the loop's runs aside).
const REFRESH_AT_ONCE: usize = 2;
/// How long a part is looked for in the shared store ([`Shared`]) before it is worked out instead: a store's read is
/// tens of milliseconds.
const SHARED_GET: Duration = Duration::from_millis(400);
/// How long an ask answered at once looks only at the answer kept here before it also looks in the shared store: on
/// a server that kept one, it answers well within this, and the store isn't read for nothing.
const SHARED_AFTER: Duration = Duration::from_millis(100);

/// Each watched reader's last answer, worked out ahead, by memory and reader ([`ahead_key`]).
static AHEAD: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::summary(500));

/// The readers whose answers are worked out ahead: by scope namespace, then by [`ahead_key`].
static WATCHED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, HashMap<String, Watched>>>> =
    std::sync::LazyLock::new(Default::default);

/// The turns of [`AT_ONCE`].
static TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(AT_ONCE);

/// The turns of [`REFRESH_AT_ONCE`].
static REFRESH_TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(REFRESH_AT_ONCE);

/// The readers whose answer an ask has being worked out in the background ([`refresh`]), by scope namespace and
/// [`ahead_key`]: one at a time each.
static REFRESHING: std::sync::LazyLock<std::sync::Mutex<HashSet<(String, String)>>> =
    std::sync::LazyLock::new(Default::default);

/// When each org's last run began, by org (a person's own memory: by its namespace): a run is due [`run_every`]
/// after it.
static RAN: std::sync::LazyLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
    std::sync::LazyLock::new(Default::default);

/// The least time between two runs of an org's answers worked out ahead (`AMPUP_MEMORY_HOME_AHEAD_SECS`, default 60):
/// each reads every part for its reader again, from full reads of the memory, on the server that serves the console.
fn run_every() -> Duration {
    Duration::from_secs(
        std::env::var("AMPUP_MEMORY_HOME_AHEAD_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60),
    )
}

/// A reader whose answer is worked out ahead: what their ask had, to ask as they would.
#[derive(Clone)]
struct Watched {
    state: Arc<State>,
    scope: Scope,
    who: Principal,
    via: SignIn,
    superuser: bool,
    every_session: bool,
    asked: std::time::Instant,
    worked: Option<Worked>,
}

/// A watched reader's last answer worked out ahead.
#[derive(Clone)]
struct Worked {
    when: std::time::Instant,
    /// Main's state it was worked out at, the answer log aside ([`state_now`]); none when it failed.
    main: Option<crate::kept_answers::At>,
    /// Whether it is worked out once more at the next run, though main's state didn't move: its parts may have
    /// answered from earlier states while worked out again in the background.
    again: bool,
}

/// Whether a request is a Home ask (`/browse {"op": "home"}`), from its path and body, before it is pinned.
pub(super) fn asked(path: &str, body: &[u8]) -> bool {
    path == "/browse" && serde_json::from_slice::<Value>(body).is_ok_and(|v| v["op"] == "home")
}

/// The parts an ask wants: those named in `parts` (a first ask said they were pending), else every part.
fn wanted_of(req: &Value) -> Vec<&'static str> {
    match req["parts"].as_array() {
        Some(asked) => PARTS
            .into_iter()
            .filter(|p| asked.iter().any(|a| a == p))
            .collect(),
        None => PARTS.to_vec(),
    }
}

/// A Home ask on main answered at once, before the request pins anything, from the asking reader's last answer
/// ([`AHEAD`]): when there is one memory lost nothing since (checked as [`checked_now`] reads main, never waiting
/// for a build), younger than [`KEPT_FOR`] and with every part asked for. It says so (`kept`: how old it is), and
/// when it is old enough ([`due`]), the reader's next answer is worked out in the background ([`refresh`]). Else,
/// every part asked for found in the shared store at main's state now ([`shared_whole`]). None: the ask is answered as
/// before ([`run`]).
pub(super) async fn at_once(
    state: &Arc<State>,
    store: &Store,
    scope: &Scope,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
    req: &Value,
) -> Option<Value> {
    crate::phases::mark("home");
    let wanted = wanted_of(req);
    let standing = standing(store, scope, reader, via, superuser).await;
    let key = ahead_key(reader, &standing);
    crate::phases::mark("home key");
    watch(state, scope, reader, via, superuser, &key);
    let shared = async {
        let shared = Shared::read(store, scope, reader.who, &standing, Read::Since).await?;
        let out = shared_whole(&shared, &wanted).await?;
        // Answered from an earlier state memory lost nothing since: the asker's answer at this one is worked out
        // in the background, for their next ask.
        if shared.since() {
            refresh(&scope.namespace(), &key);
        }
        Some(out)
    };
    let out = tokio::time::timeout(
        first_left(),
        first_found(kept(store, scope, &key, &wanted), shared, SHARED_AFTER),
    )
    .await
    .ok()
    .flatten();
    if out.is_some() {
        crate::phases::mark("home at once");
    }
    out
}

/// What `here` finds, else what `there` does: `there` starts once `here` found nothing, or after `head` at most,
/// and the first to find something answers.
async fn first_found<T>(
    here: impl Future<Output = Option<T>>,
    there: impl Future<Output = Option<T>>,
    head: Duration,
) -> Option<T> {
    let mut here = std::pin::pin!(here);
    let started = tokio::time::Instant::now();
    let here_none = tokio::select! {
        found = &mut here => match found {
            Some(v) => return Some(v),
            None => true,
        },
        () = tokio::time::sleep_until(started + head) => false,
    };
    if here_none {
        return there.await;
    }
    let mut there = std::pin::pin!(there);
    let (mut here_done, mut there_done) = (false, false);
    loop {
        tokio::select! {
            found = &mut here, if !here_done => match found {
                Some(v) => return Some(v),
                None if there_done => return None,
                None => here_done = true,
            },
            found = &mut there, if !there_done => match found {
                Some(v) => return Some(v),
                None if here_done => return None,
                None => there_done = true,
            },
        }
    }
}

/// The `wanted` parts of `key`'s kept answer, when memory lost nothing since it was worked out (see [`at_once`]).
async fn kept(store: &Store, scope: &Scope, key: &str, wanted: &[&str]) -> Option<Value> {
    let ns = scope.namespace();
    let at = match crate::behind_own_writes(true, checked_now(store, scope)).await {
        Ok(Ok(at)) => at,
        Ok(Err(why)) => {
            crate::phases::mark("home kept unchecked");
            crate::phases::mark(why);
            return None;
        }
        Err(e) => {
            eprintln!("home: the kept answer in {scope} not checked: {e:#}");
            return None;
        }
    };
    let Some((kept, exact, age)) = AHEAD.peek_within(&ns, key, &at, KEPT_FOR) else {
        crate::phases::mark("home kept none");
        return None;
    };
    let out = parts_of(&kept, wanted, age)?;
    if due(exact, age) {
        refresh(&ns, key);
    }
    Some(out)
}

/// Main's state as it is now, as a kept answer is checked against: the loss count and the facts table read first,
/// then main's last built state where the tables as read now show nothing taken from it since
/// ([`Store::main_lost_nothing`]), then the answer log. Where only a build of main could tell, why.
async fn checked_now(
    store: &Store,
    scope: &Scope,
) -> Result<std::result::Result<crate::kept_answers::At, &'static str>> {
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    crate::phases::mark("home mark");
    let vis = match store.main_lost_nothing(scope).await? {
        Ok(vis) => vis,
        Err(why) => return Ok(Err(why)),
    };
    crate::phases::mark("home state");
    let log = crate::kept_answers::Log::read(store, scope).await?;
    Ok(Ok(crate::kept_answers::At::with_log(mark, &vis, log)))
}

/// The `wanted` parts of a kept answer, marked as kept (`kept`: how long ago it was worked out): none when a part
/// asked for again is pending in it. Every part asked for: the whole answer, its pending parts named, for an ask
/// again.
fn parts_of(kept: &Value, wanted: &[&str], age: Duration) -> Option<Value> {
    let mark = json!({"age_ms": age.as_millis() as u64});
    if wanted.len() == PARTS.len() {
        let mut out = kept.clone();
        out["kept"] = mark;
        return Some(out);
    }
    let pending = |p: &&str| {
        kept["pending"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x == p))
    };
    if wanted.iter().any(pending) {
        return None;
    }
    let mut out = json!({"since": kept["since"], "pending": [], "kept": mark});
    for p in wanted {
        let fields: &[&str] = match *p {
            "week" => &["week", "sources", "advice", "month"],
            p => &[p],
        };
        for f in fields {
            if let Some(v) = kept.get(*f) {
                out[*f] = v.clone();
            }
        }
    }
    Some(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    state: &Arc<State>,
    store: &Store,
    session: &Session,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
    req: &Value,
) -> Result<Value> {
    crate::phases::mark("home");
    // `parts`: those a first ask said were pending, waited for longer; else every part.
    let wanted = wanted_of(req);
    // Main as it is now: the answer worked out ahead, where there is one this ask may have. The ask's pin may be
    // behind this server's own writes ([`crate::behind_own_writes`], as the server pins a Home ask): enough to check
    // a kept answer against, whose mark refuses one from before any removal, but not to work one out from, so the
    // parts are worked out on main pinned again, as any other read pins it.
    let pinned;
    let mut keep = None;
    let mut shared = None;
    let session = match session.on_main_now() {
        true => {
            let standing = standing(store, session.scope(), reader, via, superuser).await;
            let key = ahead_key(reader, &standing);
            watch(state, session.scope(), reader, via, superuser, &key);
            // A first ask looks within `FIRST` too (the answer log's first read since a restart is a catalog
            // read): past it every part is pending, as below.
            let first = !req["parts"].is_array();
            let found = match first {
                true => tokio::time::timeout(first_left(), ahead(store, session, &key, &wanted))
                    .await
                    .map_err(|_| ()),
                false => Ok(ahead(store, session, &key, &wanted).await),
            };
            let ns = session.scope().namespace();
            match found {
                Ok(Some(out)) => {
                    crate::phases::mark("home ahead");
                    return Ok(out);
                }
                Ok(None) => {}
                Err(()) => {
                    // Worked out in the background meanwhile, for the ask again and the next open.
                    refresh(&ns, &key);
                    return Ok(json!({"since": week_since(), "pending": wanted}));
                }
            }
            // A first ask waits for the pin within `FIRST` too: past it every part is pending, worked out in the
            // background meanwhile for the ask again. Pinned with what the answer is kept by, read before it.
            let generation = crate::kept_answers::generation(&ns);
            // The shared store's ids are read before the pin too ([`Shared::read`]).
            let pin = async {
                let shared =
                    Shared::read(store, session.scope(), reader.who, &standing, Read::Since).await;
                anyhow::Ok((shared, state_now(store, session.scope()).await?))
            };
            let (found, (at, _, s)) = match first {
                false => pin.await?,
                true => match tokio::time::timeout(first_left(), pin).await {
                    Ok(s) => s?,
                    Err(_) => {
                        refresh(&ns, &key);
                        return Ok(json!({"since": week_since(), "pending": wanted}));
                    }
                },
            };
            shared = found;
            pinned = s;
            keep = Some((ns, key, at, generation));
            crate::phases::mark("home pinned");
            &pinned
        }
        false => session,
    };
    // A first ask waits at most `FIRST` since the request began; an ask again, `AGAIN`.
    let wait = match req["parts"].is_array() {
        true => AGAIN,
        false => first_left(),
    };
    let until = tokio::time::Instant::now() + wait;
    // Its parts' own reads of main too, and the work they start ([`crate::kept_answers`]).
    let out = crate::lagging_past_endings(
        true,
        answer(
            state,
            store,
            session,
            reader,
            (via, superuser),
            &wanted,
            until,
            Some(BESIDE),
            shared.as_ref(),
        ),
    )
    .await?;
    // On main: a whole answer is the reader's kept answer, for their next ask; one with parts still pending has
    // its whole worked out in the background, for the ask again and the next open.
    if let Some((ns, key, at, generation)) = keep {
        // One with a part from an earlier state (the shared store's last answer) isn't this state's either.
        let whole = wanted.len() == PARTS.len()
            && out["pending"].as_array().is_none_or(|p| p.is_empty())
            && !shared.as_ref().is_some_and(Shared::since);
        match whole {
            true => AHEAD.put(&ns, &key, at, generation, out.clone()),
            false => refresh(&ns, &key),
        }
    }
    Ok(out)
}

/// What is left of a first ask's [`FIRST`], counted from when its request began.
fn first_left() -> Duration {
    FIRST.saturating_sub(crate::phases::elapsed().unwrap_or_default())
}

/// The answer's `wanted` parts, worked out by `until`; with `beside`, the proposals are left pending that long after
/// the other parts asked for have answered (unless they are all that was asked for). With `shared` (main as it is now,
/// its ids read before `session` was pinned), each part is looked for in the shared store first where it says to, and
/// those worked out are put there once shown to be at its state ([`Shared::put_later`]).
#[allow(clippy::too_many_arguments)]
async fn answer(
    state: &Arc<State>,
    store: &Store,
    session: &Session,
    reader: crate::browse::Reader<'_>,
    (via, superuser): (&SignIn, bool),
    wanted: &[&str],
    until: tokio::time::Instant,
    beside: Option<Duration>,
    shared: Option<&Shared>,
) -> Result<Value> {
    let who = reader.who;
    let want = |p: &str| wanted.contains(&p);
    // The admins' and reviewers' parts are of the memory browsed: its org, and its team if it is one's (a local
    // server's own memory needs none: `/admin` names it). A person's own memory has neither.
    let (org, team) = match session.scope() {
        _ if via.local_admin.is_some() => (None, None),
        Scope::Team(id) => (
            session.scope().org_name(),
            Some(id.split_once('/').map_or(id.as_str(), |(_, t)| t)),
        ),
        s => (s.org_name(), None),
    };
    // The week is counted back from the start of this hour: Home's numbers move with the hour, not with every load.
    let since = week_since();
    // Not across an hour's turn: the parts' key names the hour their ids were read in.
    let shared = shared.filter(|s| s.hour == crate::shared_answers::hour_of(since + WEEK_MICROS));
    // The parts worked out here that may be put in the shared store.
    let worked: std::sync::Mutex<Vec<(&'static str, Value)>> = Default::default();
    let browse = |req: Value| async move {
        Box::pin(crate::browse::run(store, session, reader, &req))
            .await
            .map_err(|e| quiet("browse", &req, e))
            .ok()
    };
    // An admin's or a reviewer's op, through `/admin` as the caller's own request would go: its checks decide.
    let admin = |mut req: Value| async move {
        if org.is_none() && via.local_admin.is_none() {
            return None;
        }
        if let Some(o) = org {
            req["org"] = json!(o);
        }
        if let Some(t) = team {
            req["team"] = json!(t);
        }
        Box::pin(directory_change(
            state,
            who,
            via,
            superuser,
            req.to_string().as_bytes(),
        ))
        .await
        .map_err(|e| quiet("admin", &req, e))
        .ok()
    };
    let counts = async {
        // The root's counts, once counted: until then each folder's count is null.
        loop {
            let root = browse(json!({"op": "ls", "path": []})).await?;
            let children = root["children"].as_array().cloned().unwrap_or_default();
            if children.iter().all(|c| !c["count"].is_null()) {
                let mut m = serde_json::Map::new();
                for c in children {
                    if let Some(name) = c["name"].as_str() {
                        m.insert(name.into(), c["count"].clone());
                    }
                }
                crate::phases::mark("home counts");
                return Some(Value::Object(m));
            }
            tokio::time::sleep(COUNTS_EVERY).await;
        }
    };
    let week = async {
        let work = || {
            let (store, session, who) = (store.clone(), session.clone(), who.clone());
            let every_session = reader.every_session;
            async move {
                let reader = crate::browse::Reader {
                    who: &who,
                    every_session,
                };
                week(&store, &session, reader, since).await
            }
        };
        // Kept only for main as it is now, and only when main's state is still the one the request pinned, read
        // after the scope's loss count ([`crate::kept_answers::Mark`]).
        let at = match session.on_main_now() {
            true => async {
                let mark = crate::kept_answers::Mark::read(store, session.scope()).await?;
                let now = store
                    .lagging_visibility(session.scope(), crate::MAIN)
                    .await?;
                anyhow::Ok(
                    std::ptr::eq(session.vis(), &*now)
                        .then(|| crate::kept_answers::At::of(mark, &now)),
                )
            }
            .await
            .unwrap_or_default(),
            false => None,
        };
        crate::phases::mark("week pinned");
        let got = match at {
            Some(at) => {
                let key = format!(
                    "{}\n{}\n{since}",
                    crate::kept_answers::reader_key(who),
                    reader.every_session
                );
                WEEK.get(&session.scope().namespace(), &key, at, true, work)
                    .await
                    .map(|v| (*v).clone())
            }
            None => work().await,
        };
        crate::phases::mark("home week");
        // A week with a read that failed is shown, never shared.
        if got.as_ref().is_ok_and(|v| v["partial"] == true) {
            crate::kept_answers::not_sharable();
        }
        got.map_err(|e| {
            crate::kept_answers::not_sharable();
            eprintln!("home: the week in {} not read: {e:#}", session.scope())
        })
        .ok()
    };
    let lessons = async {
        let l = browse(json!({"op": "lessons", "since": since, "count": true})).await;
        crate::phases::mark("home lessons");
        l.map(|l| l["count"].clone())
    };
    // Branches as built already: Home builds none (a branch's state is a build of the whole memory), so a branch
    // not built has its skills unknown (null).
    let branches = async {
        let b = browse(json!({"op": "branches", "build": false})).await;
        crate::phases::mark("home branches");
        b.map(|b| b["branches"].clone())
    };
    let activity = async {
        let a =
            admin(json!({"op": "activity", "since_secs": WEEK_DAYS * 86_400, "limit": 1})).await;
        crate::phases::mark("home activity");
        let a = a?;
        let people: Vec<&Value> = a["people"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| n(&p["sessions"]) + n(&p["runs"]) + n(&p["answers"]) > 0)
            .collect();
        let mut v = json!({
            "people": people.len(),
            "sessions": people.iter().map(|p| n(&p["sessions"]) + n(&p["runs"]) + n(&p["answers"])).sum::<u64>(),
        });
        // Sessions, runs and answers begun on each of the last 30 days, for the chart of the month.
        if a["by_day"].is_array() {
            v["by_day"] = a["by_day"].clone();
        }
        // How many sessions were given advice, of how many, where the log says.
        for (k, from) in [("served", "sessions_served"), ("of", "sessions")] {
            if !a["serving"][from].is_null() {
                v[k] = a["serving"][from].clone();
            }
        }
        Some(v)
    };
    // Counted once none of those waiting needs its branch's state built: read from main's, or built already (those
    // main's state can't tell are built meanwhile in the background, one after another: never on this request).
    let proposals = async {
        loop {
            let p = admin(json!({"op": "list_proposals", "count": true})).await?;
            let w = &p["result"]["waiting"];
            if !w.is_null() {
                return Some(json!({"mine": w["mine"], "pending": w["pending"]}));
            }
            tokio::time::sleep(COUNTS_EVERY).await;
        }
    };
    let conflicts = async {
        let c = admin(json!({"op": "list_conflicts", "mine": true, "view": "count"})).await;
        crate::phases::mark("home conflicts");
        c.map(|c| c["result"]["total"].clone())
    };
    let dream = async {
        let d = admin(
            json!({"op": "dream_decisions", "status": "applied", "since": since, "limit": 1}),
        )
        .await;
        crate::phases::mark("home dream");
        let mut d = d?;
        d["result"]["decisions"] = json!([]);
        Some(d["result"].take())
    };
    // The proposals main's state can't tell wait for their branches' states to be built, which may take long after a
    // restart: they hold no other part back, and are named pending once the rest has answered, unless they are all
    // that was asked for.
    let (rest_done, mut rest_heard) = tokio::sync::watch::channel(false);
    let alone = wanted == ["proposals"] || beside.is_none();
    let rest = async {
        let got = tokio::join!(
            part(
                want("counts"),
                until,
                shared_part(shared, "counts", &worked, Box::pin(counts))
            ),
            part(
                want("week"),
                until,
                shared_part(shared, "week", &worked, Box::pin(week))
            ),
            part(
                want("lessons"),
                until,
                shared_part(shared, "lessons", &worked, Box::pin(lessons))
            ),
            part(
                want("branches"),
                until,
                shared_part(shared, "branches", &worked, Box::pin(branches))
            ),
            part(
                want("activity"),
                until,
                shared_part(shared, "activity", &worked, Box::pin(activity))
            ),
            part(
                want("conflicts"),
                until,
                shared_part(shared, "conflicts", &worked, Box::pin(conflicts))
            ),
            part(
                want("dream"),
                until,
                shared_part(shared, "dream", &worked, Box::pin(dream))
            ),
        );
        crate::phases::mark("home parts");
        let _ = rest_done.send(true);
        got
    };
    let proposals = async {
        let after_rest = async {
            if alone {
                return std::future::pending().await;
            }
            let _ = rest_heard.wait_for(|done| *done).await;
            tokio::time::sleep(beside.unwrap_or_default()).await;
        };
        tokio::select! {
            got = part(want("proposals"), until, shared_part(shared, "proposals", &worked, Box::pin(proposals))) => got,
            () = after_rest => Some(Err(Pending)),
        }
    };
    let ((counts, week, lessons, branches, activity, conflicts, dream), proposals) =
        tokio::join!(rest, proposals);
    crate::phases::mark("home answered");
    let mut out = json!({"since": since});
    let mut pending = vec![];
    for (name, got) in [
        ("counts", counts),
        ("week", week),
        ("lessons", lessons),
        ("branches", branches),
        ("activity", activity),
        ("proposals", proposals),
        ("conflicts", conflicts),
        ("dream", dream),
    ] {
        match got {
            None => {}
            Some(Err(_)) => pending.push(name),
            Some(Ok(v)) if name == "week" => {
                let v = v.unwrap_or_default();
                for k in ["week", "sources", "advice", "month"] {
                    out[k] = v[k].clone();
                }
            }
            Some(Ok(v)) => out[name] = v.unwrap_or_default(),
        }
    }
    if want("schema") {
        out["schema"] = json!({"types": session.schema().map_or(0, |s| s.types.len())});
        worked
            .lock()
            .unwrap()
            .push(("schema", out["schema"].clone()));
    }
    out["pending"] = json!(pending);
    if let Some(shared) = shared {
        shared.put_later(store, session, worked.into_inner().unwrap());
    }
    Ok(out)
}

/// Part `name`'s answer: from the shared store, where `shared` says to look there and it is found; else worked out
/// by `work`, and noted in `worked` for the store when nothing it read answered from an earlier state or failed
/// ([`crate::kept_answers::sharable`]) and it is whole ([`whole_part`]).
async fn shared_part(
    shared: Option<&Shared>,
    name: &'static str,
    worked: &std::sync::Mutex<Vec<(&'static str, Value)>>,
    work: std::pin::Pin<Box<dyn Future<Output = Option<Value>> + Send + '_>>,
) -> Option<Value> {
    let Some(shared) = shared else {
        return work.await;
    };
    if let Some(v) = shared.get(name).await {
        return (!v.is_null()).then_some(v);
    }
    let (got, ok) = crate::kept_answers::sharable(work).await;
    let v = got.clone().unwrap_or_default();
    if ok && whole_part(name, &v) {
        worked.lock().unwrap().push((name, v));
    }
    got
}

/// Whether part `name`'s answer `v` holds everything it can: the branches with every branch's skills known (whether
/// a branch's state is built is this server's, not the memory's: one not built yet is never shared). The rest of
/// what a part says of this process, not the memory, is settled before it is noted (the counts and the proposals
/// once counted and built, [`answer`]), or made this process's on a hit (the dream's, [`Shared::get`]).
fn whole_part(name: &str, v: &Value) -> bool {
    match name {
        "branches" => v
            .as_array()
            .is_none_or(|b| b.iter().all(|b| !b["skills"].is_null())),
        _ => true,
    }
}

/// The key of a reader's answers worked out ahead: who they are and every group they read as
/// ([`crate::kept_answers::reader_key`]) and their [`standing`]. A reader the directory no longer lets read the
/// admins' parts (an admin no longer) has another key, and never the answers worked out while they could.
fn ahead_key(reader: crate::browse::Reader<'_>, standing: &str) -> String {
    format!("{}{standing}", crate::kept_answers::reader_key(reader.who))
}

/// What a reader's answer turns on beyond who they are: how they signed in, whether they read every session, and
/// what the directory, as this ask finds it, lets them read of the admins' parts.
async fn standing(
    store: &Store,
    scope: &Scope,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
) -> String {
    let (admin, shows) = match scope.org_name() {
        Some(org) => match store.directory_behind().await {
            Ok(f) => (
                f.dir.is_admin(reader.who.user(), via, org),
                f.dir.shows_activity(org),
            ),
            Err(_) => (false, false),
        },
        None => (false, false),
    };
    format!(
        "\n{via:?}\n{superuser}\n{}\n{admin}\n{shows}",
        reader.every_session
    )
}

/// Who each of Home's parts' shared answers is for, as its key names it (hashed, [`crate::shared_answers::key`]), by
/// part: the class [`crate::shared_answers::class_for`] works out for `who` with `standing` in `scope` as it is now
/// (the reader alone, unless `AMPUP_MEMORY_SHARED_ANSWERS_CLASSES` lets readers who read the same share it). The one
/// place a part's class is said. A part whose class can't be worked out is left out: its answer is neither looked
/// for nor put.
async fn classes_of(
    store: &Store,
    scope: &Scope,
    who: &Principal,
    standing: &str,
) -> std::collections::HashMap<&'static str, String> {
    let parts = crate::shared_answers::PART_TABLES.iter().map(|(p, _)| *p);
    futures::future::join_all(parts.map(|part| async move {
        crate::shared_answers::class_for(store, scope, part, who, standing)
            .await
            .map_err(|e| eprintln!("home: no shared class for {part} in {scope}: {e:#}"))
            .ok()
            .map(|class| (part, class))
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Whether part `part`'s answer `v` reads no state its class `class` leaves out: a class of the readers who read the
/// same of each state (`branches`') names every branch the answer counts skills on, so a branch whose state was
/// built while the answer was worked out, or built on another server, is never counted for a class that left it out.
/// A class of that kind not written as [`crate::shared_answers::class_for`] writes it fails closed.
fn class_covers(part: &str, class: &str, v: &Value) -> bool {
    use crate::shared_answers::{Sharing, classes_on, sharing};
    if !classes_on() || sharing(part) != Sharing::Visible {
        return true;
    }
    let Some(states) = class
        .strip_prefix("visible\n")
        .and_then(|s| serde_json::from_str::<Vec<(String, Value)>>(s).ok())
    else {
        return false;
    };
    let named: std::collections::HashSet<&str> = states.iter().map(|(b, _)| b.as_str()).collect();
    v.as_array().is_some_and(|branches| {
        branches
            .iter()
            .all(|b| b["name"].as_str().is_some_and(|n| named.contains(n)))
    })
}

/// Home's parts in the shared store ([`crate::shared_answers`]) for one answer on main as it is now: the snapshot ids
/// of the tables they read, read before the state the answer is worked out from is pinned, the hour they were read
/// in, and who the answer is for.
#[derive(Clone)]
struct Shared {
    answers: Arc<dyn crate::shared_answers::SharedAnswers>,
    /// The store the ids were read from: a part moved since its answer was stored has its history read there.
    store: Store,
    scope: Scope,
    ids: crate::shared_answers::Snapshots,
    hour: i64,
    who: Principal,
    standing: String,
    /// The scope's and the system's loss counts and the changes made here to the scope's reviewed memory
    /// ([`crate::kept_answers::generation`]), read before the ids: a part found is answered, and one worked out put,
    /// only while none moved.
    guard: (crate::shared_answers::Losses, u64),
    /// When the ids were about to be read: a part is put only within [`crate::shared_answers::PUT_WITHIN`] of it.
    read_at: std::time::Instant,
    /// Which answers found in the store are taken before a part is worked out ([`Read`]).
    read: Read,
    /// Whether a part was answered from an earlier state memory lost nothing since ([`Read::Since`]): the asker's
    /// answer is then worked out again in the background, not kept as this state's.
    since: Arc<std::sync::atomic::AtomicBool>,
    /// The histories of the tables parts found moved since their answers were stored, loaded once for every part.
    histories: Arc<crate::shared_answers::Histories>,
    /// Each part's class ([`classes_of`]), worked out before the answer: a part is put only under the class worked
    /// out again after it, when the two are the same ([`Shared::put_later`]).
    classes: std::collections::HashMap<&'static str, String>,
}

/// Which answers found in the shared store are taken before a part is worked out ([`crate::shared_answers::Holds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    /// None: parts are only put there.
    No,
    /// One at the state read now (every table it reads as it was): the background work of a reader's first answer
    /// on this server.
    Exact,
    /// Also the last one memory lost nothing since: an ask, answered at once, with the asker's next answer worked
    /// out in the background ([`refresh`]).
    Since,
}

/// Every table any of Home's parts reads ([`crate::shared_answers::PART_TABLES`]): one read of their ids per answer.
fn every_table() -> Vec<crate::shared_answers::Table> {
    crate::shared_answers::PART_TABLES
        .iter()
        .flat_map(|(_, t)| t.iter().copied())
        .collect()
}

/// The scope's and the system's loss counts and the scope's review changes here, as [`Shared::guard`] holds them.
fn guard_of(scope: &Scope) -> (crate::shared_answers::Losses, u64) {
    let ns = scope.namespace();
    (
        crate::shared_answers::Losses::read(&ns),
        crate::kept_answers::generation(&ns),
    )
}

impl Shared {
    /// The shared store's view of `scope`'s main as it is now, for `who` with `standing`: read before the state the
    /// answer is worked out from is pinned. None when the server shares no answers, or the ids can't be read (the
    /// answer is then worked out as before, and put nowhere).
    async fn read(
        store: &Store,
        scope: &Scope,
        who: &Principal,
        standing: &str,
        read: Read,
    ) -> Option<Shared> {
        let answers = crate::shared_answers::handle()?;
        // The classes first: working one out may build main's state here, and a first build counts a loss (nothing
        // this process kept is answered past it). Read after it, the guard holds; read before, every part found would
        // be refused on a server's first Home.
        let classes = classes_of(store, scope, who, standing).await;
        let guard = guard_of(scope);
        let read_at = std::time::Instant::now();
        let hour = crate::shared_answers::hour_of(crate::now_micros());
        let ids = crate::shared_answers::Snapshots::read(store, scope, &every_table())
            .await
            .map_err(|e| eprintln!("home: {scope}'s snapshot ids not read, nothing shared: {e:#}"))
            .ok()?;
        Some(Shared {
            answers,
            store: store.clone(),
            scope: scope.clone(),
            ids,
            hour,
            who: who.clone(),
            standing: standing.to_string(),
            guard,
            read_at,
            read,
            since: Default::default(),
            histories: Default::default(),
            classes,
        })
    }

    /// Whether a part was answered from an earlier state memory lost nothing since ([`Read::Since`]).
    fn since(&self) -> bool {
        self.since.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Part `part`'s key, under its class worked out before the answer: none for a part without one.
    fn key(&self, part: &str) -> Option<String> {
        let class = self.classes.get(part)?;
        crate::shared_answers::last_key(&self.scope, part, class)
            .map_err(|e| eprintln!("home: no shared key for {part} in {}: {e:#}", self.scope))
            .ok()
    }

    /// Whether the scope lost nothing here, and its reviewed memory didn't change here, since the ids were read.
    fn holds(&self) -> bool {
        guard_of(&self.scope) == self.guard
    }

    /// Part `part`'s answer from the store (null: the reader may not read it), when the one it holds may answer at
    /// the state the ids name ([`crate::shared_answers::Last::holds`], as [`Read`] lets it) and it answers within
    /// [`SHARED_GET`]. A store that fails is one that holds none.
    async fn get(&self, part: &str) -> Option<Value> {
        if self.read == Read::No {
            return None;
        }
        let key = self.key(part)?;
        let class = self.classes.get(part)?;
        let got = tokio::time::timeout(SHARED_GET, self.answers.get(&key)).await;
        let bytes = match got {
            Ok(Ok(Some(b))) => b,
            Ok(Ok(None)) => {
                crate::phases::mark("home shared miss");
                return None;
            }
            Ok(Err(e)) => {
                eprintln!(
                    "home: shared answer {part} in {} not read: {e:#}",
                    self.scope
                );
                return None;
            }
            Err(_) => {
                crate::phases::mark("home shared slow");
                return None;
            }
        };
        let Some(last) = crate::shared_answers::Last::from_bytes(&bytes) else {
            crate::phases::mark("home shared refused: not an answer");
            return None;
        };
        let held = tokio::time::timeout(
            SHARED_GET,
            last.holds(
                &self.store,
                &self.scope,
                part,
                class,
                self.hour,
                &self.ids,
                &self.histories,
            ),
        )
        .await;
        let holds = match held {
            Ok(Ok(h)) => h,
            Ok(Err(why)) => {
                crate::phases::mark("home shared refused");
                crate::phases::mark(why.why);
                if let Some(t) = why.table {
                    crate::phases::mark(t);
                }
                return None;
            }
            Err(_) => {
                crate::phases::mark("home shared refused: slow");
                return None;
            }
        };
        if holds == crate::shared_answers::Holds::Since && self.read != Read::Since {
            crate::phases::mark("home shared refused: moved since");
            return None;
        }
        let mut v = last.answer;
        // What the dream's list says of the process that worked it out is this one's here.
        if part == "dream" {
            super::dream_review::as_here(&self.scope, &mut v);
        }
        if !self.holds() {
            crate::phases::mark("home shared refused: lost since");
            return None;
        }
        match holds {
            crate::shared_answers::Holds::Exact => crate::phases::mark("home shared hit"),
            crate::shared_answers::Holds::Since => {
                self.since.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::phases::mark("home shared hit since");
            }
        }
        Some(v)
    }

    /// Puts the parts `worked` out at `session` (main pinned after the ids were read) in the store, each as its last
    /// answer with the ids it is at ([`crate::shared_answers::Last`]), in the background, once shown to be at the
    /// ids' state: the ids read again the same, main's state the one a pin now
    /// would serve, built from the tables as they are now ([`Store::pin_if_built`]: not a state served while the next
    /// is built), and nothing lost here since. Until that state stops holding ([`crate::shared_answers::expiry_for`],
    /// worked out once for every part).
    /// Each part only under its class worked out again now the same as before the answer, naming every state the
    /// answer read ([`class_covers`]): a class that moved while the answer was worked out isn't the answer's.
    fn put_later(&self, store: &Store, session: &Session, worked: Vec<(&'static str, Value)>) {
        if worked.is_empty() {
            return;
        }
        let (shared, store, session) = (self.clone(), store.clone(), session.clone());
        tokio::spawn(crate::loads::later(async move {
            let scope = &shared.scope;
            match crate::shared_answers::Snapshots::read(&store, scope, &every_table()).await {
                Ok(ids) if ids == shared.ids => {}
                _ => return,
            }
            match store.pin_if_built(scope, crate::MAIN).await {
                Ok(Some(now)) if std::ptr::eq(now.vis(), session.vis()) => {}
                _ => return,
            }
            let until = crate::shared_answers::expiry_for(session.vis());
            if !shared.holds() || until <= crate::now_micros() {
                return;
            }
            let after = classes_of(&store, scope, &shared.who, &shared.standing).await;
            let ns = scope.namespace();
            // Each part's answer holds while only the answer log commits ([`crate::shared_answers::without_answers`]).
            let without_answers =
                crate::shared_answers::without_answers(&store, scope, &shared.ids)
                    .await
                    .ok();
            for (part, v) in worked {
                let class = shared.classes.get(part);
                if class.is_none_or(|c| after.get(part) != Some(c) || !class_covers(part, c, &v)) {
                    continue;
                }
                let Some(key) = shared.key(part) else {
                    continue;
                };
                // Only when its answer changed, or to move the state it is stored with on ([`put_due`]).
                let Ok(answer) = serde_json::to_vec(&v) else {
                    continue;
                };
                if !crate::shared_answers::put_due(&key, &answer) {
                    continue;
                }
                let Some(class) = class else {
                    continue;
                };
                let last = crate::shared_answers::Last::new(
                    scope,
                    part,
                    class,
                    shared.hour,
                    &shared.ids,
                    without_answers.clone(),
                    v,
                );
                let Ok(bytes) = last.and_then(|l| l.to_bytes()) else {
                    continue;
                };
                // Not once the scope lost something since the ids were read (its purge may be done already), and
                // purged again when it lost something while this was put.
                let put = crate::shared_answers::put_unless_lost(
                    &*shared.answers,
                    &ns,
                    shared.guard.0,
                    shared.read_at,
                    &key,
                    bytes,
                    until,
                );
                if let Err(e) = put.await {
                    eprintln!("home: shared answer {part} in {scope} not put: {e:#}");
                }
            }
        }));
    }
}

/// Every `wanted` part from the shared store at `shared`'s state, as one answer (as [`answer`] composes its parts):
/// none unless each is found.
async fn shared_whole(shared: &Shared, wanted: &[&str]) -> Option<Value> {
    let found = futures::future::join_all(
        wanted
            .iter()
            .map(|p| async move { (*p, shared.get(p).await) }),
    )
    .await;
    let since = shared.hour * 3_600_000_000 - WEEK_MICROS;
    let mut out = json!({"since": since, "pending": [], "shared": true});
    for (name, v) in found {
        let v = v?;
        match name {
            "week" => {
                for k in ["week", "sources", "advice", "month"] {
                    out[k] = v[k].clone();
                }
            }
            name => out[name] = v,
        }
    }
    crate::phases::mark("home shared whole");
    Some(out)
}

/// Watch the asking reader: their answer is worked out ahead from now on, until [`WATCHED_FOR`] after their last ask.
fn watch(
    state: &Arc<State>,
    scope: &Scope,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
    key: &str,
) {
    let ns = scope.namespace();
    let mut all = WATCHED.lock().unwrap();
    let first = !all.contains_key(&ns);
    let readers = all.entry(ns.clone()).or_default();
    match readers.get_mut(key) {
        Some(w) => w.asked = std::time::Instant::now(),
        None => {
            readers.insert(
                key.to_string(),
                Watched {
                    state: state.clone(),
                    scope: scope.clone(),
                    who: reader.who.clone(),
                    via: via.clone(),
                    superuser,
                    every_session: reader.every_session,
                    asked: std::time::Instant::now(),
                    worked: None,
                },
            );
        }
    }
    if first {
        tokio::spawn(crate::loads::by("home_ahead", ahead_loop(ns)));
    }
}

/// Main's state as it is now, as an answer worked out ahead is kept by: the loss count and the facts table read
/// before the state is pinned, and the answer log ([`crate::kept_answers::At::with_log`]), so an answer is never
/// answered once the log lost rows. With it, the same state without the log: a move of it is what works an answer
/// out again, not the log growing.
async fn state_now(
    store: &Store,
    scope: &Scope,
) -> Result<(crate::kept_answers::At, crate::kept_answers::At, Session)> {
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    // Past a quote, closure or decision written since (they take nothing away a reader may read): on a busy memory
    // one lands every minute or so, and a build for it kept the ask waiting seconds.
    let session = crate::lagging_past_endings(true, store.pin_lagging(scope, crate::MAIN)).await?;
    let log = crate::kept_answers::Log::read(store, scope).await?;
    Ok((
        crate::kept_answers::At::with_log(mark.clone(), session.vis(), log),
        crate::kept_answers::At::of(mark, session.vis()),
        session,
    ))
}

/// The `wanted` parts from the answer worked out ahead for `key`, when there is one this ask may have (see the
/// module's doc): none when there isn't, or it has a part asked for again still pending.
async fn ahead(store: &Store, session: &Session, key: &str, wanted: &[&str]) -> Option<Value> {
    let ns = session.scope().namespace();
    // Read behind this server's own writes, as behind another's: the mark's loss count, compared below, refuses an
    // answer from before any removal.
    let (at, _, _) = crate::behind_own_writes(true, state_now(store, session.scope()))
        .await
        .ok()?;
    let (kept, exact, age) = AHEAD.peek_within(&ns, key, &at, KEPT_FOR)?;
    if due(exact, age) {
        refresh(&ns, key);
    }
    parts_of(&kept, wanted, age)
}

/// Whether a kept answer served `age` old is worked out again in the background ([`refresh`]): once it is older than
/// [`RENEW`], or than [`REFRESH_AFTER`] when main's state moved since. Under steady writes it always has, and a
/// console asking every few seconds would otherwise work an answer out at every ask.
fn due(exact: bool, age: Duration) -> bool {
    age >= RENEW || (!exact && age >= REFRESH_AFTER)
}

/// Works out `key`'s answer in the background, for the ask that found none kept young enough (or an old one), unless
/// it is being worked out for an ask already: at most [`REFRESH_AT_ONCE`] at once across the server, beside the
/// loop's runs, waiting [`REFRESH_WAIT`] for its parts.
fn refresh(ns: &str, key: &str) {
    let Some(w) = WATCHED
        .lock()
        .unwrap()
        .get(ns)
        .and_then(|r| r.get(key))
        .cloned()
    else {
        return;
    };
    let id = (ns.to_string(), key.to_string());
    if !REFRESHING.lock().unwrap().insert(id.clone()) {
        return;
    }
    tokio::spawn(crate::loads::by("home_ahead", async move {
        let (ns, key) = &id;
        let worked = async {
            let _turn = REFRESH_TURNS.acquire().await?;
            work_out(ns, key, &w, REFRESH_WAIT, true).await
        }
        .await;
        match worked {
            Ok(worked) => {
                if let Some(w) = WATCHED
                    .lock()
                    .unwrap()
                    .get_mut(ns)
                    .and_then(|r| r.get_mut(key))
                {
                    w.worked = Some(worked);
                }
            }
            Err(e) => eprintln!(
                "home: {}'s answer in {ns} not worked out for an ask: {e:#}",
                w.who.user()
            ),
        }
        REFRESHING.lock().unwrap().remove(&id);
    }));
}

/// Warm-up's hook: works out `reader`'s Home on `scope`'s main as it is now, as their first answer on this server is
/// worked out ahead, so each part another server put in the shared store at this state is read from it rather than
/// worked out ([`Shared`]), and keeps it here for their first ask ([`at_once`]); they are watched from now on
/// ([`watch`]). Warm-up names the readers (those who opened Home lately, say): no server keeps who did, and the store's
/// keys hash who each answer is for, so they can't be listed from it.
#[allow(dead_code)]
pub(super) async fn prime(
    state: &Arc<State>,
    scope: &Scope,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
) -> Result<()> {
    let store = state.store.read().unwrap().clone();
    let standing = standing(&store, scope, reader, via, superuser).await;
    let key = ahead_key(reader, &standing);
    watch(state, scope, reader, via, superuser, &key);
    let ns = scope.namespace();
    let Some(w) = WATCHED
        .lock()
        .unwrap()
        .get(&ns)
        .and_then(|r| r.get(&key))
        .cloned()
    else {
        return Ok(());
    };
    let worked = {
        let _turn = REFRESH_TURNS.acquire().await?;
        work_out(&ns, &key, &w, AHEAD_WAIT, true).await?
    };
    if let Some(w) = WATCHED
        .lock()
        .unwrap()
        .get_mut(&ns)
        .and_then(|r| r.get_mut(&key))
    {
        w.worked = Some(worked);
    }
    Ok(())
}

/// Works out the answers of a memory's watched readers ahead, while any is watched: at most [`PER_RUN`] a run, the one
/// worked out longest ago (or never) first, and at most one run of its org every [`run_every`].
async fn ahead_loop(ns: String) {
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let mut readers: Vec<(String, Watched)> = {
            let mut all = WATCHED.lock().unwrap();
            let Some(readers) = all.get_mut(&ns) else {
                return;
            };
            readers.retain(|_, w| w.asked.elapsed() < WATCHED_FOR);
            if readers.is_empty() {
                all.remove(&ns);
                return;
            }
            readers
                .iter()
                .map(|(k, w)| (k.clone(), w.clone()))
                .collect()
        };
        // Never worked out first, then the longest ago: every reader has their turn however many are watched.
        readers.sort_by_key(|(_, w)| w.worked.as_ref().map(|d| d.when));
        let org = readers[0]
            .1
            .scope
            .org_name()
            .map_or_else(|| ns.clone(), String::from);
        let Some(before) = run_due(&org) else {
            continue;
        };
        let mut ran = 0;
        for (key, w) in readers {
            if ran == PER_RUN {
                break;
            }
            let Ok(_turn) = TURNS.acquire().await else {
                return;
            };
            let worked = match work_ahead(&ns, &key, &w).await {
                Ok(None) => continue,
                Ok(Some(worked)) => worked,
                // A failure is a run too (it is tried again at the next, not at the next look).
                Err(e) => {
                    eprintln!(
                        "home: {}'s answer in {ns} not worked out ahead: {e:#}",
                        w.who.user()
                    );
                    Worked {
                        when: std::time::Instant::now(),
                        main: None,
                        again: false,
                    }
                }
            };
            ran += 1;
            if let Some(w) = WATCHED
                .lock()
                .unwrap()
                .get_mut(&ns)
                .and_then(|r| r.get_mut(&key))
            {
                w.worked = Some(worked);
            }
        }
        if ran == 0 {
            run_not_made(&org, before);
        }
    }
}

/// Whether a run of `org`'s answers is due ([`run_every`] since its last began): if so it begins now, and with it
/// when the last began, for [`run_not_made`].
fn run_due(org: &str) -> Option<Option<std::time::Instant>> {
    let mut ran = RAN.lock().unwrap();
    let last = ran.get(org).copied();
    if last.is_some_and(|t| t.elapsed() < run_every()) {
        return None;
    }
    ran.insert(org.to_string(), std::time::Instant::now());
    Some(last)
}

/// A run begun by [`run_due`] that had no answer to work out: the next look may run.
fn run_not_made(org: &str, last: Option<std::time::Instant>) {
    let mut ran = RAN.lock().unwrap();
    match last {
        Some(t) => ran.insert(org.to_string(), t),
        None => ran.remove(org),
    };
}

/// Works out `w`'s answer at main's state now, unless the one kept may still be answered and was worked out at main's
/// state as it is now (the answer log aside), less than [`RENEW`] ago, in this hour, with every part, and needn't be
/// worked out once more: what it worked out, if it did.
async fn work_ahead(ns: &str, key: &str, w: &Watched) -> Result<Option<Worked>> {
    let store = w.state.store.read().unwrap().clone();
    let shared = shared_of(&store, w).await;
    let (at, main, session) = state_now(&store, &w.scope).await?;
    let since = week_since();
    let last = w.worked.as_ref();
    let moved = last.is_none_or(|l| l.main.as_ref() != Some(&main));
    if !moved
        && last.is_some_and(|l| !l.again)
        && let Some((kept, _, age)) = AHEAD.peek(ns, key, &at)
        && age < RENEW
        && kept["since"] == since
        && settled(&kept)
    {
        return Ok(None);
    }
    // Once more after this at the next run, unless this is that once more.
    let again = moved || last.is_none_or(|l| !l.again);
    work_on(
        ns,
        key,
        w,
        (store, at, main, session),
        shared,
        AHEAD_WAIT,
        again,
    )
    .await
    .map(Some)
}

/// The shared store's view for `w`'s next answer, read before its state is pinned: looked in for its parts only for
/// its first answer on this server, and only for those stored at the state read now (after it, each is worked out
/// and put again: see the module's doc). Work in the background is what an ask answered from an earlier state has
/// started, so it never takes one.
async fn shared_of(store: &Store, w: &Watched) -> Option<Shared> {
    let reader = crate::browse::Reader {
        who: &w.who,
        every_session: w.every_session,
    };
    let standing = standing(store, &w.scope, reader, &w.via, w.superuser).await;
    let read = match w.worked {
        None => Read::Exact,
        Some(_) => Read::No,
    };
    Shared::read(store, &w.scope, &w.who, &standing, read).await
}

/// Works out `w`'s answer at main's state now, waiting `wait` for its parts; `again`: it is worked out once more at
/// the loop's next run (its parts may have answered from earlier states).
async fn work_out(ns: &str, key: &str, w: &Watched, wait: Duration, again: bool) -> Result<Worked> {
    let store = w.state.store.read().unwrap().clone();
    let shared = shared_of(&store, w).await;
    let (at, main, session) = state_now(&store, &w.scope).await?;
    work_on(ns, key, w, (store, at, main, session), shared, wait, again).await
}

/// [`work_out`] at a state already read.
async fn work_on(
    ns: &str,
    key: &str,
    w: &Watched,
    (store, at, main, session): (
        Arc<Store>,
        crate::kept_answers::At,
        crate::kept_answers::At,
        Session,
    ),
    shared: Option<Shared>,
    wait: Duration,
    again: bool,
) -> Result<Worked> {
    let w = w.clone();
    AHEAD
        .renew(ns, key, at, move || async move {
            let reader = crate::browse::Reader {
                who: &w.who,
                every_session: w.every_session,
            };
            let until = tokio::time::Instant::now() + wait;
            let parts = answer(
                &w.state,
                &store,
                &session,
                reader,
                (&w.via, w.superuser),
                &PARTS,
                until,
                None,
                shared.as_ref(),
            );
            crate::lagging_past_endings(true, parts).await
        })
        .await?;
    Ok(Worked {
        when: std::time::Instant::now(),
        main: Some(main),
        again,
    })
}

/// Whether an answer worked out ahead has every part: none pending, and every branch's skills known (a branch's
/// state is built in the background, and its skills read once it is, at the same memory state). One that hasn't is
/// worked out again at the next run, not only once the state moves or [`RENEW`] is up.
fn settled(kept: &Value) -> bool {
    kept["pending"].as_array().is_none_or(|p| p.is_empty())
        && kept["branches"]
            .as_array()
            .is_none_or(|b| b.iter().all(|b| !b["skills"].is_null()))
}

/// Where the week Home counts begins: seven days before the start of this hour.
fn week_since() -> i64 {
    let hour = crate::now_micros() / 3_600_000_000 * 3_600_000_000;
    hour - WEEK_MICROS
}

/// A part not worked out in time: named in `pending`, for an ask again.
struct Pending;

/// A part of the answer: none when not asked for; [`Pending`] when not worked out by `until`; else its answer
/// (none when the caller may not read it, or it failed).
async fn part(
    asked: bool,
    until: tokio::time::Instant,
    work: impl Future<Output = Option<Value>>,
) -> Option<Result<Option<Value>, Pending>> {
    match asked {
        true => Some(
            tokio::time::timeout_at(until, work)
                .await
                .map_err(|_| Pending),
        ),
        false => None,
    }
}

/// A whole number of a JSON value, 0 where it is none.
fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or_default()
}

/// A part not read: said in the log when it isn't the caller's to read (a refusal is Home leaving it out).
fn quiet(what: &str, req: &Value, e: anyhow::Error) -> anyhow::Error {
    let refused = e.is::<crate::people::directory::Refused>()
        || e.is::<crate::browse::NotFound>()
        || e.is::<crate::browse::BadRequest>();
    if !refused {
        crate::kept_answers::not_sharable();
        eprintln!("home: {what} {} not read: {e:#}", req["op"]);
    }
    e
}

/// The week's facts (how many, and of the newest [`FACT_SAMPLE`] the entities and where they came from), the team's
/// notes served in it, and the newest source folders, as `reader` reads them.
async fn week(
    store: &Store,
    session: &Session,
    reader: crate::browse::Reader<'_>,
    since: i64,
) -> Result<Value> {
    // The month's facts start at the first of its 30 days (UTC), as the activity's days do.
    let month_from = (since + WEEK_DAYS * DAY_MICROS).div_euclid(DAY_MICROS) * DAY_MICROS
        - (MONTH_DAYS - 1) * DAY_MICROS;
    let (facts, advice, sources, month) = (
        // The facts as `entities/` counts them: a note's own fact is a note, counted under notes.
        json!({"op": "facts", "valid_from": {"gte": since}, "limit": FACT_SAMPLE, "order": "newest", "notes": false}),
        json!({"op": "advice_usage", "days": WEEK_DAYS}),
        json!({"op": "ls", "path": ["sources"], "limit": 1000}),
        json!({"op": "facts", "valid_from": {"gte": month_from}, "limit": 1, "notes": false, "by_day": true}),
    );
    let facts = crate::browse::run(store, session, reader, &facts);
    let advice = crate::browse::run(store, session, reader, &advice);
    let sources = crate::browse::run(store, session, reader, &sources);
    let month = crate::browse::run(store, session, reader, &month);
    let (facts, advice, sources, month) = tokio::join!(
        Box::pin(facts),
        Box::pin(advice),
        Box::pin(sources),
        Box::pin(month)
    );
    // A read that failed leaves its numbers out: shown so, never shared ([`answer`]).
    let partial = facts.is_err() || advice.is_err() || sources.is_err() || month.is_err();
    let week = facts.ok().map(|f| {
        let sample = f["facts"].as_array().cloned().unwrap_or_default();
        let entities: HashSet<&str> = sample.iter().filter_map(|f| f["entity"].as_str()).collect();
        let mut by: BTreeMap<String, u64> = BTreeMap::new();
        for f in &sample {
            let source = f["sources"][0]["source"].as_str().unwrap_or_default();
            let prefix = source.split(':').next().filter(|p| !p.is_empty()).unwrap_or("unknown");
            *by.entry(prefix.to_string()).or_default() += 1;
        }
        let mut by: Vec<(String, u64)> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        json!({"count": f["count"], "sample": sample.len(), "entities": entities.len(), "by_source": by})
    });
    let advice = advice.ok().map(|a| {
        let used: Vec<&Value> = a["advice"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| n(&x["used"]) > 0)
            .collect();
        json!({
            "times": used.iter().map(|x| n(&x["used"])).sum::<u64>(),
            "notes": used.len(),
            // A person's own note is never ratified: none waits.
            "waiting": used.iter().filter(|x| x["mine"] != true && x["ratified"] != true && x["held"] != true).count(),
        })
    });
    let sources = sources.ok().map(|s| {
        let at = |c: &Value| {
            c["latest"]
                .as_i64()
                .or(c["occurred_at"].as_i64())
                .unwrap_or_default()
        };
        let mut children = s["children"].as_array().cloned().unwrap_or_default();
        children.sort_by_key(|c| std::cmp::Reverse(at(c)));
        children.truncate(SOURCES_SHOWN);
        children
    });
    // The facts that took effect on each of the month's days, oldest first, a day with none included.
    let month = month.ok().map(|m| {
        let got: BTreeMap<i64, u64> = m["by_day"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| Some((d[0].as_i64()?, d[1].as_u64()?)))
            .collect();
        let days: Vec<Value> = (0..MONTH_DAYS)
            .map(|i| {
                let d = month_from + i * DAY_MICROS;
                json!([d, got.get(&d).copied().unwrap_or(0)])
            })
            .collect();
        json!({"facts": days})
    });
    let mut out = json!({"week": week, "advice": advice, "sources": sources, "month": month});
    if partial {
        out["partial"] = json!(true);
    }
    Ok(out)
}
