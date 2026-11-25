//! What the memory learned, lesson by lesson: `POST /lessons` (the `lessons` op of `/browse`, with its scope and
//! branch selection, its ACLs and its metering). A lesson is a learned advice note ([`crate::advice`]), a person's own
//! (`mine`) or the team's, and each comes with its trail:
//!
//! - **learned from**: the runs and sessions its situation was learned from (the map's assignments), each with how
//!   it went: right or wrong, and who said so (a person's grade or rating, what came of its PRs, a recorded reward,
//!   or the judge); how many people they came from, and apart from them how many were imported as the org's
//!   (`imported`) and from which import sources (`imports`, `import:<label>`: `crate::runs::import`);
//! - **served in**: the sessions it was served in since, each with where in the session it was handed (`start`,
//!   `prompt`, `stuck`, …), whether the judge found it helped, and how the session went, by the same order of who
//!   said so.
//!
//! A note an org admin wrote rather than learned from runs (`crate::advice::write_admin`) says so: `written_by`, with
//! the label they gave where it came from.
//!
//! What a caller reads is what they may read elsewhere: their own lessons and the team's, and in a trail the runs
//! and sessions that are their own; everyone else's are only counted. An org's admins on the org's own memory, where
//! the org shows its people's activity ([`crate::browse::Reader::every_session`], as `notes/` and the advice trail
//! decide it), read every person's own lessons and every trail entry in full.
//!
//! Request: `{"space"?: "mine" | "team" | "all", "person"?: <user>, "key"?: <lesson key>, "since"?: <time>,
//! "limit"?: <trail entries per side>}`. `person` narrows `mine` to one person's (only an admin names someone else);
//! `since` keeps the lessons written at or after it (µs, an RFC 3339 time or a date); `limit` is 12 by default and
//! at most 200, newest first, with the rest counted. `"probe": true` answers at once, with no lessons: whether the
//! server has them. `"count": true` answers only how many there are (and whose), with no lessons and no trails.
//!
//! A page of them: `"show"?: "team" | "mine" | <user>` keeps the team's, the caller's own or one person's own (an
//! admin's view of someone's), `"page"?: <lessons>` lists at most that many (1 to 200; all when absent) and
//! `"after"?: <next>` starts after the last page's, whose `next` says where (null on the last page). `count` is how
//! many `show` keeps, and `groups` how many there are of each whose (the team's, `owner` null, and each person's),
//! with the runs they were learned from, the sessions they were served in and how many it helped, summed.
//!
//! Counted as the Advice page counts (`/browse {op: "advice_usage"}`): `"days"?: <1 to 3650>` counts only the runs
//! and sessions asked in the last that many days (all of them when absent), by the same cut `advice_usage` makes, so
//! a lesson's runs, sessions served and helped, its trails and the `groups` summed from them are that window's.
//! `"as_usage": true` reads the notes `advice_usage` lists: the team's and the reader's own, or `person`'s own in
//! their place (an admin's view of someone), leaving out conversation situations' notes and merged situations'
//! (neither is served as advice). The answer echoes `days` and `as_usage` as it applied them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::advice::{Listed, Space};
use crate::auth::acl::norm_id;
use crate::browse::{Reader, bad};
use crate::qa_log::Logged;
use crate::{Principal, Session, Store};

const LIMIT: usize = 12;
const MAX_LIMIT: usize = 200;
/// The longest question a trail entry shows, in characters.
const QUESTION: usize = 280;

