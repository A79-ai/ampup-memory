//! The weekly digest: one short email a week to each person who used the memory that week, so what it learned is
//! seen without opening anything. It says what the memory learned that week (each new or changed lesson the
//! person may read, and which of their sessions it came from), where advice was served to them and how those
//! sessions went, how many sessions they had and the share they rewarded, and one or two of the team's lessons,
//! each linking into the Explorer (`AMPUP_MEMORY_EXPLORER_URL`) for the detail.
//!
//! - **Who.** An org's members with an email address who had a session or a run that week (Monday to Sunday, UTC),
//!   less anyone disabled or unsubscribed. On a server that identifies no one, the people its sessions were
//!   recorded for (`x-memstore-as`). Nobody with no activity that week is mailed.
//! - **What they may read.** Everything in it is read as the person: their own sessions and runs, the notes of
//!   their own space and those of the team's map their access reads ([`crate::advice::listed`]). A teammate's
//!   session a team note was learned from is counted, never named.
//! - **When.** `serve` sends last week's digests from Monday at `AMPUP_MEMORY_DIGEST_HOUR` (14, UTC) through the
//!   rest of that week, from the server holding the writer lease only.
//! - **Exactly once.** A person's digest of a week is recorded in their org's answer log ([`KIND`], under their
//!   name, so forgetting them takes it) as `sending` before the provider is called and `sent` after. A recorded
//!   send is never made again; one left `sending` (the server stopped mid-send) or `failed` is retried with the same
//!   idempotency key, which the provider keeps a day, and given up after that.
//! - **Off.** Without a mail transport ([`crate::mail`]), `AMPUP_MEMORY_PUBLIC_URL` and `AMPUP_MEMORY_SECRET_KEY`
//!   (the unsubscribe link is sealed with it) nothing is sent: `serve --local` sends nothing by default.
//!   `AMPUP_MEMORY_DIGEST=off` turns it off for the server, the org setting `digest: false` for an org, and the
//!   link in every digest for one person (`/digest/unsubscribe`, which also subscribes again).
//! - **Preview.** `ampup-memory digest preview --person <email>` renders a person's digest without sending it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::secret::SecretKey;
use crate::mail::{self, Mailer};
use crate::qa_log::{Logged, StoreQaLog};
use crate::{MAIN, Principal, Scope, Session, Store};

/// The answer log's kind for the digest's own rows: a person's sends and their subscription.
pub const KIND: &str = "digest";
/// The path of the unsubscribe link, under the server's public URL.
pub const UNSUBSCRIBE_PATH: &str = "/digest/unsubscribe";
const TOKEN_CONTEXT: &str = "digest-unsubscribe";
const WEEK: i64 = 7 * 86_400_000_000;
/// A send left `sending` or `failed` is retried with its idempotency key for this long, then given up.
const RETRY_FOR: i64 = 22 * 3_600_000_000;
/// The most lessons, served sessions and team lessons a digest lists.
const MAX_LEARNED: usize = 5;
const MAX_SERVED: usize = 5;
const MAX_TEAM: usize = 2;

/// A week, Monday 00:00 UTC to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Week {
    /// Its start (µs).
    pub start: i64,
}

impl Week {
    /// The week `at` (µs) falls in.
    pub fn containing(at: i64) -> Week {
        let t = Utc.timestamp_micros(at).single().unwrap_or_else(Utc::now);
        let monday = t.date_naive() - chrono::Days::new(t.weekday().num_days_from_monday() as u64);
        let start = monday
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp_micros();
        Week { start }
    }

    pub fn end(&self) -> i64 {
        self.start + WEEK
    }

    pub fn previous(&self) -> Week {
        Week {
            start: self.start - WEEK,
        }
    }

    pub fn holds(&self, at: i64) -> bool {
        at >= self.start && at < self.end()
    }

    /// Its ISO name, `2026-W46`.
    pub fn label(&self) -> String {
        let d = Utc
            .timestamp_micros(self.start)
            .single()
            .unwrap_or_else(Utc::now);
        let w = d.iso_week();
        format!("{}-W{:02}", w.year(), w.week())
    }

    /// Its days, `Nov 9 – 15, 2026`.
    pub fn span(&self) -> String {
        let a = Utc
            .timestamp_micros(self.start)
            .single()
            .unwrap_or_else(Utc::now);
        let b = Utc
            .timestamp_micros(self.end() - 1)
            .single()
            .unwrap_or_else(Utc::now);
        if a.month() == b.month() {
            format!("{} – {}, {}", a.format("%b %-d"), b.format("%-d"), b.year())
        } else if a.year() == b.year() {
            format!(
                "{} – {}, {}",
                a.format("%b %-d"),
                b.format("%b %-d"),
                b.year()
            )
        } else {
            format!("{} – {}", a.format("%b %-d, %Y"), b.format("%b %-d, %Y"))
        }
    }

    /// `this`, `last`, an ISO week (`2026-W46`) or any day in it (`2026-11-12`), as of `now` (µs).
    pub fn parse(s: &str, now: i64) -> Result<Week> {
        let s = s.trim();
        match s {
            "this" => return Ok(Week::containing(now)),
            "last" | "" => return Ok(Week::containing(now).previous()),
            _ => {}
        }
        if let Some((y, w)) = s.split_once("-W") {
            let (y, w): (i32, u32) = (y.parse()?, w.parse()?);
            let d = chrono::NaiveDate::from_isoywd_opt(y, w, chrono::Weekday::Mon)
                .with_context(|| format!("no week {s}"))?;
            return Ok(Week::containing(
                d.and_hms_opt(0, 0, 0)
                    .expect("midnight")
                    .and_utc()
                    .timestamp_micros(),
            ));
        }
        let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| {
            format!("a week is this, last, 2026-W46 or a day in it (2026-11-12), not {s:?}")
        })?;
        Ok(Week::containing(
            d.and_hms_opt(12, 0, 0)
                .expect("noon")
                .and_utc()
                .timestamp_micros(),
        ))
    }
}

/// One scope of an org as a digest reads it: the scope pinned once, and its answer log.
pub struct Loaded {
    pub scope: Scope,
    /// The team it is, for a team's scope.
    pub team: Option<String>,
    session: Session,
    log: Vec<Logged>,
}

impl Loaded {
    pub async fn load(store: &Store, scope: &Scope, team: Option<String>) -> Result<Loaded> {
        Ok(Loaded {
            scope: scope.clone(),
            team,
            session: store.pin(scope, MAIN).await?,
            log: crate::sessions::list(store, scope).await?,
        })
    }

