//! Claims of absence: facts that say something did not happen over a period ("no new Linear tickets since nine",
//! "the Sentry feed stopped", "nothing from Slack reached memory", "she hasn't replied since Monday"). News never
//! supersedes one: a new ticket gives facts about that ticket, never "tickets arrive", so a claim that none did
//! would be served, and restated by every reader who trusts it, for ever. Whether anything arrived is the store's
//! own record of what it received ([`crate::arrivals`], on the [`Clock::Received`] clock), not what a search of
//! facts finds (an episode not yet read for facts has none), so each read checks the claims it serves against it.
//!
//! - **Recognised** by their words ([`candidate`], [`Claim::of`]): a negation with a period (`no ... since`,
//!   `hasn't ... in the last hour`, `not ... anymore`), a negation before arriving (`nothing ... reached`, `no ...
//!   ingested`), or a feed, sync, job or kind of source that stalled or stopped; or by the tag `absence` a writer
//!   gave the fact. A read-time reading of words, not a model's: it reads every fact already stored, whichever
//!   extraction wrote it, and costs nothing, but knows only the phrasings it lists.
//! - **About** the kinds of source it names (`linear`, `sentry`: those the reader's scopes have received, named
//!   with a word about arriving), or memory as a whole (`memory`, `ingestion`: any kind), or nothing the store
//!   can check: a person who hasn't replied, or a claim narrowed past a kind of source (`from the customer`,
//!   `about the Acme deal`, a deal's fact), which one more ticket of that kind says nothing about. A kind of
//!   source connectors deliver (`sentry`, `slack`, [`SOURCE_KINDS`]) is a kind, never a narrowing, whether or not
//!   the reader has received it; a claim naming several ("nothing from Sentry, Slack or Linear") counts those the
//!   reader has received, and names nothing to check when it has received none of them.
//! - **Checked** where it is about something the store receives: the episodes the reader may read that the store
//!   received from the start of the period it covers to the time read, of those kinds, the claim's own episodes
//!   and notes that name no record aside. The period starts when the claim was stated (`valid_from`), or earlier
//!   where its words name a start (`since 22:15`, `since Nov 8`, `in the last 3 hours`, `for the fifth hour in a
//!   row`), never more than 30 days earlier; words it can't read leave `valid_from`. Any arrived: `contradicted: 14 linear items arrived since ...`, and a read as of
//!   now leaves it out (`as_of` and history reads keep it, marked). None arrived: it holds so far, and says so.
//! - **Short-lived** where nothing checks it: `a claim of absence, as of ..., not checked since`, and `stale:`
//!   once older than [`HOURS_ENV`] (24 hours unless set).
//!
//! The marks are [`crate::freshness::Mark`]s, given where freshness marks are and in every scope whatever its
//! `freshness` setting, and on the notes a session's start or its first ask for advice hands whole ([`handed`]):
//! nothing is written, no model is asked and no job runs. Only the claims among the facts a
//! read serves are checked, each set of kinds and start once per read ([`Checked`]), at most [`CHECKS`] of them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::Datelike;

use crate::arrivals::{self, Arrivals, Ask, Clock};
use crate::freshness::Mark;
use crate::{Fact, Principal, Session, Store};

/// How long, in hours, a claim of absence nothing checks reads as current (`AMPUP_MEMORY_ABSENCE_HOURS=24`); `0`
/// or `off` turns the whole check off.
pub const HOURS_ENV: &str = "AMPUP_MEMORY_ABSENCE_HOURS";
const HOURS: i64 = 24;
const HOUR_US: i64 = 3_600_000_000;

/// The tag a writer gives a fact to say it is a claim of absence, whatever its words.
pub const TAG: &str = "absence";

/// Most arrivals counts one read makes for claims of absence; a claim past them reads as not checked.
pub const CHECKS: usize = 16;

