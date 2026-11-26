//! What a deployment may tell the control plane: the telemetry contract.
//!
//! A deployment runs in the customer's account and holds their memory. Like ClickHouse BYOC's state
//! exporter, billing scraper and AlertManager, it sends bounded channels out and nothing else: `state`
//! (how the deployment is: version, health, index lag), `inventory` (how much it holds: counts of
//! scopes, episodes by type and by connector, facts, and schemas) and `alerts` (which of a fixed set of
//! alerts the engine raised on its own metrics, how bad, since when; `crate::alerts`) and `usage` (an hour's
//! counters for credits and billing; `crate::usage`) and `advice` (how the advice served in agents' sessions
//! worked out: counts per note, the note named by an id salted with the deployment's own secret, so it can be
//! followed over time and never read or guessed; `crate::sessions`). Never sent: fact, episode or passage
//! text, a query, a source, an entity, predicate or schema name, an org, team, scope or user name, a
//! key, a log line, an advice note or its situation, a session's context, conversation or transcript. The full Prometheus set (`/metrics`) stays in the account too: its labels carry org
//! names. One exception, for a deployment the control plane runs itself for many orgs (hosted): once its answer
//! to telemetry says `per_org`, `usage` is sent per hour and org, each row naming its org, so each org is
//! billed for its own use. A deployment in a customer's account is never told to, so it never names an org.
//!
//! The contract is these types. Every field is a number, a time, a version, or a value from a closed
//! set the engine owns; none holds caller text, so a payload can't carry what it has no field for. The
//! control plane reads them with `deny_unknown_fields`, so a field outside the contract is refused,
//! not stored. `memstore telemetry schema` prints the JSON schema; `GET /telemetry` (admin key) shows
//! what this server would send now.

use crate::browse::StoreBrowse as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The contract's version: raised when a field is added or its meaning changes. 2 added `inventory`,
/// 3 `alerts`, 4 `usage`, 5 `advice`, 6 `usage`'s `org` (a hosted deployment's rows per org) and
/// `model_tokens_byok`.
pub const CONTRACT: u32 = 6;

/// One payload a deployment sends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// [`CONTRACT`] as the sender knew it.
    pub contract: u32,
    /// The deployment's id: assigned by the control plane at enrollment (`unenrolled` before).
    pub deployment: DeploymentId,
    /// When the payload was made (RFC 3339, UTC).
    pub sent_at: chrono::DateTime<chrono::Utc>,
    pub payload: Payload,
}

/// The channel and its body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "channel",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Payload {
    State(State),
    Inventory(Inventory),
    Alerts(Alerts),
    Usage(Usage),
    Advice(Advice),
}

/// A deployment id: lowercase letters, digits and `-`, 1 to 64 of them. The control plane mints it;
/// the format keeps an operator from putting a customer's name in by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "DeploymentIdSchema")]
pub struct DeploymentId(String);

#[derive(JsonSchema)]
#[schemars(transparent)]
#[allow(dead_code)]
struct DeploymentIdSchema(#[schemars(regex(pattern = r"^[a-z0-9-]{1,64}$"))] String);

impl DeploymentId {
    pub const UNENROLLED: &str = "unenrolled";

    /// The id in `AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL` (what `memstore deployment enroll` wrote), else
    /// `AMPUP_MEMORY_DEPLOYMENT_ID`, else `unenrolled`. Both set: they must agree.
    pub fn from_env() -> anyhow::Result<Self> {
        let named = std::env::var("AMPUP_MEMORY_DEPLOYMENT_ID")
            .ok()
            .map(Self::try_from)
            .transpose()?;
        let Ok(path) = std::env::var("AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL") else {
            return Ok(named.unwrap_or_else(|| Self(Self::UNENROLLED.into())));
        };
        let enrolled = crate::deployment::Credential::load(path.as_ref())?.deployment;
        anyhow::ensure!(
            named.is_none_or(|n| n == enrolled),
            "AMPUP_MEMORY_DEPLOYMENT_ID isn't the id enrolled in AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL ({})",
            enrolled.0
        );
        Ok(enrolled)
    }
}

impl TryFrom<String> for DeploymentId {
    type Error = anyhow::Error;
    fn try_from(v: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !v.is_empty()
                && v.len() <= 64
                && v.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "a deployment id is 1 to 64 of a-z, 0-9 and -"
        );
        Ok(Self(v))
    }
}

impl DeploymentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<DeploymentId> for String {
    fn from(d: DeploymentId) -> String {
        d.0
    }
}

/// A version: digits, letters, `.`, `+` and `-`, 1 to 32 of them (never free text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "EngineVersionSchema")]
pub struct EngineVersion(String);

#[derive(JsonSchema)]
#[schemars(transparent)]
#[allow(dead_code)]
struct EngineVersionSchema(#[schemars(regex(pattern = r"^[0-9A-Za-z.+-]{1,32}$"))] String);

impl EngineVersion {
    /// This engine's.
    pub fn this() -> Self {
        Self(env!("CARGO_PKG_VERSION").into())
    }
}

impl TryFrom<String> for EngineVersion {
    type Error = anyhow::Error;
    fn try_from(v: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !v.is_empty()
                && v.len() <= 32
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b)),
            "a version is 1 to 32 of A-Z, a-z, 0-9, ., + and -"
        );
        Ok(Self(v))
    }
}

impl From<EngineVersion> for String {
    fn from(v: EngineVersion) -> String {
        v.0
    }
}

/// How the deployment is: sent every heartbeat and on each change.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The engine's version (the crate's).
    pub engine_version: EngineVersion,
    /// The directory change format this engine writes.
    pub directory_event_version: i64,
    /// The catalog it runs on.
    pub platform: Platform,
    pub uptime_seconds: u64,
    pub health: Health,
    /// Why it isn't `ok` (empty when it is).
    pub conditions: Vec<Condition>,
    /// Seconds since the last index update that brought every served scope up to date started.
    pub index_lag_seconds: u64,
    /// Index updates that failed since the server started.
    pub index_failures: u64,
}

/// `AMPUP_MEMORY_PLATFORM`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Local,
    Rest,
    Gcp,
    Snowflake,
    Databricks,
    Other,
}