    /// Its answer log, as loaded.
    pub fn log(&self) -> &[Logged] {
        &self.log
    }
}

/// A person's week, as their digest shows it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Digest {
    pub person: String,
    /// The org, where the server keeps a directory.
    pub org: Option<String>,
    pub week: String,
    pub span: String,
    /// Their sessions that week, and how many of those they (or what became of their PRs) rewarded.
    pub sessions: usize,
    pub rewarded: usize,
    /// Of their sessions, those that went well and those that didn't, by their reward or the judge's.
    pub went_well: usize,
    pub went_badly: usize,
    /// Their recorded runs that week.
    pub runs: usize,
    /// Their sessions advice was served in, newest first.
    pub served: Vec<Served>,
    /// Lessons written or rewritten that week that they may read, newest first.
    pub learned: Vec<Lesson>,
    /// One or two of the team's lessons.
    pub team: Vec<Lesson>,
}

impl Digest {
    /// Whether they used the memory that week: a digest goes only to those who did.
    pub fn active(&self) -> bool {
        self.sessions + self.runs > 0
    }
}

/// A session of theirs advice was served in.
#[derive(Clone, Debug, Serialize)]
pub struct Served {
    pub team: Option<String>,
    pub id: String,
    /// What they asked for, cut short.
    pub task: String,
    pub at: i64,
    /// The names of the notes served.
    pub notes: Vec<String>,
    /// `went well`, `didn't go well`, `unclear` or `not judged yet`.
    pub outcome: String,
    /// The notes the judge found helped.
    pub helped: usize,
    /// An episode of the session's own, to open it by.
    pub episode: Option<String>,
}

/// A note of advice: a lesson.
#[derive(Clone, Debug, Serialize)]
pub struct Lesson {
    pub team: Option<String>,
    /// As sessions name it (`mine:`-tagged for the person's own).
    pub key: String,
    /// The person's own, or the team's.
    pub mine: bool,
    pub situation: String,
    /// When it applies.
    pub when: String,
    /// When its newest version was written (µs), and whether a version came before the week.
    pub at: i64,
    pub changed: bool,
    pub note_id: Option<String>,
    /// Its first step (else its first pitfall).
    pub first: Option<String>,
    /// The person's own sessions and runs it was learned from, and how many of other people's.
    pub from_yours: Vec<Source>,
    pub from_others: usize,
    /// Judged sessions it was served in, and those it helped (the team's notes; a person's own carry none).
    pub served: usize,
    pub helped: usize,
}

/// One of the person's sessions or runs a lesson was learned from.
#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    /// A session's, or a recorded run's.
    pub session: bool,
    pub task: String,
    pub episode: Option<String>,
}

/// `s` cut to `n` characters at a word, one line.
fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    let cut: String = one.chars().take(n).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a);
    format!("{}…", cut.trim_end_matches([',', '.', ';', ':']))
}

/// An item's text without the kind its writer tagged it with (`[how-to] Create …` → `Create …`).
fn untagged(t: &str) -> &str {
    match t.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((tag, rest)) if !tag.contains(' ') || tag.len() < 20 => rest,
        _ => t,
    }
}

/// Whether two phrases hold the same words, ignoring case and punctuation.
fn same_words(a: &str, b: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    words(a) == words(b)
}

