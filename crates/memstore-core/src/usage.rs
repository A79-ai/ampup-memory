//! Usage metering: what the deployment did each hour, for credits and billing.
//!
//! The server counts its operations as it serves them ([`METER`]): searches, asks (on the customer's own
//! model key or not), episodes ingested and their 4 KB units, agents' turns, reads by id, extraction tokens
//! by model family, and the distinct callers of all of these. The inventory count sets two gauges (facts
//! stored, bytes stored). Counters go in an hourly bucket, the hour named by its start (UTC).
//!
//! Every process (a server, a worker, a cell) counts what it served, and writes its book beside the system
//! tables every minute. The process sending telemetry adds every process's book up ([`merge`]), a stopped
//! one's too, and sends each hour on the `usage` channel when it changed; the control plane replaces an
//! hour's counters with each send, so a resent hour is never billed twice. Hours are kept for
//! [`KEEP_DAYS`]. An older engine kept the sender's book in the catalog (`memory_system.usage_hours`); its
//! hours the control plane didn't take are added in too.
//!
//! What a bucket keeps beside its counters is hashes of caller names (to count distinct callers across
//! processes); they stay in the customer's account. Only the counts leave.

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

/// This process's book: the server counts into it and the telemetry loop sends from it.
pub static METER: LazyLock<Meter> = LazyLock::new(Meter::default);

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

/// One hour as kept: its counters, the hashes of its callers, and whether the control plane took it after
/// it closed. What the catalog stores, as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub counters: UsageCounters,
    /// Callers' names, hashed ([`caller_hash`]): `active_seats` is how many.
    pub callers: BTreeSet<String>,
    /// The control plane took the hour's counters after the hour closed.
    pub acked: bool,
}

impl Bucket {
    /// The hour's usage as sent: its counters, `active_seats` its distinct callers.
    pub fn usage(&self, hour: DateTime<Utc>) -> Usage {
        let mut counters = self.counters.clone();
        counters.active_seats = self.callers.len() as u64;
        Usage { hour, counters }
    }
}

#[derive(Default)]
struct Book {
    hours: BTreeMap<DateTime<Utc>, Bucket>,
    /// The last facts and bytes measured: a new hour starts from them.
    gauges: Option<(u64, u64)>,
}

impl Book {
    fn bucket(&mut self, hour: DateTime<Utc>) -> &mut Bucket {
        let gauges = self.gauges;
        self.hours.entry(hour).or_insert_with(|| {
            let mut b = Bucket::default();
            if let Some((facts, bytes)) = gauges {
                b.counters.facts_stored = facts;
                b.counters.storage_bytes = bytes;
            }
            b
        })
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
    for (f, n) in &c.extraction_tokens {
        *to.extraction_tokens.entry(*f).or_default() += n;
    }
}

impl Meter {
    /// `caller` made `op`, now.
    pub fn op(&self, op: Op, caller: &str) {
        self.op_at(Utc::now(), op, caller)
    }

