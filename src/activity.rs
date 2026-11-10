//! Each person's activity in a memory, for its org's admins (`/admin {op: "activity"}`, the console's Studio):
//! what the answer log holds of them ([`crate::qa_log`]): their sessions, runs and logged answers, with the
//! questions asked, the answers given, and the facts, episodes and advice each used.
//!
//! An org's admins read all of it, every person's words included, and a session's or a run's transcript
//! (`/admin {op: "conversation"}`, [`transcript`]): Studio is where they trace what went wrong in a conversation.
//! The org's `activity` setting turns all of it off (`set_settings {activity: false}`): an org that keeps people's
//! questions to themselves. Forgetting a person erases their rows from the log, so they leave this too.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{Value, json};

use crate::qa_log::Logged;
use crate::{Scope, Store};

/// Longest answer shown, in characters: the console shows a preview, the log keeps it whole.
const ANSWER_PREVIEW: usize = 2_000;
/// Longest transcript shown, in characters: its end, where a conversation went wrong.
const TRANSCRIPT_MAX: usize = 200_000;

/// What `entries` (the log, newest first) says of each person active since `since` (µs), and, for `user`, their
/// newest `limit` entries; for no `user`, everyone's.
pub fn view(entries: &[Logged], user: Option<&str>, since: i64, limit: usize) -> Value {
    #[derive(Default)]
    struct Person {
        sessions: usize,
        runs: usize,
        answers: usize,
        helpful: usize,
        not_helpful: usize,
        facts: BTreeSet<String>,
        episodes: BTreeSet<String>,
        advice: BTreeSet<String>,
        last: i64,
    }
    let recent: Vec<&Logged> = entries
        .iter()
        .filter(|l| l.updated_at.max(l.asked_at) >= since)
        // A session that kept nothing (a harness's subprocess, or one closed before anything was asked) isn't a
        // conversation anyone had.
        .filter(|l| !crate::sessions::discarded(l))
        .collect();
    let mut people: BTreeMap<&str, Person> = BTreeMap::new();
    for l in &recent {
        let p = people.entry(l.asker.as_str()).or_default();
        match kind_of(l) {
            "session" => p.sessions += 1,
            "run" => p.runs += 1,
            _ => p.answers += 1,
        }
        match l.rating.as_deref() {
            Some("helpful") => p.helpful += 1,
            Some("not_helpful") => p.not_helpful += 1,
            _ => {}
        }
        p.facts.extend(l.cited.iter().cloned());
        p.episodes.extend(l.episodes.iter().cloned());
        if let Some(s) = &l.session {
            p.advice.extend(s.served.iter().cloned());
        }
        p.last = p.last.max(l.updated_at.max(l.asked_at));
    }
    let mut summary: Vec<Value> = people
        .iter()
        .map(|(u, p)| {
            json!({"user": u, "sessions": p.sessions, "runs": p.runs, "answers": p.answers,
                   "helpful": p.helpful, "not_helpful": p.not_helpful, "facts": p.facts.len(),
                   "episodes": p.episodes.len(), "advice": p.advice.len(), "last_active": p.last})
        })
        .collect();
    summary.sort_by_key(|v| std::cmp::Reverse(v["last_active"].as_i64()));
    let mut mine: Vec<&&Logged> = recent
        .iter()
        .filter(|l| user.is_none_or(|u| l.asker == u))
        .collect();
    mine.sort_by_key(|l| std::cmp::Reverse(l.updated_at.max(l.asked_at)));
    let shown: Vec<Value> = mine.iter().take(limit).map(|l| entry(l)).collect();
    let serving = serving(mine.iter().map(|l| **l), since);
    json!({"people": summary, "entries": shown, "more": mine.len() > limit, "serving": serving})
}

/// Lookups and sessions counted together: how many, how many gave advice, and what the rest came to.
#[derive(Default)]
struct Tally {
    sessions: usize,
    /// Sessions served at least one note (their `served`, kept since sessions began).
    sessions_served: usize,
    /// Sessions in the holdout arm that had advice withheld.
    sessions_withheld: usize,
    lookups: usize,
    served: usize,
    outcomes: BTreeMap<String, usize>,
}

impl Tally {
    fn session(&mut self, l: &Logged) {
        let Some(s) = &l.session else { return };
        self.sessions += 1;
        self.sessions_served += usize::from(!s.served.is_empty());
        self.sessions_withheld += usize::from(!s.withheld.is_empty());
    }