/// A situation's key as a title: `deploy-billing` → `Deploy billing`.
fn title(situation: &str) -> String {
    let s = situation.replace(['-', '_'], " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

fn is_session(l: &Logged) -> bool {
    l.kind.as_deref() == Some(crate::sessions::KIND)
}

/// The person's own sessions and runs of the scope's log that count as their activity.
fn theirs<'a>(log: &'a [Logged], who: &'a Principal) -> impl Iterator<Item = &'a Logged> + 'a {
    let me = crate::auth::acl::norm_id(who.user());
    log.iter().filter(move |l| {
        crate::auth::acl::norm_id(&l.asker) == me
            && (is_session(l) || l.kind.as_deref() == Some("run"))
            && !crate::sessions::automated(l)
            && !crate::sessions::discarded(l)
    })
}

fn verdicts(l: &Logged) -> Option<Value> {
    l.session
        .as_ref()?
        .verdicts
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
}

/// How a session or run went: its reward where it has one (theirs, its PRs' or the judge's), else the judge's
/// outcome. Some(true) went well, Some(false) didn't, None unclear or not known yet.
fn went_well(l: &Logged) -> Option<bool> {
    if let Some(r) = l.reward {
        return (r != 0.0).then_some(r > 0.0);
    }
    if let Some(c) = crate::sessions::checked_outcome(l) {
        return Some(c);
    }
    match verdicts(l)?["outcome"].as_str() {
        Some("succeeded") => Some(true),
        Some("failed") => Some(false),
        _ => None,
    }
}

/// The lessons of `loaded` that `who` may read whose newest version was written in `[since, until)`: their own and
/// the team's, each with which of their sessions and runs it was learned from. Read through the Explorer's lessons
/// query ([`crate::lessons`], `POST /lessons`) as `who`, so a teammate's run or session is counted, never named.
pub async fn lessons_learned(
    store: &Store,
    loaded: &Loaded,
    who: &Principal,
    since: i64,
    until: i64,
) -> Result<Vec<Lesson>> {
    let v = lessons(
        store,
        loaded,
        who,
        json!({"space": "all", "since": since, "limit": 50}),
    )
    .await?;
    Ok(v.iter()
        .filter(|l| l["learned_at"].as_i64().is_some_and(|at| at < until))
        .map(|l| lesson(loaded, who, l))
        .collect())
}

/// The Explorer's lessons of `loaded` as `who` reads them (`crate::lessons::run`, never as anyone's admin).
async fn lessons(
    store: &Store,
    loaded: &Loaded,
    who: &Principal,
    req: Value,
) -> Result<Vec<Value>> {
    let reader = crate::browse::Reader {
        who,
        every_session: false,
    };
    let v = Box::pin(crate::lessons::run(store, &loaded.session, reader, &req)).await?;
    Ok(v["lessons"].as_array().cloned().unwrap_or_default())
}

/// A lesson as `/lessons` answers it, as the digest shows it.
fn lesson(loaded: &Loaded, who: &Principal, v: &Value) -> Lesson {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let key = s("key");
    let items = crate::advice::note_items(v["lesson"].as_str().unwrap_or_default());
    let first = items
        .iter()
        .find(|(s, _)| s == "Steps")
        .or_else(|| items.first())
        .map(|(_, t)| short(untagged(t), 200));
    // One line per task of theirs (runs often repeat a task word for word), newest first as listed.
    let mut from_yours: Vec<Source> = vec![];
    for e in v["learned_from"]["trail"].as_array().into_iter().flatten() {
        let task = short(e["question"].as_str().unwrap_or_default(), 90);
        if task.is_empty() || from_yours.iter().any(|y| y.task == task) {
            continue;
        }
        let id = e["id"].as_str().unwrap_or_default().to_string();
        from_yours.push(Source {
            episode: loaded
                .log
                .iter()
                .find(|l| l.answer_id == id)
                .and_then(|l| l.episodes.first().cloned()),
            session: e["kind"] == "session",
            id,
            task,
        });
    }
    let at = v["learned_at"].as_i64().unwrap_or_default();
    let (situation, when) = (s("title"), s("when"));
    Lesson {
        team: loaded.team.clone(),
        mine: v["space"] == "mine",
        // A version they could read before this one's week: rewritten, not new.
        changed: loaded
            .session
            .version_number(
                who,
                &crate::advice::Space::served(who.user(), &key),
                Some(Week::containing(at).start - 1),
            )
            .is_some(),
        key,
        // Said only where it says more than the title.
        when: if same_words(&when, &situation) || same_words(&when, &s("situation")) {
            String::new()
        } else {
            short(&when, 160)
        },
        situation: title(&situation),
        at,
        note_id: v["note_id"].as_str().map(String::from),
        first,
        from_yours,
        from_others: v["learned_from"]["hidden"].as_u64().unwrap_or_default() as usize,
        served: v["served_in"]["sessions"].as_u64().unwrap_or_default() as usize,
        helped: v["served_in"]["helped"].as_u64().unwrap_or_default() as usize,
    }
}

/// One or two of the team's lessons `who` may read and that are served: those that helped the most people, else
/// the newest. None of `skip`.
async fn team_lessons(
    store: &Store,
    loaded: &Loaded,
    who: &Principal,
    skip: &BTreeSet<String>,
    n: usize,
) -> Result<Vec<Lesson>> {
    let mut all: Vec<Lesson> = lessons(store, loaded, who, json!({"space": "team", "limit": 1}))
        .await?
        .iter()
        .filter(|v| v["status"] == "served")
        .map(|v| lesson(loaded, who, v))
        .filter(|l| !skip.contains(&l.key))
        .collect();
    all.sort_by_key(|l| (std::cmp::Reverse(l.helped), std::cmp::Reverse(l.at)));
    all.truncate(n);
    Ok(all)
}

/// A person's digest of `week`, read as `who` from the scopes of their org they may read.
pub async fn gather(
    store: &Store,
    org: Option<&str>,
    scopes: &[&Loaded],
    who: &Principal,
    week: Week,
) -> Result<Digest> {
    let mut d = Digest {
        person: who.user().to_string(),
        org: org.map(String::from),
        week: week.label(),
        span: week.span(),
        ..Default::default()
    };
    for loaded in scopes {
        for l in theirs(&loaded.log, who).filter(|l| week.holds(l.asked_at)) {
            if !is_session(l) {
                d.runs += 1;
                continue;
            }
            d.sessions += 1;
            if crate::advice::given(l) {
                d.rewarded += 1;
            }
            match went_well(l) {
                Some(true) => d.went_well += 1,
                Some(false) => d.went_badly += 1,
                None => {}
            }
            let Some(s) = l.session.as_ref().filter(|s| !s.served.is_empty()) else {
                continue;
            };
            let v = verdicts(l);
            let helped = s
                .served
                .iter()
                .filter(|k| {
                    v.as_ref()
                        .is_some_and(|v| v["notes"][k.as_str()]["helped"].as_bool() == Some(true))
                })
                .count();
            d.served.push(Served {
                team: loaded.team.clone(),
                id: l.answer_id.clone(),
                task: short(&l.question, 90),
                at: l.asked_at,
                // Keys for now; named below, as the lessons shown name them.
                notes: s.served.clone(),
                outcome: match went_well(l) {
                    Some(true) => "went well",
                    Some(false) => "didn't go well",
                    None if s.ended_at.is_some() && v.is_some() => "unclear",
                    None => "not judged yet",
                }
                .into(),
                helped,
                episode: l.episodes.first().cloned(),
            });
        }
        d.learned
            .extend(lessons_learned(store, loaded, who, week.start, week.end()).await?);
    }
    d.served.sort_by_key(|s| std::cmp::Reverse(s.at));
    d.served.truncate(MAX_SERVED);
    d.learned.sort_by_key(|l| std::cmp::Reverse(l.at));
    d.learned.truncate(MAX_LEARNED);
    let shown: BTreeSet<String> = d.learned.iter().map(|l| l.key.clone()).collect();
    for loaded in scopes {
        if d.team.len() >= MAX_TEAM {
            break;
        }
        let more = team_lessons(store, loaded, who, &shown, MAX_TEAM - d.team.len()).await?;
        d.team.extend(more);
    }
    let names: BTreeMap<String, String> = (d.learned.iter().chain(&d.team))
        .map(|l| (l.key.clone(), l.situation.clone()))
        .collect();
    for s in &mut d.served {
        for k in &mut s.notes {
            *k = names
                .get(k.as_str())
                .cloned()
                .unwrap_or_else(|| title(k.rsplit('/').next().unwrap_or(k)));
        }
    }
    Ok(d)
}

/// Where a digest's links go.
#[derive(Clone, Debug, Default)]
pub struct Links {
    /// The Explorer, `{org}` standing for the org (`AMPUP_MEMORY_EXPLORER_URL`); no links without it.
    pub explorer: Option<String>,
    /// The person's unsubscribe link.
    pub unsubscribe: Option<String>,
}

impl Links {
    pub fn from_env() -> Links {
        Links {
            explorer: std::env::var("AMPUP_MEMORY_EXPLORER_URL")
                .ok()
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
            unsubscribe: None,
        }
    }

    fn base(&self, org: Option<&str>, team: Option<&str>) -> Option<String> {
        let e = self.explorer.as_ref()?;
        let mut u = e.replace("{org}", &enc(org.unwrap_or("")));
        if let Some(t) = team {
            u.push_str(if u.contains('?') { "&" } else { "?" });
            u.push_str(&format!("team={}", enc(t)));
        }
        Some(u)
    }

    /// The Explorer, on the memory.
    pub fn home(&self, org: Option<&str>) -> Option<String> {
        self.base(org, None)
    }

    /// The Explorer, open on an episode.
    pub fn episode(&self, org: Option<&str>, team: Option<&str>, id: &str) -> Option<String> {
        Some(format!("{}#/x/{}", self.base(org, team)?, enc(id)))
    }
}

/// A digest as mailed.
#[derive(Clone, Debug, Serialize)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// `s` as a URL's path or query part carries it.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The subject line: what it learned first, since that is the news.
fn subject(d: &Digest) -> String {
    let learned = match d.learned.len() {
        0 => String::new(),
        n => format!("{}, ", plural(n, "new lesson", "new lessons")),
    };
    format!(
        "Your week with AmpUp Memory: {learned}{} ({})",
        plural(d.sessions, "session", "sessions"),
        d.span
    )
}

fn pct(a: usize, b: usize) -> String {
    match (a * 100 + b / 2).checked_div(b) {
        Some(p) => format!("{p}%"),
        None => "0%".into(),
    }
}

/// The digest's text and HTML parts. The HTML is what mail clients render: tables, inline styles, no script, no
/// images.
pub fn render(d: &Digest, links: &Links) -> Rendered {
    let org = d.org.as_deref();
    let e = mail::esc;
    // Text.
    let mut t = String::new();
    t.push_str(&format!("Your week with AmpUp Memory, {}\n", d.span));
    if let Some(o) = org {
        t.push_str(&format!("Org: {o}\n"));
    }
    t.push('\n');
    t.push_str(&format!(
        "{} · {} rewarded ({}) · {} went well\n",
        plural(d.sessions, "session", "sessions"),
        d.rewarded,
        pct(d.rewarded, d.sessions),
        d.went_well
    ));
    if d.runs > 0 {
        t.push_str(&format!("{} recorded\n", plural(d.runs, "run", "runs")));
    }
    let from_line = |l: &Lesson| -> String {
        let mut parts: Vec<String> = l
            .from_yours
            .iter()
            .take(2)
            .map(|s| {
                format!(
                    "your {} \"{}\"",
                    if s.session { "session" } else { "run" },
                    s.task
                )
            })
            .collect();
        if l.from_yours.len() > 2 {
            parts.push(format!("{} more of yours", l.from_yours.len() - 2));
        }
        if l.from_others > 0 {
            parts.push(format!("{} from teammates", l.from_others));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("Learned from {}", parts.join(", "))
        }
    };
    t.push_str("\nWHAT IT LEARNED THIS WEEK\n");
    if d.learned.is_empty() {
        t.push_str("No new lessons this week. Rewarding your sessions is what teaches it.\n");
    }
    for l in &d.learned {
        t.push_str(&format!(
            "\n- {} ({}, {})\n",
            l.situation,
            if l.changed { "updated" } else { "new" },
            if l.mine { "yours" } else { "team" }
        ));
        if !l.when.is_empty() {
            t.push_str(&format!("  When: {}\n", l.when));
        }
        if let Some(f) = &l.first {
            t.push_str(&format!("  First step: {f}\n"));
        }
        let f = from_line(l);
        if !f.is_empty() {
            t.push_str(&format!("  {f}\n"));
        }
        if let Some(u) = l
            .note_id
            .as_deref()
            .and_then(|id| links.episode(org, l.team.as_deref(), id))
        {
            t.push_str(&format!("  {u}\n"));
        }
    }
    t.push_str("\nWHERE ADVICE WAS SERVED\n");
    if d.served.is_empty() {
        t.push_str("None of your sessions was served advice this week.\n");
    }
    for s in &d.served {
        t.push_str(&format!(
            "\n- \"{}\": {} ({}{})\n",
            s.task,
            s.notes.join(", "),
            s.outcome,
            if s.helped > 0 {
                format!(", {} helped", plural(s.helped, "note", "notes"))
            } else {
                String::new()
            }
        ));
        if let Some(u) = s
            .episode
            .as_deref()
            .and_then(|id| links.episode(org, s.team.as_deref(), id))
        {
            t.push_str(&format!("  {u}\n"));
        }
    }
    if !d.team.is_empty() {
        t.push_str("\nFROM YOUR TEAM\n");
        for l in &d.team {
            t.push_str(&format!("\n- {}\n", l.situation));
            if let Some(f) = &l.first {
                t.push_str(&format!("  {f}\n"));
            }
            if l.helped > 0 {
                t.push_str(&format!(
                    "  Helped in {}\n",
                    plural(l.helped, "session", "sessions")
                ));
            }
            if let Some(u) = l
                .note_id
                .as_deref()
                .and_then(|id| links.episode(org, l.team.as_deref(), id))
            {
                t.push_str(&format!("  {u}\n"));
            }
        }
    }
    if let Some(u) = links.home(org) {
        t.push_str(&format!("\nEverything in the Explorer: {u}\n"));
    }
    t.push_str(&format!(
        "\nYou get this because you used AmpUp Memory this week ({}).",
        d.person
    ));
    match &links.unsubscribe {
        Some(u) => t.push_str(&format!(" Unsubscribe: {u}\n")),
        None => t.push('\n'),
    }

    // HTML.
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif";
    let ink = "#18181b";
    let muted = "#71717a";
    let line = "#e4e4e7";
    let link = |u: &str, label: &str| {
        format!(
            "<a href=\"{}\" style=\"color:#2563eb;text-decoration:none\">{}</a>",
            e(u),
            e(label)
        )
    };
    let h2 = |s: &str| {
        format!(
            "<tr><td style=\"padding:28px 0 8px;font:600 13px/1.4 {font};letter-spacing:.06em;text-transform:uppercase;color:{muted}\">{}</td></tr>",
            e(s)
        )
    };
    let note = |s: &str| {
        format!(
            "<tr><td style=\"padding:4px 0 0;font:14px/1.5 {font};color:{muted}\">{}</td></tr>",
            e(s)
        )
    };
    let badge = |s: &str, fg: &str, bg: &str| {
        format!(
            "<span style=\"display:inline-block;padding:1px 7px;margin-left:6px;border-radius:9px;background:{bg};color:{fg};font:600 11px/1.6 {font};vertical-align:1px\">{}</span>",
            e(s)
        )
    };
    let mut rows = String::new();
    // Header.
    rows.push_str(&format!(
        "<tr><td style=\"padding:0 0 4px;font:600 13px/1.4 {font};color:{muted}\">AmpUp Memory{}</td></tr>",
        org.map(|o| format!(" · {}", e(o))).unwrap_or_default()
    ));
    rows.push_str(&format!(
        "<tr><td style=\"padding:0 0 18px;font:600 22px/1.3 {font};color:{ink}\">Your week, {}</td></tr>",
        e(&d.span)
    ));
    // Numbers.
    let stat = |n: String, label: &str| {
        format!(
            "<td width=\"33%\" style=\"padding:12px 14px;border:1px solid {line};border-radius:8px;background:#fafafa\"><div style=\"font:600 22px/1.2 {font};color:{ink}\">{}</div><div style=\"font:13px/1.4 {font};color:{muted}\">{}</div></td>",
            e(&n),
            e(label)
        )
    };
    let gap = "<td width=\"8\" style=\"font-size:0;line-height:0\">&nbsp;</td>";
    rows.push_str(&format!(
        "<tr><td><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"border-collapse:separate\"><tr>{}{gap}{}{gap}{}</tr></table></td></tr>",
        stat(d.sessions.to_string(), if d.sessions == 1 { "session" } else { "sessions" }),
        stat(
            pct(d.rewarded, d.sessions),
            &format!("rewarded ({} of {})", d.rewarded, d.sessions)
        ),
        stat(d.learned.len().to_string(), if d.learned.len() == 1 { "new lesson" } else { "new lessons" }),
    ));
    if d.runs > 0 {
        rows.push_str(&note(&format!(
            "Also {} recorded.",
            plural(d.runs, "run", "runs")
        )));
    }
    // What it learned.
    rows.push_str(&h2("What it learned this week"));
    if d.learned.is_empty() {
        rows.push_str(&note(
            "No new lessons this week. Rewarding your sessions is what teaches it.",
        ));
    }
    let lesson_rows = |l: &Lesson, with_badges: bool| -> String {
        let mut r = String::new();
        let open = l
            .note_id
            .as_deref()
            .and_then(|id| links.episode(org, l.team.as_deref(), id));
        let name = match &open {
            Some(u) => format!(
                "<a href=\"{}\" style=\"color:{ink};text-decoration:none\">{}</a>",
                e(u),
                e(&l.situation)
            ),
            None => e(&l.situation),
        };
        let badges = if with_badges {
            format!(
                "{}{}",
                if l.changed {
                    badge("updated", "#92400e", "#fef3c7")
                } else {
                    badge("new", "#166534", "#dcfce7")
                },
                if l.mine {
                    badge("yours", "#1e40af", "#dbeafe")
                } else {
                    badge("team", "#3f3f46", "#f4f4f5")
                }
            )
        } else {
            String::new()
        };
        r.push_str(&format!(
            "<div style=\"font:600 15px/1.4 {font};color:{ink}\">{name}{badges}</div>"
        ));
        if !l.when.is_empty() {
            r.push_str(&format!(
                "<div style=\"padding-top:2px;font:14px/1.5 {font};color:{muted}\">{}</div>",
                e(&l.when)
            ));
        }
        if let Some(f) = &l.first {
            r.push_str(&format!(
                "<div style=\"margin-top:8px;padding:8px 12px;border-left:3px solid #a5b4fc;background:#f5f7ff;font:14px/1.5 {font};color:{ink}\">{}</div>",
                e(f)
            ));
        }
        let mut meta: Vec<String> = vec![];
        if with_badges {
            let f = from_line(l);
            if !f.is_empty() {
                meta.push(e(&f));
            }
        } else if l.helped > 0 {
            meta.push(format!(
                "Helped in {}",
                e(&plural(l.helped, "session", "sessions"))
            ));
        }
        if let Some(u) = &open {
            meta.push(link(u, "Open in the Explorer"));
        }
        if !meta.is_empty() {
            r.push_str(&format!(
                "<div style=\"padding-top:6px;font:13px/1.5 {font};color:{muted}\">{}</div>",
                meta.join(" · ")
            ));
        }
        format!("<tr><td style=\"padding:12px 0;border-top:1px solid {line}\">{r}</td></tr>")
    };
    for l in &d.learned {
        rows.push_str(&lesson_rows(l, true));
    }
    // Where advice was served.
    rows.push_str(&h2("Where advice was served"));
    if d.served.is_empty() {
        rows.push_str(&note("None of your sessions was served advice this week."));
    }
    for s in &d.served {
        let (dot, color) = match s.outcome.as_str() {
            "went well" => ("●", "#16a34a"),
            "didn't go well" => ("●", "#dc2626"),
            _ => ("○", "#a1a1aa"),
        };
        let open = s
            .episode
            .as_deref()
            .and_then(|id| links.episode(org, s.team.as_deref(), id));
        let task = match &open {
            Some(u) => format!(
                "<a href=\"{}\" style=\"color:{ink};text-decoration:none\">{}</a>",
                e(u),
                e(&s.task)
            ),
            None => e(&s.task),
        };
        let helped = if s.helped > 0 {
            format!(" · {} helped", e(&plural(s.helped, "note", "notes")))
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td style=\"padding:10px 0;border-top:1px solid {line}\"><div style=\"font:14px/1.5 {font};color:{ink}\">{task}</div><div style=\"padding-top:2px;font:13px/1.5 {font};color:{muted}\">Served: {} · <span style=\"color:{color}\">{dot}</span> {}{helped}</div></td></tr>",
            e(&s.notes.join(", ")),
            e(&s.outcome)
        ));
    }
    // From the team.
    if !d.team.is_empty() {
        rows.push_str(&h2("From your team"));
        for l in &d.team {
            rows.push_str(&lesson_rows(l, false));
        }
    }
    // Footer.
    let mut foot = format!(
        "You get this because you used AmpUp Memory this week as {}.",
        e(&d.person)
    );
    if let Some(u) = links.home(org) {
        foot.push_str(&format!(" {}.", link(&u, "Open the Explorer")));
    }
    if let Some(u) = &links.unsubscribe {
        foot.push_str(&format!(" {}.", link(u, "Unsubscribe")));
    }
    rows.push_str(&format!(
        "<tr><td style=\"padding:28px 0 0;border-top:1px solid {line};font:12px/1.6 {font};color:{muted}\">{foot}</td></tr>"
    ));
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title></head><body style=\"margin:0;padding:0;background:#f4f4f5\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f4f4f5\"><tr><td align=\"center\" style=\"padding:24px 12px\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:600px;background:#ffffff;border:1px solid {line};border-radius:10px\"><tr><td style=\"padding:28px 28px 24px\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">{rows}</table></td></tr></table></td></tr></table></body></html>",
        e(&subject(d))
    );
    Rendered {
        subject: subject(d),
        text: t,
        html,
    }
}