pub async fn run(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    req: &Value,
) -> Result<Value> {
    let me = norm_id(reader.who.user());
    let space = req["space"].as_str().unwrap_or("all");
    if !["mine", "team", "all"].contains(&space) {
        return Err(bad("space is mine, team or all"));
    }
    let person = match req["person"].as_str().filter(|p| !p.is_empty()) {
        Some(p) if !reader.every_session && norm_id(p) != me => {
            return Err(bad("you read only your own lessons and the team's"));
        }
        p => p.map(norm_id),
    };
    let only = req["key"].as_str().filter(|k| !k.is_empty());
    let since = match &req["since"] {
        Value::Null => None,
        Value::Number(n) => n.as_i64(),
        Value::String(s) => Some(
            crate::query::schema::parse_time(s)
                .map(|t| t.timestamp_micros())
                .ok_or_else(|| bad("since is µs, an RFC 3339 time or a YYYY-MM-DD date"))?,
        ),
        _ => return Err(bad("since is µs, an RFC 3339 time or a YYYY-MM-DD date")),
    };
    let limit = req["limit"]
        .as_u64()
        .map_or(LIMIT, |n| n as usize)
        .clamp(1, MAX_LIMIT);
    // The window the runs and sessions are counted over, as `advice_usage` reads it.
    let days = match &req["days"] {
        Value::Null => None,
        v => Some(
            v.as_u64()
                .filter(|d| (1..=3650).contains(d))
                .ok_or_else(|| bad("days is a whole number of days, 1 to 3650"))?,
        ),
    };
    let as_usage = req["as_usage"] == true;
    let want = Want {
        space: space.to_string(),
        person,
        as_usage,
    };
    // A probe (the Studio asking whether this server has lessons): yes, without reading every session.
    if req["probe"] == true {
        return Ok(
            json!({"reader": {"user": reader.who.user(), "everyone": reader.every_session},
                         "people": [], "count": null, "lessons": []}),
        );
    }

    if req["count"] == true {
        return count(store, session, reader, &want, only, since).await;
    }
    // Which of them the page shows: the team's, the reader's own, or one person's own (an admin's view of them).
    let show = match req["show"].as_str().unwrap_or_default() {
        "" => Show::All,
        "team" => Show::Team,
        "mine" => Show::Owner(me.clone()),
        p if !reader.every_session && norm_id(p) != me => {
            return Err(bad("you read only your own lessons and the team's"));
        }
        p => Show::Owner(norm_id(p)),
    };
    let page = match &req["page"] {
        Value::Null => None,
        v => Some(
            v.as_u64()
                .filter(|n| (1..=MAX_PAGE as u64).contains(n))
                .ok_or_else(|| bad(format!("page is a whole number from 1 to {MAX_PAGE}")))?
                as usize,
        ),
    };
    let after = match &req["after"] {
        Value::Null => None,
        Value::String(a) => Some(cursor_of(a).ok_or_else(|| bad("after is a page's next"))?),
        _ => return Err(bad("after is a page's next")),
    };
    let full = full_kept(store, session, reader, &want, limit, days).await?;
    crate::phases::mark("lessons read");
    let matching: Vec<&Value> = full
        .lessons
        .iter()
        .filter(|v| {
            only.is_none_or(|k| v["key"] == k)
                && since.is_none_or(|t| v["learned_at"].as_i64().is_some_and(|at| at >= t))
        })
        .collect();
    if only.is_some() && matching.is_empty() {
        return Err(crate::browse::NotFound.into());
    }
    let groups = groups_of(&matching);
    let shown: Vec<&Value> = matching.into_iter().filter(|v| show.holds(v)).collect();
    let from = after.map_or(0, |a| shown.partition_point(|v| order(v) <= a));
    let to = page.map_or(shown.len(), |n| (from + n).min(shown.len()));
    let next = (to < shown.len()).then(|| cursor(shown[to - 1]));
    Ok(json!({
        "reader": {"user": reader.who.user(), "everyone": reader.every_session},
        "people": full.owners,
        "count": shown.len(),
        "groups": groups,
        "lessons": &shown[from..to],
        "next": next,
        "days": days,
        "as_usage": as_usage,
    }))
}

/// Whose lessons a request reads: `space`, `person`'s (only an admin names someone else), and whether as
/// `advice_usage` lists them (`as_usage`: the team's and the reader's own or `person`'s, served situations only).
struct Want {
    space: String,
    person: Option<String>,
    as_usage: bool,
}

impl Want {
    /// What a kept answer is kept by, of what it reads.
    fn key(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.space,
            self.person.as_deref().unwrap_or_default(),
            self.as_usage
        )
    }

    /// Whether the team's lessons are read: not for `mine`, and not where a person is named, unless as
    /// `advice_usage` reads them (the team's beside that person's own).
    fn team(&self) -> bool {
        self.space != "mine" && (self.person.is_none() || self.as_usage)
    }

    /// Whether `l` is read: as `advice_usage` reads them, no conversation situation's note and no merged one.
    fn holds(&self, l: &Listed) -> bool {
        !self.as_usage || (!l.conversation && l.merged_into.is_none())
    }
}

/// Lessons one page lists at most (`page`).
const MAX_PAGE: usize = 200;

/// Which lessons a page shows (`show`): all it reads, the team's, or one person's own.
enum Show {
    All,
    Team,
    Owner(String),
}

