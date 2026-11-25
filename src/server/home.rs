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
//! Home builds no branch's visibility state: each is a build of the whole memory, seconds on a large one. Which
//! proposals a reader may see is read from main's state ([`crate::Store::proposal_branches_built`]), so they are
//! listed at once. The few main's state can't tell wait for their branch's state, built meanwhile in the background
//! one at a time, and no longer than the other parts asked for.
//!
//! The parts that read every fact or record are kept per reader by memory state ([`crate::kept_answers`]): the root
//! counts, the review lists and the lessons are kept by their own ops; the week's facts, the advice served and the
//! sources here, for the hour the week is counted back from. The advice served and the activity are counted from the
//! answer log as kept with them, which may be up to [`crate::kept_answers::STALE`] behind it: a session logged since
//! counts once they are worked out again.
//!
//! A first ask waits [`FIRST`] for its parts, counted from when the request began (signing in and pinning the
//! memory included): those not worked out by then (a large memory's first read since a restart, or since memory was
//! taken away) are named in `pending` and left out, and go on being worked out. An ask naming them (`"parts":
//! [...]`) waits up to [`AGAIN`] for them, joining that work rather than starting it again; the root's counts,
//! counted in the background, are waited for as they land.
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
//! that is younger than [`crate::kept_answers::STALE`] (its numbers then miss only what was added since, until the
//! loop's next answers, normally a run or two later), and works its parts out itself (as above) only when there is
//! none. Each answer is one reader's, never another's:
//! kept by who they are and every group they read as, how they signed in, and whether the directory lets them read
//! the admins' parts (an admin of the memory's org, an org that shows its people's activity), as the ask finds
//! them. Not per memory and shared between readers: every part reads only what its reader may read, so the same
//! memory's numbers differ between two readers.

use std::future::Future;

use super::*;

/// The days Home counts back over.
const WEEK_DAYS: i64 = 7;
/// The days Home's chart shows, one bar each (the activity's `by_day` counts as many).
const MONTH_DAYS: i64 = crate::activity::BY_DAY_DAYS;
const DAY_MICROS: i64 = 86_400_000_000;
/// The newest of the week's facts read to say where they came from (`count` is the whole week's).
const FACT_SAMPLE: usize = 200;
/// Source folders named, the newest first.
const SOURCES_SHOWN: usize = 8;

/// The week's facts, the advice served and the sources, as one reader reads them at one memory state.
static WEEK: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

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
/// [`run_every`] and fewer than ten of them watched, each is worked out again within
/// [`crate::kept_answers::STALE`], and an ask with none kept young enough works out its own parts.
const PER_RUN: usize = 1;
/// How long an answer worked out ahead waits for its parts (the proposals main's state can't tell, for their
/// branches' states): one not [`settled`] by then is worked out again at the next run, so a memory whose branches
/// are still being built doesn't hold the server's turn ([`AT_ONCE`]).
const AHEAD_WAIT: Duration = Duration::from_secs(5);
/// How many memories' answers are worked out at once, across the server.
const AT_ONCE: usize = 1;

/// Each watched reader's last answer, worked out ahead, by memory and reader ([`ahead_key`]).
static AHEAD: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

/// The readers whose answers are worked out ahead: by scope namespace, then by [`ahead_key`].
static WATCHED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, HashMap<String, Watched>>>> =
    std::sync::LazyLock::new(Default::default);