// ---- Who wants it, and what was sent ----

/// The answer id of a person's subscription row.
fn pref_id(person: &str) -> String {
    format!("{KIND}:subscription:{}", crate::auth::acl::norm_id(person))
}

/// The answer id of a person's send of a week.
fn sent_id(person: &str, week: Week) -> String {
    format!(
        "{KIND}:{}:{}",
        week.label(),
        crate::auth::acl::norm_id(person)
    )
}

fn row<'a>(log: &'a [Logged], id: &str) -> Option<&'a Logged> {
    log.iter()
        .find(|l| l.answer_id == id && l.kind.as_deref() == Some(KIND))
}

/// Whether `person` unsubscribed, by the newest row of their subscription in their org's log.
pub fn unsubscribed(log: &[Logged], person: &str) -> bool {
    row(log, &pref_id(person)).is_some_and(|l| l.answer.as_deref() == Some("off"))
}

/// Unsubscribes `person` from their digest of `scope` (their org's), or subscribes them again.
pub async fn subscribe(store: &Store, scope: &Scope, person: &str, on: bool) -> Result<()> {
    let person = crate::auth::acl::norm_id(person);
    store
        .write_qa(
            scope,
            &person,
            &Logged {
                answer_id: pref_id(&person),
                asker: person.clone(),
                question: "weekly digest".into(),
                answer: Some(if on { "on" } else { "off" }.into()),
                asked_at: crate::now_micros(),
                kind: Some(KIND.into()),
                ..Default::default()
            },
        )
        .await
}

