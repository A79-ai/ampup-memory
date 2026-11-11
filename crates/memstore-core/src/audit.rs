//! The audit log: who changed access, who signed in, who was refused, and which key read or wrote what.
//!
//! Seven kinds of event, one view (`memstore audit --org <org>`, the `/admin` op `audit`):
//!
//! - `change`: a directory change (`memory_system.directory`, already an append-only log with each
//!   change's time, actor and org). It is read from there, not copied.
//! - `signin`: a person's identity-provider token, first seen by a server in an hour for an org.
//! - `refusal`: a 401 or 403: the caller (`-` when no credential proved who they are), the route, the
//!   status and why ([`Reason`]).
//! - `key`: a service key's use, per key, route and hour: how many reads and writes. A recorder key's use
//!   for a member it named (`x-memstore-as`) is its own row, per member, naming them.
//! - `support`: a break-glass support session the deployment agent saw open, close or expire, and each
//!   diagnostic it ran in one (`crate::agent::support`). The agent writes them with the operator's admin
//!   key (the `/admin` op `support_event`, [`SupportEvent`]); they belong to no org, so they show in the
//!   operator's view (`memstore audit --operator`).
//! - `read`: one request that read memory: who, which memory, through which route or tool, how many items
//!   came back and (up to [`MAX_READ_IDS`]) which episodes they were from. Ids are the store's own, never
//!   anything the caller typed. A request whose read doesn't go through the shared read path (a query, a
//!   brief) is a row with no episodes.
//! - `maintenance`: a run of the server's own storage maintenance that rewrites a system table (the ledger's
//!   compaction, [`maintenance`]): what it did, in counts only, and how it ended. Under no org, by the
//!   server (`server`): the operator's view.
//!
//! An event belongs to an org only if its caller is authenticated as belonging to that org (a member, one
//! of its keys, or someone its provider signed in); the server decides that against the directory.
//! Anything else (anonymous callers, outsiders probing an org) is recorded under no org, in the operator's
//! view of the table only. So nobody can write into another org's audit, and an org's admins never see
//! rows made by callers outside their org.
//!
//! Nothing a caller types is stored. A row holds values from fixed sets only: a reason code
//! ([`Reason`]), a route template ([`route`], never the query string), an MCP tool from the server's own
//! list ([`mcp_path`]), a provider code, and org and scope names the directory resolved. The caller column
//! holds the identity the server verified (a provider-verified email, `key:<org>/<name>`, a file key's id,
//! or `-`), cleaned ([`clean`]). Text for people is made from the codes when the audit is read.
//!
//! The last three kinds are frequent, so a server buffers them in memory ([`Recorder`]) and appends them
//! to their own table (`memory_system.audit`) in one write every so often (`AMPUP_MEMORY_AUDIT_FLUSH_SECS`,
//! default 60), on shutdown, and when an audit is read through it (at most once every [`FORCED_GAP`]: in
//! between, a read shows what is written plus this server's buffer). Recording never waits on the
//! catalog and never fails a request: a write that fails is logged and retried with the next one, and
//! past [`MAX_PENDING`] buffered rows new events are dropped (and counted in the log) rather than held.
//! Refusals, which anyone can cause, get half of that and sign-ins and key use the other half.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use arrow_array::{
    Array, ArrayRef, Int64Array, RecordBatch, StringArray, cast::AsArray, types::Int64Type,
};
use iceberg::spec::{PrimitiveType, Schema};
use serde_json::{Value, json};

use crate::people::directory::Directory;
use crate::{OnConflict, SYSTEM_NS, Store, append, optional_strings, schema, timestamps, ts};

/// The high-volume events' table (`memory_system.audit`).
pub const AUDIT: &str = "audit";
/// Buffered rows a server holds at most; beyond this, new events are dropped (and logged) until a write
/// lands, so a flood of refusals can't grow memory without bound.
pub const MAX_PENDING: usize = 10_000;
/// Buffered `read` rows a server holds at most, apart from [`MAX_PENDING`] so a burst of reads can't push
/// out sign-ins and refusals.
pub const MAX_READS: usize = 10_000;
/// Rows of their own one org's identity provider's refused callers get in an hour on a server, whatever the
/// writes in between; past them, its callers' refusals are counted together in one row under the provider
/// ([`Recorder::refusal_via`]). The provider can mint any name, so its names alone can't take the refusals' half
/// of the buffer from everyone else's, nor fill the audit a buffer at a time (one written every minute, or every
/// [`FORCED_GAP`] while someone reads the audit).
pub const MAX_REFUSED_PER_PROVIDER: usize = 100;
/// Episode ids one `read` row names at most; its item count says how many there were.
pub const MAX_READ_IDS: usize = 100;
/// How often a server writes its buffered events, unless `AMPUP_MEMORY_AUDIT_FLUSH_SECS` says otherwise.
pub const DEFAULT_FLUSH: Duration = Duration::from_secs(60);
/// An audit read makes its server write its buffer at most this often.
pub const FORCED_GAP: Duration = Duration::from_secs(5);
const HOUR_US: i64 = 3_600_000_000;
/// Longest stored caller id (characters).
const MAX_TEXT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Change,
    Signin,
    Refusal,
    Key,
    Support,
    Maintenance,
    Read,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Change => "change",
            Kind::Signin => "signin",
            Kind::Refusal => "refusal",
            Kind::Key => "key",
            Kind::Support => "support",
            Kind::Maintenance => "maintenance",
            Kind::Read => "read",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "change" => Kind::Change,
            "signin" => Kind::Signin,
            "refusal" => Kind::Refusal,
            "key" => Kind::Key,
            "support" => Kind::Support,
            "maintenance" => Kind::Maintenance,
            "read" => Kind::Read,
            _ => {
                anyhow::bail!(
                    "an audit kind is change, signin, refusal, key, read, support or maintenance, got {s:?}"
                )
            }
        })
    }
}

/// Why a request was refused: stored as its code, described when read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    NoCredential,
    Unauthenticated,
    Expired,
    Revoked,
    Disabled,
    Origin,
    NotAdminKey,
    NotMember,
    ProviderRequired,
    IngestRead,
    ServiceKeyAdmin,
    AuditDenied,
    AdminDenied,
    RateLimited,
    OperatorOnly,
    DirectoryKeyData,
    AsDenied,
    AsNotMember,
    RecorderAlone,
}

