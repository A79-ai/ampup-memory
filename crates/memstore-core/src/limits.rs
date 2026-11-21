//! An org's limits (`/admin {"op": "set_limits"}`): a monthly allowance of model tokens, a cap on its episodes,
//! and a time it expires at. None set is no limit, so an org nobody limited runs as it always did.
//!
//! - **Model tokens.** Every model call the engine makes for an org (extracting, learning and routing advice,
//!   judging, dreaming, compiling, summarising, profiles, answering) counts the tokens it read and wrote against
//!   that org, per calendar month (UTC); a remote embedding call counts too, a local one is no spend, and a
//!   recording transcribed counts its minutes at a token rate (`crate::transcribe::tokens_for`). Past the
//!   allowance, the org's background model work waits (nothing it would have read is given up: it is read once
//!   the month turns or the allowance is raised), and a request that would call a model now is refused (402).
//!   Writing and reading go on.
//! - **Episodes.** A write that would take the org past its cap is refused (403); reads go on.
//! - **Expiry.** Past it, writes and model calls are refused (402) and background model work waits; reads,
//!   search and export go on.
//! - **The plan's stops.** A hosted deployment's control plane says, in its answer to telemetry, which orgs it
//!   stopped (`org_limits`: their credits ran out, say): an org whose ingestion is stopped takes no new memory
//!   (402), and one whose asks are stopped asks no model (402) while its background model work waits, as past
//!   its allowance. Search and reads go on. Only the process sending telemetry hears the answer: it writes the
//!   stops beside the reports (`memstore::server::reports`), and every process reads them from there
//!   ([`set_plan_stops`]).
//!
//! The limits live in the directory (an org-wide `org.settings` change; `people::directory::limits_in` says why), so
//! every server holds the same ones. Each process counts the
//! tokens it spent per org here ([`count`]) and writes its counts with its usage report beside the system tables
//! (`memstore::server::reports`); an org's month is every process's count added up, a stopped one's too, so it
//! holds across replicas and restarts.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex, RwLock};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What an org is held to. A limit that is None isn't one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgLimits {
    /// Model tokens (read and written) per calendar month, UTC.
    #[serde(default)]
    pub monthly_model_tokens: Option<u64>,
    /// Episodes the org may hold, over all its scopes.
    #[serde(default)]
    pub max_episodes: Option<u64>,
    /// When it stops taking writes and model work.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl OrgLimits {
    pub fn is_empty(&self) -> bool {
        self.monthly_model_tokens.is_none()
            && self.max_episodes.is_none()
            && self.expires_at.is_none()
    }
}

/// `YYYY-MM` of `t`.
pub fn month_of(t: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", t.year(), t.month())
}

/// The first instant of the month after `t`'s, UTC.
pub fn next_month(t: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = match t.month() {
        12 => (t.year() + 1, 1),
        m => (t.year(), m + 1),
    };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0)
        .single()
        .expect("the first of a month at midnight UTC is one instant")
}

/// Which of an org's limits stopped something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    ModelTokens,
    Expired,
    Episodes,
    /// The control plane stopped the org's ingestion ([`PlanStop`]).
    IngestStopped,
    /// The control plane stopped the org's asks ([`PlanStop`]).
    AskStopped,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::ModelTokens => "model_tokens",
            Limit::Expired => "expired",
            Limit::Episodes => "episodes",
            Limit::IngestStopped => "plan_ingest",
            Limit::AskStopped => "plan_ask",
        }
    }
}

/// Something an org's limit refuses: the sentence for the caller, and the answer a request gets.
#[derive(Clone, Debug)]
pub struct LimitReached {
    pub limit: Limit,
    pub message: String,
    /// When the allowance resets (model tokens) or when the org expired.
    pub at: Option<DateTime<Utc>>,
}

impl LimitReached {
    /// 403 for the episode cap, as the deployment's cap answers; 402 for the others.
    pub fn status(&self) -> u16 {
        match self.limit {
            Limit::Episodes => 403,
            _ => 402,
        }
    }

    /// The JSON a request gets.
    pub fn body(&self) -> Value {
        let at = self
            .at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        match self.limit {
            Limit::ModelTokens => {
                json!({"error": self.message, "limit": "model_tokens", "resets_at": at})
            }
            Limit::Expired => json!({"error": self.message, "limit": "expired", "expired_at": at}),
            Limit::Episodes => {
                json!({"error": self.message, "cap_reached": true, "limit": "episodes"})
            }
            Limit::IngestStopped => {
                json!({"error": self.message, "limit": "plan", "stopped": "ingest"})
            }
            Limit::AskStopped => json!({"error": self.message, "limit": "plan", "stopped": "ask"}),
        }
    }

    /// Whether `e` is one, anywhere in its chain.
    pub fn of(e: &anyhow::Error) -> Option<&LimitReached> {
        e.chain().find_map(|c| c.downcast_ref::<LimitReached>())
    }
}

impl std::fmt::Display for LimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LimitReached {}

/// Tokens per month (`YYYY-MM`) per org.
pub type Book = BTreeMap<String, BTreeMap<String, u64>>;