/// A send as recorded: `sending`, `sent`, `failed` or `given_up`, and when the first attempt began.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Mark {
    status: String,
    first: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn mark_of(log: &[Logged], person: &str, week: Week) -> Option<Mark> {
    row(log, &sent_id(person, week))
        .and_then(|l| l.answer.as_deref())
        .and_then(|a| serde_json::from_str(a).ok())
}

async fn mark(store: &Store, scope: &Scope, person: &str, week: Week, m: &Mark) -> Result<()> {
    let person = crate::auth::acl::norm_id(person);
    store
        .write_qa(
            scope,
            &person,
            &Logged {
                answer_id: sent_id(&person, week),
                asker: person.clone(),
                question: format!("weekly digest {}", week.label()),
                answer: Some(serde_json::to_string(m)?),
                asked_at: m.first,
                kind: Some(KIND.into()),
                ..Default::default()
            },
        )
        .await
}

/// What an unsubscribe link carries: the org (none on a server that identifies no one) and the person.
#[derive(Serialize, Deserialize)]
pub struct Unsubscribe {
    pub org: Option<String>,
    pub user: String,
}

/// A sealed token as a URL carries it (base64's `+/=` would need escaping).
fn to_url(sealed: &str) -> String {
    sealed
        .trim_start_matches("sealed:")
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

fn from_url(t: &str) -> String {
    let mut s = t.replace('-', "+").replace('_', "/");
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    format!("sealed:{s}")
}

/// The link that unsubscribes `user` from `org`'s digest. It doesn't expire: an old digest's link still works.
pub fn unsubscribe_link(key: &SecretKey, base: &str, org: Option<&str>, user: &str) -> String {
    let u = Unsubscribe {
        org: org.map(String::from),
        user: crate::auth::acl::norm_id(user),
    };
    let sealed = key.seal(
        TOKEN_CONTEXT,
        &serde_json::to_string(&u).expect("serializes"),
    );
    format!(
        "{}{UNSUBSCRIBE_PATH}?t={}",
        base.trim_end_matches('/'),
        to_url(&sealed)
    )
}

/// What an unsubscribe link says, if it is one a server with this key made.
pub fn open_link(key: &SecretKey, t: &str) -> Result<Unsubscribe> {
    Ok(serde_json::from_str(
        &key.unseal(TOKEN_CONTEXT, &from_url(t))?,
    )?)
}

// ---- Sending ----

/// Whether `AMPUP_MEMORY_DIGEST` leaves the digest on (it is, unless `off`).
pub fn on_by_env() -> bool {
    !std::env::var("AMPUP_MEMORY_DIGEST").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false"
        )
    })
}

