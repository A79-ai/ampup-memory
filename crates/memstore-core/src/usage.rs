//! Usage metering: what the deployment did each hour, for credits and billing.
//!
//! The server counts its operations as it serves them ([`METER`]): searches, asks (on the customer's own
//! model key or not), episodes ingested and their 4 KB units, agents' turns, reads by id, model tokens by
//! model family (on the customer's own key apart), and the distinct callers of all of these. Each is counted
//! against the org the work is for (the task's org, `crate::log_org`, as `crate::limits` counts an org's
//! month), or [`SYSTEM`] when no org is in scope. The inventory count sets two gauges per org (facts stored,
//! bytes stored). Counters go in an hourly bucket per org, the hour named by its start (UTC).
//!
//! Every process (a server, a worker, a cell) counts what it served, and writes its book beside the system
//! tables every minute. The process sending telemetry adds every process's book up ([`merge`], [`rows`]), a
//! stopped one's too, and sends each hour on the `usage` channel when it changed: one row per hour for the
//! whole deployment, or, once the control plane says the deployment is hosted (it serves many orgs, and bills
//! each its own), one row per hour and org. The control plane replaces an hour's counters with each send, so a
//! resent hour is never billed twice. Hours are kept for [`KEEP_DAYS`]. An older engine kept the sender's
//! book in the catalog (`memory_system.usage_hours`), and an older process writes a book without orgs; their
//! hours are added in too, to nobody's org ([`SYSTEM`]) when rows are per org.
//!
//! What a bucket keeps beside its counters is hashes of caller names (to count distinct callers across
//! processes); they stay in the customer's account. Only the counts leave, and an org's name only to a control
//! plane that asked for rows per org.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use schemars::JsonSchema;

/// Closed hours the control plane hasn't taken are kept this long, then dropped.
pub const KEEP_DAYS: i64 = 35;
/// An ingested body's unit: each started block of this many bytes is one.
pub const UNIT_BYTES: usize = 4096;
/// The org that work done for no org is counted against: the engine's own rounds, a person's own memory.
/// Billed to nobody. No org can take the name (an org's name never starts with `_`).
pub const SYSTEM: &str = "_system";

/// This process's book: the server counts into it and the telemetry loop sends from it.
pub static METER: LazyLock<Meter> = LazyLock::new(Meter::default);

/// Whether the deployment's model calls run on the customer's own model key (`AMPUP_MEMORY_BYOK=on`): their
/// tokens are counted as `model_tokens_byok`, not `extraction_tokens`, and their asks as `asks_byok`.
pub fn byok() -> bool {
    static BYOK: LazyLock<bool> =
        LazyLock::new(|| std::env::var("AMPUP_MEMORY_BYOK").as_deref() == Ok("on"));
    *BYOK
}

/// The org a count goes to: `org`, else [`SYSTEM`].
pub fn org_or_system(org: Option<&str>) -> String {
    org.unwrap_or(SYSTEM).to_string()
}

/// An operation a caller made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    /// An `ask_memory` answer; `byok`: on the customer's own model key.
    Ask {
        byok: bool,
    },
    Turn,
    Read,
}

/// One hour of one org as kept: its counters, the hashes of its callers, and whether the control plane took it
/// after it closed (an older engine's catalog book). What a report and the catalog store, as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub counters: UsageCounters,
    /// Callers' names, hashed ([`caller_hash`]): `active_seats` is how many.
    pub callers: BTreeSet<String>,
    /// The control plane took the hour's counters after the hour closed.
    pub acked: bool,
}

impl Bucket {
    /// The hour's usage as sent, for `org` (none: the deployment's): its counters, `active_seats` its distinct
    /// callers.
    pub fn usage(&self, hour: DateTime<Utc>, org: Option<&str>) -> Usage {
        let mut counters = self.counters.clone();
        counters.active_seats = self.callers.len() as u64;
        Usage {
            hour,
            org: org.map(str::to_string),
            counters,
        }
    }

    /// `other` added in: counters added, callers joined. Gauges are left as they are ([`rows`] sets them).
    fn add(&mut self, other: &Bucket) {
        add(&mut self.counters, &other.counters);
        self.callers.extend(other.callers.iter().cloned());
    }
}

/// An hour and an org: a bucket's key.
pub type HourOrg = (DateTime<Utc>, String);

