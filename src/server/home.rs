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
//! - `lessons`: how many lessons were learned in the week (`lessons {count: true}`);
//! - `advice`: the team's notes served in the week (`advice_usage`): how often, how many notes, and how many of
//!   those served wait for ratifying;
//! - `activity` (an org's admins, where it shows its people's activity): who asked or ran something in the week,
//!   how many sessions, and how many of them were given advice;
//! - `proposals`: those that wait on the caller, and all that wait for review; `conflicts`: those on what the
//!   caller owns;
//! - `dream` (an org's admins): its last pass, what waits and what it applied (as `dream_decisions` answers, with
//!   no decisions);
//! - `branches`: the memory's branches with their skills (`branches`), a branch whose state isn't built with none
//!   known; `schema`: how many types it declares.
//!
//! Home builds no branch's visibility state: each is a build of the whole memory, seconds on a large one. The
//! proposals are listed once every proposal's branch has its state, built meanwhile in the background one at a
//! time, and wait no longer than the other parts asked for.
//!
//! The parts that read every fact or record are kept per reader by memory state ([`crate::kept_answers`]): the root
//! counts, the review lists and the lessons are kept by their own ops; the week's facts, the advice served and the
//! sources here, for the hour the week is counted back from. The advice served is counted from the answer log as of
//! the memory's last state: a session logged since counts once the state moves on (it is rebuilt every few seconds
//! while writes come in).
//!
//! A first ask waits [`FIRST`] for its parts: those not worked out by then (a large memory's first read since a
//! restart, or since memory was taken away) are named in `pending` and left out, and go on being worked out. An ask
//! naming them (`"parts": [...]`) waits up to [`AGAIN`] for them, joining that work rather than starting it again;
//! the root's counts, counted in the background, are waited for as they land.

use std::future::Future;

use super::*;

/// The days Home counts back over.
const WEEK_DAYS: i64 = 7;
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
/// and a wait for the proposals' branches to be built.
const COUNTS_EVERY: Duration = Duration::from_millis(250);
/// How long the proposals are waited for once every other part asked for has answered.
const BESIDE: Duration = Duration::from_millis(300);

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
    let who = reader.who;
    crate::phases::mark("home");
    // `parts`: those a first ask said were pending, waited for longer; else every part, for at most `FIRST`.
    let (wanted, wait): (Vec<&str>, Duration) = match req["parts"].as_array() {
        Some(asked) => (
            PARTS
                .into_iter()
                .filter(|p| asked.iter().any(|a| a == p))
                .collect(),
            AGAIN,
        ),
        None => (PARTS.to_vec(), FIRST),
    };
    let until = tokio::time::Instant::now() + wait;
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
    let hour = crate::now_micros() / 3_600_000_000 * 3_600_000_000;
    let since = hour - WEEK_DAYS * DAY_MICROS;
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
        // How many sessions were given advice, of how many, where the log says.
        for (k, from) in [("served", "sessions_served"), ("of", "sessions")] {
            if !a["serving"][from].is_null() {
                v[k] = a["serving"][from].clone();
            }
        }
        Some(v)
    };
    // Listed once every proposal's branch has its state built (a list reads each one's), built meanwhile in the
    // background one after another: never on this request, side by side.
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
    // The proposals wait for their branches' states to be built, which may take long after a restart: they hold no
    // other part back, and are named pending once the rest has answered, unless they are all that was asked for.
    let (rest_done, mut rest_heard) = tokio::sync::watch::channel(false);
    let alone = wanted == ["proposals"];
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
            tokio::time::sleep(BESIDE).await;
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
                for k in ["week", "sources", "advice"] {
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
    let (facts, advice, sources) = (
        json!({"op": "facts", "valid_from": {"gte": since}, "limit": FACT_SAMPLE, "order": "newest"}),
        json!({"op": "advice_usage", "days": WEEK_DAYS}),
        json!({"op": "ls", "path": ["sources"], "limit": 1000}),
    );
    let facts = crate::browse::run(store, session, reader, &facts);
    let advice = crate::browse::run(store, session, reader, &advice);
    let sources = crate::browse::run(store, session, reader, &sources);
    let (facts, advice, sources) =
        tokio::join!(Box::pin(facts), Box::pin(advice), Box::pin(sources));
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
            "waiting": used.iter().filter(|x| x["ratified"] != true && x["held"] != true).count(),
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
    Ok(json!({"week": week, "advice": advice, "sources": sources}))
}