/// The hour of Monday (UTC) from which last week's digests are sent: `AMPUP_MEMORY_DIGEST_HOUR`, default 14.
pub fn hour() -> i64 {
    std::env::var("AMPUP_MEMORY_DIGEST_HOUR")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|h| (0..24).contains(h))
        .unwrap_or(14)
}

/// The week whose digests are due at `now` (µs): last week's, from Monday at [`hour`] on.
pub fn due(now: i64) -> Option<Week> {
    let this = Week::containing(now);
    (now >= this.start + hour() * 3_600_000_000).then(|| this.previous())
}

/// Everything a send needs: the mail transport, the key sealing unsubscribe links and the public URL they open.
pub struct Sender {
    pub mailer: Mailer,
    pub key: SecretKey,
    pub public_url: String,
    pub links: Links,
}

impl Sender {
    /// The sender the environment configures, or why there is none.
    pub fn from_env() -> std::result::Result<Sender, String> {
        if !on_by_env() {
            return Err("AMPUP_MEMORY_DIGEST is off".into());
        }
        let mailer = mail::from_env().ok_or(
            "no mail is configured (AMPUP_MEMORY_MAIL_FROM with RESEND_API_KEY or MAILERSEND_API_KEY)",
        )?;
        let public_url = std::env::var("AMPUP_MEMORY_PUBLIC_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .ok_or("no AMPUP_MEMORY_PUBLIC_URL for the unsubscribe link to open")?;
        let key = SecretKey::from_env()
            .map_err(|e| format!("AMPUP_MEMORY_SECRET_KEY: {e:#}"))?
            .ok_or("no AMPUP_MEMORY_SECRET_KEY to seal the unsubscribe link with")?;
        Ok(Sender {
            mailer,
            key,
            public_url,
            links: Links::from_env(),
        })
    }
}

/// One org's digests to send: where its sends and subscriptions are kept (its org scope, or the served scope on a
/// server that identifies no one), its scopes, and who may get one.
pub struct Org {
    pub org: Option<String>,
    pub home: Scope,
    pub scopes: Vec<Loaded>,
    /// Each candidate with the scopes of `scopes` (by index) they may read; whoever had no activity is skipped.
    pub people: Vec<(String, Vec<usize>)>,
}

/// What one org's round did.
#[derive(Debug, Default, Serialize)]
pub struct Round {
    pub sent: usize,
    pub inactive: usize,
    pub unsubscribed: usize,
    pub already: usize,
    pub failed: usize,
    pub given_up: usize,
}

/// The people a server that identifies no one sends to: those its sessions and runs of `week` were recorded for
/// with an email address.
pub fn askers(log: &[Logged], week: Week) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for l in log {
        if (is_session(l) || l.kind.as_deref() == Some("run"))
            && week.holds(l.asked_at)
            && l.asker.contains('@')
            && crate::auth::acl::is_acl_id(&l.asker)
        {
            out.insert(crate::auth::acl::norm_id(&l.asker));
        }
    }
    out.into_iter().collect()
}