    pub fn op_at(&self, now: DateTime<Utc>, op: Op, caller: &str) {
        let mut book = self.book.lock().unwrap();
        let b = book.bucket(hour_of(now));
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

    /// Episodes with bodies of these sizes (bytes) were ingested, now.
    pub fn ingested(&self, bodies: impl IntoIterator<Item = usize>) {
        self.ingested_at(Utc::now(), bodies)
    }

    pub fn ingested_at(&self, now: DateTime<Utc>, bodies: impl IntoIterator<Item = usize>) {
        let mut book = self.book.lock().unwrap();
        let c = &mut book.bucket(hour_of(now)).counters;
        for b in bodies {
            c.ingested_episodes += 1;
            c.ingested_units += units(b);
        }
    }

    /// The extraction model `model` read and wrote `tokens`, now.
    pub fn tokens(&self, model: &str, tokens: u64) {
        self.tokens_at(Utc::now(), model, tokens)
    }

    pub fn tokens_at(&self, now: DateTime<Utc>, model: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let mut book = self.book.lock().unwrap();
        *book
            .bucket(hour_of(now))
            .counters
            .extraction_tokens
            .entry(ModelFamily::of(model))
            .or_default() += tokens;
    }

    /// The facts and bytes stored, as just measured: the current hour's gauges, and the next hours' until
    /// measured again.
    pub fn gauges(&self, facts: u64, bytes: u64) {
        self.gauges_at(Utc::now(), facts, bytes)
    }

    pub fn gauges_at(&self, now: DateTime<Utc>, facts: u64, bytes: u64) {
        let mut book = self.book.lock().unwrap();
        book.gauges = Some((facts, bytes));
        let c = &mut book.bucket(hour_of(now)).counters;
        c.facts_stored = facts;
        c.storage_bytes = bytes;
    }

    /// The current hour as it stands now.
    pub fn current(&self, now: DateTime<Utc>) -> Usage {
        let mut book = self.book.lock().unwrap();
        let hour = hour_of(now);
        book.bucket(hour).usage(hour)
    }

    /// Every hour this process counted in the last [`KEEP_DAYS`] (older ones dropped), as it stands now:
    /// what it reports for the process sending telemetry to add up ([`merge`]).
    pub fn book(&self, now: DateTime<Utc>) -> Vec<(DateTime<Utc>, Bucket)> {
        let mut book = self.book.lock().unwrap();
        let oldest = hour_of(now) - TimeDelta::days(KEEP_DAYS);
        book.hours.retain(|h, _| *h >= oldest);
        book.hours.iter().map(|(h, b)| (*h, b.clone())).collect()
    }
}

/// The deployment's hours from several processes' books (each counts only what it served): counters added,
/// callers joined, and the gauges (facts and bytes stored, measured by one process) the largest reported.
/// Hours older than [`KEEP_DAYS`] before `now` are left out.
pub fn merge<'a>(
    books: impl IntoIterator<Item = &'a [(DateTime<Utc>, Bucket)]>,
    now: DateTime<Utc>,
) -> BTreeMap<DateTime<Utc>, Bucket> {
    let oldest = hour_of(now) - TimeDelta::days(KEEP_DAYS);
    let mut out: BTreeMap<DateTime<Utc>, Bucket> = BTreeMap::new();
    for book in books {
        for (hour, b) in book {
            if *hour < oldest || !Usage::on_the_hour(*hour) {
                continue;
            }
            let m = out.entry(*hour).or_default();
            add(&mut m.counters, &b.counters);
            m.counters.facts_stored = m.counters.facts_stored.max(b.counters.facts_stored);
            m.counters.storage_bytes = m.counters.storage_bytes.max(b.counters.storage_bytes);
            m.callers.extend(b.callers.iter().cloned());
        }
    }
    out
}

/// Counts keyed by a closed set: a key outside it fails to read, and the schema says so.
pub type Counts<K> = std::collections::BTreeMap<K, u64>;

/// One hour of the deployment's use (this module), the hour named by its start. The hour's counters are
/// sent as they stand, again as they grow, and once more after the hour ends: each send replaces the last
/// for that hour on the control plane, so a resent hour is never counted twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "UsageFields")]
#[schemars(with = "UsageFields")]
pub struct Usage {
    /// The hour's start (UTC, on the hour).
    pub hour: chrono::DateTime<chrono::Utc>,
    pub counters: UsageCounters,
}

/// [`Usage`] as read, checked into one: `hour` on an hour's start.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Usage")]
struct UsageFields {
    /// The hour's start (RFC 3339, UTC, minutes, seconds and fractions all zero).
    hour: chrono::DateTime<chrono::Utc>,
    counters: UsageCounters,
}

impl TryFrom<UsageFields> for Usage {
    type Error = &'static str;
    fn try_from(f: UsageFields) -> Result<Self, Self::Error> {
        if !Usage::on_the_hour(f.hour) {
            return Err("usage names an hour by its start: minutes, seconds and fractions zero");
        }
        Ok(Usage {
            hour: f.hour,
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
    /// Tokens the extraction model read and wrote, by its family ([`ModelFamily::of`]).
    pub extraction_tokens: Counts<ModelFamily>,
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