impl Platform {
    pub fn from_env() -> Self {
        match std::env::var("AMPUP_MEMORY_PLATFORM").as_deref() {
            Ok("local") => Platform::Local,
            Ok("rest") => Platform::Rest,
            Ok("gcp") => Platform::Gcp,
            Ok("snowflake") => Platform::Snowflake,
            Ok("databricks") => Platform::Databricks,
            _ => Platform::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// The directory log holds a change newer than this engine reads: it serves the directory as of
    /// the change before and refuses directory writes until upgraded.
    DirectoryBlocked,
}

impl State {
    /// The health its conditions add up to.
    pub fn health_of(conditions: &[Condition]) -> Health {
        if conditions.is_empty() {
            Health::Ok
        } else {
            Health::Degraded
        }
    }
}

/// How much the deployment holds, summed over every scope in its catalog: sent hourly. Counts only, and
/// every map key is from a closed set the engine owns (a key outside it is refused), so no source,
/// org or schema name can ride along. A key whose count is 0 is left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub scopes: Scopes,
    /// Episode rows stored, from each table's snapshot summary.
    pub episodes: u64,
    /// Fact rows stored, superseded versions included, from each table's snapshot summary.
    pub facts: u64,
    /// Episode rows by their type ([`EpisodeKind::of`]).
    pub episodes_by_type: Counts<EpisodeKind>,
    /// Episode rows by the connector their source names ([`Connector::of`]).
    pub episodes_by_connector: Counts<Connector>,
    pub schemas: Schemas,
}

pub use crate::usage::Counts;

/// The scopes in the catalog, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scopes {
    pub orgs: u64,
    pub teams: u64,
    pub users: u64,
}

/// The schemas scopes run on (on main), and what they declare in all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Schemas {
    /// Scopes on a built-in schema, unchanged.
    pub builtin: Counts<BuiltinSchema>,
    /// Scopes on any other schema (its name is never sent).
    pub custom: u64,
    /// Scopes without a schema.
    pub none: u64,
    /// Types, predicates and metrics declared, summed over every scope's schema.
    pub types: u64,
    pub predicates: u64,
    pub metrics: u64,
}

/// The schemas in `schemas/` (`query::schema::BUILTIN`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinSchema {
    Sales,
    Engineering,
    Corpus,
}

impl BuiltinSchema {
    /// The built-in `s` is: named as one and declaring what it declares. A schema that took a built-in's
    /// name and changed it is custom.
    pub fn of(s: &crate::query::schema::Schema) -> Option<Self> {
        let b = serde_json::from_value(serde_json::Value::String(s.name.clone())).ok()?;
        let shipped =
            crate::query::schema::Schema::parse(crate::query::schema::builtin(&s.name)?).ok()?;
        (s.types == shipped.types && s.metrics == shipped.metrics).then_some(b)
    }
}

/// An episode's type (`episode_types`); `engine` for the engine's own documents (compiled skills, learned advice, old goals)
/// (`episode_types::ENGINE_SOURCES`); `untyped` when no type claims the source (written before types,
/// or into the catalog by hand). Variants carry no doc comment: with one, the schema can't close the
/// map keys to this set.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    SalesCall,
    SlackThread,
    Email,
    CrmUpdate,
    Doc,
    Engine,
    Untyped,
}

impl EpisodeKind {
    /// The kind of an episode with this source: the type that claims it, as `episode_types::of` picks.
    pub fn of(source: &str) -> Self {
        if crate::episode_types::engine_source(source) {
            return EpisodeKind::Engine;
        }
        crate::episode_types::claiming(source)
            .and_then(|t| serde_json::from_value(serde_json::Value::String(t.name().into())).ok())
            .unwrap_or(EpisodeKind::Untyped)
    }
}

/// Where an episode came from, by the longest known prefix of its source ([`Connector::PREFIXES`]);
/// anything else is `other`. `agent_memory` is an agent's note (`/remember`, MCP `remember_memory`),
/// `engine` one of the engine's own documents. No doc comments on variants, as for [`EpisodeKind`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Connector {
    Slack,
    Github,
    Ampup,
    Gong,
    Zoom,
    Recall,
    Meeting,
    Hubspot,
    Salesforce,
    DynamicsCrm,
    Crm,
    Gmail,
    Outlook,
    Email,
    Granola,
    Doc,
    Note,
    AgentMemory,
    Engine,
    Other,
}

impl Connector {
    /// Source prefixes and the connector each names. Case matters, as it does for episode types.
    pub const PREFIXES: &[(&str, Connector)] = &[
        ("slack:", Connector::Slack),
        ("github:", Connector::Github),
        ("ampup:", Connector::Ampup),
        ("gong:", Connector::Gong),
        ("zoom:", Connector::Zoom),
        ("recall:", Connector::Recall),
        ("meeting:", Connector::Meeting),
        ("hubspot:", Connector::Hubspot),
        ("salesforce:", Connector::Salesforce),
        ("dynamicsCRM:", Connector::DynamicsCrm),
        ("crm:", Connector::Crm),
        ("gmail:", Connector::Gmail),
        ("outlook:", Connector::Outlook),
        ("email:", Connector::Email),
        ("granola:", Connector::Granola),
        ("doc:", Connector::Doc),
        ("note:", Connector::Note),
        ("mcp:remember_memory:", Connector::AgentMemory),
        ("goal:", Connector::Engine),
        ("compile:", Connector::Engine),
    ];

    pub fn of(source: &str) -> Self {
        Self::PREFIXES
            .iter()
            .filter(|(p, _)| source.starts_with(p))
            .max_by_key(|(p, _)| p.len())
            .map_or(Connector::Other, |(_, c)| *c)
    }
}

/// Alerts the engine raised or cleared on its own metrics (`crate::alerts`): 1 to [`Alerts::MAX`] events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "AlertsFields")]
#[schemars(with = "AlertsFields")]
pub struct Alerts {
    pub events: Vec<AlertEvent>,
}

impl Alerts {
    /// Most events one payload carries.
    pub const MAX: usize = 64;
}

/// [`Alerts`] as read, checked into one.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Alerts")]
struct AlertsFields {
    #[schemars(length(min = 1, max = 64))]
    events: Vec<AlertEvent>,
}

impl TryFrom<AlertsFields> for Alerts {
    type Error = String;
    fn try_from(a: AlertsFields) -> Result<Self, String> {
        if a.events.is_empty() || a.events.len() > Alerts::MAX {
            return Err(format!("alerts carry 1 to {} events", Alerts::MAX));
        }
        Ok(Alerts { events: a.events })
    }
}

/// One alert's change: sent when it fires, on each change of severity and every few evaluations while it
/// fires (its count growing), and once when it resolves. An alert is the same one for as long as it fires:
/// `name` and `started_at` name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "AlertEventFields")]
#[schemars(with = "AlertEventFields")]
pub struct AlertEvent {
    pub name: AlertName,
    pub severity: Severity,
    pub state: AlertState,
    /// When its condition began to hold (RFC 3339, UTC, whole seconds).
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// When it stopped holding: present when, and only when, it is resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The evaluations its condition has held for so far.
    pub count: u64,
}