/// Sends `org`'s digests of `week`: each active, subscribed person's, once. `reader` is who a person reads as;
/// `holds` whether this process may still send (the writer lease), checked before each send.
pub async fn send_org(
    store: &Store,
    sender: &Sender,
    org: &Org,
    week: Week,
    reader: &(dyn Fn(&str) -> Principal + Sync),
    holds: &(dyn Fn() -> bool + Sync),
) -> Result<Round> {
    let mut round = Round::default();
    // The org's own log, read fresh, holds its sends and subscriptions.
    let home = crate::qa_log::list(store, &org.home, None).await?;
    let now = crate::now_micros();
    for (person, readable) in &org.people {
        if unsubscribed(&home, person) {
            round.unsubscribed += 1;
            continue;
        }
        let was = mark_of(&home, person, week);
        match &was {
            Some(m) if m.status == "sent" || m.status == "given_up" => {
                round.already += 1;
                continue;
            }
            Some(m) if now - m.first > RETRY_FOR => {
                // Past the provider's memory of the key: a retry could send it twice. It stays unsent.
                mark(
                    store,
                    &org.home,
                    person,
                    week,
                    &Mark {
                        status: "given_up".into(),
                        ..m.clone()
                    },
                )
                .await?;
                eprintln!(
                    "digest {}: {person}'s send of {} given up, unconfirmed since {}",
                    org.home,
                    week.label(),
                    m.first
                );
                round.given_up += 1;
                continue;
            }
            _ => {}
        }
        let who = reader(person);
        let scopes: Vec<&Loaded> = readable.iter().filter_map(|i| org.scopes.get(*i)).collect();
        let d = gather(store, org.org.as_deref(), &scopes, &who, week).await?;
        if !d.active() {
            round.inactive += 1;
            continue;
        }
        if !holds() {
            anyhow::bail!("the writer lease moved: the next holder sends the rest");
        }
        let first = was.as_ref().map_or(now, |m| m.first);
        mark(
            store,
            &org.home,
            person,
            week,
            &Mark {
                status: "sending".into(),
                first,
                error: None,
            },
        )
        .await?;
        let unsubscribe =
            unsubscribe_link(&sender.key, &sender.public_url, org.org.as_deref(), person);
        let links = Links {
            unsubscribe: Some(unsubscribe.clone()),
            ..sender.links.clone()
        };
        let r = render(&d, &links);
        let extra = mail::Extra {
            idempotency_key: Some(idempotency_key(&org.home, person, week)),
            headers: vec![
                ("List-Unsubscribe".into(), json!(format!("<{unsubscribe}>"))),
                (
                    "List-Unsubscribe-Post".into(),
                    json!("List-Unsubscribe=One-Click"),
                ),
            ],
        };
        let sent = sender
            .mailer
            .send_with(person, &r.subject, &r.text, &r.html, &extra)
            .await;
        let (status, error) = match &sent {
            Ok(()) => ("sent", None),
            Err(e) => ("failed", Some(format!("{e:#}"))),
        };
        mark(
            store,
            &org.home,
            person,
            week,
            &Mark {
                status: status.into(),
                first,
                error: error.clone(),
            },
        )
        .await?;
        match error {
            None => {
                eprintln!(
                    "digest {}: sent {person} {} by {}",
                    org.home,
                    week.label(),
                    sender.mailer.provider()
                );
                round.sent += 1;
            }
            Some(e) => {
                eprintln!(
                    "digest {}: {person}'s {} failed, retried later: {e}",
                    org.home,
                    week.label()
                );
                round.failed += 1;
            }
        }
    }
    Ok(round)
}

/// The same for every attempt at one person's digest of one week, and no other.
fn idempotency_key(home: &Scope, person: &str, week: Week) -> String {
    use sha2::Digest as _;
    let h = sha2::Sha256::digest(format!(
        "{}\0{}\0{}",
        home.namespace(),
        crate::auth::acl::norm_id(person),
        week.label()
    ));
    format!("digest-{}", crate::auth::hex(&h[..16]))
}

/// The scopes of `org` a person reads, by the directory: the org's, and each team's they are a member of.
pub fn readable(
    dir: &crate::people::directory::Directory,
    org: &str,
    person: &str,
    scopes: &[Loaded],
) -> Vec<usize> {
    scopes
        .iter()
        .enumerate()
        .filter(|(_, l)| dir.holds_member(person, org, l.team.as_deref()))
        .map(|(i, _)| i)
        .collect()
}