/// How long an unchecked claim of absence holds, in microseconds; None: the check is off.
pub fn window() -> Option<i64> {
    match std::env::var(HOURS_ENV) {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim().to_ascii_lowercase();
            match v.parse::<f64>() {
                Ok(h) if h > 0.0 => Some((h * HOUR_US as f64) as i64),
                Ok(_) => None,
                Err(_) if matches!(v.as_str(), "off" | "false" | "no") => None,
                Err(_) => Some(HOURS * HOUR_US),
            }
        }
        _ => Some(HOURS * HOUR_US),
    }
}

/// Lets the store's structured queries (`/query`, `get_entity`) mark claims of absence as search does.
pub fn register() {
    let _ = crate::freshness::ABSENCE.set(|store, session, who, facts, at| {
        Box::pin(async move {
            let memo = Checked::default();
            marks(store, &[session], who, facts, at, &memo).await
        })
    });
}

/// What one read has counted: which words name a kind of source the reader's scopes have received, and the
/// arrivals each check found. Shared by the clones of a read.
#[derive(Clone, Default)]
pub struct Checked(Arc<Mutex<Memo>>);

#[derive(Default)]
struct Memo {
    kinds: HashMap<String, bool>,
    counts: HashMap<String, Option<Vec<(String, usize)>>>,
}

/// Words that say a negation: something did not happen.
const NONE: [&str; 12] = [
    "no", "nothing", "none", "zero", "never", "nobody", "hasn't", "haven't", "hadn't", "didn't",
    "wasn't", "weren't",
];
/// Words before `not` that make it say something did not happen (`has not`, `did not`), not what a thing is.
const NOT_AFTER: [&str; 7] = ["has", "have", "had", "did", "was", "were", "been"];
/// Units a period is counted in (`in the last 3 hours`).
const UNITS: [&str; 16] = [
    "minute", "minutes", "hour", "hours", "day", "days", "week", "weeks", "month", "months", "run",
    "runs", "cycle", "cycles", "sync", "syncs",
];
/// Words for arriving, as a negation before one says nothing did.
const ARRIVING: [&str; 17] = [
    "arrive",
    "arrived",
    "arrives",
    "arriving",
    "reach",
    "reached",
    "reaches",
    "ingest",
    "ingested",
    "ingesting",
    "received",
    "landed",
    "synced",
    "imported",
    "flowing",
    "flowed",
    "delivered",
];
/// What stopped arriving, sending or running, after `stopped` (`stopped arriving`).
const STOPPED: [&str; 14] = [
    "arriving",
    "coming",
    "sending",
    "syncing",
    "flowing",
    "working",
    "running",
    "firing",
    "reporting",
    "updating",
    "ingesting",
    "posting",
    "responding",
    "replying",
];
/// What stalls or stops that the store could see: a feed, a sync, a job, items arriving.
const SYSTEM: [&str; 24] = [
    "ingest",
    "ingestion",
    "ingested",
    "feed",
    "feeds",
    "sync",
    "syncs",
    "syncing",
    "import",
    "imports",
    "connector",
    "connectors",
    "webhook",
    "webhooks",
    "job",
    "cron",
    "events",
    "tickets",
    "issues",
    "messages",
    "alerts",
    "memory",
    "extraction",
    "queue",
];