impl Show {
    fn holds(&self, v: &Value) -> bool {
        match self {
            Show::All => true,
            Show::Team => v["owner"].is_null(),
            Show::Owner(o) => v["owner"].as_str().is_some_and(|w| norm_id(w) == *o),
        }
    }
}

/// A lesson's place in the order lessons are listed in: newest first, then by key, then by whose.
type Order = (std::cmp::Reverse<i64>, String, String);

fn order(v: &Value) -> Order {
    (
        std::cmp::Reverse(v["learned_at"].as_i64().unwrap_or_default()),
        v["key"].as_str().unwrap_or_default().to_string(),
        v["owner"].as_str().unwrap_or_default().to_string(),
    )
}

/// A page's `next`: where the next page starts, after the lesson `v`.
fn cursor(v: &Value) -> String {
    let (at, key, owner) = order(v);
    format!("{}|{owner}|{key}", at.0)
}

fn cursor_of(s: &str) -> Option<Order> {
    let (at, rest) = s.split_once('|')?;
    let (owner, key) = rest.split_once('|')?;
    Some((
        std::cmp::Reverse(at.parse().ok()?),
        key.to_string(),
        owner.to_string(),
    ))
}

/// The lessons by whose they are (the team's, `owner` null, or a person's own): how many, and the runs they were
/// learned from, the sessions they were served in and how many it helped, summed: a page's tiles and tabs.
fn groups_of(lessons: &[&Value]) -> Vec<Value> {
    let mut by: BTreeMap<Option<&str>, [u64; 4]> = BTreeMap::new();
    for v in lessons {
        let g = by.entry(v["owner"].as_str()).or_default();
        g[0] += 1;
        g[1] += v["learned_from"]["runs"].as_u64().unwrap_or_default();
        g[2] += v["served_in"]["sessions"].as_u64().unwrap_or_default();
        g[3] += v["served_in"]["helped"].as_u64().unwrap_or_default();
    }
    by.into_iter()
        .map(|(owner, [n, runs, sessions, helped])| {
            json!({"owner": owner, "lessons": n, "runs": runs, "sessions": sessions, "helped": helped})
        })
        .collect()
}

/// Every lesson a reader reads of `space` (and `person`'s), with trails of `limit` entries, in the order they are
/// listed, and the people whose own lessons are among them.
struct Full {
    lessons: Vec<Value>,
    owners: BTreeSet<String>,
}

static FULL: std::sync::LazyLock<crate::kept_answers::Kept<Full>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(200));

/// [`full`], kept by memory state ([`crate::kept_answers`]) for main as it is now: it reads every person's advice
/// map and notes, and the sessions log. Kept by the reader, what they read (`space`, `person`, `limit`) and the
/// sessions log as well as main's state: a session logged since is a newer state, answered as main moving on is
/// (the last answer while the next is worked out), and a session forgotten or retracted is a loss, never answered
/// from what was kept before it.
async fn full_kept(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    want: &Want,
    limit: usize,
    days: Option<u64>,
) -> Result<std::sync::Arc<Full>> {
    // The window ends when the answer is worked out: kept by the span of time it was asked in, as `advice_usage`.
    let since = move || days.map_or(0, |d| crate::now_micros() - d as i64 * 86_400_000_000);
    let at = match session.on_main_now() {
        true => {
            let mark = crate::kept_answers::Mark::read(store, session.scope()).await?;
            let now = store
                .lagging_visibility(session.scope(), crate::MAIN)
                .await?;
            std::ptr::eq(session.vis(), &*now).then(|| crate::kept_answers::At::of(mark, &now))
        }
        false => None,
    };
    crate::phases::mark("lessons state");
    let log = std::sync::Arc::new(crate::sessions::list(store, session.scope()).await?);
    crate::phases::mark("lessons sessions");
    let Some(mut at) = at else {
        return Ok(std::sync::Arc::new(
            full(store, session, reader, want, limit, since(), &log).await?,
        ));
    };
    let newest = log.iter().map(|l| l.updated_at.max(l.asked_at)).max();
    at.heads.push(format!(
        "sessions {} {}",
        log.len(),
        newest.unwrap_or_default()
    ));
    let key = format!(
        "{}\n{}\n{}\n{limit}\n{}",
        crate::kept_answers::reader_key(reader.who),
        reader.every_session,
        want.key(),
        days.map_or_else(
            || "all".to_string(),
            |d| format!("{d} in {}", crate::kept_answers::span())
        )
    );
    let (store2, session2, who, every, want2) = (
        store.clone(),
        session.clone(),
        reader.who.clone(),
        reader.every_session,
        Want {
            space: want.space.clone(),
            person: want.person.clone(),
            as_usage: want.as_usage,
        },
    );
    FULL.get(
        &session.scope().namespace(),
        &key,
        at,
        true,
        || async move {
            let reader = Reader {
                who: &who,
                every_session: every,
            };
            full(&store2, &session2, reader, &want2, limit, since(), &log).await
        },
    )
    .await
}