/// The turns of [`AT_ONCE`].
static TURNS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(AT_ONCE);

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
    let wanted: Vec<&str> = match req["parts"].as_array() {
        Some(asked) => PARTS
            .into_iter()
            .filter(|p| asked.iter().any(|a| a == p))
            .collect(),
        None => PARTS.to_vec(),
    };
    // Main as it is now: the answer worked out ahead, where there is one this ask may have. The ask's pin may be
    // behind this server's own writes ([`crate::behind_own_writes`], as the server pins a Home ask): enough to check
    // a kept answer against, whose mark refuses one from before any removal, but not to work one out from, so the
    // parts are worked out on main pinned again, as any other read pins it.
    let pinned;
    let session = match session.on_main_now() {
        true => {
            let key = ahead_key(store, session, reader, via, superuser).await;
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
            match found {
                Ok(Some(out)) => {
                    crate::phases::mark("home ahead");
                    return Ok(out);
                }
                Ok(None) => {}
                Err(()) => return Ok(json!({"since": week_since(), "pending": wanted})),
            }
            // A first ask waits for the pin within `FIRST` too: past it every part is pending, worked out ahead
            // meanwhile (the ask watched its reader) for the ask again.
            let pin = store.pin_lagging(session.scope(), crate::MAIN);
            pinned = match first {
                false => pin.await?,
                true => match tokio::time::timeout(first_left(), pin).await {
                    Ok(s) => s?,
                    Err(_) => return Ok(json!({"since": week_since(), "pending": wanted})),
                },
            };
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
    answer(
        state,
        store,
        session,
        reader,
        (via, superuser),
        &wanted,
        until,
        Some(BESIDE),
    )
    .await
}

/// What is left of a first ask's [`FIRST`], counted from when its request began.
fn first_left() -> Duration {
    FIRST.saturating_sub(crate::phases::elapsed().unwrap_or_default())
}

/// The answer's `wanted` parts, worked out by `until`; with `beside`, the proposals are left pending that long after
/// the other parts asked for have answered (unless they are all that was asked for).
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
        got.map_err(|e| eprintln!("home: the week in {} not read: {e:#}", session.scope()))
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
    // Listed once no proposal needs its branch's state built: read from main's, or built already (those main's state
    // can't tell are built meanwhile in the background, one after another: never on this request, side by side).
    let proposals = async {
        let p = loop {
            let p = admin(json!({"op": "list_proposals", "built": true})).await?;
            if !p["result"]["proposals"].is_null() {
                break p;
            }
            tokio::time::sleep(COUNTS_EVERY).await;
        };
        let all: Vec<&Value> = p["result"]["proposals"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let pending = |p: &&&Value| p["status"] == "pending_review";
        let has = |list: &Value, me: &str| list.as_array().into_iter().flatten().any(|u| u == me);
        Some(json!({
            "mine": all.iter().filter(pending).filter(|p| has(&p["required_owners"], who.user()) && !has(&p["approvals"], who.user())).count(),
            "pending": all.iter().filter(pending).count(),
        }))
    };
    let conflicts = async {
        let c = admin(json!({"op": "list_conflicts", "mine": true, "limit": 1})).await;
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
            part(want("counts"), until, counts),
            part(want("week"), until, week),
            part(want("lessons"), until, lessons),
            part(want("branches"), until, branches),
            part(want("activity"), until, activity),
            part(want("conflicts"), until, conflicts),
            part(want("dream"), until, dream),
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
            got = part(want("proposals"), until, proposals) => got,
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
    }
    out["pending"] = json!(pending);
    Ok(out)
}

/// The key of a reader's answers worked out ahead: who they are and every group they read as
/// ([`crate::kept_answers::reader_key`]), how they signed in, and what the directory, as this ask finds it, lets them
/// read of the admins' parts. A reader the directory no longer lets read them (an admin no longer) has another key,
/// and never the answers worked out while they could.
async fn ahead_key(
    store: &Store,
    session: &Session,
    reader: crate::browse::Reader<'_>,
    via: &SignIn,
    superuser: bool,
) -> String {
    let (admin, shows) = match session.scope().org_name() {
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
        "{}\n{via:?}\n{superuser}\n{}\n{admin}\n{shows}",
        crate::kept_answers::reader_key(reader.who),
        reader.every_session
    )
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
    let session = store.pin_lagging(scope, crate::MAIN).await?;
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
    let (kept, _, _) = AHEAD.peek(&ns, key, &at)?;
    // Counted back from this hour, not the last (worked out again as the hour turns).
    if kept["since"] != week_since() {
        return None;
    }
    if wanted.len() == PARTS.len() {
        return Some((*kept).clone());
    }
    let pending = |p: &&str| {
        kept["pending"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x == p))
    };
    if wanted.iter().any(pending) {
        return None;
    }
    let mut out = json!({"since": kept["since"], "pending": []});
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
    let w = w.clone();
    AHEAD
        .renew(ns, key, at, move || async move {
            let reader = crate::browse::Reader {
                who: &w.who,
                every_session: w.every_session,
            };
            let until = tokio::time::Instant::now() + AHEAD_WAIT;
            answer(
                &w.state,
                &store,
                &session,
                reader,
                (&w.via, w.superuser),
                &PARTS,
                until,
                None,
            )
            .await
        })
        .await?;
    Ok(Some(Worked {
        when: std::time::Instant::now(),
        main: Some(main),
        again,
    }))
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
    hour - WEEK_DAYS * DAY_MICROS
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
    Ok(json!({"week": week, "advice": advice, "sources": sources, "month": month}))
}