/// A text's words, lower case, a curly apostrophe as a straight one; `/` and other punctuation split them.
fn words(text: &str) -> Vec<String> {
    text.replace(['\u{2019}', '\u{2018}'], "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-' || c == '_'))
        .map(|w| w.trim_matches(|c| c == '\'' || c == '-').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// What a claim of absence's words are read from: the thing it is about and its value.
fn text(f: &Fact) -> String {
    format!("{} {}", f.entity.replace(['_', ':'], " "), f.value)
}

/// Where in `w` something is said not to have happened.
fn negations(w: &[String]) -> Vec<usize> {
    (0..w.len())
        .filter(|&i| {
            let t = w[i].as_str();
            (NONE.contains(&t) && !(t == "no" && w.get(i + 1).is_some_and(|n| n == "longer")))
                || (t == "not" && i > 0 && NOT_AFTER.contains(&w[i - 1].as_str()))
        })
        .collect()
}

/// Whether `w` names a period (`since`, `anymore`, `in the last 3 hours`).
fn period(w: &[String]) -> bool {
    w.iter().enumerate().any(|(i, t)| {
        t == "since"
            || t == "anymore"
            || (t == "row" && i >= 2 && w[i - 1] == "a" && w[i - 2] == "in")
            || (t == "for" && w.get(i + 2).is_some_and(|u| unit(u).is_some()))
            || ((t == "past" || t == "last")
                && w[i + 1..]
                    .iter()
                    .take(2)
                    .any(|u| UNITS.contains(&u.as_str())))
    })
}

/// Whether a negation in `w` comes shortly before a word for arriving (`nothing from Sentry reached memory`).
fn none_arrived(w: &[String], negs: &[usize]) -> bool {
    negs.iter().any(|&i| {
        w[i + 1..].iter().take(6).enumerate().any(|(j, t)| {
            ARRIVING.contains(&t.as_str())
                || (matches!(t.as_str(), "came" | "come" | "coming")
                    && w.get(i + 2 + j)
                        .is_some_and(|n| n == "in" || n == "through"))
        })
    })
}

/// Whether `w` says something stalled or stopped (`stalled`, `stopped arriving`, `went quiet`).
fn stalled(w: &[String]) -> bool {
    w.iter().enumerate().any(|(i, t)| {
        let next = w.get(i + 1).map(String::as_str);
        match t.as_str() {
            "stalled" | "stalling" | "halted" => true,
            "stopped" => next.is_none_or(|n| STOPPED.contains(&n) || n == "since"),
            "went" | "gone" => matches!(next, Some("quiet" | "silent")),
            "dried" => next == Some("up"),
            _ => false,
        }
    })
}

/// Whether `f` may be a claim of absence: what [`Claim::of`] reads further, cheaply and before anything is
/// counted. A stall alone needs a system or a kind of source to have stalled, which [`Claim::of`] decides.
pub fn candidate(f: &Fact) -> bool {
    if f.tags.iter().any(|t| t == TAG) {
        return true;
    }
    let w = words(&text(f));
    let negs = negations(&w);
    (!negs.is_empty() && (period(&w) || none_arrived(&w, &negs))) || stalled(&w)
}

/// A claim of absence and what it is about.
#[derive(Clone, Debug, PartialEq)]
pub enum About {
    /// These kinds of source.
    Kinds(Vec<String>),
    /// Memory, or its ingestion, as a whole: any kind.
    Any,
    /// Nothing the store receives.
    Nothing,
}

/// A fact read as a claim of absence: what it is about, and when the period it covers starts: when it was stated
/// (`valid_from`), or earlier where its words say so (`since 22:15`, `in the last 3 hours`), at most
/// [`MAX_BACK_DAYS`] earlier.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub about: About,
    pub since: i64,
}

impl Claim {
    /// `f` as a claim of absence, given which of its words name a kind of source the reader's scopes have
    /// received (`kind`); None when it isn't one. A claim narrowed past a kind of source (`from the customer`,
    /// `about the Acme deal`, a deal's or a person's fact) is about nothing arrivals can check: one Linear ticket
    /// arriving says nothing of whether the one meant did.
    pub fn of(f: &Fact, kind: impl Fn(&str) -> bool) -> Option<Claim> {
        let w = words(&text(f));
        let negs = negations(&w);
        let tagged = f.tags.iter().any(|t| t == TAG);
        let arrived = !negs.is_empty() && none_arrived(&w, &negs);
        // The kinds of source it names, and of those, the ones the reader has received: only those are counted.
        let named = w.iter().any(|t| kind(t) || source_kind(t));
        let kinds: Vec<String> = w
            .iter()
            .filter(|t| kind(t))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let system = w.iter().any(|t| SYSTEM.contains(&t.as_str()));
        let stall = stalled(&w) && (system || named);
        if !(tagged || (!negs.is_empty() && period(&w)) || arrived || stall) {
            return None;
        }
        // About something the store receives: said of arriving, of new items, or of what stalled.
        let receipt = arrived || stall || w.iter().any(|t| t == "new") || system;
        let whole = w.iter().any(|t| t == "memory" || t.starts_with("ingest"));
        let about = match (receipt, kinds.is_empty(), whole) {
            _ if narrowed(f, &kind) => About::Nothing,
            (true, false, _) => About::Kinds(kinds),
            // It names kinds of source, none of which this reader has received: nothing to count.
            _ if named => About::Nothing,
            (_, true, true) => About::Any,
            _ => About::Nothing,
        };
        let said = format!("{} {}", f.value, f.quote.as_deref().unwrap_or_default());
        let since = match stated_start(&said, f.valid_from) {
            Some(t) => t
                .max(f.valid_from - MAX_BACK_DAYS * DAY_US)
                .min(f.valid_from),
            None => f.valid_from,
        };
        Some(Claim { about, since })
    }
}

/// How far before it was stated a claim's own words may start its period, at most.
const MAX_BACK_DAYS: i64 = 30;
const DAY_US: i64 = 24 * HOUR_US;

/// Kinds of entity a claim about one of is narrower than anything arriving says (`deal:acme`, `contact:…`).
const NARROW_TYPES: [&str; 8] = [
    "deal", "account", "contact", "rep", "person", "customer", "project", "meeting",
];
/// Words after which a claim names what it is about (`from the customer`, `about the Acme deal`, `for project
/// X`), unless what follows is a kind of source, memory, or a period.
const NARROWING: [&str; 8] = [
    "from",
    "about",
    "regarding",
    "concerning",
    "for",
    "on",
    "in",
    "to",
];
/// Words that don't say what a claim is about, skipped after a [`NARROWING`] word.
const DETERMINERS: [&str; 10] = [
    "the", "a", "an", "any", "our", "their", "its", "this", "that", "my",
];
/// Words a period is said in, after a [`NARROWING`] word (`for the past hour`, `on Monday`, `in a row`).
const PERIOD_WORDS: [&str; 16] = [
    "since",
    "past",
    "last",
    "row",
    "now",
    "today",
    "yesterday",
    "morning",
    "afternoon",
    "evening",
    "tonight",
    "while",
    "ages",
    "first",
    "hours",
    "days",
];

/// Whether a claim names something narrower than a kind of source or memory as a whole: its entity is a deal,
/// a person, a project; or its value says `from`, `about`, `for` ... something else; or names something
/// (a word capitalised mid-sentence, as `Acme` or `OPS-12`) that is no kind of source.
fn narrowed(f: &Fact, kind: &impl Fn(&str) -> bool) -> bool {
    if f.entity
        .split_once(':')
        .is_some_and(|(t, _)| NARROW_TYPES.contains(&t.trim().to_lowercase().as_str()))
    {
        return true;
    }
    let broad = |t: &str| {
        kind(t)
            || source_kind(t)
            || t == "memory"
            || t.starts_with("ingest")
            || UNITS.contains(&t)
            || PERIOD_WORDS.contains(&t)
            || weekday(t).is_some()
            || month(t).is_some()
            || number(t).is_some()
            || ordinal(t).is_some()
            || t.starts_with(|c: char| c.is_ascii_digit())
    };
    let w = words(&f.value);
    for (i, t) in w.iter().enumerate() {
        if !NARROWING.contains(&t.as_str()) {
            continue;
        }
        let next = w[i + 1..]
            .iter()
            .find(|n| !DETERMINERS.contains(&n.as_str()));
        if next.is_some_and(|n| !broad(n)) {
            return true;
        }
    }
    // A name mid-sentence: capitalised, after a word that doesn't end one.
    let raw: Vec<&str> = f.value.split_whitespace().collect();
    raw.iter().enumerate().skip(1).any(|(i, r)| {
        let starts = raw[i - 1].ends_with(['.', '!', '?', ':', ';']);
        let name = r.trim_matches(|c: char| !c.is_alphanumeric());
        !starts
            && name.starts_with(|c: char| c.is_uppercase())
            && name
                .split(|c: char| !c.is_alphanumeric())
                .filter(|p| !p.is_empty())
                .any(|p| {
                    let p = p.to_lowercase();
                    !broad(&p) && p != "utc" && p != "i"
                })
    })
}

/// Kinds of source connectors deliver, whether or not this store has received any: a claim naming one names a
/// kind of source, never something narrower. Knowing the list says nothing about what a store holds.
const SOURCE_KINDS: [&str; 26] = [
    "sentry",
    "slack",
    "linear",
    "github",
    "gitlab",
    "bitbucket",
    "jira",
    "asana",
    "trello",
    "notion",
    "confluence",
    "gmail",
    "outlook",
    "email",
    "hubspot",
    "salesforce",
    "zendesk",
    "intercom",
    "gong",
    "zoom",
    "meeting",
    "pagerduty",
    "datadog",
    "discord",
    "figma",
    "stripe",
];

/// Whether `w` (or its singular, `emails`) is a kind of source: one in [`SOURCE_KINDS`], or one an episode type
/// claims ([`crate::episode_types`]: `slack` of `slack:`).
fn source_kind(w: &str) -> bool {
    let one = |w: &str| {
        SOURCE_KINDS.contains(&w)
            || crate::episode_types::all().iter().any(|t| {
                t.source_prefixes().iter().any(|p| {
                    let k = p.split(':').next().unwrap_or_default().to_lowercase();
                    k.len() >= 3 && !matches!(k.as_str(), "mcp" | "recall") && k == w
                })
            })
    };
    one(w) || w.strip_suffix('s').is_some_and(one)
}

/// A weekday's number from Monday (0), by its name or short name.
fn weekday(w: &str) -> Option<u32> {
    const DAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    DAYS.iter()
        .position(|d| w == *d || (w.len() >= 3 && d.starts_with(w) && w.len() <= 4))
        .map(|i| i as u32)
}

/// A month's number (1 to 12), by its name or short name.
fn month(w: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let w = w.trim_end_matches('.');
    MONTHS
        .iter()
        .position(|m| w == *m || (w.len() >= 3 && w.len() <= 4 && m.starts_with(w)))
        .map(|i| i as u32 + 1)
}

/// A count, in figures or words (`3`, `three`, `an` of `an hour`).
fn number(w: &str) -> Option<i64> {
    const WORDS: [&str; 13] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve",
    ];
    match w {
        "a" | "an" => Some(1),
        _ => w
            .parse::<i64>()
            .ok()
            .or_else(|| WORDS.iter().position(|x| *x == w).map(|i| i as i64)),
    }
}

/// An ordinal, in figures or words (`5th`, `fifth`).
fn ordinal(w: &str) -> Option<i64> {
    const WORDS: [&str; 12] = [
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        "tenth", "eleventh", "twelfth",
    ];
    WORDS
        .iter()
        .position(|x| *x == w)
        .map(|i| i as i64 + 1)
        .or_else(|| {
            ["st", "nd", "rd", "th"]
                .iter()
                .find_map(|s| w.strip_suffix(s))
                .and_then(|n| n.parse().ok())
        })
}

/// A unit of time, in microseconds.
fn unit(w: &str) -> Option<i64> {
    match w.trim_end_matches('s') {
        "minute" | "min" => Some(60_000_000),
        "hour" | "hr" => Some(HOUR_US),
        "day" => Some(DAY_US),
        "week" => Some(7 * DAY_US),
        _ => None,
    }
}

/// When the period a claim's words name starts, given the time it was stated (`at`): `since 22:15`, `since
/// 9am`, `since 2026-11-08`, `since Nov 8`, `since Monday`, `since yesterday`, `in the last 3 hours`, `for 5
/// hours`, `for the fifth hour in a row`. The earliest it names; None when it names none it can read. Times are
/// UTC; a time of day later than `at` is the day before's.
fn stated_start(text: &str, at: i64) -> Option<i64> {
    let now = chrono::DateTime::from_timestamp_micros(at)?;
    let today = now.date_naive();
    let midnight = |d: chrono::NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc().timestamp_micros())
    };
    let lower = text.to_lowercase().replace('\u{2019}', "'");
    let w: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"' | '/'))
        .map(|w| w.trim_end_matches(['.', '!', '?']))
        .filter(|w| !w.is_empty())
        .collect();
    // A time of day (`22:15`, `9am`, `9:30 pm`, `9` before `utc`), as hours and minutes.
    let clock = |w: &[&str]| -> Option<(u32, u32)> {
        let t = *w.first()?;
        let next = w.get(1).copied();
        let (body, pm) = match (t.strip_suffix("pm"), t.strip_suffix("am")) {
            (Some(b), _) => (b, Some(true)),
            (_, Some(b)) => (b, Some(false)),
            _ => match next {
                Some("pm") => (t, Some(true)),
                Some("am") => (t, Some(false)),
                _ => (t, None),
            },
        };
        let (h, m) = match body.split_once(':') {
            Some((h, m)) => (h.parse::<u32>().ok()?, m.get(..2)?.parse::<u32>().ok()?),
            None if pm.is_some() || matches!(next, Some("utc" | "today")) => {
                (body.parse().ok()?, 0)
            }
            None => return None,
        };
        let h = match pm {
            Some(true) if h < 12 => h + 12,
            Some(false) if h == 12 => 0,
            _ => h,
        };
        (h < 24 && m < 60).then_some((h, m))
    };
    let on = |d: chrono::NaiveDate, (h, m): (u32, u32)| {
        d.and_hms_opt(h, m, 0)
            .map(|t| t.and_utc().timestamp_micros())
    };
    let mut found: Vec<i64> = vec![];
    for i in 0..w.len() {
        let rest = &w[i + 1..];
        match w[i] {
            "since" => {
                let Some(&first) = rest.first() else {
                    continue;
                };
                let t = if first == "yesterday" {
                    today.pred_opt().and_then(midnight)
                } else if let Some(d) = weekday(first) {
                    let back = (today.weekday().num_days_from_monday() + 7 - d) % 7;
                    today
                        .checked_sub_days(chrono::Days::new(back as u64))
                        .and_then(midnight)
                } else if let Ok(d) = chrono::NaiveDate::parse_from_str(first, "%Y-%m-%d") {
                    match rest.get(1..).and_then(clock) {
                        Some(c) => on(d, c),
                        None => midnight(d),
                    }
                } else if let Some(c) = clock(rest) {
                    on(today, c).map(|t| if t > at { t - DAY_US } else { t })
                } else {
                    // `Nov 8`, `November 8th`, `8 Nov`: this year's, else last year's.
                    let day = |s: &str| {
                        s.trim_end_matches(|c: char| c.is_ascii_alphabetic())
                            .parse::<u32>()
                            .ok()
                    };
                    let md = match (month(first), rest.get(1).copied()) {
                        (Some(m), Some(d)) => day(d).map(|d| (m, d)),
                        _ => day(first)
                            .zip(rest.get(1).and_then(|m| month(m)))
                            .map(|(d, m)| (m, d)),
                    };
                    md.and_then(|(m, d)| {
                        let this = chrono::NaiveDate::from_ymd_opt(today.year(), m, d)?;
                        let d = match this > today {
                            true => chrono::NaiveDate::from_ymd_opt(today.year() - 1, m, d)?,
                            false => this,
                        };
                        midnight(d)
                    })
                };
                found.extend(t);
            }
            // `in the last 3 hours`, `over the past hour`.
            "last" | "past" => {
                let (n, u) = match (rest.first().and_then(|n| number(n)), rest.get(1)) {
                    (Some(n), Some(u)) => (n, unit(u)),
                    _ => (1, rest.first().and_then(|u| unit(u))),
                };
                found.extend(u.map(|u| at - n * u));
            }
            // `for 5 hours`.
            "for" => {
                if let (Some(n), Some(u)) = (
                    rest.first()
                        .and_then(|n| n.parse::<i64>().ok().or_else(|| number(n))),
                    rest.get(1).and_then(|u| unit(u)),
                ) && !matches!(rest.first(), Some(&"a") | Some(&"an"))
                {
                    found.push(at - n * u);
                }
            }
            // `for the fifth hour in a row`, `5 days in a row`.
            "row" if i >= 4 && w[i - 1] == "a" && w[i - 2] == "in" => {
                let u = unit(w[i - 3]);
                let n = ordinal(w[i - 4]).or_else(|| number(w[i - 4]));
                if let (Some(u), Some(n)) = (u, n) {
                    found.push(at - n * u);
                }
            }
            _ => {}
        }
    }
    found.into_iter().min()
}