const REASONS: [Reason; 19] = [
    Reason::NoCredential,
    Reason::Unauthenticated,
    Reason::Expired,
    Reason::Revoked,
    Reason::Disabled,
    Reason::Origin,
    Reason::NotAdminKey,
    Reason::NotMember,
    Reason::ProviderRequired,
    Reason::IngestRead,
    Reason::ServiceKeyAdmin,
    Reason::AuditDenied,
    Reason::AdminDenied,
    Reason::RateLimited,
    Reason::OperatorOnly,
    Reason::DirectoryKeyData,
    Reason::AsDenied,
    Reason::AsNotMember,
    Reason::RecorderAlone,
];

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Reason::NoCredential => "no_credential",
            Reason::Unauthenticated => "unauthenticated",
            Reason::Expired => "expired",
            Reason::Revoked => "revoked",
            Reason::Disabled => "disabled",
            Reason::Origin => "origin",
            Reason::NotAdminKey => "not_admin_key",
            Reason::NotMember => "not_member",
            Reason::ProviderRequired => "provider_required",
            Reason::IngestRead => "ingest_read",
            Reason::ServiceKeyAdmin => "service_key_admin",
            Reason::AuditDenied => "audit_denied",
            Reason::AdminDenied => "admin_denied",
            Reason::RateLimited => "rate_limited",
            Reason::OperatorOnly => "operator_only",
            Reason::DirectoryKeyData => "directory_key_data",
            Reason::AsDenied => "as_denied",
            Reason::AsNotMember => "as_not_member",
            Reason::RecorderAlone => "recorder_alone",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        REASONS.into_iter().find(|r| r.code() == code)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Reason::NoCredential => "no credential",
            Reason::Unauthenticated => "credential not accepted",
            Reason::Expired => "credential expired",
            Reason::Revoked => "key revoked",
            Reason::Disabled => "user is disabled",
            Reason::Origin => "origin not allowed",
            Reason::NotAdminKey => "needs the operator's admin key",
            Reason::NotMember => "not a member of the org or team selected",
            Reason::ProviderRequired => {
                "the org takes sign-ins through its own identity provider only"
            }
            Reason::IngestRead => "an ingest key doesn't read memory",
            Reason::ServiceKeyAdmin => "a service key doesn't manage the org or read its audit",
            Reason::AuditDenied => "only the org's admins may read its audit",
            Reason::AdminDenied => "the directory request was refused",
            Reason::RateLimited => "over the caller's rate limit (429)",
            Reason::OperatorOnly => "only the operator's admin key may read this",
            Reason::DirectoryKeyData => {
                "a directory key manages members only: it reads and writes no memory"
            }
            Reason::AsDenied => {
                "x-memstore-as is a recorder key's, on runs, rewards, advice and sessions only"
            }
            Reason::AsNotMember => {
                "x-memstore-as named no member of the recorder key's org or team (or a disabled one)"
            }
            Reason::RecorderAlone => {
                "a recorder key acts only for a member it names in x-memstore-as"
            }
        }
    }

    /// A 401's reason, from the server's own error (only which fixed code it is: none of its text).
    pub fn of_401(error: &str) -> Self {
        if error.contains("disabled") {
            Reason::Disabled
        } else if error.contains("revoked") {
            Reason::Revoked
        } else if error.contains("expired") {
            Reason::Expired
        } else if error.contains("not an admin key") {
            Reason::NotAdminKey
        } else {
            Reason::Unauthenticated
        }
    }
}

/// The routes the server answers on (`other`: anything else). The probes' ([`PROBES`]) among them, so their
/// answers are counted apart from callers'.
const ROUTES: [&str; 27] = [
    "/ready",
    "/health",
    "/healthz",
    "/search",
    "/agent/run",
    "/query",
    "/episode",
    "/mcp",
    "/admin",
    "/me",
    "/acl",
    "/retract",
    "/team",
    "/closure",
    "/acl_roles",
    "/whoami",
    "/metrics",
    "/browse",
    "/telemetry",
    "/brief",
    "/ingest",
    "/log_answer",
    "/advice",
    "/runs",
    "/runs/reward",
    "/sessions",
    "/sessions/end",
];

/// The routes the kubelet's probes ask: readiness (`/ready`, 503 while the server warms up or stops: the
/// answer the probe is for, never sent to a caller, who is routed only to a ready server) and liveness.
pub const PROBES: [&str; 3] = ["/ready", "/health", "/healthz"];

/// A request path as it may be stored: one of the server's routes, never the caller's own text.
pub fn route(path: &str) -> &'static str {
    ROUTES.into_iter().find(|r| *r == path).unwrap_or("other")
}

/// The server's MCP tools, and the path a call of each is recorded under.
const MCP_TOOLS: [(&str, &str); 12] = [
    ("query_memory", "/mcp query_memory"),
    ("get_entity", "/mcp get_entity"),
    ("search_memory", "/mcp search_memory"),
    ("get_episode", "/mcp get_episode"),
    ("ask_memory", "/mcp ask_memory"),
    ("remember_memory", "/mcp remember_memory"),
    ("forget_memory", "/mcp forget_memory"),
    ("close_memory", "/mcp close_memory"),
    ("skills", "/mcp skills"),
    ("get_brief", "/mcp get_brief"),
    ("log_answer", "/mcp log_answer"),
    ("get_advice", "/mcp get_advice"),
];

/// The MCP tools that write: a call of one counts as a write where a key's use is recorded.
pub const MCP_WRITES: [&str; 3] = [
    "/mcp remember_memory",
    "/mcp forget_memory",
    "/mcp close_memory",
];

/// The path an MCP tool call is recorded under (`/mcp other` for a tool the server doesn't offer).
pub fn mcp_path(tool: &str) -> &'static str {
    MCP_TOOLS
        .into_iter()
        .find(|(t, _)| *t == tool)
        .map_or("/mcp other", |(_, p)| p)
}

fn known_path(p: &str) -> bool {
    p == "other"
        || p == "/mcp other"
        || ROUTES.contains(&p)
        || MCP_TOOLS.iter().any(|(_, q)| *q == p)
}

/// How a person signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// The server's own identity provider.
    Server,
    /// The org's own identity provider.
    Org,
    /// The platform's Auth0 tenant, for the Auth0 organization the org is bound to.
    Auth0,
}

impl Provider {
    fn code(self) -> &'static str {
        match self {
            Provider::Server => "server_provider",
            Provider::Org => "org_provider",
            Provider::Auth0 => "auth0",
        }
    }

    fn describe(code: &str) -> &'static str {
        match code {
            "server_provider" => "through the server's identity provider",
            "org_provider" => "through the org's own identity provider",
            "auth0" => "through the platform's Auth0 tenant",
            _ => "unknown",
        }
    }
}