/// How many people's lessons [`full`] reads at once.
const PEOPLE_PARALLEL: usize = 8;

/// Every lesson `reader` reads of `want`, each with its trails of the runs and sessions asked at `since` (µs) or
/// later, as [`Full`] holds them.
async fn full(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    want: &Want,
    limit: usize,
    since: i64,
    log: &[Logged],
) -> Result<Full> {
    let me = norm_id(reader.who.user());
    let scope = session.scope();
    let by_id: BTreeMap<&str, &Logged> = log.iter().map(|l| (l.answer_id.as_str(), l)).collect();
    let people = people_of(log, &me, reader.every_session, want);
    let build_started = std::time::Instant::now();
    let mut out: Vec<Value> = vec![];
    let mut owners: BTreeSet<String> = BTreeSet::new();
    // Each person's map and notes are read [`PEOPLE_PARALLEL`] people at a time: an admin reads everyone's, and
    // one after another they took seconds on a server that hadn't kept the answer yet.
    let me_ = me.as_str();
    let each = futures::stream::iter(people.iter().cloned())
        .map(|owner: String| async move {
            // A person's own lessons are written for them alone: read as them (an admin reads them as `notes/`
            // does).
            let as_owner = Principal::from_parts(owner.clone(), vec![]);
            let who = if owner == me_ { reader.who } else { &as_owner };
            let started = std::time::Instant::now();
            let (listed, assigned, times) =
                crate::advice::listed_assigned(session, store, who, &Space::mine(&owner)).await?;
            Ok::<_, anyhow::Error>((owner, listed, assigned, times, started.elapsed()))
        })
        .buffered(PEOPLE_PARALLEL)
        .collect::<Vec<_>>()
        .await;
    let people_took = build_started.elapsed();
    let mut read = PeopleRead::default();
    for r in each {
        let (owner, listed, assigned, times, took) = r?;
        read.add(&owner, times, took);
        for l in listed.iter().filter(|l| want.holds(l)) {
            if let Some(v) = lesson(
                l,
                Some(&owner),
                &assigned,
                log,
                &by_id,
                &me,
                reader,
                limit,
                since,
            ) {
                owners.insert(owner.clone());
                out.push(v);
            }
        }
    }
    let mut team_read: Option<String> = None;
    if want.team() {
        let team = Space::team();
        let team_started = std::time::Instant::now();
        let (((listed, assigned, times), map_took), (bar, bar_took), (tallies, tallies_took)) = tokio::try_join!(
            timed(crate::advice::listed_assigned(
                session, store, reader.who, &team
            )),
            timed(crate::sessions::evidence_bar(store, scope)),
            timed(crate::sessions::evidence(store, scope)),
        )?;
        team_read = Some(format!(
            "team in {} ms (map {} ms, notes {} ms for {}, merged notes {} ms for {}, curation {} ms, all of it {} ms; \
             evidence bar {} ms, tallies {} ms)",
            team_started.elapsed().as_millis(),
            times.map.as_millis(),
            times.notes.as_millis(),
            times.notes_read,
            times.merged.as_millis(),
            times.merged_read,
            times.withheld.as_millis(),
            map_took.as_millis(),
            bar_took.as_millis(),
            tallies_took.as_millis(),
        ));
        if !listed.is_empty() {
            for l in listed.iter().filter(|l| want.holds(l)) {
                if let Some(mut v) =
                    lesson(l, None, &assigned, log, &by_id, &me, reader, limit, since)
                {
                    // Ratified: by an admin, or backed by outcomes or people and past the evidence bar. Every
                    // lesson listed here is served either way.
                    let earned = tallies
                        .get(&l.key)
                        .cloned()
                        .unwrap_or_default()
                        .ratified(bar);
                    let ratified =
                        crate::advice::team_ratified(l.approved, l.needs_backing.is_some(), earned);
                    v["ratified"] = json!(ratified);
                    if let Some(t) = tallies.get(&l.key) {
                        let mut e = json!(t);
                        e["ratified"] = json!(ratified);
                        v["evidence"] = e;
                    }
                    out.push(v);
                }
            }
        }
    }
    out.sort_by_key(order);
    let took = build_started.elapsed();
    if took >= SLOW_BUILD {
        eprintln!(
            "slow lessons build: {} in {} ms: {}; {}",
            scope.namespace(),
            took.as_millis(),
            read.line(people.len(), people_took),
            team_read.as_deref().unwrap_or("no team")
        );
    }
    Ok(Full {
        lessons: out,
        owners,
    })
}