/// A claim of absence nothing checks, as of when it was stated (not the start of the period it names), and
/// stale once older than `window` at `at`.
fn unchecked(f: &Fact, at: i64, window: i64) -> Mark {
    Mark::Absent {
        since: f.valid_from,
        checked: None,
        stale: at.saturating_sub(f.valid_from) > window,
    }
}

/// What arrived across `sessions` as `ask` asks, an episode two of them hold counted once. Another scope that
/// can't be read now is left out (and logged).
async fn arrived(
    store: &Store,
    sessions: &[&Session],
    who: &Principal,
    ask: &Ask<'_>,
) -> anyhow::Result<Arrivals> {
    let mut seen = HashSet::new();
    let mut out = Arrivals::default();
    let many = sessions.len() > 1;
    for (i, s) in sessions.iter().enumerate() {
        match arrivals::of(store, s, who, ask, many.then_some(&mut seen)).await {
            Ok(a) => out.add(a, 0),
            Err(e) if i > 0 => eprintln!(
                "absence: arrivals in {}: {}",
                s.scope(),
                crate::server::writer::redacted(&e)
            ),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// The marks on the claims of absence among `facts`, read in `sessions` as `who` may read them, as of `at`, by
/// fact id: `contradicted` where what arrived since says otherwise, else a claim that holds so far, or one nothing
/// checks (stale once older than [`window`]). Counting that fails leaves a claim unchecked; it never fails a read.
pub async fn marks(
    store: &Store,
    sessions: &[&Session],
    who: &Principal,
    facts: &[&Fact],
    at: i64,
    memo: &Checked,
) -> HashMap<String, Mark> {
    let mut out = HashMap::new();
    let Some(window) = window() else {
        return out;
    };
    let mut once = HashSet::new();
    let claims: Vec<&Fact> = facts
        .iter()
        .copied()
        .filter(|f| f.valid_from <= at && once.insert(f.fact_id.as_str()) && candidate(f))
        .collect();
    if claims.is_empty() || sessions.is_empty() {
        return out;
    }
    // Which of the claims' words name a kind of source the reader may read episodes of, at any time.
    let asked: Vec<String> = {
        let held = memo.0.lock().unwrap();
        claims
            .iter()
            .flat_map(|f| words(&text(f)))
            .filter(|w| !held.kinds.contains_key(w))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    if !asked.is_empty() {
        let refs: Vec<&str> = asked.iter().map(String::as_str).collect();
        let ask = Ask {
            kinds: &refs,
            clock: Clock::Received,
            records: true,
            ..Default::default()
        };
        match arrived(store, sessions, who, &ask).await {
            Ok(a) => {
                let mut held = memo.0.lock().unwrap();
                for w in &asked {
                    held.kinds
                        .insert(w.clone(), a.kinds.iter().any(|k| &k.kind == w));
                }
            }
            Err(e) => eprintln!("absence: kinds: {}", crate::server::writer::redacted(&e)),
        }
    }
    for f in claims {
        let claim = {
            let held = memo.0.lock().unwrap();
            Claim::of(f, |w| held.kinds.get(w).copied().unwrap_or(false))
        };
        let Some(claim) = claim else {
            continue;
        };
        let kinds: Vec<&str> = match &claim.about {
            About::Kinds(k) => k.iter().map(String::as_str).collect(),
            About::Any => vec![],
            About::Nothing => {
                out.insert(f.fact_id.clone(), unchecked(f, at, window));
                continue;
            }
        };
        let mut except: Vec<&str> = f.source_episodes.iter().map(String::as_str).collect();
        except.sort();
        let key = format!(
            "{}|{}|{}|{}",
            kinds.join(","),
            claim.since,
            at,
            except.join(",")
        );
        let held = memo.0.lock().unwrap().counts.get(&key).cloned();
        let counted = match held {
            Some(c) => c,
            None if memo.0.lock().unwrap().counts.len() >= CHECKS => None,
            None => {
                let ask = Ask {
                    from: Some(claim.since),
                    to: Some(at),
                    kinds: &kinds,
                    clock: Clock::Received,
                    except: &except,
                    records: true,
                    ..Default::default()
                };
                let c = match arrived(store, sessions, who, &ask).await {
                    Ok(a) => Some(a.kinds.into_iter().map(|k| (k.kind, k.episodes)).collect()),
                    Err(e) => {
                        eprintln!("absence: arrivals: {}", crate::server::writer::redacted(&e));
                        None
                    }
                };
                memo.0.lock().unwrap().counts.insert(key, c.clone());
                c
            }
        };
        let mark = match counted {
            Some(arrived) if !arrived.is_empty() => Mark::Contradicted {
                since: claim.since,
                arrived,
            },
            Some(_) => Mark::Absent {
                since: claim.since,
                checked: Some(kinds.iter().map(|k| k.to_string()).collect()),
                stale: false,
            },
            None => unchecked(f, at, window),
        };
        out.insert(f.fact_id.clone(), mark);
    }
    out
}

/// A note handed to an agent whole (what a session's start, or its first ask for advice, hands of what someone asked
/// to be remembered): its words, when it was last said, and the notes (episodes) that say it.
pub struct Note<'a> {
    pub text: &'a str,
    pub at: i64,
    pub notes: &'a [String],
}

/// What each of `notes` is handed as, read in `session` as `who` may read it, as of `at`: what search serves of the
/// same words as a fact. None where it is a claim of absence that what arrived since contradicts, which a read as of
/// now leaves out ([`marks`]); else its words, with the mark search gives a claim of absence (that it holds so far,
/// or `not checked since`, `stale:` once old) and, where only an agent wrote it, search's label for that (`an agent
/// concluded, <day>`, [`crate::origin`]). Nothing is counted unless one of them reads as a claim of absence
/// ([`candidate`]), so handing notes that hold none costs no arrivals query. `memo`: what this read counted already.
pub async fn handed(
    store: &Store,
    session: &Session,
    who: &Principal,
    notes: &[Note<'_>],
    at: i64,
    memo: &Checked,
) -> Vec<Option<String>> {
    let facts: Vec<Fact> = notes
        .iter()
        .enumerate()
        .map(|(i, n)| Fact {
            fact_id: format!("handed:{i}"),
            value: n.text.to_string(),
            valid_from: n.at,
            source_episodes: n.notes.to_vec(),
            ..Default::default()
        })
        .collect();
    let refs: Vec<&Fact> = facts.iter().collect();
    let marks = marks(store, &[session], who, &refs, at, memo).await;
    facts
        .iter()
        .zip(notes)
        .map(|(f, n)| {
            let mut text = n.text.to_string();
            match marks.get(&f.fact_id) {
                Some(Mark::Contradicted { .. }) => return None,
                Some(m) => text.push_str(&format!(" — {}", m.note(&f.predicate))),
                None => {}
            }
            let origin = session.vis().fact_origin(f);
            if let Some(o) = origin.and_then(|o| o.note(f.valid_from)) {
                text.push_str(&format!(" — {o}"));
            }
            Some(text)
        })
        .collect()
}