    fn lookup(&mut self, o: &str) {
        self.lookups += 1;
        self.served += usize::from(crate::advice_lookups::outcome::served(o));
        *self.outcomes.entry(o.to_string()).or_default() += 1;
    }

    fn json(&self) -> Value {
        json!({"sessions": self.sessions, "sessions_served": self.sessions_served,
               "sessions_withheld": self.sessions_withheld, "lookups": self.lookups, "served": self.served,
               "outcomes": self.outcomes})
    }
}

/// When advice was served and when it wasn't, over `entries`' sessions and their lookups since `since` (µs): in sum,
/// by outcome, by moment, by caller (people, or programs: sessions a program ran, [`crate::sessions::automated`]),
/// by person and by harness; the notes served most, and the kinds of task most often matched with no advice to serve
/// or judged off-task. Sessions kept before lookups were recorded count in `sessions` and `sessions_served` only:
/// `lookups_since` says from when lookups are known.
fn serving<'a>(entries: impl Iterator<Item = &'a Logged>, since: i64) -> Value {
    use crate::advice_lookups::outcome;
    let (mut all, mut moments, mut callers) = (
        Tally::default(),
        BTreeMap::<String, Tally>::new(),
        BTreeMap::<&str, Tally>::new(),
    );
    let (mut people, mut harnesses) = (
        BTreeMap::<String, Tally>::new(),
        BTreeMap::<String, Tally>::new(),
    );
    let (mut top_served, mut no_advice, mut off_task) = (
        BTreeMap::<String, (usize, BTreeSet<&str>)>::new(),
        BTreeMap::<String, usize>::new(),
        BTreeMap::<String, usize>::new(),
    );
    let mut first: Option<i64> = None;
    for l in entries {
        let Some(s) = &l.session else { continue };
        let caller = if crate::sessions::automated(l) {
            "programs"
        } else {
            "people"
        };
        let harness = s.harness.clone().unwrap_or_else(|| "unknown".into());
        all.session(l);
        callers.entry(caller).or_default().session(l);
        people.entry(l.asker.clone()).or_default().session(l);
        harnesses.entry(harness.clone()).or_default().session(l);
        for k in &s.served {
            top_served
                .entry(k.clone())
                .or_default()
                .1
                .insert(l.answer_id.as_str());
        }
        for x in s.lookups.iter().filter(|x| x.at >= since) {
            first = Some(first.map_or(x.at, |f| f.min(x.at)));
            let o = x.outcome.as_str();
            all.lookup(o);
            moments.entry(x.moment.clone()).or_default().lookup(o);
            callers.entry(caller).or_default().lookup(o);
            people.entry(l.asker.clone()).or_default().lookup(o);
            harnesses.entry(harness.clone()).or_default().lookup(o);
            if let Some(k) = &x.key {
                match o {
                    outcome::SERVED | outcome::SERVED_OWN | outcome::SERVED_PROCEDURES => {
                        top_served.entry(k.clone()).or_default().0 += 1;
                    }
                    outcome::NO_ADVICE => *no_advice.entry(k.clone()).or_default() += 1,
                    outcome::OFF_TASK => *off_task.entry(k.clone()).or_default() += 1,
                    _ => {}
                }
            }
        }
    }
    let ranked = |m: BTreeMap<String, usize>| -> Vec<Value> {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter()
            .take(15)
            .map(|(key, n)| json!({"key": key, "lookups": n}))
            .collect()
    };
    let mut served: Vec<(String, usize, usize)> = top_served
        .into_iter()
        .map(|(k, (n, sessions))| (k, n, sessions.len()))
        .collect();
    served.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
    let by = |m: BTreeMap<String, Tally>, name: &str| -> Vec<Value> {
        let mut v: Vec<(String, Tally)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.sessions.cmp(&a.1.sessions).then(a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(k, t)| {
                let mut j = t.json();
                j[name] = json!(k);
                j
            })
            .collect()
    };
    let mut out = all.json();
    out["lookups_since"] = json!(first);
    out["by_moment"] = json!(
        moments
            .into_iter()
            .map(|(k, t)| (k, t.json()))
            .collect::<BTreeMap<_, _>>()
    );
    out["by_caller"] = json!(
        callers
            .into_iter()
            .map(|(k, t)| (k, t.json()))
            .collect::<BTreeMap<_, _>>()
    );
    out["by_person"] = json!(by(people, "user"));
    out["by_harness"] = json!(by(harnesses, "harness"));
    out["top_served"] = json!(
        served
            .into_iter()
            .take(15)
            .map(|(key, lookups, sessions)| json!({"key": key, "lookups": lookups, "sessions": sessions}))
            .collect::<Vec<_>>()
    );
    out["top_no_advice"] = json!(ranked(no_advice));
    out["top_off_task"] = json!(ranked(off_task));
    out
}