/// One audit event.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// µs since the epoch: when it happened (`key`: the start of its hour).
    pub at: i64,
    pub kind: Kind,
    pub org: Option<String>,
    /// Who: a person's verified id, `key:<org>/<name>`, `file:<id>`, an operator key's user, or `-`.
    pub actor: String,
    /// `key`: the memory it used (`org:acme`, `team:acme/sales`).
    pub scope: Option<String>,
    /// `refusal`, `key`: the route (`/search`; `/mcp search_memory` for a tool call).
    pub path: Option<String>,
    /// `change`: what changed (made when read). `signin`: a provider code. `refusal`: a reason code. `key`:
    /// the member a recorder key acted for (`x-memstore-as`), if it did.
    pub detail: Option<String>,
    /// `refusal`: the HTTP status.
    pub status: Option<i64>,
    /// `refusal`: how many identical refusals this row stands for; otherwise 1.
    pub count: i64,
    /// `key`: reads and writes in the hour.
    pub reads: i64,
    pub writes: i64,
    /// Which org of that name: its incarnation ([`crate::people::directory::Org::created_at`]) in the
    /// directory the server authorized the caller against when it recorded the row. An org deleted and
    /// created again is another incarnation, and sees none of the old one's rows. None: no org, or a
    /// row written before incarnations were recorded.
    pub incarnation: Option<i64>,
}

/// An org a row is recorded under, and which incarnation of it.
pub type OrgOf<'a> = Option<(&'a str, i64)>;

impl Entry {
    fn new(at: i64, kind: Kind, org: Option<(String, i64)>, actor: String) -> Self {
        let (org, incarnation) = match org {
            Some((o, i)) => (Some(o), Some(i)),
            None => (None, None),
        };
        Entry {
            at,
            kind,
            org,
            incarnation,
            actor,
            scope: None,
            path: None,
            detail: None,
            status: None,
            count: 1,
            reads: 0,
            writes: 0,
        }
    }

    /// As the audit view shows it: codes become words here, and only known codes and routes show.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "at": rfc3339(self.at),
            "kind": self.kind.as_str(),
            "org": self.org,
            "actor": self.actor,
        });
        let o = v.as_object_mut().unwrap();
        if let Some(s) = &self.scope {
            o.insert("scope".into(), json!(s));
        }
        if let Some(p) = &self.path {
            o.insert(
                "path".into(),
                json!(if known_path(p) { p.as_str() } else { "other" }),
            );
        }
        let code = self.detail.as_deref().unwrap_or_default();
        match self.kind {
            Kind::Change => {
                o.insert("detail".into(), json!(code));
            }
            Kind::Signin => {
                o.insert("detail".into(), json!(Provider::describe(code)));
            }
            Kind::Refusal => {
                let r = Reason::parse(code);
                o.insert("reason".into(), json!(r.map_or("unknown", |r| r.code())));
                o.insert(
                    "detail".into(),
                    json!(r.map_or("unknown", |r| r.describe())),
                );
                o.insert("count".into(), json!(self.count));
            }
            Kind::Key => {
                o.insert("reads".into(), json!(self.reads));
                o.insert("writes".into(), json!(self.writes));
                if let Some(person) = &self.detail {
                    o.insert("as".into(), json!(person));
                }
            }
            Kind::Read => {
                o.insert("items".into(), json!(self.reads));
                let ids: Vec<&str> = self
                    .detail
                    .as_deref()
                    .unwrap_or_default()
                    .split(',')
                    .filter(|i| !i.is_empty())
                    .collect();
                o.insert("episodes".into(), json!(ids));
            }
            // Made by the server from counts and a fixed outcome ([`maintenance`]): shown as it is.
            Kind::Maintenance => {
                o.insert("detail".into(), json!(code));
            }
            Kind::Support => match SupportEvent::from_row(self) {
                Some(e) => {
                    o.insert("detail".into(), json!(e.describe()));
                    o.insert("event".into(), e.to_json());
                }
                None => {
                    o.insert("detail".into(), json!("unknown"));
                }
            },
        }
        if let Some(s) = self.status {
            o.insert("status".into(), json!(s));
        }
        v
    }
}

/// A row for a run of the server's storage maintenance: `what` ran (`ledger_compaction`), how it ended (`ok`,
/// `failed`, ...), and what it did as named numbers (`data_files_before=812`). Codes and numbers only, so the row
/// holds no stored value, caller text or credential. Under no org, by `server`.
pub fn maintenance(
    what: &'static str,
    outcome: &'static str,
    counts: &[(&'static str, u64)],
) -> Entry {
    let mut e = Entry::new(now_us(), Kind::Maintenance, None, "server".into());
    let mut detail = format!("{what} {outcome}");
    for (k, v) in counts {
        detail.push_str(&format!(" {k}={v}"));
    }
    e.detail = Some(detail);
    e
}

/// `2026-10-14T09:12:03Z`.
pub fn rfc3339(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn now_us() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

/// A caller id as it may be stored: no control characters, nothing shaped like a key or a token, bounded.
pub fn clean(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::with_capacity(s.len().min(MAX_TEXT));
    let mut rest = s.as_str();
    while !rest.is_empty() {
        let next = ["msk_", "eyJ"].iter().filter_map(|p| rest.find(p)).min();
        match next {
            None => {
                out.push_str(rest);
                break;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                let tail = &rest[i..];
                let end = tail
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric()
                            || matches!(c, '_' | '-' | '.' | '+' | '/' | '='))
                    })
                    .unwrap_or(tail.len());
                out.push_str("[redacted]");
                rest = &tail[end..];
            }
        }
    }
    out.chars().take(MAX_TEXT).collect()
}

/// Text for a terminal: control characters (tabs, newlines, escapes) shown as escapes, never acted on.
pub fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// An org name as it may be stored: a valid one, else none.
fn org_name(org: OrgOf) -> Option<(String, i64)> {
    org.filter(|(o, _)| crate::check_name("org", o).is_ok())
        .map(|(o, i)| (o.to_string(), i))
}