/// [`AlertEvent`] as read, checked into one: `ended_at` with `resolved` and never without.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "AlertEvent", transform = ended_at_iff_resolved)]
struct AlertEventFields {
    name: AlertName,
    severity: Severity,
    state: AlertState,
    /// When its condition began to hold (RFC 3339).
    started_at: chrono::DateTime<chrono::Utc>,
    /// When it stopped holding (RFC 3339): present when, and only when, `state` is `resolved`.
    #[serde(default, deserialize_with = "present_time")]
    #[schemars(with = "chrono::DateTime<chrono::Utc>")]
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The evaluations its condition has held for so far.
    count: u64,
}

/// A time that is there: `null` is refused, as an absent field is the only way to say none.
fn present_time<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error> {
    chrono::DateTime::<chrono::Utc>::deserialize(d).map(Some)
}

fn ended_at_iff_resolved(s: &mut schemars::Schema) {
    s.insert(
        "if".into(),
        serde_json::json!({"properties": {"state": {"const": "resolved"}}}),
    );
    s.insert("then".into(), serde_json::json!({"required": ["ended_at"]}));
    s.insert(
        "else".into(),
        serde_json::json!({"not": {"required": ["ended_at"]}}),
    );
}

impl TryFrom<AlertEventFields> for AlertEvent {
    type Error = &'static str;
    fn try_from(f: AlertEventFields) -> Result<Self, Self::Error> {
        if (f.state == AlertState::Resolved) != f.ended_at.is_some() {
            return Err("a resolved alert has ended_at, and only a resolved one");
        }
        Ok(AlertEvent {
            name: f.name,
            severity: f.severity,
            state: f.state,
            started_at: f.started_at,
            ended_at: f.ended_at,
            count: f.count,
        })
    }
}

/// The alerts the engine evaluates (`crate::alerts` holds their rules). No doc comments on variants, as
/// for [`EpisodeKind`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum AlertName {
    #[serde(rename = "http_5xx_rate")]
    Http5xxRate,
    #[serde(rename = "p99_latency")]
    P99Latency,
    #[serde(rename = "disk_usage")]
    DiskUsage,
    #[serde(rename = "directory_blocked")]
    DirectoryBlocked,
    #[serde(rename = "extraction_backlog")]
    ExtractionBacklog,
    #[serde(rename = "store_commit_failures")]
    StoreCommitFailures,
    #[serde(rename = "key_expiry")]
    KeyExpiry,
    #[serde(rename = "advice_slow")]
    AdviceSlow,
}

impl AlertName {
    pub const ALL: [AlertName; 8] = [
        AlertName::Http5xxRate,
        AlertName::P99Latency,
        AlertName::DiskUsage,
        AlertName::DirectoryBlocked,
        AlertName::ExtractionBacklog,
        AlertName::StoreCommitFailures,
        AlertName::KeyExpiry,
        AlertName::AdviceSlow,
    ];

    /// Its name on the wire and on `/metrics`.
    pub fn as_str(self) -> &'static str {
        match self {
            AlertName::Http5xxRate => "http_5xx_rate",
            AlertName::P99Latency => "p99_latency",
            AlertName::DiskUsage => "disk_usage",
            AlertName::DirectoryBlocked => "directory_blocked",
            AlertName::ExtractionBacklog => "extraction_backlog",
            AlertName::StoreCommitFailures => "store_commit_failures",
            AlertName::KeyExpiry => "key_expiry",
            AlertName::AdviceSlow => "advice_slow",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub const ALL: [Severity; 2] = [Severity::Warning, Severity::Critical];

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    Firing,
    Resolved,
}

pub use crate::usage::{ModelFamily, Usage, UsageCounters};

/// What the control plane says the deployment may do now, in its answer to telemetry. Not part of what a
/// deployment sends; read leniently (a field it adds later is ignored), and an answer without `limits`
/// changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub ingest: Gate,
    pub ask: Gate,
    /// Why, when something is stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    #[default]
    Ok,
    Stopped,
}

impl Limits {
    /// Longest reason kept: it is shown to callers, and the control plane's text is bounded here.
    pub const MAX_REASON: usize = 300;

    /// The `limits` in a telemetry answer: `None` when it has none, or none that reads as limits.
    pub fn in_answer(body: &serde_json::Value) -> Option<Self> {
        let v = body.get("limits").filter(|v| !v.is_null())?;
        match serde_json::from_value::<Limits>(v.clone()) {
            Ok(mut l) => {
                l.reason = l.reason.map(|r| {
                    let r: String = r.chars().filter(|c| !c.is_control()).collect();
                    r.chars().take(Self::MAX_REASON).collect()
                });
                Some(l)
            }
            Err(e) => {
                eprintln!("telemetry: the control plane's limits don't read ({e}); unchanged");
                None
            }
        }
    }

    /// Why `what` is stopped, for the caller refused.
    pub fn refusal(&self, what: &str) -> String {
        match self.reason.as_deref().filter(|r| !r.trim().is_empty()) {
            Some(r) => format!("{what} is stopped for this deployment: {r}"),
            None => format!("{what} is stopped for this deployment by its plan's limits"),
        }
    }
}

/// What the control plane stopped for each org it stopped something for.
pub type OrgStops = std::collections::BTreeMap<String, crate::limits::PlanStop>;

/// What the control plane's answer says of a hosted deployment's orgs: whether to send usage per org
/// (`per_org`), and what it stopped for each org (`org_limits`, each as [`Limits`]). Read leniently, as
/// [`Limits`] are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrgAnswer {
    pub per_org: bool,
    /// None: the answer says nothing of orgs' limits (they stand as they were). Some: every org the control
    /// plane stopped something for (an org not in it is stopped for nothing).
    pub org_limits: Option<std::collections::BTreeMap<String, crate::limits::PlanStop>>,
}

impl OrgAnswer {
    /// `per_org` and `org_limits` in a telemetry answer. An org's entry that doesn't read, or whose name isn't
    /// an org's, is left out (said).
    pub fn in_answer(body: &serde_json::Value) -> Self {
        let per_org = body.get("per_org").and_then(|v| v.as_bool()) == Some(true);
        let org_limits = body
            .get("org_limits")
            .filter(|v| !v.is_null())
            .and_then(|v| {
                let mut out = std::collections::BTreeMap::new();
                let Some(m) = v.as_object() else {
                    eprintln!(
                        "telemetry: the control plane's org_limits aren't an object; unchanged"
                    );
                    return None;
                };
                for (org, l) in m {
                    if crate::check_name("org", org).is_err() {
                        eprintln!("telemetry: org_limits name an org that can't be one; left out");
                        continue;
                    }
                    let Some(l) = Limits::in_answer(&serde_json::json!({ "limits": l })) else {
                        continue;
                    };
                    out.insert(
                        org.clone(),
                        crate::limits::PlanStop {
                            ingest: l.ingest == Gate::Stopped,
                            ask: l.ask == Gate::Stopped,
                            reason: l.reason,
                        },
                    );
                }
                Some(out)
            });
        OrgAnswer {
            per_org,
            org_limits,
        }
    }
}