/// The org's people a digest may go to: its members with an email address, less anyone disabled.
pub fn members(dir: &crate::people::directory::Directory, org: &str) -> Vec<String> {
    dir.orgs
        .get(org)
        .map(|o| {
            o.members
                .keys()
                .filter(|u| u.contains('@') && dir.holds_member(u, org, None))
                .filter(|u| {
                    !dir.disabled
                        .get(&crate::auth::acl::disabled_id(u))
                        .copied()
                        .unwrap_or(false)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The team of a team's scope.
pub fn team_of(scope: &Scope) -> Option<String> {
    match scope {
        Scope::Team(id) => Some(
            id.rsplit_once('/')
                .map_or(id.as_str(), |(_, t)| t)
                .to_string(),
        ),
        _ => None,
    }
}

/// Who `person` reads as in `org`, by the directory alone (a command run beside the servers): the org's members'
/// group and the closure edits pushed for them. A server applies its own closures, its team csv's too.
pub fn directory_reader(
    dir: &crate::people::directory::Directory,
    org: &str,
    person: &str,
) -> Principal {
    let person = crate::auth::acl::norm_id(person);
    let mut groups = vec![crate::auth::acl::org_wide(org)];
    if let Some(e) = dir.closure_edits.get(&person) {
        groups.retain(|g| !e.remove.contains(g));
        for a in &e.add {
            if !groups.contains(a) {
                groups.push(a.clone());
            }
        }
    }
    Principal::from_parts(person, groups)
}

// ---- From the command line ----

/// `org`'s digest round as a command beside the servers builds it (`ampup-memory digest`): with an org, its scope and
/// its teams' that hold memory, its members, and readers by the directory; without, a `serve --local` store's own
/// scope, the people its sessions were recorded for, reading as `groups`.
async fn org_for(
    store: &Store,
    org: Option<&str>,
    week: Week,
    groups: &[String],
) -> Result<(Org, Reader)> {
    match org {
        None => {
            let loaded = Loaded::load(store, &Scope::Org, None).await?;
            let people = askers(loaded.log(), week)
                .into_iter()
                .map(|p| (p, vec![0]))
                .collect();
            Ok((
                Org {
                    org: None,
                    home: Scope::Org,
                    scopes: vec![loaded],
                    people,
                },
                Reader::Groups(groups.to_vec()),
            ))
        }
        Some(o) => {
            let dir = crate::people::directory::Directory::load(store).await?;
            let held = dir
                .orgs
                .get(o)
                .with_context(|| format!("the directory holds no org {o}"))?;
            let mut scopes = vec![Scope::org(o)?];
            for t in held.teams.keys() {
                let s = Scope::team(o, t)?;
                if store.has_episodes(&s).await? {
                    scopes.push(s);
                }
            }
            let mut loaded = vec![];
            for s in &scopes {
                loaded.push(Loaded::load(store, s, team_of(s)).await?);
            }
            let people = members(&dir, o)
                .into_iter()
                .map(|p| {
                    let r = readable(&dir, o, &p, &loaded);
                    (p, r)
                })
                .filter(|(_, r)| !r.is_empty())
                .collect();
            Ok((
                Org {
                    org: Some(o.to_string()),
                    home: Scope::org(o)?,
                    scopes: loaded,
                    people,
                },
                Reader::Directory(Box::new(dir), o.to_string()),
            ))
        }
    }
}

/// Who a person reads as, from the command line.
enum Reader {
    Groups(Vec<String>),
    Directory(Box<crate::people::directory::Directory>, String),
}

impl Reader {
    fn of(&self, person: &str) -> Principal {
        match self {
            Reader::Groups(g) => {
                Principal::from_parts(crate::auth::acl::norm_id(person), g.clone())
            }
            Reader::Directory(dir, org) => directory_reader(dir, org, person),
        }
    }
}

/// What a preview found.
pub struct Preview {
    pub digest: Digest,
    pub rendered: Rendered,
    /// Whether they unsubscribed, and whether this week's was sent already.
    pub unsubscribed: bool,
    pub sent: Option<String>,
}

impl Preview {
    /// As `/digest/preview` answers it.
    pub fn json(&self) -> Value {
        json!({"digest": self.digest, "subject": self.rendered.subject, "text": self.rendered.text,
               "html": self.rendered.html, "unsubscribed": self.unsubscribed, "sent": self.sent,
               "would_send": self.digest.active() && !self.unsubscribed})
    }

    /// From what `/digest/preview` answered.
    pub fn from_json(v: &Value) -> Result<(Value, Rendered, bool, Option<String>)> {
        Ok((
            v["digest"].clone(),
            Rendered {
                subject: v["subject"].as_str().unwrap_or_default().into(),
                text: v["text"].as_str().unwrap_or_default().into(),
                html: v["html"].as_str().unwrap_or_default().into(),
            },
            v["unsubscribed"].as_bool().unwrap_or(false),
            v["sent"].as_str().map(String::from),
        ))
    }
}

/// `who`'s digest of `week` from `scopes` (those of their org they read), with where their sends and subscription are
/// kept (`home`), as it would be mailed with `unsubscribe` as its link. Sent to no one.
pub async fn preview_in(
    store: &Store,
    org: Option<&str>,
    scopes: &[Loaded],
    home: &Scope,
    who: &Principal,
    week: Week,
    unsubscribe: Option<String>,
) -> Result<Preview> {
    let refs: Vec<&Loaded> = scopes.iter().collect();
    let d = gather(store, org, &refs, who, week).await?;
    let links = Links {
        unsubscribe,
        ..Links::from_env()
    };
    let log = crate::qa_log::list(store, home, None).await?;
    Ok(Preview {
        rendered: render(&d, &links),
        digest: d,
        unsubscribed: unsubscribed(&log, who.user()),
        sent: mark_of(&log, who.user(), week).map(|m| m.status),
    })
}

/// `person`'s digest of `week` as it would be mailed, read from the store directly (`ampup-memory digest preview
/// --local`, or on the catalog beside the servers), sent to no one. The unsubscribe link is made where
/// `AMPUP_MEMORY_SECRET_KEY` and `AMPUP_MEMORY_PUBLIC_URL` are set.
pub async fn preview(
    store: &Store,
    org: Option<&str>,
    person: &str,
    week: Week,
    groups: &[String],
) -> Result<Preview> {
    let (o, reader) = org_for(store, org, week, groups).await?;
    let person = crate::auth::acl::norm_id(person);
    let readable: Vec<usize> = match org {
        None => (0..o.scopes.len()).collect(),
        Some(_) => o
            .people
            .iter()
            .find(|(p, _)| *p == person)
            .map(|(_, r)| r.clone())
            .with_context(|| {
                format!(
                    "{person} is not a member of {} with an email address",
                    o.home
                )
            })?,
    };
    let Org {
        home, mut scopes, ..
    } = o;
    let mut i = 0;
    scopes.retain(|_| {
        i += 1;
        readable.contains(&(i - 1))
    });
    let unsubscribe = match (
        SecretKey::from_env(),
        std::env::var("AMPUP_MEMORY_PUBLIC_URL"),
    ) {
        (Ok(Some(key)), Ok(base)) if !base.trim().is_empty() => {
            Some(unsubscribe_link(&key, &base, org, &person))
        }
        _ => None,
    };
    preview_in(
        store,
        org,
        &scopes,
        &home,
        &reader.of(&person),
        week,
        unsubscribe,
    )
    .await
}

/// Sends `week`'s digests now, as `serve` does from Monday on (each person's once; `only` narrows it to one), through
/// the transport the environment configures.
pub async fn send_now(
    store: &Store,
    org: Option<&str>,
    week: Week,
    groups: &[String],
    only: Option<&str>,
) -> Result<Round> {
    let sender = Sender::from_env().map_err(|why| anyhow::anyhow!("nothing is sent: {why}"))?;
    let (mut o, reader) = org_for(store, org, week, groups).await?;
    if let Some(p) = only {
        let p = crate::auth::acl::norm_id(p);
        o.people.retain(|(q, _)| *q == p);
        anyhow::ensure!(!o.people.is_empty(), "{p} gets no digest of {}", o.home);
    }
    let reader = |p: &str| reader.of(p);
    send_org(store, &sender, &o, week, &reader, &|| true).await
}