/// Refusals buffered: (org and incarnation, actor, route, status, reason) -> (first at, count).
type RefusalKey = (Option<(String, i64)>, String, &'static str, i64, Reason);
/// Key uses buffered: (org and incarnation, key, scope, route, hour, the member it acted for) -> (reads,
/// writes).
type KeyUseKey = (
    (String, i64),
    String,
    String,
    &'static str,
    i64,
    Option<String>,
);

#[derive(Default)]
struct Pending {
    refusals: HashMap<RefusalKey, (i64, i64)>,
    /// Per provider: when its callers' first refusal row of their own in this hour was made, and how many they
    /// have had since ([`MAX_REFUSED_PER_PROVIDER`]). Kept across writes: a write empties the buffer, not the count.
    vouched: HashMap<String, (i64, usize)>,
    signins: Vec<Entry>,
    /// (user, org) first seen in `seen_hour`.
    seen: std::collections::HashSet<(String, Option<(String, i64)>)>,
    seen_hour: i64,
    keys: HashMap<KeyUseKey, (i64, i64)>,
    /// Rows a failed write handed back, written with the next: refusals, and the rest.
    retry_refusals: Vec<Entry>,
    retry: Vec<Entry>,
    /// Support session events, written as they come ([`Recorder::support`]).
    support: Vec<Entry>,
    /// Reads ([`Recorder::read`]), one row each, capped apart at [`MAX_READS`].
    reads: Vec<Entry>,
    /// Events dropped because the buffer was full, since the last log line about it.
    dropped: u64,
}

impl Pending {
    /// Buffered rows: refusals, and the rest.
    pub fn rows(&self) -> (usize, usize) {
        (
            self.refusals.len() + self.retry_refusals.len(),
            self.signins.len() + self.keys.len() + self.retry.len() + self.support.len(),
        )
    }

    /// Room for one more row (a refusal, or another kind); if not, count it as dropped.
    fn room(&mut self, refusal: bool) -> bool {
        let (r, other) = self.rows();
        let ok = if refusal { r } else { other } < MAX_PENDING / 2;
        if !ok {
            self.dropped += 1;
        }
        ok
    }

    /// What is buffered, as rows.
    fn entries(&self) -> Vec<Entry> {
        let mut rows = self.retry.clone();
        rows.extend(self.retry_refusals.iter().cloned());
        rows.extend(self.signins.iter().cloned());
        rows.extend(self.support.iter().cloned());
        rows.extend(self.reads.iter().cloned());
        for ((org, actor, path, status, reason), (at, n)) in &self.refusals {
            let mut e = Entry::new(*at, Kind::Refusal, org.clone(), actor.clone());
            e.path = Some(path.to_string());
            e.status = Some(*status);
            e.detail = Some(reason.code().into());
            e.count = *n;
            rows.push(e);
        }
        for ((org, key, scope, path, hour, person), (r, w)) in &self.keys {
            let mut e = Entry::new(*hour, Kind::Key, Some(org.clone()), key.clone());
            e.scope = Some(scope.clone());
            e.path = Some(path.to_string());
            e.detail = person.clone();
            e.reads = *r;
            e.writes = *w;
            rows.push(e);
        }
        rows
    }
}

/// A server's audit buffer: recording is a map update under a lock, never a catalog call.
#[derive(Default)]
pub struct Recorder {
    pending: Mutex<Pending>,
    /// One write at a time (the timer's, a reader's, shutdown's).
    writing: tokio::sync::Mutex<()>,
    /// When an audit read last made this server write.
    forced: Mutex<Option<Instant>>,
    /// The last write failed (its rows are kept for the next).
    failing: AtomicBool,
    /// Failed writes and dropped events, for `/metrics`.
    metrics: crate::metrics::AuditMetrics,
    /// Where each event also goes as it is flushed (a log shipper, a SIEM), if the deployment set one.
    sink: Mutex<Option<Sink>>,
}

/// Takes the rows a flush has just taken, once each, and the number of events dropped since the last one.
/// It must not block: a slow destination does its work elsewhere.
pub type Sink = Arc<dyn Fn(&[Entry], u64) + Send + Sync>;

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// A 401 or 403. `org`: the org the caller is authenticated as belonging to, if the refusal is about
    /// it (none otherwise). `actor` is `-` when no credential proved who the caller is.
    pub fn refusal(&self, org: OrgOf, actor: &str, route: &'static str, status: u16, why: Reason) {
        self.refusal_via(org, actor, None, route, status, why)
    }

    /// [`Recorder::refusal`] of a caller `provider` vouched for (an org's own identity provider, as
    /// `idp:<org>`): once that provider's callers have had [`MAX_REFUSED_PER_PROVIDER`] rows in an hour, across
    /// writes, the refusals of any other caller of it are counted in one row whose caller is `provider`.
    pub fn refusal_via(
        &self,
        org: OrgOf,
        actor: &str,
        provider: Option<&str>,
        route: &'static str,
        status: u16,
        why: Reason,
    ) {
        let org = if actor == "-" { None } else { org_name(org) };
        let key = (org, clean(actor), route, status as i64, why);
        let mut p = self.pending.lock().unwrap();
        if let Some((_, n)) = p.refusals.get_mut(&key) {
            *n += 1;
            return;
        }
        let provider = provider.filter(|_| actor != "-").map(clean);
        let now = now_us();
        let full = provider.as_ref().is_some_and(|v| {
            p.vouched
                .get(v)
                .is_some_and(|(since, n)| now - since < HOUR_US && *n >= MAX_REFUSED_PER_PROVIDER)
        });
        let (key, own) = match provider {
            Some(v) if full => ((key.0, v, route, status as i64, why), None),
            v => (key, v),
        };
        if let Some((_, n)) = p.refusals.get_mut(&key) {
            *n += 1;
        } else if p.room(true) {
            p.refusals.insert(key, (now, 1));
            if let Some(v) = own {
                let (since, n) = p.vouched.entry(v).or_insert((now, 0));
                if now - *since >= HOUR_US {
                    (*since, *n) = (now, 0);
                }
                *n += 1;
            }
        }
    }

    /// A person's identity-provider token was accepted in `org` (one they belong to); recorded the first
    /// time this server sees them in an hour for it.
    pub fn signin(&self, org: OrgOf, user: &str, via: Provider) {
        let at = now_us();
        let hour = at.div_euclid(HOUR_US);
        let (org, user) = (org_name(org), clean(user));
        let mut p = self.pending.lock().unwrap();
        if p.seen_hour != hour {
            p.seen.clear();
            p.seen_hour = hour;
        }
        if p.seen.contains(&(user.clone(), org.clone())) || !p.room(false) {
            return;
        }
        p.seen.insert((user.clone(), org.clone()));
        let mut e = Entry::new(at, Kind::Signin, org, user);
        e.detail = Some(via.code().into());
        p.signins.push(e);
    }

    /// A service key of `org` read (or wrote) `scope` through `route`.
    pub fn key_use(
        &self,
        org: (&str, i64),
        key: &str,
        scope: &str,
        route: &'static str,
        write: bool,
    ) {
        self.key_use_for(org, key, scope, route, write, None)
    }

    /// A recorder key of `org` read (or wrote) `scope` through `route` for `person`, a member it named
    /// (`x-memstore-as`) that the directory holds: counted apart from its other members' uses.
    pub fn key_use_as(
        &self,
        org: (&str, i64),
        key: &str,
        scope: &str,
        route: &'static str,
        write: bool,
        person: &str,
    ) {
        self.key_use_for(org, key, scope, route, write, Some(clean(person)))
    }

    fn key_use_for(
        &self,
        org: (&str, i64),
        key: &str,
        scope: &str,
        route: &'static str,
        write: bool,
        person: Option<String>,
    ) {
        let Some(org) = org_name(Some(org)) else {
            return;
        };
        let hour = now_us().div_euclid(HOUR_US) * HOUR_US;
        let k = (org, clean(key), clean(scope), route, hour, person);
        let mut p = self.pending.lock().unwrap();
        if !p.keys.contains_key(&k) && !p.room(false) {
            return;
        }
        let (r, w) = p.keys.entry(k).or_default();
        if write {
            *w += 1;
        } else {
            *r += 1;
        }
    }

    /// A request of `actor` (a person's verified id, `key:<org>/<name>` or `file:<id>`) read `scope` through
    /// `route`: `items` came back, from `episodes` (at most [`MAX_READ_IDS`] are kept, in order). One row
    /// per request, unlike a key's use, so who read what, and when, can be told.
    pub fn read(
        &self,
        org: OrgOf,
        actor: &str,
        scope: &str,
        route: &'static str,
        items: usize,
        episodes: &[String],
    ) {
        let Some(org) = org_name(org) else {
            return;
        };
        let mut e = Entry::new(now_us(), Kind::Read, Some(org), clean(actor));
        e.scope = Some(clean(scope));
        e.path = Some(route.to_string());
        e.reads = items as i64;
        let ids: Vec<&str> = episodes
            .iter()
            .map(String::as_str)
            .filter(|i| i.len() <= 64 && i.bytes().all(|b| b.is_ascii_hexdigit()))
            .take(MAX_READ_IDS)
            .collect();
        if !ids.is_empty() {
            e.detail = Some(ids.join(","));
        }
        let mut p = self.pending.lock().unwrap();
        if p.reads.len() >= MAX_READS {
            p.dropped += 1;
            return;
        }
        p.reads.push(e);
    }

    /// Also send each event to `sink` as it is flushed.
    pub fn set_sink(&self, sink: Sink) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    /// A support session event the deployment agent reported; buffered like the rest (the caller then
    /// writes it at once, [`Recorder::flush`]). Whether there was room for it.
    pub fn support(&self, e: &SupportEvent) -> bool {
        let mut p = self.pending.lock().unwrap();
        if !p.room(false) {
            return false;
        }
        p.support.push(e.to_row(now_us()));
        true
    }

    /// What this server holds and hasn't written yet.
    pub fn buffered(&self) -> Vec<Entry> {
        self.pending.lock().unwrap().entries()
    }

    /// Failed writes and dropped events.
    pub fn metrics(&self) -> &crate::metrics::AuditMetrics {
        &self.metrics
    }

    /// Whether the last write failed.
    pub fn failing(&self) -> bool {
        self.failing.load(Ordering::Relaxed)
    }

    /// Everything buffered, as rows; the buffer is left empty (sign-ins already seen stay seen).
    fn take(&self) -> (Vec<Entry>, usize, u64) {
        let mut p = self.pending.lock().unwrap();
        let rows = p.entries();
        // Rows a failed write handed back come first ([`Pending::entries`]); the sink has had them.
        let retried = p.retry.len() + p.retry_refusals.len();
        p.retry.clear();
        p.retry_refusals.clear();
        p.signins.clear();
        p.support.clear();
        p.reads.clear();
        p.refusals.clear();
        // Each provider's count outlives the write, until its hour is up.
        let now = now_us();
        p.vouched.retain(|_, (since, _)| now - *since < HOUR_US);
        p.keys.clear();
        (rows, retried, std::mem::take(&mut p.dropped))
    }

    /// Write what is buffered, in one append. On failure the rows go back to be written next time (as many
    /// as fit), and the error is returned for the caller to log: nothing a request waits on.
    pub async fn flush(&self, store: &Store) -> Result<usize> {
        let _one = self.writing.lock().await;
        let (rows, retried, dropped) = self.take();
        // The sink gets each event once, before the catalog write, so a catalog that is down loses nothing
        // the sink can keep, and what couldn't be buffered is said as a count.
        if let Some(sink) = self.sink.lock().unwrap().clone() {
            sink(&rows[retried..], dropped);
        }
        if dropped > 0 {
            self.metrics.dropped.inc_by(dropped);
            eprintln!(
                "audit: {dropped} events dropped: the buffer held {MAX_PENDING} rows the catalog hadn't taken"
            );
        }
        if rows.is_empty() {
            return Ok(0);
        }
        let n = rows.len();
        match store.append_audit(&rows).await {
            Ok(()) => {
                self.failing.store(false, Ordering::Relaxed);
                Ok(n)
            }
            Err(e) => {
                self.failing.store(true, Ordering::Relaxed);
                self.metrics.flush_failures.inc();
                let mut guard = self.pending.lock().unwrap();
                let p = &mut *guard;
                let (r, other) = p.rows();
                let (mut room_r, mut room) = (
                    (MAX_PENDING / 2).saturating_sub(r),
                    (MAX_PENDING / 2).saturating_sub(other),
                );
                for row in rows {
                    let (left, to) = if row.kind == Kind::Refusal {
                        (&mut room_r, &mut p.retry_refusals)
                    } else {
                        (&mut room, &mut p.retry)
                    };
                    if *left > 0 {
                        *left -= 1;
                        to.push(row);
                    } else {
                        p.dropped += 1;
                    }
                }
                Err(e)
            }
        }
    }

    /// The write an audit read asks for: at most once every [`FORCED_GAP`] per server (otherwise none,
    /// and the read shows the buffer alongside what is written). Whether it wrote.
    pub async fn flush_for_read(&self, store: &Store) -> Result<bool> {
        {
            let mut last = self.forced.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < FORCED_GAP) {
                return Ok(false);
            }
            *last = Some(Instant::now());
        }
        self.flush(store).await.map(|_| true)
    }

    /// Write every `every`, for as long as the server runs; a failure is a warning, retried next time.
    pub async fn flush_every(self: Arc<Self>, every: Duration, store: impl Fn() -> Arc<Store>) {
        loop {
            tokio::time::sleep(every).await;
            if let Err(e) = self.flush(&store()).await {
                eprintln!("audit: events not written yet (kept, retried in {every:?}): {e:#}");
            }
        }
    }
}