/// What `store` holds now, as [`Inventory`]. Totals come from each table's snapshot summary (no data
/// read); the counts by type and connector read the episodes' `source` column alone, every scope's.
pub async fn inventory(store: &crate::Store) -> anyhow::Result<Inventory> {
    Ok(holdings(store).await?.0)
}

/// [`inventory`], and the facts and bytes (of the episode and fact data files, from the same snapshot summaries)
/// each org holds, over its scopes (a person's own memory, no org's, under `crate::usage::SYSTEM`): the gauges
/// `usage` reports. A scope that doesn't read (an org deleted meanwhile, or half-deleted: its catalog entries
/// left, their files gone) is left out and said, so one scope never stops the count of all the others; an error
/// only when no scope read.
pub async fn holdings(store: &crate::Store) -> anyhow::Result<(Inventory, crate::usage::Gauges)> {
    let mut inv = Inventory::default();
    let mut by_org = crate::usage::Gauges::new();
    let mut failed = Failed::default();
    for scope in store.scopes().await? {
        let mut one = Inventory::default();
        let bytes = match scope_holdings(store, &scope, &mut one).await {
            Ok(b) => b,
            Err(e) => {
                failed.left_out("inventory", &scope, e);
                continue;
            }
        };
        failed.read += 1;
        let g = by_org
            .entry(crate::usage::org_or_system(scope.org_name()))
            .or_default();
        g.0 += one.facts;
        g.1 += bytes;
        add_inventory(&mut inv, one);
    }
    failed.check()?;
    Ok((inv, by_org))
}

/// The facts and bytes (of the episode and fact data files) each org `keep` takes holds, over its scopes, from each
/// table's snapshot summary alone: what [`holdings`] counts per org, without the rest of the inventory. A cell
/// other than the sender's measures its own orgs with it. Scopes that don't read are left out and said, as
/// [`holdings`] leaves them; an error only when none read.
pub async fn stored(
    store: &crate::Store,
    keep: impl Fn(&crate::Scope) -> bool,
) -> anyhow::Result<crate::usage::Gauges> {
    let mut by_org = crate::usage::Gauges::new();
    let mut failed = Failed::default();
    for scope in store.scopes().await?.into_iter().filter(|s| keep(s)) {
        let (_, facts, bytes) = match scope_totals(store, &scope).await {
            Ok(t) => t,
            Err(e) => {
                failed.left_out("stored count", &scope, e);
                continue;
            }
        };
        failed.read += 1;
        let g = by_org
            .entry(crate::usage::org_or_system(scope.org_name()))
            .or_default();
        g.0 += facts;
        g.1 += bytes;
    }
    failed.check()?;
    Ok(by_org)
}

/// A scope's episodes and facts, and the bytes of its episode and fact data files, from the tables' snapshot
/// summaries (no data read).
async fn scope_totals(
    store: &crate::Store,
    scope: &crate::Scope,
) -> anyhow::Result<(u64, u64, u64)> {
    let ns = scope.namespace();
    let (episodes, episode_bytes) = store.snapshot_totals(&ns, crate::EPISODES).await?;
    let (facts, fact_bytes) = store.snapshot_totals(&ns, crate::FACTS).await?;
    Ok((episodes, facts, episode_bytes + fact_bytes))
}

/// The scopes a count over every scope read, and the first that didn't.
#[derive(Default)]
struct Failed {
    read: usize,
    first: Option<anyhow::Error>,
}

impl Failed {
    fn left_out(&mut self, what: &str, scope: &crate::Scope, e: anyhow::Error) {
        eprintln!("telemetry: {scope} left out of the {what}, it doesn't read: {e:#}");
        self.first.get_or_insert(e);
    }

    /// An error when scopes failed and none read.
    fn check(self) -> anyhow::Result<()> {
        match self.first {
            Some(e) if self.read == 0 => Err(e),
            _ => Ok(()),
        }
    }
}

fn add_inventory(to: &mut Inventory, one: Inventory) {
    to.scopes.orgs += one.scopes.orgs;
    to.scopes.teams += one.scopes.teams;
    to.scopes.users += one.scopes.users;
    to.episodes += one.episodes;
    to.facts += one.facts;
    for (k, n) in one.episodes_by_type {
        *to.episodes_by_type.entry(k).or_default() += n;
    }
    for (k, n) in one.episodes_by_connector {
        *to.episodes_by_connector.entry(k).or_default() += n;
    }
    for (k, n) in one.schemas.builtin {
        *to.schemas.builtin.entry(k).or_default() += n;
    }
    to.schemas.custom += one.schemas.custom;
    to.schemas.none += one.schemas.none;
    to.schemas.types += one.schemas.types;
    to.schemas.predicates += one.schemas.predicates;
    to.schemas.metrics += one.schemas.metrics;
}

/// What `scope` holds, counted into `inv`; the bytes of its episode and fact data files.
async fn scope_holdings(
    store: &crate::Store,
    scope: &crate::Scope,
    inv: &mut Inventory,
) -> anyhow::Result<u64> {
    use crate::Scope;
    match scope {
        Scope::Org | Scope::NamedOrg(_) => inv.scopes.orgs += 1,
        Scope::Team(_) => inv.scopes.teams += 1,
        Scope::User(_) => inv.scopes.users += 1,
    }
    let ns = scope.namespace();
    let (episodes, facts, bytes) = scope_totals(store, scope).await?;
    inv.episodes += episodes;
    inv.facts += facts;
    let ident = iceberg::TableIdent::new(
        iceberg::NamespaceIdent::new(ns.clone()),
        crate::EPISODES.into(),
    );
    if store.has_table(&ident).await? {
        let table = store.catalog().load_table(&ident).await?;
        for b in crate::scan_rows(&table, Some(&["source"]), None).await? {
            for source in crate::strings(&b, "source") {
                *inv.episodes_by_type
                    .entry(EpisodeKind::of(&source))
                    .or_default() += 1;
                *inv.episodes_by_connector
                    .entry(Connector::of(&source))
                    .or_default() += 1;
            }
        }
    }
    match store.schema(scope, crate::MAIN).await? {
        None => inv.schemas.none += 1,
        Some(s) => {
            match BuiltinSchema::of(&s) {
                Some(b) => *inv.schemas.builtin.entry(b).or_default() += 1,
                None => inv.schemas.custom += 1,
            }
            inv.schemas.types += s.types.len() as u64;
            inv.schemas.predicates += s
                .types
                .values()
                .map(|t| t.predicates.len() as u64)
                .sum::<u64>();
            inv.schemas.metrics += s.metrics.len() as u64;
        }
    }
    Ok(bytes)
}