/// A build of the lessons this long says where its time went.
const SLOW_BUILD: std::time::Duration = std::time::Duration::from_secs(1);

/// `f`, and how long it took.
async fn timed<T>(
    f: impl std::future::Future<Output = Result<T>>,
) -> Result<(T, std::time::Duration)> {
    let started = std::time::Instant::now();
    let v = f.await?;
    Ok((v, started.elapsed()))
}

/// What reading each person's lessons took, summed over them, and the slowest person.
#[derive(Default)]
struct PeopleRead {
    times: crate::advice::ListedTimes,
    slowest: Option<(String, std::time::Duration)>,
}

impl PeopleRead {
    fn add(&mut self, owner: &str, t: crate::advice::ListedTimes, took: std::time::Duration) {
        self.times.map += t.map;
        self.times.withheld += t.withheld;
        self.times.notes += t.notes;
        self.times.notes_read += t.notes_read;
        self.times.merged += t.merged;
        self.times.merged_read += t.merged_read;
        if self.slowest.as_ref().is_none_or(|(_, s)| took > *s) {
            self.slowest = Some((owner.to_string(), took));
        }
    }

    fn line(&self, people: usize, took: std::time::Duration) -> String {
        let t = &self.times;
        format!(
            "{people} people in {} ms, {PEOPLE_PARALLEL} at a time (summed over them: maps {} ms, notes {} ms for {}, \
             merged notes {} ms for {}, curation {} ms; slowest {} ms)",
            took.as_millis(),
            t.map.as_millis(),
            t.notes.as_millis(),
            t.notes_read,
            t.merged.as_millis(),
            t.merged_read,
            t.withheld.as_millis(),
            self.slowest.as_ref().map_or(0, |(_, d)| d.as_millis()),
        )
    }
}

/// Whose own lessons a reader reads: their own, or for a reader of everyone's, everyone who ran or asked something
/// here; only `person`'s where one is named; none for the team's (`space` `team`). As `advice_usage` reads them,
/// only the reader's own or `person`'s.
fn people_of(log: &[Logged], me: &str, every_session: bool, want: &Want) -> BTreeSet<String> {
    let person = want.person.as_deref();
    let mut people: BTreeSet<String> = BTreeSet::new();
    if want.space != "team" {
        people.insert(me.to_string());
        // An import source ([`crate::runs::import`]) is no one: it has no lessons of its own.
        if every_session && !want.as_usage {
            people.extend(
                log.iter()
                    .filter(|l| !crate::runs::is_import(&l.asker))
                    .map(|l| norm_id(l.asker.trim())),
            );
        }
        if let Some(p) = person {
            people.retain(|u| u == p);
            people.insert(p.to_string());
        }
        people.retain(|u| !u.is_empty());
    }
    people
}

/// The lessons a reader reads, as (whose: none for the team's, key, when its note was written), kept by memory state
/// ([`crate::kept_answers`]) for `"count": true`: reading them reads every person's advice map and notes.
type Noted = Vec<(Option<String>, String, i64)>;

static NOTED: std::sync::LazyLock<crate::kept_answers::Kept<Noted>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