/// The flush interval: `AMPUP_MEMORY_AUDIT_FLUSH_SECS`, else [`DEFAULT_FLUSH`].
pub fn flush_interval() -> Duration {
    std::env::var("AMPUP_MEMORY_AUDIT_FLUSH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(DEFAULT_FLUSH, Duration::from_secs)
}

fn audit_schema() -> Result<Schema> {
    schema(&[
        ("at", true, PrimitiveType::Timestamptz),
        ("kind", true, PrimitiveType::String),
        ("org", false, PrimitiveType::String),
        ("actor", true, PrimitiveType::String),
        ("scope", false, PrimitiveType::String),
        ("path", false, PrimitiveType::String),
        ("detail", false, PrimitiveType::String),
        ("status", false, PrimitiveType::Long),
        ("count", false, PrimitiveType::Long),
        ("reads", false, PrimitiveType::Long),
        ("writes", false, PrimitiveType::Long),
        ("org_created_at", false, PrimitiveType::Long),
    ])
}

fn longs(b: &arrow_array::RecordBatch, col: &str) -> Vec<Option<i64>> {
    match b.column_by_name(col) {
        Some(a) if a.data_type() == &arrow_schema::DataType::Int64 => {
            let a = a.as_primitive::<Int64Type>();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                .collect()
        }
        _ => vec![None; b.num_rows()],
    }
}

/// Whether an audit row of `org`, recorded under the incarnation `incarnation` (the `at` of the org's
/// `org.create` then), is of an org `dir` has now, as that incarnation. An org deleted since, or deleted and
/// created again, is gone: its delete removed its rows, so no server writes it more of them, however long its
/// buffer held them. A row of no org, and an org's row from before rows carried the incarnation, need the org only.
pub fn row_stays(dir: &Directory, org: Option<&str>, incarnation: Option<i64>) -> bool {
    org.is_none_or(|o| {
        dir.orgs
            .get(o)
            .is_some_and(|live| incarnation.is_none_or(|i| i == live.created_at))
    })
}

/// Whether a key usage row of `org`'s key `sha256` is of a key `dir` has now in that org. A deleted org's keys
/// go with it, and an org created again under its name holds none of them.
pub fn key_use_stays(dir: &Directory, org: &str, sha256: &str) -> bool {
    dir.orgs
        .get(org)
        .is_some_and(|o| o.keys.values().any(|k| k.sha256 == sha256))
}

/// What, beside its org, tells which org a system table's row is of ([`Store::unwrite_gone`]): the audit's
/// incarnation, a key usage row's key.
pub(crate) trait KeyOf: Sized {
    fn stays(&self, dir: &Directory, org: &str) -> bool;
    fn column(b: &RecordBatch) -> Vec<Self>;
}

impl KeyOf for Option<i64> {
    fn stays(&self, dir: &Directory, org: &str) -> bool {
        row_stays(dir, Some(org), *self)
    }
    fn column(b: &RecordBatch) -> Vec<Self> {
        longs(b, "org_created_at")
    }
}

impl KeyOf for String {
    fn stays(&self, dir: &Directory, org: &str) -> bool {
        key_use_stays(dir, org, self)
    }
    fn column(b: &RecordBatch) -> Vec<Self> {
        crate::strings(b, "sha256")
    }
}

impl Store {
    /// Append audit rows (`memory_system.audit`), all in one write. Rows of an org the directory, read now, no
    /// longer has as they were recorded ([`row_stays`]) are left out; any the org's delete committed while they
    /// were written are taken out again ([`Store::unwrite_gone`]).
    pub async fn append_audit(&self, rows: &[Entry]) -> Result<()> {
        let of_orgs = rows.iter().any(|e| e.org.is_some());
        let kept: Vec<Entry> = match of_orgs {
            true => {
                let dir = Directory::fresh(self).await?;
                rows.iter()
                    .filter(|e| row_stays(&dir, e.org.as_deref(), e.incarnation))
                    .cloned()
                    .collect()
            }
            false => rows.to_vec(),
        };
        let rows = &kept[..];
        if rows.is_empty() {
            return Ok(());
        }
        self.ensure_table(SYSTEM_NS, AUDIT, audit_schema()?).await?;
        self.no_side_table
            .lock()
            .unwrap()
            .remove(&format!("{SYSTEM_NS}.{AUDIT}"));
        let text = |f: &dyn Fn(&Entry) -> Option<&str>| -> ArrayRef {
            Arc::new(StringArray::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let long = |f: &dyn Fn(&Entry) -> Option<i64>| -> ArrayRef {
            Arc::new(Int64Array::from(rows.iter().map(f).collect::<Vec<_>>()))
        };
        let table = self.table(SYSTEM_NS, AUDIT).await?;
        append(
            &*self.catalog(),
            &table,
            vec![
                ("at", ts(rows.iter().map(|e| Some(e.at)))),
                ("kind", text(&|e| Some(e.kind.as_str()))),
                ("org", text(&|e| e.org.as_deref())),
                ("actor", text(&|e| Some(e.actor.as_str()))),
                ("scope", text(&|e| e.scope.as_deref())),
                ("path", text(&|e| e.path.as_deref())),
                ("detail", text(&|e| e.detail.as_deref())),
                ("status", long(&|e| e.status)),
                ("count", long(&|e| Some(e.count))),
                ("reads", long(&|e| Some(e.reads))),
                ("writes", long(&|e| Some(e.writes))),
                ("org_created_at", long(&|e| e.incarnation)),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(SYSTEM_NS, AUDIT);
        if of_orgs {
            let written: BTreeSet<(String, Option<i64>)> = rows
                .iter()
                .filter_map(|e| Some((e.org.clone()?, e.incarnation)))
                .collect();
            self.unwrite_gone(AUDIT, written).await;
        }
        Ok(())
    }

    /// After rows of the orgs in `written` (org, incarnation, as [`row_stays`] takes them) were appended to
    /// `memory_system.<table>` (the audit or key usage table): the directory is read again, and the rows of any
    /// of them that is gone now are removed again. An org's delete commits to the directory before it rewrites
    /// these tables, so either it is in this read, or its rewrite comes after these rows landed and removes
    /// them itself. A key usage row's "incarnation" is its key's sha256, which only the org that holds the key
    /// keeps ([`key_use_stays`]). A failure is logged: the rows were written, and are not written again.
    pub(crate) async fn unwrite_gone<T>(&self, table: &str, written: BTreeSet<(String, T)>)
    where
        T: Ord + Clone + Send + Sync + KeyOf,
    {
        let gone: BTreeSet<(String, T)> = match Directory::fresh(self).await {
            Ok(dir) => written
                .into_iter()
                .filter(|(org, k)| !k.stays(&dir, org))
                .collect(),
            Err(e) => {
                eprintln!(
                    "{table}: not checked against org deletes after the write (directory unreadable): {e:#}"
                );
                return;
            }
        };
        if gone.is_empty() {
            return;
        }
        let removed = self
            .rewrite_table(SYSTEM_NS, table, &mut |b| {
                let orgs = optional_strings(b, "org");
                let keys = T::column(b);
                Ok(orgs
                    .into_iter()
                    .zip(keys)
                    .map(|(org, k)| Some(!org.is_some_and(|o| gone.contains(&(o, k)))))
                    .collect())
            })
            .await;
        match removed {
            Ok(n) => {
                eprintln!("{table}: {n} rows of orgs deleted while they were written removed again")
            }
            Err(e) => eprintln!(
                "{table}: rows of orgs deleted while they were written not removed: {e:#}"
            ),
        }
    }

    /// Every row of `memory_system.audit` (none before the first write).
    pub async fn audit_rows(&self) -> Result<Vec<Entry>> {
        let batches = self.side_rows(SYSTEM_NS, AUDIT).await?;
        let mut out = vec![];
        for b in batches.iter() {
            if b.column_by_name("at").is_none() {
                continue;
            }
            let at = timestamps(b, "at");
            let (kind, org, actor) = (
                optional_strings(b, "kind"),
                optional_strings(b, "org"),
                optional_strings(b, "actor"),
            );
            let (scope, path, detail) = (
                optional_strings(b, "scope"),
                optional_strings(b, "path"),
                optional_strings(b, "detail"),
            );
            let (status, count, reads, writes) = (
                longs(b, "status"),
                longs(b, "count"),
                longs(b, "reads"),
                longs(b, "writes"),
            );
            let incarnation = longs(b, "org_created_at");
            for i in 0..b.num_rows() {
                let Some(kind) = kind[i].as_deref().and_then(|k| Kind::parse(k).ok()) else {
                    continue;
                };
                out.push(Entry {
                    at: at[i].unwrap_or_default(),
                    kind,
                    org: org[i].clone(),
                    actor: actor[i].clone().unwrap_or_default(),
                    scope: scope[i].clone(),
                    path: path[i].clone(),
                    detail: detail[i].clone(),
                    status: status[i],
                    count: count[i].unwrap_or(1),
                    reads: reads[i].unwrap_or_default(),
                    writes: writes[i].unwrap_or_default(),
                    incarnation: incarnation[i],
                });
            }
        }
        Ok(out)
    }
}

/// An org's audit since `since` (µs since the epoch), oldest first, optionally of one kind: its
/// directory changes, the events servers wrote, and `buffered` (a server's events not written yet). A
/// key's uses in one hour, in several rows (one per write, per server), come back as one. The second
/// value lists what couldn't be read.
pub async fn collect(
    store: &Store,
    org: &str,
    since: i64,
    kind: Option<Kind>,
    buffered: Vec<Entry>,
) -> Result<(Vec<Entry>, Vec<String>)> {
    let wants = |k: Kind| kind.is_none_or(|w| w == k);
    let mut out = vec![];
    let mut warnings = vec![];
    // A name taken again is another org, another incarnation: the time the log stamped on its latest
    // `org.create` (0: the default org), as the directory fold has it ([`Org::created_at`]): the same fold
    // that stamped the audit rows when they were recorded. Audit rows carry the incarnation they were
    // recorded for, whenever and by whichever server they were written (a lagging replica's rows for the
    // deleted org, written after its purge, are not this org's).
    //
    // [`Org::created_at`]: crate::people::directory::Org::created_at
    let folded = store.directory().await?;
    let Some(incarnation) = folded.dir.orgs.get(org).map(|o| o.created_at) else {
        // Deleted (its audit rows went with it), or never made: no incarnation's rows are its.
        warnings.push(format!("the directory has no org {org}"));
        return Ok((out, warnings));
    };
    let ours = |e: &Entry| match e.incarnation {
        Some(i) => i == incarnation,
        // Written before rows carried one: only after this org's creation (a key row's whole hour).
        None => e.at >= incarnation,
    };
    if wants(Kind::Change) {
        // Only this incarnation's changes, read from the log's files since then.
        for e in store.directory_events_since(since.max(incarnation)).await? {
            if e.org != org {
                continue;
            }
            // What changed, in words: never the event's raw detail, which holds a key's hash or a
            // provider's (sealed) client secret.
            let mut a = Entry::new(
                e.at,
                Kind::Change,
                Some((e.org.clone(), incarnation)),
                clean(&e.actor),
            );
            a.detail = Some(clean(&e.describe()));
            out.push(a);
        }
    }
    if [Kind::Signin, Kind::Refusal, Kind::Key, Kind::Read]
        .into_iter()
        .any(wants)
    {
        let mut rows = match store.audit_rows().await {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("audit: table unreadable: {e:#}");
                warnings.push(
                    "written sign-ins, refusals and key use are unavailable (see the server's log)"
                        .to_string(),
                );
                vec![]
            }
        };
        rows.extend(buffered);
        let mut keys: Vec<Entry> = vec![];
        for e in rows {
            if e.org.as_deref() != Some(org) || !wants(e.kind) || !ours(&e) {
                continue;
            }
            if e.kind == Kind::Key {
                // An hour that ends after `since` had uses after it.
                if e.at + HOUR_US <= since {
                    continue;
                }
                // A recorder key's uses for each member it acted for stay apart.
                match keys.iter_mut().find(|k| {
                    (k.at, &k.actor, &k.scope, &k.path, &k.detail)
                        == (e.at, &e.actor, &e.scope, &e.path, &e.detail)
                }) {
                    Some(k) => {
                        k.reads += e.reads;
                        k.writes += e.writes;
                    }
                    None => keys.push(e),
                }
            } else if e.at >= since {
                out.push(e);
            }
        }
        out.extend(keys);
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.kind.as_str().cmp(b.kind.as_str())));
    Ok((out, warnings))
}

/// The operator's view (`memstore audit --operator`): the rows that belong to no org (refusals of
/// anonymous callers and of outsiders, whatever org they named), since `since`, oldest first, optionally
/// of one kind, with `buffered` (a server's events not written yet). The second value lists what
/// couldn't be read.
pub async fn collect_operator(
    store: &Store,
    since: i64,
    kind: Option<Kind>,
    buffered: Vec<Entry>,
) -> Result<(Vec<Entry>, Vec<String>)> {
    let mut warnings = vec![];
    let mut rows = match store.audit_rows().await {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("audit: table unreadable: {e:#}");
            warnings.push(
                "written sign-ins, refusals and key use are unavailable (see the server's log)"
                    .to_string(),
            );
            vec![]
        }
    };
    rows.extend(buffered);
    let mut out: Vec<Entry> = rows
        .into_iter()
        .filter(|e| e.org.is_none() && e.kind != Kind::Change)
        .filter(|e| kind.is_none_or(|k| k == e.kind) && e.at >= since)
        .collect();
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.kind.as_str().cmp(b.kind.as_str())));
    Ok((out, warnings))
}