/// How the advice served in agents' sessions worked out (`crate::sessions`), over every scope, as it stands:
/// sent with the inventory, each send replacing the last. Notes are named by [`NoteId`] alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "AdviceFields")]
#[schemars(with = "AdviceFields")]
pub struct Advice {
    pub sessions: SessionTotals,
    /// Sessions by the client they ran in ([`Harness::of`]).
    pub sessions_by_harness: Counts<Harness>,
    /// The notes served or withheld in a judged session, the most served first: at most [`Advice::MAX`].
    pub notes: Vec<NoteOutcomes>,
}

impl Advice {
    /// Most notes one payload carries.
    pub const MAX: usize = 500;
}

/// [`Advice`] as read, checked into one.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Advice")]
struct AdviceFields {
    sessions: SessionTotals,
    sessions_by_harness: Counts<Harness>,
    #[schemars(length(max = 500))]
    notes: Vec<NoteOutcomes>,
}

impl TryFrom<AdviceFields> for Advice {
    type Error = String;
    fn try_from(a: AdviceFields) -> Result<Self, String> {
        if a.notes.len() > Advice::MAX {
            return Err(format!("advice carries at most {} notes", Advice::MAX));
        }
        Ok(Advice {
            sessions: a.sessions,
            sessions_by_harness: a.sessions_by_harness,
            notes: a.notes,
        })
    }
}

/// Sessions by how they stand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionTotals {
    pub started: u64,
    pub open: u64,
    /// Ended by the agent (`end_session`, or its hook). A session ended discarded (its run kept no transcript) counts
    /// in neither this nor `ended_idle`, only in `started`; one ended empty (nothing asked in it) counts nowhere.
    pub ended_by_agent: u64,
    /// Ended by the server once idle: an agent that never called `end_session`.
    pub ended_idle: u64,
    pub judged: u64,
    /// Ended, but the judge failed.
    pub unscored: u64,
    /// In the holdout arm (advice routed but withheld).
    pub holdout: u64,
}

/// The client a session ran in, from the harness it named. No doc comments on variants, as for
/// [`EpisodeKind`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Gemini,
    Cursor,
    Copilot,
    ClaudeAi,
    Chatgpt,
    Mcp,
    Other,
}

impl Harness {
    /// The harness a session named (`start_session`'s `harness`, free text): one of the set, else `other`. An
    /// automated session's (`crate::sessions::AUTOMATED`) is its harness's.
    pub fn of(name: &str) -> Self {
        let name = name.trim();
        let name = name
            .strip_suffix(crate::sessions::AUTOMATED)
            .unwrap_or(name);
        match name.to_ascii_lowercase().replace('_', "-").as_str() {
            "claude-code" | "claude" => Harness::ClaudeCode,
            "codex" => Harness::Codex,
            "gemini" => Harness::Gemini,
            "cursor" => Harness::Cursor,
            "copilot" | "vscode" | "vs-code" => Harness::Copilot,
            "claude-ai" | "claude.ai" | "claude-desktop" => Harness::ClaudeAi,
            "chatgpt" | "openai" => Harness::Chatgpt,
            "mcp" => Harness::Mcp,
            _ => Harness::Other,
        }
    }
}

/// One note's sessions: every judged session it was served in (with what the judge said) or withheld from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteOutcomes {
    pub note: NoteId,
    /// Distinct people among those sessions.
    pub people: u64,
    pub served: u64,
    pub applied: u64,
    pub helped: u64,
    pub not_helped: u64,
    pub served_succeeded: u64,
    pub served_failed: u64,
    pub withheld: u64,
    pub withheld_succeeded: u64,
    pub withheld_failed: u64,
}

/// A note's id: 16 lowercase hex digits, the start of a SHA-256 over the deployment's own secret, the scope
/// and the note's situation ([`NoteId::of`]). The same note keeps its id from one send to the next; the
/// control plane can't read the situation out of it, or test a guess against it.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "NoteIdSchema")]
pub struct NoteId(String);

#[derive(JsonSchema)]
#[schemars(transparent)]
#[allow(dead_code)]
struct NoteIdSchema(#[schemars(regex(pattern = r"^[0-9a-f]{16}$"))] String);

impl NoteId {
    /// The id of `situation`'s note in the scope whose namespace is `ns`, salted with `salt`.
    pub fn of(salt: &[u8; 32], ns: &str, situation: &str) -> Self {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(salt);
        h.update(ns.as_bytes());
        h.update(b"\0");
        h.update(situation.as_bytes());
        let d = h.finalize();
        Self(d[..8].iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The salt note ids are made with: the enrolled deployment's own (`crate::deployment::Credential::salt`),
    /// else one of this process's alone (a server that isn't enrolled sends nothing).
    pub fn salt() -> [u8; 32] {
        static OWN: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        std::env::var("AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL")
            .ok()
            .and_then(|p| crate::deployment::Credential::load(p.as_ref()).ok())
            .and_then(|c| c.salt("advice note ids").ok())
            .unwrap_or_else(|| {
                *OWN.get_or_init(|| {
                    let mut s = [0; 32];
                    s[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    s[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    s
                })
            })
    }
}

impl TryFrom<String> for NoteId {
    type Error = anyhow::Error;
    fn try_from(v: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            v.len() == 16 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "a note id is 16 lowercase hex digits"
        );
        Ok(Self(v))
    }
}

impl From<NoteId> for String {
    fn from(n: NoteId) -> String {
        n.0
    }
}

/// How the advice served in `store`'s sessions worked out, every scope's, as [`Advice`]: note ids salted
/// with `salt`.
pub async fn advice(store: &crate::Store, salt: &[u8; 32]) -> anyhow::Result<Advice> {
    let mut a = Advice::default();
    let mut failed = Failed::default();
    for scope in store.scopes().await? {
        // A scope that doesn't read is left out, as from the inventory.
        let o = match crate::sessions::outcomes(store, &scope).await {
            Ok(o) => {
                failed.read += 1;
                o
            }
            Err(e) => {
                failed.left_out("advice", &scope, e);
                continue;
            }
        };
        let t = &mut a.sessions;
        t.started += o.started;
        t.open += o.open;
        t.ended_by_agent += o.ended_by_agent;
        t.ended_idle += o.ended_idle;
        t.judged += o.judged;
        t.unscored += o.unscored;
        t.holdout += o.holdout;
        for (h, n) in o.by_harness {
            *a.sessions_by_harness.entry(Harness::of(&h)).or_default() += n;
        }
        let ns = scope.namespace();
        for (situation, (n, people)) in o.notes {
            a.notes.push(NoteOutcomes {
                note: NoteId::of(salt, &ns, &situation),
                people: people.len() as u64,
                served: n.served as u64,
                applied: n.applied as u64,
                helped: n.helped as u64,
                not_helped: n.not_helped as u64,
                served_succeeded: n.served_succeeded as u64,
                served_failed: n.served_failed as u64,
                withheld: n.withheld as u64,
                withheld_succeeded: n.withheld_succeeded as u64,
                withheld_failed: n.withheld_failed as u64,
            });
        }
    }
    a.notes.sort_by(|x, y| {
        (y.served + y.withheld)
            .cmp(&(x.served + x.withheld))
            .then(x.note.cmp(&y.note))
    });
    a.notes.truncate(Advice::MAX);
    failed.check()?;
    Ok(a)
}

/// Check payloads against the contract, one JSON object per line (a mirror file, or what a
/// deployment sent): the number that hold to it, or the first line that doesn't and why.
pub fn check(jsonl: &str) -> anyhow::Result<usize> {
    let mut n = 0;
    for (i, line) in jsonl
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let e: Envelope = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("line {}: outside the contract: {e}", i + 1))?;
        anyhow::ensure!(
            e.contract <= CONTRACT,
            "line {}: contract {} is newer than this engine's ({CONTRACT})",
            i + 1,
            e.contract
        );
        n += 1;
    }
    Ok(n)
}

/// The contract's JSON schema.
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Envelope)).expect("a schema serializes")
}