fn kind_of(l: &Logged) -> &'static str {
    match (&l.session, l.kind.as_deref()) {
        (Some(_), _) | (_, Some("session")) => "session",
        (_, Some("run")) => "run",
        _ => "answer",
    }
}

pub(crate) fn entry(l: &Logged) -> Value {
    let kind = kind_of(l);
    let mut v = json!({"id": l.answer_id, "kind": kind, "user": l.asker, "question": l.question,
                       "asked_at": l.asked_at, "updated_at": l.updated_at, "cited": l.cited,
                       "episodes": l.episodes, "episodes_hidden": 0,
                       "transcript": kind != "answer" && transcript_of(l).is_some()});
    if let Some(a) = &l.answer {
        let cut: String = a.chars().take(ANSWER_PREVIEW).collect();
        v["answer"] = json!(cut);
        v["answer_cut"] = json!(cut.len() < a.len());
    }
    for (k, x) in [
        ("goal", &l.goal),
        ("rating", &l.rating),
        ("reward_reason", &l.reward_reason),
    ] {
        if let Some(x) = x {
            v[k] = json!(x);
        }
    }
    if let Some(r) = l.reward {
        v["reward"] = json!(r);
    }
    if let Some(s) = &l.session {
        v["session"] = json!({"harness": s.harness, "arm": s.arm, "served": s.served,
                              "served_versions": s.served_versions, "withheld": s.withheld, "ended_at": s.ended_at, "ended_by": s.ended_by,
                              "fires": s.fires, "lookups": s.lookups, "prs": s.prs, "verdicts": s.verdicts.as_deref()
                                  .and_then(|v| serde_json::from_str::<Value>(v).ok())});
    }
    v
}

/// The episode holding `l`'s conversation: an open session's conversation so far, else the transcript an ended
/// session or a run kept, added after the episodes its cited facts come from (`sessions::end`,
/// `qa_log::log_runs`). Only ever an episode id (64 hex digits), never a source's.
fn transcript_of(l: &Logged) -> Option<&str> {
    l.session
        .as_ref()
        .and_then(|s| s.conversation.as_deref())
        .or_else(|| l.episodes.last().map(String::as_str))
        .filter(|e| e.len() == 64 && e.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The transcripts of `user`'s sessions and runs in `log`: the episodes holding what they said.
pub fn transcripts_of(log: &[Logged], user: &str) -> std::collections::HashSet<String> {
    let user = crate::people::directory::user_id(user);
    log.iter()
        .filter(|l| kind_of(l) != "answer" && crate::people::directory::user_id(&l.asker) == user)
        .filter_map(transcript_of)
        .map(str::to_string)
        .collect()
}

/// Entry `id` of `log` with its transcript (its end, past [`TRANSCRIPT_MAX`] characters), for an org's admin
/// (`/admin {op: "conversation"}`): read whatever its ACL, since the admins read their people's activity.
/// None: no such entry.
pub async fn transcript(
    store: &Store,
    scope: &Scope,
    log: &[Logged],
    id: &str,
) -> Result<Option<Value>> {
    let Some(l) = log.iter().find(|l| l.answer_id == id) else {
        return Ok(None);
    };
    let mut v = entry(l);
    let body = match transcript_of(l).filter(|_| kind_of(l) != "answer") {
        Some(e) => store
            .episode_bodies(scope, vec![e.to_string()])
            .await?
            .remove(e)
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        None => None,
    };
    if let Some(t) = body {
        let n = t.chars().count();
        let tail: String = t.chars().skip(n.saturating_sub(TRANSCRIPT_MAX)).collect();
        v["transcript_text"] = json!(tail);
        v["transcript_cut"] = json!(n > TRANSCRIPT_MAX);
    }
    Ok(Some(v))
}