/// This process's counts since it started.
static MINE: LazyLock<Mutex<Book>> = LazyLock::new(Default::default);
/// Every other process's, as their reports last said.
static OTHERS: LazyLock<RwLock<Book>> = LazyLock::new(Default::default);
/// Each limited org's limits, as the directory last folded here said.
static LIMITS: LazyLock<RwLock<HashMap<String, OrgLimits>>> = LazyLock::new(Default::default);
/// The orgs the control plane stopped something for, as last read ([`set_plan_stops`]).
static PLAN: LazyLock<RwLock<BTreeMap<String, PlanStop>>> = LazyLock::new(Default::default);

/// What the control plane stopped for one org of a hosted deployment (its answer's `org_limits`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStop {
    /// New memory is refused.
    #[serde(default)]
    pub ingest: bool,
    /// Asks are refused, and background model work waits.
    #[serde(default)]
    pub ask: bool,
    /// Why, as the control plane said (bounded, no control characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PlanStop {
    /// Whether it stops anything.
    pub fn any(&self) -> bool {
        self.ingest || self.ask
    }
}

/// The orgs the control plane stopped something for now (an org it stopped nothing for has no entry): said when
/// they change.
pub fn set_plan_stops(stops: BTreeMap<String, PlanStop>) {
    let stops: BTreeMap<String, PlanStop> = stops.into_iter().filter(|(_, s)| s.any()).collect();
    let mut held = PLAN.write().unwrap();
    if *held != stops {
        let names = |m: &BTreeMap<String, PlanStop>, f: fn(&PlanStop) -> bool| {
            m.iter()
                .filter(|(_, s)| f(s))
                .map(|(o, _)| o.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        eprintln!(
            "limits: the control plane stops ingestion for [{}] and asks for [{}]",
            names(&stops, |s| s.ingest),
            names(&stops, |s| s.ask)
        );
        *held = stops;
    }
}

/// The orgs the control plane stopped something for, as last read here.
pub fn plan_stops() -> BTreeMap<String, PlanStop> {
    PLAN.read().unwrap().clone()
}

/// The refusal of `org`'s ingestion (`ask` false) or asks (`ask` true), if the control plane stopped it.
fn plan_refusal(org: &str, ask: bool) -> Option<LimitReached> {
    let plan = PLAN.read().unwrap();
    let stop = plan
        .get(org)
        .filter(|s| if ask { s.ask } else { s.ingest })?;
    let what = if ask {
        "asks no model"
    } else {
        "takes no new memory"
    };
    let why = stop
        .reason
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .map_or_else(|| "its plan's limits".to_string(), str::to_string);
    Some(LimitReached {
        limit: if ask {
            Limit::AskStopped
        } else {
            Limit::IngestStopped
        },
        message: format!("org {org} {what} for now: {why}. Its memory is still read and searched"),
        at: None,
    })
}

/// Model calls whose tokens no org was known for, while some org is limited: a path that doesn't say which org it
/// works for (`crate::log_org`).
pub static UNATTRIBUTED: LazyLock<prometheus::IntCounter> = LazyLock::new(|| {
    prometheus::IntCounter::new(
        "memstore_model_tokens_unattributed",
        "Model tokens spent while some org has limits, with no org known to count them against",
    )
    .unwrap()
});

/// The tokens a model call (`model`) just read and wrote, counted against the org this task works for
/// ([`crate::log_org`]).
pub fn count(model: &str, tokens: u64) {
    count_for(crate::log_org::current().as_deref(), model, tokens)
}

/// [`count`] against `org` (none: nobody's). The one place model tokens are counted: the org's month here, and
/// the hour's usage (`crate::usage`, under `_system` for no org), so the two never disagree.
pub fn count_for(org: Option<&str>, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }
    crate::usage::METER.tokens(org, model, tokens, crate::usage::byok());
    let Some(org) = org else {
        if !LIMITS.read().unwrap().is_empty() {
            UNATTRIBUTED.inc_by(tokens);
            static SAID: Mutex<Option<std::time::Instant>> = Mutex::new(None);
            let mut said = SAID.lock().unwrap();
            if said.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(60)) {
                *said = Some(std::time::Instant::now());
                eprintln!(
                    "memstore: {tokens} model tokens spent with no org to count them against (memstore_model_tokens_unattributed)"
                );
            }
        }
        return;
    };
    let month = month_of(Utc::now());
    *MINE
        .lock()
        .unwrap()
        .entry(month)
        .or_default()
        .entry(org.to_string())
        .or_default() += tokens;
}

/// This process's counts, this month's and last month's: what its report keeps.
pub fn book() -> Book {
    let now = Utc::now();
    let this = month_of(now);
    let last = month_of(now - chrono::TimeDelta::days(now.day() as i64));
    let mut mine = MINE.lock().unwrap();
    mine.retain(|m, _| *m == this || *m == last);
    mine.clone()
}

/// Every other process's counts, added up from their reports.
pub fn set_others(books: impl IntoIterator<Item = Book>) {
    let mut sum = Book::new();
    for b in books {
        for (month, orgs) in b {
            let m = sum.entry(month).or_default();
            for (org, n) in orgs {
                *m.entry(org).or_default() += n;
            }
        }
    }
    *OTHERS.write().unwrap() = sum;
}