/// Where payloads wait to be sent, and the customer's own copy of each.
///
/// Every payload is appended to the mirror (`telemetry.jsonl`, the customer's record of everything
/// that leaves) and to the spool before any attempt to send it. The spool is sent in batches and
/// keeps what the control plane hasn't taken, so an unreachable control plane loses nothing and stops
/// nothing: the server goes on serving, and the spool drains when it's back.
///
/// The engine and the deployment agent (`drain_spool`) both send the spool. A sender claims a batch before sending
/// it: under the spool's lock it moves the batch out of the spool into a claim file of its own beside it
/// (`telemetry.spool.jsonl.claim-<claimed at, ms>-<pid>-<n>`), sends it without the lock, and deletes the claim once
/// the control plane took it. So no two senders send the same payload. A send that fails puts what it didn't send
/// back where it was in the spool, ahead of what was recorded since; a claim whose sender stopped (crashed, killed)
/// is put back once it is [`CLAIM_STALE`] old, by the time in its name.
use anyhow::Context as _;

pub struct Outbox {
    credential: std::path::PathBuf,
    mirror: std::path::PathBuf,
    spool: std::path::PathBuf,
    /// The last `limits` the control plane answered with (`telemetry.limits.json`, beside the spool), so
    /// a restart keeps them until the next answer.
    limits: std::path::PathBuf,
    /// Limits answered since [`Outbox::take_limits`] last looked.
    answered: std::sync::Mutex<Option<Limits>>,
    /// What the last answer said of orgs: `per_org` (none before this process's first answer), and the
    /// `org_limits` answered since [`Outbox::take_org_limits`] last looked.
    orgs: std::sync::Mutex<(Option<bool>, Option<OrgStops>)>,
}

/// Most payloads the spool keeps (the oldest go first): a day of heartbeats a minute apart.
pub const SPOOL_MAX: usize = 1440;
/// Most payloads one request carries: the control plane takes at most 100.
pub const BATCH_MAX: usize = 100;
/// The mirror is rolled over to `.1` past this size.
pub const MIRROR_MAX_BYTES: u64 = 10 << 20;
/// Longest a sender spends on a claimed batch before it gives up and puts the rest back: a batch is one request,
/// or, refused as malformed, up to [`BATCH_MAX`] one by one, each cut off at 15 s, so it rarely nears this.
pub const CLAIM_SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// A claim this old, by the time in its name, was left by a sender that stopped (crashed, killed) and is put back
/// in the spool. Six times [`CLAIM_SEND_DEADLINE`], so a live sender's claim is never taken from it (which would
/// send its payloads twice), even on a stalled machine or a clock a little off between the engine and the agent; a
/// crashed sender's payloads wait at most this long, well inside the day the spool holds.
pub const CLAIM_STALE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