/// `"count": true`: how many lessons there are, of `only` and learned at `since` or later, and whose.
async fn count(
    store: &Store,
    session: &Session,
    reader: Reader<'_>,
    want: &Want,
    only: Option<&str>,
    since: Option<i64>,
) -> Result<Value> {
    // Kept only for main as it is now: a read of a branch or of a past state reads them itself. The request pinned
    // its state before the scope's loss count could be read ([`crate::kept_answers::Mark`]): kept only when, read
    // after it, main's state is still the one pinned.
    let at = match session.on_main_now() {
        true => {
            let mark = crate::kept_answers::Mark::read(store, session.scope()).await?;
            let now = store
                .lagging_visibility(session.scope(), crate::MAIN)
                .await?;
            std::ptr::eq(session.vis(), &*now).then(|| crate::kept_answers::At::of(mark, &now))
        }
        false => None,
    };
    crate::phases::mark("lessons pinned");
    let noted = match at {
        Some(at) => {
            let key = format!(
                "{}\n{}\n{}",
                crate::kept_answers::reader_key(reader.who),
                reader.every_session,
                want.key()
            );
            let (store, session2, who, every, want2) = (
                store.clone(),
                session.clone(),
                reader.who.clone(),
                reader.every_session,
                Want {
                    space: want.space.clone(),
                    person: want.person.clone(),
                    as_usage: want.as_usage,
                },
            );
            NOTED
                .get(
                    &session.scope().namespace(),
                    &key,
                    at,
                    true,
                    || async move { noted(&store, &session2, &who, every, &want2).await },
                )
                .await?
        }
        None => std::sync::Arc::new(
            noted(store, session, reader.who, reader.every_session, want).await?,
        ),
    };
    let mut owners: BTreeSet<&str> = BTreeSet::new();
    let mut counts = 0;
    for (owner, _, _) in noted
        .iter()
        .filter(|(_, k, at)| since.is_none_or(|t| *at >= t) && only.is_none_or(|o| k == o))
    {
        counts += 1;
        if let Some(o) = owner {
            owners.insert(o);
        }
    }
    if only.is_some() && counts == 0 {
        return Err(crate::browse::NotFound.into());
    }
    Ok(json!({
        "reader": {"user": reader.who.user(), "everyone": reader.every_session},
        "people": owners,
        "count": counts,
        "lessons": [],
    }))
}

/// Every lesson `who` reads that has a note: each person's own ([`people_of`]), then the team's.
async fn noted(
    store: &Store,
    session: &Session,
    who: &Principal,
    every_session: bool,
    want: &Want,
) -> Result<Noted> {
    let me = norm_id(who.user());
    let log = crate::sessions::list(store, session.scope()).await?;
    let mut out = vec![];
    for owner in people_of(&log, &me, every_session, want) {
        // A person's own lessons are written for them alone: read as them (an admin reads them as `notes/` does).
        let as_owner = Principal::from_parts(owner.clone(), vec![]);
        let reading = if owner == me { who } else { &as_owner };
        for l in crate::advice::listed(session, store, reading, &Space::mine(&owner)).await? {
            if !want.holds(&l) {
                continue;
            }
            if let Some((_, at, _)) = l.note {
                out.push((Some(owner.clone()), l.key, at));
            }
        }
    }
    if want.team() {
        for l in crate::advice::listed(session, store, who, &Space::team()).await? {
            if !want.holds(&l) {
                continue;
            }
            if let Some((_, at, _)) = l.note {
                out.push((None, l.key, at));
            }
        }
    }
    Ok(out)
}