/// Facts and bytes stored, per org, as last measured ([`Meter::gauges`]).
pub type Gauges = BTreeMap<String, (u64, u64)>;

#[derive(Default)]
struct Book {
    hours: BTreeMap<HourOrg, Bucket>,
    /// The last facts and bytes measured per org, and the hour they were measured in.
    gauges: Option<(DateTime<Utc>, Gauges)>,
}

impl Book {
    fn bucket(&mut self, hour: DateTime<Utc>, org: Option<&str>) -> &mut Bucket {
        self.hours.entry((hour, org_or_system(org))).or_default()
    }
}

/// The hourly book of what was used.
#[derive(Default)]
pub struct Meter {
    book: Mutex<Book>,
}

/// The start of the hour `t` is in.
pub fn hour_of(t: DateTime<Utc>) -> DateTime<Utc> {
    t.duration_trunc(TimeDelta::hours(1))
        .expect("an hour divides any time")
}

/// A caller's name as a bucket keeps it: the first 16 bytes of its sha256, in hex.
pub fn caller_hash(caller: &str) -> String {
    Sha256::digest(caller.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An ingested body's units: its started 4 KB blocks (an empty body is none).
pub fn units(bytes: usize) -> u64 {
    bytes.div_ceil(UNIT_BYTES) as u64
}

fn add(to: &mut UsageCounters, c: &UsageCounters) {
    to.searches += c.searches;
    to.asks += c.asks;
    to.asks_byok += c.asks_byok;
    to.ingested_episodes += c.ingested_episodes;
    to.ingested_units += c.ingested_units;
    to.turns += c.turns;
    to.reads += c.reads;
    to.model_tokens_byok += c.model_tokens_byok;
    for (f, n) in &c.extraction_tokens {
        *to.extraction_tokens.entry(*f).or_default() += n;
    }
}

impl Meter {
    /// `caller` made `op`, now, for the org this task works for.
    pub fn op(&self, op: Op, caller: &str) {
        self.op_at(Utc::now(), crate::log_org::current().as_deref(), op, caller)
    }

    pub fn op_at(&self, now: DateTime<Utc>, org: Option<&str>, op: Op, caller: &str) {
        let mut book = self.book.lock().unwrap();
        let b = book.bucket(hour_of(now), org);
        let c = &mut b.counters;
        match op {
            Op::Search => c.searches += 1,
            Op::Ask { byok: false } => c.asks += 1,
            Op::Ask { byok: true } => c.asks_byok += 1,
            Op::Turn => c.turns += 1,
            Op::Read => c.reads += 1,
        }
        b.callers.insert(caller_hash(caller));
    }

    /// Episodes with bodies of these sizes (bytes) were ingested into `org`, now.
    pub fn ingested(&self, org: Option<&str>, bodies: impl IntoIterator<Item = usize>) {
        self.ingested_at(Utc::now(), org, bodies)
    }

    pub fn ingested_at(
        &self,
        now: DateTime<Utc>,
        org: Option<&str>,
        bodies: impl IntoIterator<Item = usize>,
    ) {
        let mut book = self.book.lock().unwrap();
        let c = &mut book.bucket(hour_of(now), org).counters;
        for b in bodies {
            c.ingested_episodes += 1;
            c.ingested_units += units(b);
        }
    }

    /// A model call for `org` (`model`) read and wrote `tokens`, now: `byok`, on the customer's own key.
    /// Counted by `crate::limits::count_for` alone, with the org's month, so the two never disagree.
    pub(crate) fn tokens(&self, org: Option<&str>, model: &str, tokens: u64, byok: bool) {
        self.tokens_at(Utc::now(), org, model, tokens, byok)
    }

    pub fn tokens_at(
        &self,
        now: DateTime<Utc>,
        org: Option<&str>,
        model: &str,
        tokens: u64,
        byok: bool,
    ) {
        if tokens == 0 {
            return;
        }
        let mut book = self.book.lock().unwrap();
        let c = &mut book.bucket(hour_of(now), org).counters;
        if byok {
            c.model_tokens_byok += tokens;
        } else {
            *c.extraction_tokens
                .entry(ModelFamily::of(model))
                .or_default() += tokens;
        }
    }

    /// The facts and bytes stored per org, as just measured: the gauges of the current hour's rows, and of the
    /// next hours' until measured again.
    pub fn gauges(&self, by_org: Gauges) {
        self.gauges_at(Utc::now(), by_org)
    }

    pub fn gauges_at(&self, now: DateTime<Utc>, by_org: Gauges) {
        self.book.lock().unwrap().gauges = Some((hour_of(now), by_org));
    }

    /// The facts and bytes per org as last measured here; none before the first measure.
    pub fn last_gauges(&self) -> Option<Gauges> {
        self.book
            .lock()
            .unwrap()
            .gauges
            .as_ref()
            .map(|(_, g)| g.clone())
    }

    /// The current hour as it stands now, for the whole deployment (what `GET /telemetry` shows).
    pub fn current(&self, now: DateTime<Utc>) -> Usage {
        let hour = hour_of(now);
        let mut by_org = BTreeMap::new();
        for ((h, org), b) in self.book(now) {
            if h == hour {
                by_org.insert((h, org), b);
            }
        }
        let merged = Merged {
            by_org,
            legacy: BTreeMap::new(),
        };
        rows(
            &merged,
            false,
            &BTreeMap::new(),
            self.last_gauges().as_ref(),
            now,
        )
        .into_iter()
        .find(|u| u.hour == hour)
        .unwrap_or(Usage {
            hour,
            org: None,
            counters: UsageCounters::default(),
        })
    }

    /// Every hour and org this process counted in the last [`KEEP_DAYS`] (older ones dropped), as it stands
    /// now: what it reports for the process sending telemetry to add up ([`merge`]).
    pub fn book(&self, now: DateTime<Utc>) -> Vec<(HourOrg, Bucket)> {
        let mut book = self.book.lock().unwrap();
        let oldest = hour_of(now) - TimeDelta::days(KEEP_DAYS);
        book.hours.retain(|(h, _), _| *h >= oldest);
        book.hours
            .iter()
            .map(|(k, b)| (k.clone(), b.clone()))
            .collect()
    }
}

/// `book` (hours per org) added up per hour: what a process that sends no orgs reports beside it, for an older
/// sender to read.
pub fn collapsed(book: &[(HourOrg, Bucket)]) -> Vec<(DateTime<Utc>, Bucket)> {
    let mut out: BTreeMap<DateTime<Utc>, Bucket> = BTreeMap::new();
    for ((h, _), b) in book {
        out.entry(*h).or_default().add(b);
    }
    out.into_iter().collect()
}

/// The deployment's hours from several processes' books (each counts only what it served), added up: per hour
/// and org from books that keep orgs, per hour from those that don't (an older process's, the catalog's).
#[derive(Debug, Default)]
pub struct Merged {
    pub by_org: BTreeMap<HourOrg, Bucket>,
    pub legacy: BTreeMap<DateTime<Utc>, Bucket>,
}

/// Whether the control plane needs to keep `u` among the rows it took: a closed hour's row of gauges alone is
/// never made again ([`rows`] makes rows of gauges alone for the current hour only), so it needn't be. Keeps the
/// rows kept from growing with every org that holds anything, every hour.
pub fn keep_taken(u: &Usage, now: DateTime<Utc>) -> bool {
    if u.hour >= hour_of(now) {
        return true;
    }
    let c = &u.counters;
    c.searches
        + c.asks
        + c.asks_byok
        + c.ingested_episodes
        + c.turns
        + c.reads
        + c.model_tokens_byok
        > 0
        || c.extraction_tokens.values().any(|n| *n > 0)
}

/// Every process's book added up: counters added and callers joined, per hour and org (`by_org`) and per hour
/// (`legacy`, books without orgs). Gauges in the books are left out: they are the sender's own ([`rows`]).
/// Hours older than [`KEEP_DAYS`] before `now` are left out.
pub fn merge<'a>(
    by_org: impl IntoIterator<Item = &'a [(HourOrg, Bucket)]>,
    legacy: impl IntoIterator<Item = &'a [(DateTime<Utc>, Bucket)]>,
    now: DateTime<Utc>,
) -> Merged {
    let oldest = hour_of(now) - TimeDelta::days(KEEP_DAYS);
    let kept = |h: &DateTime<Utc>| *h >= oldest && Usage::on_the_hour(*h);
    let mut out = Merged::default();
    for book in by_org {
        for ((h, org), b) in book {
            if kept(h) {
                out.by_org.entry((*h, org.clone())).or_default().add(b);
            }
        }
    }
    for book in legacy {
        for (h, b) in book {
            if kept(h) {
                out.legacy.entry(*h).or_default().add(b);
            }
        }
    }
    out
}

/// The rows to send, as `merged` stands now, leaving out those the control plane already took (`took`, keyed by
/// hour and org, none for a row of the whole deployment).
///
/// - `per_org` false: one row per hour, no org, as an engine always sent.
/// - `per_org` true: one row per hour and org; work for no org, and books without orgs, under [`SYSTEM`].
///
/// An hour keeps the form the control plane first took it in, whichever is asked for now: one taken as one row
/// for the deployment stays one (it was billed to the deployment's org; sent again per org, it would be billed
/// twice), and one taken per org stays per org (sent again as one row, it would all be billed again to the
/// deployment's org). So a switch either way starts with the next hour that has nothing taken.
///
/// Gauges: the current hour's rows carry `gauges` (as last measured), a row per org that holds anything even
/// with no other use; an earlier hour's keep what was sent for it, as does the current hour's before the first
/// measure (`gauges` none).
pub fn rows(
    merged: &Merged,
    per_org: bool,
    took: &BTreeMap<(DateTime<Utc>, Option<String>), Usage>,
    gauges: Option<&Gauges>,
    now: DateTime<Utc>,
) -> Vec<Usage> {
    let current = hour_of(now);
    let taken_per_org: BTreeSet<DateTime<Utc>> = took
        .keys()
        .filter(|(_, org)| org.is_some())
        .map(|(h, _)| *h)
        .collect();
    let whole = |h: &DateTime<Utc>| {
        if per_org {
            took.contains_key(&(*h, None))
        } else {
            !taken_per_org.contains(h)
        }
    };
    // Each row's bucket, before gauges.
    let mut out: BTreeMap<(DateTime<Utc>, Option<String>), Bucket> = BTreeMap::new();
    for ((h, org), b) in &merged.by_org {
        let key = (*h, (!whole(h)).then(|| org.clone()));
        out.entry(key).or_default().add(b);
    }
    for (h, b) in &merged.legacy {
        let key = (*h, (!whole(h)).then(|| SYSTEM.to_string()));
        out.entry(key).or_default().add(b);
    }
    if let Some(gauges) = gauges.filter(|g| !g.is_empty()) {
        if whole(&current) {
            out.entry((current, None)).or_default();
        } else {
            for (org, (facts, bytes)) in gauges {
                if *facts > 0 || *bytes > 0 {
                    out.entry((current, Some(org.clone()))).or_default();
                }
            }
        }
    }
    out.into_iter()
        .map(|((h, org), b)| {
            let mut u = b.usage(h, org.as_deref());
            let (facts, bytes) = if let Some(gauges) = gauges.filter(|_| h == current) {
                match &org {
                    None => gauges
                        .values()
                        .fold((0, 0), |(f, b), (f2, b2)| (f + f2, b + b2)),
                    Some(o) => gauges.get(o).copied().unwrap_or_default(),
                }
            } else {
                took.get(&(h, org.clone())).map_or((0, 0), |t| {
                    (t.counters.facts_stored, t.counters.storage_bytes)
                })
            };
            u.counters.facts_stored = facts;
            u.counters.storage_bytes = bytes;
            u
        })
        .filter(|u| took.get(&(u.hour, u.org.clone())) != Some(u))
        .collect()
}

/// Counts keyed by a closed set: a key outside it fails to read, and the schema says so.
pub type Counts<K> = std::collections::BTreeMap<K, u64>;

/// One hour of the deployment's use, or of one org's in it (this module), the hour named by its start. The hour's counters are
/// sent as they stand, again as they grow, and once more after the hour ends: each send replaces the last
/// for that hour on the control plane, so a resent hour is never counted twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "UsageFields")]
#[schemars(with = "UsageFields")]
pub struct Usage {
    /// The hour's start (UTC, on the hour).
    pub hour: chrono::DateTime<chrono::Utc>,
    /// The org the hour's use is for: only from a hosted deployment the control plane asked for rows per org
    /// (`_system`: work for no org); none, the whole deployment's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    pub counters: UsageCounters,
}

/// [`Usage`] as read, checked into one: `hour` on an hour's start.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Usage")]
struct UsageFields {
    /// The hour's start (RFC 3339, UTC, minutes, seconds and fractions all zero).
    hour: chrono::DateTime<chrono::Utc>,
    /// The org the row is for (a hosted deployment's rows per org; `_system`: work for no org). Absent: the
    /// whole deployment's.
    #[serde(default)]
    #[schemars(regex(pattern = r"^[a-z0-9_]{1,64}$"))]
    org: Option<String>,
    counters: UsageCounters,
}

impl TryFrom<UsageFields> for Usage {
    type Error = &'static str;
    fn try_from(f: UsageFields) -> Result<Self, Self::Error> {
        if !Usage::on_the_hour(f.hour) {
            return Err("usage names an hour by its start: minutes, seconds and fractions zero");
        }
        if f.org.as_deref().is_some_and(|o| {
            !(1..=64).contains(&o.len())
                || !o
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }) {
            return Err("an org is 1 to 64 of a-z, 0-9 and _");
        }
        Ok(Usage {
            hour: f.hour,
            org: f.org,
            counters: f.counters,
        })
    }
}

impl Usage {
    /// Whether `t` is an hour's start.
    pub fn on_the_hour(t: chrono::DateTime<chrono::Utc>) -> bool {
        use chrono::Timelike;
        t.minute() == 0 && t.second() == 0 && t.nanosecond() == 0
    }
}

/// What one hour counted. Operations are counted when they are served; `facts_stored` and `storage_bytes`
/// are gauges, as last measured while the hour ran; `active_seats` counts distinct callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageCounters {
    /// Searches: `/search`, `/query`, MCP `search_memory` and `query_memory`.
    pub searches: u64,
    /// `ask_memory` answers, on a model key that isn't the customer's own.
    pub asks: u64,
    /// `ask_memory` answers on the customer's own model key (`AMPUP_MEMORY_BYOK=on`).
    pub asks_byok: u64,
    /// Episodes ingested through `/ingest`.
    pub ingested_episodes: u64,
    /// Started 4 KB blocks of those episodes' bodies: `ceil(bytes / 4096)` each.
    pub ingested_units: u64,
    /// An agent's turns remembered: `/remember` and MCP `remember_memory`.
    pub turns: u64,
    /// Reads by id: `/episode`, `/document`, `/browse`, MCP `get_episode` and `get_entity`.
    pub reads: u64,
    /// Fact rows stored, as last measured in the hour.
    pub facts_stored: u64,
    /// Bytes of the episode and fact tables' data files, as last measured in the hour.
    pub storage_bytes: u64,
    /// Distinct callers of the operations above in the hour.
    pub active_seats: u64,
    /// Model tokens read and written on a key that isn't the customer's own, by the model's family
    /// ([`ModelFamily::of`]): every model call the engine makes (extracting, advice, judging, dreaming,
    /// compiling, answering, remote embeddings; a recording transcribed at its token rate), as an org's month
    /// counts them (`crate::limits`). Named for the first it counted.
    pub extraction_tokens: Counts<ModelFamily>,
    /// Model tokens read and written on the customer's own key (`AMPUP_MEMORY_BYOK=on`). Absent: 0 (an older
    /// engine's); left out when 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub model_tokens_byok: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A model's family, from its name ([`ModelFamily::of`]). No doc comments on variants, as for
/// the episode kinds in telemetry.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    Claude,
    Gpt,
    Gemini,
    Llama,
    Mistral,
    Other,
}

impl ModelFamily {
    /// The family of a model as a provider names it: `anthropic/claude-haiku-5.5`,
    /// `us.anthropic.claude-…` (Bedrock), `gpt-5-mini`, `openai/o3`, `google/gemini-2.5-flash`,
    /// `meta-llama/llama-4`, `mistralai/mixtral-8x22b`. Anything else is `other`.
    pub fn of(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        let name = m.rsplit('/').next().unwrap_or(&m);
        let o_series = |n: &str| {
            n.strip_prefix('o')
                .and_then(|r| r.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        };
        if m.contains("claude") {
            ModelFamily::Claude
        } else if m.contains("gpt") || m.starts_with("openai/") || o_series(name) {
            ModelFamily::Gpt
        } else if m.contains("gemini") {
            ModelFamily::Gemini
        } else if m.contains("llama") {
            ModelFamily::Llama
        } else if ["mistral", "mixtral", "codestral", "ministral", "pixtral"]
            .iter()
            .any(|f| m.contains(f))
        {
            ModelFamily::Mistral
        } else {
            ModelFamily::Other
        }
    }
}