impl Outbox {
    /// Exporting: on when `AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL` is set, unless `AMPUP_MEMORY_TELEMETRY=off`.
    /// The mirror is `AMPUP_MEMORY_TELEMETRY_MIRROR`, else `telemetry.jsonl` beside the credential.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(credential) = std::env::var("AMPUP_MEMORY_DEPLOYMENT_CREDENTIAL") else {
            return Ok(None);
        };
        match std::env::var("AMPUP_MEMORY_TELEMETRY").as_deref() {
            Ok("off") => return Ok(None),
            Ok("on") | Err(_) => {}
            Ok(v) => anyhow::bail!("AMPUP_MEMORY_TELEMETRY is on or off, not {v}"),
        }
        let mirror = std::env::var("AMPUP_MEMORY_TELEMETRY_MIRROR")
            .ok()
            .map(Into::into);
        Ok(Some(Self::at(credential.into(), mirror)))
    }

    /// The outbox for `credential`. The mirror is `mirror`, else `telemetry.jsonl` beside the credential,
    /// and the spool sits beside the mirror: both are written on every payload, so a credential mounted
    /// read-only (a Kubernetes secret) only needs a writable mirror path.
    pub fn at(credential: std::path::PathBuf, mirror: Option<std::path::PathBuf>) -> Self {
        let beside = |p: &std::path::Path, name: &str| {
            p.parent()
                .map_or_else(|| std::path::PathBuf::from("."), |d| d.to_path_buf())
                .join(name)
        };
        let mirror = mirror.unwrap_or_else(|| beside(&credential, "telemetry.jsonl"));
        Outbox {
            spool: beside(&mirror, "telemetry.spool.jsonl"),
            limits: beside(&mirror, "telemetry.limits.json"),
            answered: Default::default(),
            orgs: Default::default(),
            credential,
            mirror,
        }
    }

    pub fn spool(&self) -> &std::path::Path {
        &self.spool
    }

    pub fn mirror(&self) -> &std::path::Path {
        &self.mirror
    }

    /// Queue `e`: into the mirror, then the spool.
    pub fn record(&self, e: &Envelope) -> anyhow::Result<()> {
        let line = self.mirrored(e)?;
        let _held = self.hold()?;
        let mut lines = self.spooled()?;
        lines.push(line.trim_end().to_string());
        let keep = lines.len().saturating_sub(SPOOL_MAX);
        self.write_spool(&lines[keep..])
            .with_context(|| format!("the telemetry spool {}", self.spool.display()))
    }

    /// Held while the spool or its claims are read and written back: the engine records and sends it, and the
    /// deployment agent, a process of its own, sends or clears the same file (`drain_spool`, `clear_spool`). Without
    /// it a payload recorded between one's read and its write was lost, and two senders claimed the same payloads. A
    /// lock file beside the spool, which is replaced on every write; never held while sending.
    fn hold(&self) -> anyhow::Result<std::fs::File> {
        let path = self.spool.with_extension("jsonl.lock");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("the telemetry spool's lock {}", path.display()))?;
        f.lock()?;
        Ok(f)
    }

    /// Append `e` to the mirror; its line.
    fn mirrored(&self, e: &Envelope) -> anyhow::Result<String> {
        use std::io::Write;
        let line = serde_json::to_string(e)? + "\n";
        if std::fs::metadata(&self.mirror).is_ok_and(|m| m.len() > MIRROR_MAX_BYTES) {
            std::fs::rename(&self.mirror, self.mirror.with_extension("jsonl.1"))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.mirror)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .with_context(|| format!("the telemetry mirror {}", self.mirror.display()))?;
        Ok(line)
    }

    /// Send `batch` now, without spooling it: into the mirror, then to the control plane, at most
    /// [`BATCH_MAX`] a request. `Ok` once every payload was taken or refused as malformed (and dropped);
    /// an error leaves the caller to send them again. What `usage` sends: its own book keeps each hour
    /// until it is acknowledged, and only the latest counters of an hour are worth sending.
    pub async fn deliver(&self, batch: &[Envelope]) -> anyhow::Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        for e in batch {
            self.mirrored(e)?;
        }
        let cred = crate::deployment::Credential::load(&self.credential)?;
        let mut took = 0;
        for chunk in batch.chunks(BATCH_MAX) {
            took += self.send(&cred, chunk, &mut 0).await?;
        }
        Ok(took)
    }

    /// Whether the control plane's last answer asked for usage per org (a hosted deployment); none before it
    /// answered this process at all, so a restarted sender sends no usage before it knows how.
    pub fn per_org(&self) -> Option<bool> {
        self.orgs.lock().unwrap().0
    }

    /// The orgs' limits the control plane answered with since this was last asked, if any.
    pub fn take_org_limits(
        &self,
    ) -> Option<std::collections::BTreeMap<String, crate::limits::PlanStop>> {
        self.orgs.lock().unwrap().1.take()
    }

    /// The limits the control plane answered with since this was last asked, if any.
    pub fn take_limits(&self) -> Option<Limits> {
        self.answered.lock().unwrap().take()
    }

    /// The last limits the control plane answered with, as kept beside the spool (none before any).
    pub fn saved_limits(&self) -> Option<Limits> {
        let text = std::fs::read_to_string(&self.limits).ok()?;
        serde_json::from_str(&text)
            .inspect_err(|e| eprintln!("telemetry: {} doesn't read ({e})", self.limits.display()))
            .ok()
    }

    /// The control plane answered 200 with `body`: keep what it says of orgs, and its limits, if it says any.
    fn answered(&self, body: &serde_json::Value) {
        let o = OrgAnswer::in_answer(body);
        {
            let mut orgs = self.orgs.lock().unwrap();
            if orgs.0 != Some(o.per_org) {
                eprintln!(
                    "telemetry: the control plane asks for usage {}",
                    if o.per_org {
                        "per org (a hosted deployment)"
                    } else {
                        "for the whole deployment"
                    }
                );
            }
            orgs.0 = Some(o.per_org);
            if o.org_limits.is_some() {
                orgs.1 = o.org_limits;
            }
        }
        let Some(l) = Limits::in_answer(body) else {
            return;
        };
        let tmp = unique_tmp(&self.limits);
        if let Err(e) = serde_json::to_vec(&l)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(&tmp, b)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, &self.limits)?))
        {
            eprintln!(
                "telemetry: limits not kept in {} ({e:#}); held until restart",
                self.limits.display()
            );
        }
        *self.answered.lock().unwrap() = Some(l);
    }

    fn spooled(&self) -> anyhow::Result<Vec<String>> {
        read_lines(&self.spool)
    }

    /// Caller holds [`Outbox::hold`].
    fn write_spool(&self, lines: &[String]) -> anyhow::Result<()> {
        Ok(write_lines(&self.spool, lines)?)
    }

    /// Payloads waiting to be sent: in the spool, and claimed by a sender and not yet taken by the control plane.
    pub fn pending(&self) -> anyhow::Result<usize> {
        let mut n = self.spooled()?.len();
        for (path, _) in self.claims()? {
            n += read_lines(&path)?.len();
        }
        Ok(n)
    }

    /// Drop every payload waiting to be sent, claimed ones too; how many there were. The mirror keeps its copy of
    /// each. A batch already on its way when this runs may still arrive, but a sender whose send fails puts nothing
    /// back once its claim is gone.
    pub fn clear(&self) -> anyhow::Result<usize> {
        let _held = self.hold()?;
        let mut n = self.spooled()?.len();
        if n > 0 {
            self.write_spool(&[])?;
        }
        for (path, _) in self.claims()? {
            n += read_lines(&path)?.len();
            remove_if_there(&path)?;
        }
        Ok(n)
    }

    /// The claims beside the spool, oldest first, each with when it was claimed (Unix ms, from its name).
    fn claims(&self) -> anyhow::Result<Vec<(std::path::PathBuf, i64)>> {
        let prefix = self.claim_prefix();
        let dir = self.spool.parent().unwrap_or(std::path::Path::new("."));
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut claims = vec![];
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(at) = name
                .to_str()
                .and_then(|n| n.strip_prefix(&prefix))
                .and_then(|rest| rest.split('-').next())
                .and_then(|ms| ms.parse::<i64>().ok())
            else {
                continue;
            };
            claims.push((entry.path(), at));
        }
        claims.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        Ok(claims)
    }

    fn claim_prefix(&self) -> String {
        let name = self.spool.file_name().unwrap_or_default().to_string_lossy();
        format!("{name}.claim-")
    }

    /// Move the first `n` of `spooled` (the spool as just read) into a new claim file; its path. Caller holds
    /// [`Outbox::hold`]. The claim is written before the spool, so a crash between the two leaves a payload twice,
    /// never none.
    fn claim(&self, spooled: &[String], n: usize) -> anyhow::Result<std::path::PathBuf> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let at = chrono::Utc::now().timestamp_millis();
        let prefix = self.claim_prefix();
        // The engine and the agent can each be pid 1 in their own container: the name is checked, not trusted.
        let path = loop {
            let k = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = self
                .spool
                .with_file_name(format!("{prefix}{at}-{}-{k}", std::process::id()));
            if !p.exists() {
                break p;
            }
        };
        write_lines(&path, &spooled[..n])
            .with_context(|| format!("the telemetry claim {}", path.display()))?;
        if let Err(e) = self.write_spool(&spooled[n..]) {
            let _ = std::fs::remove_file(&path);
            return Err(e).with_context(|| format!("the telemetry spool {}", self.spool.display()));
        }
        Ok(path)
    }

    /// Put `lines` back in the spool, less any already there, where they were: just after `prev` (the last line of
    /// the batch another sender had claimed before them) if it is spooled again, else just before `next` (the line
    /// that followed them) if it still is, else at the head; ahead of what was recorded since, either way. So two
    /// failed batches go back in the order they were claimed, whichever fails first. The oldest go first past
    /// [`SPOOL_MAX`], as in [`Outbox::record`]. Caller holds [`Outbox::hold`].
    fn put_back(
        &self,
        lines: Vec<String>,
        prev: Option<&str>,
        next: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut all = self.spooled()?;
        let at = prev
            .and_then(|p| all.iter().position(|l| l == p).map(|i| i + 1))
            .or_else(|| next.and_then(|n| all.iter().position(|l| l == n)))
            .unwrap_or(0);
        let back: Vec<String> = lines.into_iter().filter(|l| !all.contains(l)).collect();
        all.splice(at..at, back);
        let keep = all.len().saturating_sub(SPOOL_MAX);
        self.write_spool(&all[keep..])
    }

    /// Put back every claim [`CLAIM_STALE`] old, oldest first: its sender stopped before it finished. Caller holds
    /// [`Outbox::hold`].
    fn reclaim_stale(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let stale: Vec<_> = self
            .claims()?
            .into_iter()
            .filter(|(_, at)| now - at >= CLAIM_STALE.as_millis() as i64)
            .map(|(p, _)| p)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let mut lines = vec![];
        for p in &stale {
            lines.extend(read_lines(p)?);
        }
        let n = lines.len();
        self.put_back(lines, None, None)?;
        for p in &stale {
            remove_if_there(p)?;
        }
        eprintln!(
            "telemetry: {n} payloads a stopped sender had claimed are spooled again ({} claims)",
            stale.len()
        );
        Ok(())
    }

    /// Send `batch`; the number the control plane took. A batch refused as malformed (400) is sent again
    /// one by one, so only the payloads it refuses are dropped; any other answer is an error, and
    /// nothing is dropped. `settled` counts the payloads, from the first, the control plane took or refused as it
    /// goes: on an error, the rest are still to send.
    async fn send(
        &self,
        cred: &crate::deployment::Credential,
        batch: &[Envelope],
        settled: &mut usize,
    ) -> anyhow::Result<usize> {
        let post = |b: &[Envelope]| {
            let v = serde_json::to_value(b);
            async move { cred.call("POST", "/v1/telemetry", &v?).await }
        };
        let (status, body) = post(batch).await?;
        match status {
            200 => {
                self.answered(&body);
                *settled = batch.len();
                return Ok(batch.len());
            }
            400 if batch.len() > 1 => {}
            400 => {
                eprintln!(
                    "telemetry: the control plane refused a payload ({}); dropped",
                    body["error"]
                );
                *settled = 1;
                return Ok(0);
            }
            _ => anyhow::bail!("control plane answered {status}: {}", body["error"]),
        }
        let mut took = 0;
        for e in batch {
            let (status, body) = post(std::slice::from_ref(e)).await?;
            match status {
                200 => {
                    self.answered(&body);
                    took += 1;
                }
                400 => eprintln!(
                    "telemetry: the control plane refused a payload ({}); dropped",
                    body["error"]
                ),
                _ => anyhow::bail!("control plane answered {status}: {}", body["error"]),
            }
            *settled += 1;
        }
        Ok(took)
    }

    /// Send the spool, oldest first, in batches; the number sent. Each batch is claimed first (see [`Outbox`]), so
    /// another sender at the same time sends other payloads, never these. Anything but a payload the control plane
    /// refuses as malformed stops the round, and what wasn't sent goes back where it was in the spool.
    pub async fn flush(&self) -> anyhow::Result<usize> {
        let cred = crate::deployment::Credential::load(&self.credential)?;
        let mut sent = 0;
        loop {
            let (claim, lines, prev, next) = {
                let _held = self.hold()?;
                self.reclaim_stale()?;
                let mut spooled = self.spooled()?;
                if spooled.is_empty() {
                    return Ok(sent);
                }
                // The last line of the newest batch still out with a sender: the one before this batch's first.
                let prev = match self.claims()?.last() {
                    Some((p, _)) => read_lines(p)?.pop(),
                    None => None,
                };
                let n = spooled.len().min(BATCH_MAX);
                let claim = self.claim(&spooled, n)?;
                let next = spooled.get(n).cloned();
                spooled.truncate(n);
                (claim, spooled, prev, next)
            };
            // Only what holds to the contract leaves, whatever the spool file was made to hold.
            let (lines, batch): (Vec<String>, Vec<Envelope>) = lines
                .into_iter()
                .filter_map(|l| match serde_json::from_str(&l) {
                    Ok(e) => Some((l, e)),
                    Err(err) => {
                        eprintln!("telemetry: a spooled line outside the contract, dropped: {err}");
                        None
                    }
                })
                .unzip();
            let mut settled = 0;
            let result = if batch.is_empty() {
                Ok(0)
            } else {
                tokio::time::timeout(CLAIM_SEND_DEADLINE, self.send(&cred, &batch, &mut settled))
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "no answer within {} s",
                            CLAIM_SEND_DEADLINE.as_secs()
                        ))
                    })
            };
            match result {
                Ok(took) => {
                    sent += took;
                    remove_if_there(&claim)?;
                }
                Err(e) => {
                    // A claim that is gone was cleared: nothing goes back.
                    let _held = self.hold()?;
                    if claim.exists() {
                        self.put_back(lines[settled..].to_vec(), prev.as_deref(), next.as_deref())?;
                        remove_if_there(&claim)?;
                    }
                    return Err(e);
                }
            }
        }
    }
}

/// The non-empty lines of `path`; none when it is gone.
fn read_lines(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

/// Write `lines` to `path` whole: through a temporary file renamed over it.
fn write_lines(path: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let tmp = unique_tmp(path);
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn remove_if_there(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A temporary file beside `path` that no other writer uses: this process's id and a count of its own. The engine
/// and the agent write beside the same files, and one renaming the other's half-written file over the real one, or
/// away from under it, lost a write.
fn unique_tmp(path: &std::path::Path) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}