/// One lesson with its trail, as the caller may read it; none for a situation with no note (procedures only).
#[allow(clippy::too_many_arguments)]
fn lesson(
    l: &Listed,
    owner: Option<&String>,
    assigned: &BTreeMap<String, String>,
    log: &[Logged],
    by_id: &BTreeMap<&str, &Logged>,
    me: &str,
    reader: Reader<'_>,
    limit: usize,
    since: i64,
) -> Option<Value> {
    let (note_id, at, text) = l.note.as_ref()?;
    let untagged = l
        .key
        .strip_prefix(crate::advice::MINE_KEY)
        .unwrap_or(&l.key);
    // Whose entries the caller reads in full: their own, or everyone's for a reader of everyone's.
    let readable = |asker: &str| reader.every_session || norm_id(asker.trim()) == me;

    // Learned from: each run or session the situation's runs came from (a session's graded requests are runs of
    // it, `<session>#<offset>`), with how its runs went.
    let mut sources: BTreeMap<&str, (Vec<Option<bool>>, Vec<&'static str>)> = BTreeMap::new();
    for (run, k) in assigned {
        if k != untagged {
            continue;
        }
        let (id, at) = match run.split_once('#') {
            Some((id, at)) => (id, at.parse::<usize>().ok()),
            None => (run.as_str(), None),
        };
        // In a window, only the runs asked in it (one no longer in the log has no time: left out of a window).
        if since > 0 && by_id.get(id).is_none_or(|l| l.asked_at < since) {
            continue;
        }
        let e = sources.entry(id).or_default();
        let (right, by) = by_id
            .get(id)
            .map_or((None, "unknown"), |l| run_outcome(l, at));
        e.0.push(right);
        e.1.push(by);
    }
    let (mut runs, mut right, mut wrong, mut hidden) = (0, 0, 0, 0);
    let mut learners: BTreeSet<String> = BTreeSet::new();
    // Runs imported as the org's (`crate::runs::import`), by their import source: counted apart from people.
    let (mut imported, mut imports) = (0, BTreeSet::new());
    let mut learned: Vec<Value> = vec![];
    for (id, (outcomes, by)) in &sources {
        runs += outcomes.len();
        let (r, w) = (
            outcomes.iter().filter(|o| **o == Some(true)).count(),
            outcomes.iter().filter(|o| **o == Some(false)).count(),
        );
        right += r;
        wrong += w;
        let l = by_id.get(id);
        match l {
            Some(l) if crate::runs::is_import(&l.asker) => {
                imported += outcomes.len();
                imports.insert(l.asker.trim().to_string());
            }
            Some(l) => {
                learners.insert(norm_id(l.asker.trim()));
            }
            None => {}
        }
        match l {
            Some(l) if readable(&l.asker) => learned.push(json!({
                "id": id,
                "kind": if l.session.is_some() { "session" } else { "run" },
                "user": l.asker,
                "question": cut(&l.question),
                "at": l.asked_at,
                "runs": outcomes.len(), "right": r, "wrong": w,
                "outcome": if w > 0 { "wrong" } else if r > 0 { "right" } else { "unclear" },
                "by": by.iter().find(|b| **b != "unknown").copied().unwrap_or("unknown"),
            })),
            _ => hidden += 1,
        }
    }
    learned.sort_by_key(|e| std::cmp::Reverse(e["at"].as_i64()));
    let more = learned.len().saturating_sub(limit);
    learned.truncate(limit);

    // Served in: the sessions it was handed in (a person's own lesson, only theirs), newest first.
    let (mut sessions, mut helped, mut good, mut bad_, mut served_hidden) = (0, 0, 0, 0, 0);
    let mut servees: BTreeSet<String> = BTreeSet::new();
    let mut served: Vec<Value> = vec![];
    for x in log {
        let Some(s) = x.session.as_ref() else {
            continue;
        };
        if x.asked_at < since
            || crate::sessions::automated(x)
            || !(s.served.contains(&l.key)
                || crate::advice_lookups::recalled(s).contains(l.key.as_str()))
            || owner.is_some_and(|o| norm_id(x.asker.trim()) != *o)
        {
            continue;
        }
        sessions += 1;
        servees.insert(norm_id(x.asker.trim()));
        let verdicts = s
            .verdicts
            .as_deref()
            .and_then(|v| serde_json::from_str::<Value>(v).ok());
        let n = verdicts.as_ref().map(|v| &v["notes"][&l.key]);
        let did_help = n.and_then(|n| n["helped"].as_bool());
        if did_help == Some(true) {
            helped += 1;
        }
        let (outcome, by) = session_outcome(x, verdicts.as_ref());
        match outcome {
            "good" => good += 1,
            "bad" => bad_ += 1,
            _ => {}
        }
        if !readable(&x.asker) {
            served_hidden += 1;
            continue;
        }
        // Where in the session it was handed: the first lookup that served it.
        let first = s.lookups.iter().find(|k| {
            crate::advice_lookups::outcome::served(&k.outcome)
                && (k.key.as_deref() == Some(&l.key) || k.beside.as_deref() == Some(&l.key))
        });
        let prs: Vec<Value> = s
            .prs
            .iter()
            .map(|p| {
                json!({"repo": p.repo, "number": p.number,
                       "state": p.events.last().map(|e| e.kind.as_str()).unwrap_or("open")})
            })
            .collect();
        served.push(json!({
            "id": x.answer_id,
            "user": x.asker,
            "question": cut(&x.question),
            "at": first.map_or(x.asked_at, |k| k.at),
            "moment": first.map(|k| k.moment.as_str()),
            "harness": s.harness,
            "applied": n.and_then(|n| n["applied"].as_bool()),
            "helped": did_help,
            "outcome": outcome,
            "by": by,
            "reward": x.reward,
            "prs": prs,
            "ended_at": s.ended_at,
        }));
    }
    served.sort_by_key(|e| std::cmp::Reverse(e["at"].as_i64()));
    let served_more = served.len().saturating_sub(limit);
    served.truncate(limit);

    let today = chrono::Utc::now().date_naive();
    let readable_text = crate::advice::served_text(text, today);
    // The note's heading names it (`# Advice: <the situation, in plain words>`); the rest is what is served.
    let (title, body) = match readable_text.trim_start().split_once('\n') {
        Some((h, rest)) if h.starts_with("# ") => (
            h.trim_start_matches('#')
                .trim()
                .trim_start_matches("Advice:")
                .trim()
                .to_string(),
            rest.trim().to_string(),
        ),
        _ => (String::new(), readable_text.trim().to_string()),
    };
    let status = if l.held {
        "held"
    } else if l.withheld.is_some() {
        "withheld"
    } else if l.conversation {
        "not served"
    } else {
        "served"
    };
    let mut v = json!({
        "key": l.key,
        "space": if owner.is_some() { "mine" } else { "team" },
        "owner": owner,
        "category": l.category,
        "situation": l.situation,
        "when": l.when,
        // A heading that names nothing (`# Advice`) leaves the situation's own name.
        "title": if title.is_empty() || title.eq_ignore_ascii_case("advice") {
            l.situation.replace('-', " ")
        } else {
            title
        },
        "lesson": body,
        "note_id": note_id,
        "learned_at": at,
        "status": status,
        "learned_from": {
            "runs": runs, "right": right, "wrong": wrong, "people": learners.len(),
            "imported": imported, "imports": imports,
            "trail": learned, "more": more, "hidden": hidden,
        },
        "served_in": {
            "sessions": sessions, "people": servees.len(), "helped": helped, "good": good, "bad": bad_,
            "trail": served, "more": served_more, "hidden": served_hidden,
        },
    });
    // Why outcomes or people don't back it yet, so it isn't ratified: served all the same.
    if let Some(why) = &l.needs_backing {
        v["needs_backing"] = json!(why);
    }
    // A note an org admin wrote rather than one learned from runs: where they said it came from.
    if let Some((label, at)) = &l.admin {
        v["written_by"] = json!({"admin": true, "label": label, "at": at});
    }
    Some(v)
}

/// How one run of a lesson went: right (`Some(true)`), wrong, or not known, and who said so. `at` is the offset of a
/// session's graded request, graded by the person's next message.
fn run_outcome(l: &Logged, at: Option<usize>) -> (Option<bool>, &'static str) {
    if let Some(at) = at {
        let graded = crate::sessions::graded_tasks(l)
            .and_then(|(_, g)| g.into_iter().find(|(from, _, _)| *from == at));
        return (graded.map(|(_, _, ok)| ok), "person");
    }
    if l.session.is_some() {
        let verdicts = l
            .session
            .as_ref()
            .and_then(|s| s.verdicts.as_deref())
            .and_then(|v| serde_json::from_str::<Value>(v).ok());
        let (o, by) = session_outcome(l, verdicts.as_ref());
        return (
            match o {
                "good" => Some(true),
                "bad" => Some(false),
                _ => None,
            },
            by,
        );
    }
    match l.reward {
        Some(r) => (Some(r > 0.0), reward_by(l)),
        None => (None, "unknown"),
    }
}

/// How a session went, and who said so, surest first: what came of its PRs, the person (their rating, or how their
/// next messages graded its requests), a reward its agent or the person gave, then the judge's reading.
pub(crate) fn session_outcome(
    l: &Logged,
    verdicts: Option<&Value>,
) -> (&'static str, &'static str) {
    let s = l.session.as_ref();
    let mark = |ok: bool| if ok { "good" } else { "bad" };
    if let Some((r, _)) = s.and_then(|s| crate::session_prs::outcome(&s.prs, crate::now_micros())) {
        return (mark(r > 0.0), "pr");
    }
    if let Some(ok) = crate::sessions::checked_outcome(l) {
        return (mark(ok), "person");
    }
    if let Some(r) = l.reward.filter(|r| *r != 0.0 && reward_by(l) == "reward") {
        return (mark(r > 0.0), "reward");
    }
    match verdicts.and_then(|v| v["outcome"].as_str()) {
        Some("succeeded") => ("good", "judge"),
        Some("failed") => ("bad", "judge"),
        _ if s.is_some_and(|s| s.ended_at.is_none()) => ("open", "none"),
        _ => ("unclear", "none"),
    }
}

/// Who gave a reward: the judge, where its reason says a model read it, else whoever recorded it.
fn reward_by(l: &Logged) -> &'static str {
    match l.reward_reason.as_deref() {
        Some(r) if r.starts_with(crate::qa_log::JUDGED) => "judge",
        _ => "reward",
    }
}

fn cut(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(QUESTION).collect();
    if s.chars().count() > QUESTION {
        out.push('…');
    }
    out
}
