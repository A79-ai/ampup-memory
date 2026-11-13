//! What the memory learned, lesson by lesson: `POST /lessons` (the `lessons` op of `/browse`, with its scope and
//! branch selection, its ACLs and its metering). A lesson is a learned advice note ([`crate::advice`]), a person's own
//! (`mine`) or the team's, and each comes with its trail:
//!
//! - **learned from**: the runs and sessions its situation was learned from (the map's assignments), each with how
//!   it went: right or wrong, and who said so (a person's grade or rating, what came of its PRs, a recorded reward,
//!   or the judge);
//! - **served in**: the sessions it was served in since, each with where in the session it was handed (`start`,
//!   `prompt`, `stuck`, …), whether the judge found it helped, and how the session went, by the same order of who
//!   said so.
//!
//! What a caller reads is what they may read elsewhere: their own lessons and the team's, and in a trail the runs
//! and sessions that are their own; everyone else's are only counted. An org's admins on the org's own memory, where
//! the org shows its people's activity ([`crate::browse::Reader::every_session`], as `notes/` and the advice trail
//! decide it), read every person's own lessons and every trail entry in full.
//!
//! Request: `{"space"?: "mine" | "team" | "all", "person"?: <user>, "key"?: <lesson key>, "since"?: <time>,
//! "limit"?: <trail entries per side>}`. `person` narrows `mine` to one person's (only an admin names someone else);
//! `since` keeps the lessons written at or after it (µs, an RFC 3339 time or a date); `limit` is 12 by default and
//! at most 200, newest first, with the rest counted.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
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

    let scope = session.scope();
    let log = crate::sessions::list(store, scope).await?;
    let by_id: BTreeMap<&str, &Logged> = log.iter().map(|l| (l.answer_id.as_str(), l)).collect();
    // Whose own lessons: the caller's, or for a reader of everyone's, everyone who ran or asked something here.
    let mut people: BTreeSet<String> = BTreeSet::new();
    if space != "team" {
        people.insert(me.clone());
        if reader.every_session {
            people.extend(log.iter().map(|l| norm_id(l.asker.trim())));
        }
        if let Some(p) = &person {
            people.retain(|u| u == p);
            people.insert(p.clone());
        }
        people.retain(|u| !u.is_empty());
    }
    let mut out: Vec<Value> = vec![];
    let mut owners: BTreeSet<String> = BTreeSet::new();
    for owner in &people {
        // A person's own lessons are written for them alone: read as them (an admin reads them as `notes/` does).
        let as_owner = Principal::from_parts(owner.clone(), vec![]);
        let who = if *owner == me { reader.who } else { &as_owner };
        let mine = Space::mine(owner);
        let listed = crate::advice::listed(session, store, who, &mine).await?;
        if listed.is_empty() {
            continue;
        }
        let assigned = crate::advice::assigned(session, store, who, &mine).await?;
        for l in listed {
            if let Some(v) = lesson(&l, Some(owner), &assigned, &log, &by_id, &me, reader, limit) {
                owners.insert(owner.clone());
                out.push(v);
            }
        }
    }
    if space != "mine" && person.is_none() {
        let team = Space::team();
        let listed = crate::advice::listed(session, store, reader.who, &team).await?;
        if !listed.is_empty() {
            let assigned = crate::advice::assigned(session, store, reader.who, &team).await?;
            let bar = crate::sessions::evidence_bar(store, scope).await?;
            let tallies = crate::sessions::evidence(store, scope).await?;
            for l in listed {
                if let Some(mut v) = lesson(&l, None, &assigned, &log, &by_id, &me, reader, limit) {
                    if let Some(t) = tallies.get(&l.key) {
                        let mut e = json!(t);
                        e["ratified"] = json!(t.ratified(bar));
                        v["evidence"] = e;
                    }
                    out.push(v);
                }
            }
        }
    }
    out.retain(|v| {
        only.is_none_or(|k| v["key"] == k)
            && since.is_none_or(|t| v["learned_at"].as_i64().is_some_and(|at| at >= t))
    });
    out.sort_by(|a, b| {
        b["learned_at"]
            .as_i64()
            .cmp(&a["learned_at"].as_i64())
            .then_with(|| a["key"].as_str().cmp(&b["key"].as_str()))
    });
    if only.is_some() && out.is_empty() {
        return Err(crate::browse::NotFound.into());
    }
    Ok(json!({
        "reader": {"user": reader.who.user(), "everyone": reader.every_session},
        "people": owners,
        "count": out.len(),
        "lessons": out,
    }))
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
        let e = sources.entry(id).or_default();
        let (right, by) = by_id
            .get(id)
            .map_or((None, "unknown"), |l| run_outcome(l, at));
        e.0.push(right);
        e.1.push(by);
    }
    let (mut runs, mut right, mut wrong, mut hidden) = (0, 0, 0, 0);
    let mut learners: BTreeSet<String> = BTreeSet::new();
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
        if let Some(l) = l {
            learners.insert(norm_id(l.asker.trim()));
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
        if crate::sessions::automated(x)
            || !s.served.contains(&l.key)
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
    } else if l.needs_backing.is_some() {
        "needs backing"
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
            "trail": learned, "more": more, "hidden": hidden,
        },
        "served_in": {
            "sessions": sessions, "people": servees.len(), "helped": helped, "good": good, "bad": bad_,
            "trail": served, "more": served_more, "hidden": served_hidden,
        },
    });
    if let Some(why) = &l.needs_backing {
        v["status_reason"] = json!(why);
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
fn session_outcome(l: &Logged, verdicts: Option<&Value>) -> (&'static str, &'static str) {
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