/// The tokens `org` spent in `month`, every process's.
pub fn used(org: &str, month: &str) -> u64 {
    let mine = MINE
        .lock()
        .unwrap()
        .get(month)
        .and_then(|m| m.get(org))
        .copied()
        .unwrap_or(0);
    let others = OTHERS
        .read()
        .unwrap()
        .get(month)
        .and_then(|m| m.get(org))
        .copied()
        .unwrap_or(0);
    mine + others
}

/// Keep each org's limits as `dir` has them: called with every fold.
pub fn observe(dir: &crate::people::directory::Directory) {
    let held: HashMap<String, OrgLimits> = dir
        .orgs
        .iter()
        .filter(|(_, o)| !o.limits.is_empty())
        .map(|(n, o)| (n.clone(), o.limits.clone()))
        .collect();
    let mut l = LIMITS.write().unwrap();
    if *l != held {
        *l = held;
    }
}

/// Whether any org has limits, as last folded here.
pub fn any() -> bool {
    !LIMITS.read().unwrap().is_empty()
}

/// `org`'s limits as last folded here.
pub fn limits_of(org: &str) -> Option<OrgLimits> {
    LIMITS.read().unwrap().get(org).cloned()
}

fn expired(org: &str, l: &OrgLimits, now: DateTime<Utc>) -> Option<LimitReached> {
    let at = l.expires_at.filter(|t| *t <= now)?;
    Some(LimitReached {
        limit: Limit::Expired,
        message: format!(
            "org {org} expired at {}: its memory is still read and searched, but it takes no new memory and asks no model until its plan is renewed",
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        at: Some(at),
    })
}

tokio::task_local! {
    /// The last limit a request's work ran into ([`watching`]).
    static RAISED: RefCell<Option<LimitReached>>;
}

/// `fut` (a request's work), and the last limit it ran into, if any: a handler that answers its error as a failure
/// (500) is answered the limit's refusal instead (402, 403), wherever the refusal came from.
pub async fn watching<F: std::future::Future>(fut: F) -> (F::Output, Option<LimitReached>) {
    RAISED
        .scope(RefCell::new(None), async {
            let out = fut.await;
            (out, RAISED.with(|r| r.borrow_mut().take()))
        })
        .await
}

/// A limit refusing something, for [`watching`].
fn raised(l: Option<LimitReached>) -> Option<LimitReached> {
    if let Some(l) = &l {
        let _ = RAISED.try_with(|r| *r.borrow_mut() = Some(l.clone()));
    }
    l
}

/// Whether `org` may have a model called for it now: refused past its expiry first, then past its month's
/// allowance. None for an org with neither.
pub fn model_gate(org: Option<&str>) -> Option<LimitReached> {
    raised(model_gate_now(org))
}

fn model_gate_now(org: Option<&str>) -> Option<LimitReached> {
    let org = org?;
    if let Some(stopped) = plan_refusal(org, true) {
        return Some(stopped);
    }
    let l = limits_of(org)?;
    let now = Utc::now();
    if let Some(e) = expired(org, &l, now) {
        return Some(e);
    }
    let cap = l.monthly_model_tokens?;
    let used = used(org, &month_of(now));
    (used >= cap).then(|| {
        let resets = next_month(now);
        LimitReached {
            limit: Limit::ModelTokens,
            message: format!(
                "org {org} has used its {cap} model tokens for this month ({used}): what it keeps is stored and searched, and read by the model once the allowance resets on {} or is raised",
                resets.format("%Y-%m-%d")
            ),
            at: Some(resets),
        }
    })
}

/// Whether `org` takes new memory now: refused past its expiry, and while the control plane stops its ingestion.
/// (Its episode cap is checked where episodes are committed.)
pub fn write_gate(org: Option<&str>) -> Option<LimitReached> {
    let org = org?;
    if let Some(stopped) = plan_refusal(org, false) {
        return raised(Some(stopped));
    }
    raised(expired(org, &limits_of(org)?, Utc::now()))
}

/// Whether `org`'s background model work waits now (its expiry, its month's allowance, or the control plane
/// stopping its asks).
pub fn paused(org: Option<&str>) -> bool {
    model_gate_now(org).is_some()
}

/// What `show` says of `org`'s limits: every one, null where unset.
pub fn limits_json(l: &OrgLimits) -> Value {
    json!({
        "monthly_model_tokens": l.monthly_model_tokens,
        "max_episodes": l.max_episodes,
        "expires_at": l.expires_at.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    })
}

/// The refusal of a write of `adding` episodes that would take `org` past its cap.
pub fn episode_cap(org: &str, cap: u64, have: u64, adding: u64) -> LimitReached {
    let l = LimitReached {
        limit: Limit::Episodes,
        message: format!(
            "org {org} is capped at {cap} episodes: it has {have}, and this write adds {adding}. Reads and search still work; a larger plan raises the cap"
        ),
        at: None,
    };
    raised(Some(l.clone()));
    l
}