/// A support session event, as the deployment agent reports it (the `/admin` op `support_event`) and as
/// it is stored. A closed shape: every field is an id of a fixed form, a value from a fixed set, a code,
/// or `by`, the person the control plane named (cleaned like any caller id). Nothing else is taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportEvent {
    /// `support_session` (seen `open`, `closed` or `expired`) or `support_command` (a diagnostic run:
    /// `ok`, `failed` or `refused`).
    pub kind: &'static str,
    /// `sup-` and 16 hex digits.
    pub session: String,
    /// `cmd-` and 16 hex digits (a command's only).
    pub command: Option<String>,
    /// One of [`DIAGNOSTICS`], else `other` (a command's only).
    pub diagnostic: Option<&'static str>,
    pub outcome: &'static str,
    /// `^[a-z][a-z0-9_]{0,47}$` (a command's only).
    pub code: Option<String>,
    pub by: String,
}

const SESSION_OUTCOMES: [&str; 3] = ["open", "closed", "expired"];
const COMMAND_OUTCOMES: [&str; 3] = ["ok", "failed", "refused"];

fn hex_id(prefix: &str, s: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|h| {
        h.len() == 16
            && h.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

fn one_of(set: &[&'static str], s: &str) -> Option<&'static str> {
    set.iter().copied().find(|v| *v == s)
}

impl SupportEvent {
    /// The event a request carries: `{kind, session, command?, diagnostic?, outcome, code?, by}` and
    /// nothing else.
    pub fn from_json(v: &Value) -> Result<Self> {
        let o = v
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("a support event is an object"))?;
        const FIELDS: [&str; 7] = [
            "kind",
            "session",
            "command",
            "diagnostic",
            "outcome",
            "code",
            "by",
        ];
        if let Some(k) = o.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            anyhow::bail!("a support event has no field {:?}", escape(k));
        }
        let text = |k: &str| -> Result<Option<&str>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.as_str())),
                _ => anyhow::bail!("{k} is a string"),
            }
        };
        let kind = match text("kind")? {
            Some("support_session") => "support_session",
            Some("support_command") => "support_command",
            _ => anyhow::bail!("kind is support_session or support_command"),
        };
        let session = text("session")?.unwrap_or_default();
        anyhow::ensure!(hex_id("sup-", session), "session is sup- and 16 hex digits");
        let by = text("by")?.unwrap_or_default();
        anyhow::ensure!(!by.trim().is_empty(), "by names who");
        let outcome = text("outcome")?.unwrap_or_default();
        let (command, diagnostic, code) = (text("command")?, text("diagnostic")?, text("code")?);
        let e = if kind == "support_session" {
            anyhow::ensure!(
                command.is_none() && diagnostic.is_none() && code.is_none(),
                "a session event names no command, diagnostic or code"
            );
            SupportEvent {
                kind,
                session: session.into(),
                command: None,
                diagnostic: None,
                outcome: one_of(&SESSION_OUTCOMES, outcome)
                    .ok_or_else(|| anyhow::anyhow!("a session is open, closed or expired"))?,
                code: None,
                by: clean(by),
            }
        } else {
            let command = command.unwrap_or_default();
            anyhow::ensure!(hex_id("cmd-", command), "command is cmd- and 16 hex digits");
            if let Some(c) = code {
                anyhow::ensure!(is_code(c), "not a code");
            }
            SupportEvent {
                kind,
                session: session.into(),
                command: Some(command.into()),
                diagnostic: Some(
                    one_of(&DIAGNOSTICS, diagnostic.unwrap_or_default()).unwrap_or("other"),
                ),
                outcome: one_of(&COMMAND_OUTCOMES, outcome)
                    .ok_or_else(|| anyhow::anyhow!("a command is ok, failed or refused"))?,
                code: code.map(String::from),
                by: clean(by),
            }
        };
        Ok(e)
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({"kind": self.kind, "session": self.session, "outcome": self.outcome,
                           "by": self.by});
        let o = v.as_object_mut().unwrap();
        if let Some(c) = &self.command {
            o.insert("command".into(), json!(c));
        }
        if let Some(d) = self.diagnostic {
            o.insert("diagnostic".into(), json!(d));
        }
        if let Some(c) = &self.code {
            o.insert("code".into(), json!(c));
        }
        v
    }

    /// For people: `session sup-… open`, `command cmd-… index_status: refused (expired)`.
    pub fn describe(&self) -> String {
        match &self.command {
            None => format!("session {} {}", self.session, self.outcome),
            Some(c) => format!(
                "session {} command {c} {}: {}{}",
                self.session,
                self.diagnostic.unwrap_or("other"),
                self.outcome,
                self.code
                    .as_ref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            ),
        }
    }

    /// As a row: no org (the operator's view), `by` as its actor, the session as its scope, and the rest
    /// as `detail`: `<kind> <command|-> <diagnostic|-> <outcome> <code|->`, each from its fixed form.
    fn to_row(&self, at: i64) -> Entry {
        let mut e = Entry::new(at, Kind::Support, None, self.by.clone());
        e.scope = Some(self.session.clone());
        e.detail = Some(format!(
            "{} {} {} {} {}",
            self.kind,
            self.command.as_deref().unwrap_or("-"),
            self.diagnostic.unwrap_or("-"),
            self.outcome,
            self.code.as_deref().unwrap_or("-"),
        ));
        e
    }

    /// A row read back, each value checked again (none: not a support event's row).
    fn from_row(e: &Entry) -> Option<Self> {
        let d = e.detail.as_deref()?;
        let parts: Vec<&str> = d.split(' ').collect();
        let [kind, command, diagnostic, outcome, code] = parts.as_slice() else {
            return None;
        };
        let opt = |s: &str| (s != "-").then(|| Value::String(s.into()));
        let mut v = json!({"kind": kind, "session": e.scope.as_deref()?, "outcome": outcome,
                           "by": e.actor});
        let o = v.as_object_mut().unwrap();
        for (k, s) in [
            ("command", command),
            ("diagnostic", diagnostic),
            ("code", code),
        ] {
            if let Some(x) = opt(s) {
                o.insert(k.into(), x);
            }
        }
        Self::from_json(&v).ok()
    }
}

/// A code (the deployment agent's, as the control plane takes it): `^[a-z][a-z0-9_]{0,47}$`.
pub fn is_code(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 48
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The diagnostics a support session (the deployment agent's) may ask for, by name. Every other name is refused.
pub const DIAGNOSTICS: [&str; 7] = [
    "health",
    "telemetry",
    "metrics",
    "index_status",
    "catalog_status",
    "config",
    "logs",
];
