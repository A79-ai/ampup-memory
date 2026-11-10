//! Break-glass support sessions: read-only diagnostics the control plane may ask for while a session a
//! customer approved is open.
//!
//! The agent asks `GET /v1/support` (a signed request, like `/v1/desired`) every round, and every
//! [`super::Config::support_poll`] while a session is open. The answer is signed by the control plane's
//! pinned key as the `support` kind ([`super::Kind::Support`]), names this deployment and is at most ten
//! minutes old, or nothing in it is acted on. A session lists commands, each a diagnostic by name; the
//! agent runs each command id at most once (kept in its state, like actions) and posts its result to
//! `POST /v1/support/<command id>/result`, again until the control plane has it.
//!
//! What a diagnostic may read is fixed here, not by the control plane: one of [`DIAGNOSTICS`], each a
//! read of the engine's own health, telemetry or metrics endpoints ([`ENGINE_READS`]) or of the engine's
//! container settings, and nothing else. No name the control plane sends reaches the engine: an unknown
//! one is refused (`unknown_diagnostic`) before any request is made, so `/query`, `/search` or a fact is
//! never read whatever the control plane says. A command after the session's `expires_at` is refused
//! (`expired`), never run. An output is a JSON object of at most [`MAX_OUTPUT`] bytes; a bigger one is
//! cut down (`"truncated": true`) or, if it can't be, not sent (`output_too_large`).
//!
//! Each session seen open, each command run (its diagnostic, outcome and code) and each session seen
//! closed or expired is logged, and mirrored into the engine's own audit log through the `/admin` op
//! `support_event` with the operator's admin key ([`super::Config::admin_key`]); `memstore audit
//! --operator` lists them.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Agent, Kind, Outcome, SupportMode};
use crate::deployment::Credential;
use crate::telemetry::{Envelope, Payload};

pub use crate::audit::DIAGNOSTICS;

/// The only requests the agent makes of the engine for a diagnostic: GETs of these paths. Anything else
/// is refused before it is sent.
pub const ENGINE_READS: [&str; 3] = ["/healthz", "/telemetry", "/metrics"];

/// The largest output posted, serialized.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// Support commands remembered (the oldest are forgotten first).
pub const SUPPORT_DONE_MAX: usize = 64;

/// The most of an engine answer read.
const MAX_ENGINE_ANSWER: usize = 8 << 20;

/// What `GET /v1/support` signs.
#[derive(Debug, Clone, Deserialize)]
pub struct Support {
    pub deployment: String,
    pub issued_at: DateTime<Utc>,
    pub session: Option<Session>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub id: String,
    pub requested_by: String,
    pub approved_by: String,
    /// The engineer's reason: shown in the control plane, never logged or sent on here.
    #[serde(default)]
    pub reason: Option<String>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Command {
    pub id: String,
    pub diagnostic: String,
    pub requested_by: String,
}

/// A command's result, as `POST /v1/support/:id/result` takes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupportResult {
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

impl SupportResult {
    fn of(outcome: Outcome, code: &str) -> Self {
        SupportResult {
            outcome,
            code: Some(code.into()),
            output: None,
        }
    }

    /// An ok result with `output`, bounded to [`MAX_OUTPUT`].
    fn ok(output: Value) -> Self {
        match bound(output) {
            Some(o) => SupportResult {
                outcome: Outcome::Ok,
                code: None,
                output: Some(o),
            },
            None => SupportResult::of(Outcome::Failed, "output_too_large"),
        }
    }
}

/// The session the agent last saw open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSession {
    pub id: String,
    pub requested_by: String,
    pub expires_at: DateTime<Utc>,
    /// Its expiry was logged (and audited) already.
    #[serde(default)]
    pub expired: bool,
}

/// A support command the agent took up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ran {
    pub id: String,
    pub session: String,
    /// One of [`DIAGNOSTICS`], else `other`.
    pub diagnostic: String,
    /// None: started and never finished; reported as failed (`interrupted`), not run again.
    pub result: Option<SupportResult>,
    pub posted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupportState {
    #[serde(default)]
    pub session: Option<OpenSession>,
    #[serde(default)]
    pub commands: Vec<Ran>,
}

impl SupportState {
    fn ran(&mut self, id: &str) -> Option<&mut Ran> {
        self.commands.iter_mut().find(|r| r.id == id)
    }
}

fn hex_id(prefix: &str, s: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|h| {
        h.len() == 16
            && h.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// A session id as the control plane makes them: `sup-` and 16 hex digits.
pub fn is_session_id(id: &str) -> bool {
    hex_id("sup-", id)
}

/// A command id: `cmd-` and 16 hex digits.
pub fn is_command_id(id: &str) -> bool {
    hex_id("cmd-", id)
}

/// A diagnostic's name as it may be logged and kept: one of [`DIAGNOSTICS`], else `other`.
pub fn diagnostic_name(name: &str) -> &'static str {
    DIAGNOSTICS
        .iter()
        .copied()
        .find(|d| *d == name)
        .unwrap_or("other")
}

/// `output` as it may be posted: as it is when it serializes to at most [`MAX_OUTPUT`] bytes; else with
/// entries dropped from its biggest map (the last first) and `"truncated": true`; none if even that is
/// too big (or it isn't an object).
pub fn bound(output: Value) -> Option<Value> {
    let size = |v: &Value| serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
    if !output.is_object() {
        return None;
    }
    if size(&output) <= MAX_OUTPUT {
        return Some(output);
    }
    let Value::Object(mut o) = output else {
        return None;
    };
    o.insert("truncated".into(), json!(true));
    let biggest = o
        .iter()
        .filter(|(_, v)| v.is_object())
        .max_by_key(|(_, v)| size(v))
        .map(|(k, _)| k.clone())?;
    let mut over = size(&Value::Object(o.clone())).saturating_sub(MAX_OUTPUT);
    while over > 0 {
        let Some(Value::Object(m)) = o.get_mut(&biggest) else {
            return None;
        };
        let last = m.keys().next_back().cloned()?;
        let v = m.remove(&last).unwrap_or_default();
        let freed = last.len() + size(&v) + 4;
        over = over.saturating_sub(freed);
        if over == 0 {
            over = size(&Value::Object(o.clone())).saturating_sub(MAX_OUTPUT);
        }
    }
    let out = Value::Object(o);
    (size(&out) <= MAX_OUTPUT).then_some(out)
}

/// The Prometheus text format summed by family: each sample's value added to its metric's name, labels
/// dropped (they can hold org names), histogram buckets left out (their `_sum` and `_count` stay).
pub fn metric_families(text: &str) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name_end = line.find(['{', ' ']).unwrap_or(line.len());
        let name = &line[..name_end];
        let ok_name = (1..=128).contains(&name.len())
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b':')
            && !name.as_bytes()[0].is_ascii_digit();
        if !ok_name || name.ends_with("_bucket") {
            continue;
        }
        // The value follows the labels (which may hold spaces inside quotes).
        let rest = match line[name_end..].strip_prefix('{') {
            Some(r) => match r.rfind('}') {
                Some(i) => &r[i + 1..],
                None => continue,
            },
            None => &line[name_end..],
        };
        let Some(Ok(v)) = rest.split_whitespace().next().map(str::parse::<f64>) else {
            continue;
        };
        if v.is_finite() {
            *out.entry(name.to_string()).or_default() += v;
        }
    }
    out
}

/// Variables whose values are shown: closed sets, numbers and model names, never a secret.
const SHOWN: [&str; 19] = [
    "MEMSTORE_PLATFORM",
    "MEMSTORE_CALLER_IDENTITY",
    "MEMSTORE_EMBED",
    "MEMSTORE_EMBED_MODEL",
    "MEMSTORE_EMBED_DIMS",
    "MEMSTORE_METRICS_PORT",
    "MEMSTORE_TELEMETRY",
    "MEMSTORE_TELEMETRY_INTERVAL_SECS",
    "MEMSTORE_TELEMETRY_INVENTORY_SECS",
    "MEMSTORE_ALERTS_EVAL_SECS",
    "MEMSTORE_AUDIT_FLUSH_SECS",
    "MEMSTORE_CATALOG_REFRESH_SECS",
    "MEMSTORE_CATALOG_FRESH_SECS",
    "MEMSTORE_RATE_LIMIT",
    "MEMSTORE_INGEST_RATE_LIMIT",
    "MEMSTORE_MAX_STALENESS_MS",
    "MEMSTORE_HARD_STALENESS_MS",
    "MEMSTORE_SLOW_REQUEST_MS",
    "MEMSTORE_MODEL_TIMEOUT_SECS",
];

/// Secrets: always listed, as `set` or `unset`, never their value.
const SECRETS: [&str; 14] = [
    "MEMSTORE_ADMIN_KEY",
    "MEMSTORE_API_KEYS",
    "MEMSTORE_EMBED_KEY",
    "MEMSTORE_EMBED_KEY_FILE",
    "MEMSTORE_JWT_SECRET",
    "MEMSTORE_SECRET_KEY",
    "MEMSTORE_FN_SECRET",
    "MEMSTORE_OIDC_CLIENT_SECRET",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "SNOWFLAKE_PAT",
    "DATABRICKS_TOKEN",
    "DATABRICKS_CLIENT_SECRET",
];

fn shown(name: &str) -> bool {
    SHOWN.contains(&name) || name.starts_with(super::FLAG_PREFIX)
}

fn safe_value(v: &str) -> bool {
    v.len() <= 128
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'/' | b'-'))
}

fn var_name(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && n.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        && !n.as_bytes()[0].is_ascii_digit()
}

/// The engine's settings as a support engineer may see them: every variable by name; a value only for
/// [`SHOWN`] names (and flags), and only when it is a plain token; `set` for the rest; each of
/// [`SECRETS`] as `set` or `unset`. A name that isn't a plain variable name is counted, not shown.
pub fn config_view(env: &[(String, Option<String>)]) -> Value {
    let mut vars: BTreeMap<String, &str> =
        SECRETS.iter().map(|s| (s.to_string(), "unset")).collect();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut other = 0u64;
    for (name, value) in env {
        if !var_name(name) {
            other += 1;
            continue;
        }
        match value.as_deref() {
            Some(v) if shown(name) && !SECRETS.contains(&name.as_str()) && safe_value(v) => {
                values.insert(name.clone(), v.to_string());
                vars.remove(name);
            }
            _ => {
                vars.insert(name.clone(), "set");
            }
        }
    }
    let mut out: Map<String, Value> = vars.into_iter().map(|(k, v)| (k, json!(v))).collect();
    out.extend(values.into_iter().map(|(k, v)| (k, json!(v))));
    json!({"variables": out, "unnamed_variables": other})
}

/// What a diagnostic read from the engine, or the code it failed or was refused with.
type Read<T> = std::result::Result<T, SupportResult>;

impl Agent {
    /// The support session alone (between full rounds while one is open).
    pub async fn support_round(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.halted,
            "halted: the control plane's key changed; restart the agent once the key is sorted out"
        );
        let cred = Credential::load(&self.cfg.credential)?;
        self.support(&cred).await
    }

    pub(super) async fn support(&mut self, cred: &Credential) -> Result<()> {
        let s: Support = self.fetch(cred, Kind::Support).await?;
        let now = Utc::now();
        let Some(session) = s.session else {
            if let Some(open) = self.state.support.session.take() {
                self.state.save(&self.cfg.state)?;
                if !open.expired {
                    let how = if open.expires_at <= now {
                        "expired"
                    } else {
                        "closed"
                    };
                    self.session_ended(&open, how).await;
                }
            }
            return self.repost(cred, &[]).await;
        };
        anyhow::ensure!(
            is_session_id(&session.id),
            "a support session id that isn't one; nothing run"
        );
        // The customer's policy, from the last desired state that verified: off, or none verified yet in
        // this process, means no session runs anything here, whatever the control plane sends. Each
        // command is still answered (refused) and audited, so both sides see that it was asked and not run.
        let off = self.support_mode != Some(SupportMode::OnRequest);
        if self.state.support.session.as_ref().map(|o| o.id.as_str()) != Some(&session.id) {
            if let Some(old) = self.state.support.session.take()
                && !old.expired
            {
                self.session_ended(&old, "closed").await;
            }
            eprintln!(
                "agent: support session {} {}: requested by {}, approved by {}, until {}",
                session.id,
                if off {
                    "refused by policy (support is off)"
                } else {
                    "open"
                },
                crate::audit::escape(&session.requested_by),
                crate::audit::escape(&session.approved_by),
                session.expires_at.to_rfc3339()
            );
            self.audit_engine(json!({"kind": "support_session", "session": session.id,
                                     "outcome": if off { "refused" } else { "open" },
                                     "code": if off { Some("support_off") } else { None },
                                     "by": session.requested_by}))
                .await;
            self.state.support.session = Some(OpenSession {
                id: session.id.clone(),
                requested_by: session.requested_by.clone(),
                expires_at: session.expires_at,
                expired: false,
            });
            self.state.save(&self.cfg.state)?;
        }
        let mut delivered = vec![];
        for c in &session.commands {
            if !is_command_id(&c.id) {
                eprintln!("agent: support: a command id that isn't one, skipped");
                continue;
            }
            delivered.push(c.id.clone());
            // Delivered again: its result never reached the control plane. Say it again, never run it again.
            if let Some(r) = self.state.support.ran(&c.id) {
                let result = r
                    .result
                    .clone()
                    .unwrap_or_else(|| SupportResult::of(Outcome::Failed, "interrupted"));
                r.result = Some(result.clone());
                self.post_support(cred, &c.id, &result).await?;
                continue;
            }
            let diagnostic = diagnostic_name(&c.diagnostic);
            self.state.support.commands.push(Ran {
                id: c.id.clone(),
                session: session.id.clone(),
                diagnostic: diagnostic.into(),
                result: None,
                posted: false,
            });
            let over = self
                .state
                .support
                .commands
                .len()
                .saturating_sub(SUPPORT_DONE_MAX);
            self.state.support.commands.drain(..over);
            self.state.save(&self.cfg.state)?;
            // Checked as each command starts: nothing runs once the session is over, or while the policy
            // says off.
            let result = if off {
                SupportResult::of(Outcome::Refused, "support_off")
            } else if Utc::now() >= session.expires_at {
                SupportResult::of(Outcome::Refused, "expired")
            } else {
                self.diagnose(&c.diagnostic).await
            };
            eprintln!(
                "agent: support session {} command {} {diagnostic} (requested by {}): {}{}",
                session.id,
                c.id,
                crate::audit::escape(&c.requested_by),
                outcome_str(result.outcome),
                result
                    .code
                    .as_ref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            );
            self.audit_engine(json!({"kind": "support_command", "session": session.id,
                                     "command": c.id, "diagnostic": diagnostic,
                                     "outcome": outcome_str(result.outcome), "code": result.code,
                                     "by": c.requested_by}))
                .await;
            if let Some(r) = self.state.support.ran(&c.id) {
                r.result = Some(result.clone());
            }
            self.state.save(&self.cfg.state)?;
            self.post_support(cred, &c.id, &result).await?;
        }
        if Utc::now() >= session.expires_at
            && let Some(open) = self.state.support.session.as_mut()
            && !open.expired
        {
            open.expired = true;
            let open = open.clone();
            self.state.save(&self.cfg.state)?;
            self.session_ended(&open, "expired").await;
        }
        self.repost(cred, &delivered).await
    }

    /// Results kept but not yet taken by the control plane, other than those just delivered.
    async fn repost(&mut self, cred: &Credential, delivered: &[String]) -> Result<()> {
        let waiting: Vec<(String, SupportResult)> = self
            .state
            .support
            .commands
            .iter()
            .filter(|r| !r.posted && !delivered.contains(&r.id))
            .filter_map(|r| Some((r.id.clone(), r.result.clone()?)))
            .collect();
        for (id, result) in waiting {
            self.post_support(cred, &id, &result).await?;
        }
        Ok(())
    }

    async fn session_ended(&self, s: &OpenSession, how: &str) {
        eprintln!("agent: support session {} {how}", s.id);
        self.audit_engine(
            json!({"kind": "support_session", "session": s.id, "outcome": how,
                                 "by": s.requested_by}),
        )
        .await;
    }

    async fn post_support(&mut self, cred: &Credential, id: &str, r: &SupportResult) -> Result<()> {
        let (status, _) = match cred
            .call(
                "POST",
                &format!("/v1/support/{id}/result"),
                &serde_json::to_value(r)?,
            )
            .await
        {
            Ok(a) => a,
            Err(e) => {
                eprintln!("agent: posting the result of {id}: {e:#}");
                return Ok(());
            }
        };
        // 404: the control plane has its result already (or the session is over); nothing more to say.
        if status == 200 || status == 404 {
            if let Some(d) = self.state.support.ran(id) {
                d.posted = true;
            }
            self.state.save(&self.cfg.state)?;
        } else {
            eprintln!("agent: the result of {id} was refused ({status})");
        }
        Ok(())
    }

    /// The engine's address: [`super::Config::engine_url`], else the origin of the health URL.
    fn engine_base(&self) -> Option<String> {
        if let Some(u) = &self.cfg.engine_url {
            return Some(u.trim_end_matches('/').to_string());
        }
        let u = reqwest::Url::parse(&self.cfg.health_url).ok()?;
        Some(u.origin().ascii_serialization())
    }

    /// The key diagnostics read the engine with: the admin key, else the health token.
    fn read_key(&self) -> Option<&str> {
        self.cfg
            .admin_key
            .as_deref()
            .or(self.cfg.health_token.as_deref())
    }

    /// A GET of one of [`ENGINE_READS`] (nothing else is ever sent): its status and body.
    async fn engine_get(&self, path: &str, key: Option<&str>) -> Read<(u16, Vec<u8>)> {
        if !ENGINE_READS.contains(&path) {
            return Err(SupportResult::of(Outcome::Refused, "unsupported"));
        }
        let base = self
            .engine_base()
            .ok_or_else(|| SupportResult::of(Outcome::Failed, "no_engine_url"))?;
        let mut req = self.http.get(format!("{base}{path}"));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let unreachable = |e: reqwest::Error| {
            eprintln!("agent: support: {path}: {e}");
            SupportResult::of(Outcome::Failed, "engine_unreachable")
        };
        let resp = req.send().await.map_err(unreachable)?;
        let status = resp.status().as_u16();
        if resp
            .content_length()
            .is_some_and(|n| n as usize > MAX_ENGINE_ANSWER)
        {
            return Err(SupportResult::of(
                Outcome::Failed,
                "engine_answer_too_large",
            ));
        }
        let body = resp.bytes().await.map_err(unreachable)?;
        if body.len() > MAX_ENGINE_ANSWER {
            return Err(SupportResult::of(
                Outcome::Failed,
                "engine_answer_too_large",
            ));
        }
        Ok((status, body.to_vec()))
    }

    /// A GET that must answer 200, with the admin key.
    async fn engine_read(&self, path: &str) -> Read<Vec<u8>> {
        let Some(key) = self.read_key() else {
            return Err(SupportResult::of(Outcome::Refused, "no_admin_key"));
        };
        match self.engine_get(path, Some(key)).await? {
            (200, b) => Ok(b),
            (401 | 403, _) => Err(SupportResult::of(Outcome::Failed, "engine_refused")),
            (s, _) => {
                eprintln!("agent: support: {path} answered {s}");
                Err(SupportResult::of(Outcome::Failed, "engine_error"))
            }
        }
    }

    /// What the engine's exporter would send now: its state and inventory, read through the telemetry
    /// contract's own types (a field outside the contract fails the read).
    async fn telemetry(&self) -> Read<(crate::telemetry::State, crate::telemetry::Inventory)> {
        let body = self.engine_read("/telemetry").await?;
        let bad = || SupportResult::of(Outcome::Failed, "bad_engine_answer");
        let v: Value = serde_json::from_slice(&body).map_err(|_| bad())?;
        let (mut state, mut inventory) = (None, None);
        for e in v["would_send"].as_array().ok_or_else(bad)? {
            let e: Envelope = serde_json::from_value(e.clone()).map_err(|_| bad())?;
            match e.payload {
                Payload::State(s) => state = Some(s),
                Payload::Inventory(i) => inventory = Some(i),
                Payload::Alerts(_) | Payload::Usage(_) | Payload::Advice(_) => {}
            }
        }
        Ok((state.ok_or_else(bad)?, inventory.ok_or_else(bad)?))
    }

    /// Run the diagnostic named `name`, if it is one of [`DIAGNOSTICS`]; the engine is read only for one
    /// of those, and only through [`ENGINE_READS`].
    async fn diagnose(&self, name: &str) -> SupportResult {
        let run = async {
            match name {
                "health" => self.health_diagnostic().await,
                "telemetry" => {
                    let (s, i) = self.telemetry().await?;
                    Ok(json!({"state": s, "inventory": i}))
                }
                "metrics" => {
                    let body = self.engine_read("/metrics").await?;
                    let fams = metric_families(&String::from_utf8_lossy(&body));
                    Ok(json!({"families": fams}))
                }
                "index_status" => {
                    let (s, i) = self.telemetry().await?;
                    Ok(json!({
                        "index_lag_seconds": s.index_lag_seconds,
                        "index_failures": s.index_failures,
                        "scopes": i.scopes,
                        "episodes": i.episodes,
                        "facts": i.facts,
                    }))
                }
                "config" => self.config_diagnostic().await,
                // No engine endpoint gives table snapshots, and the engine has no contract-filtered log.
                "catalog_status" | "logs" => {
                    Err(SupportResult::of(Outcome::Refused, "unsupported"))
                }
                _ => Err(SupportResult::of(Outcome::Refused, "unknown_diagnostic")),
            }
        };
        match run.await {
            Ok(out) => SupportResult::ok(out),
            Err(r) => r,
        }
    }

    async fn health_diagnostic(&self) -> Read<Value> {
        let (status, body) = self.engine_get("/healthz", None).await?;
        let ok =
            status == 200 && serde_json::from_slice::<Value>(&body).is_ok_and(|v| v["ok"] == true);
        let mut out = json!({"healthz": {"status": status, "ok": ok}});
        if self.read_key().is_some() {
            match self.telemetry().await {
                Ok((s, _)) => {
                    out["state"] = json!({
                        "health": s.health,
                        "conditions": s.conditions,
                        "engine_version": s.engine_version,
                        "uptime_seconds": s.uptime_seconds,
                        "index_lag_seconds": s.index_lag_seconds,
                        "index_failures": s.index_failures,
                    })
                }
                Err(r) => out["state_unavailable"] = json!(r.code),
            }
        }
        Ok(out)
    }

    async fn config_diagnostic(&self) -> Read<Value> {
        let mut out = Map::new();
        if self.read_key().is_some()
            && let Ok((s, _)) = self.telemetry().await
        {
            out.insert("engine_version".into(), json!(s.engine_version));
            out.insert("platform".into(), json!(s.platform));
            out.insert(
                "directory_event_version".into(),
                json!(s.directory_event_version),
            );
        }
        match self.target.env().await {
            Ok(env) => {
                out.insert("env".into(), config_view(&env));
            }
            Err(e) => eprintln!("agent: support: config: {e:#}"),
        }
        if out.is_empty() {
            return Err(SupportResult::of(Outcome::Failed, "config_unreadable"));
        }
        Ok(Value::Object(out))
    }

    /// Mirror a support event into the engine's own audit log (the `/admin` op `support_event`), with
    /// the operator's admin key. Without one, it is only logged here. A failure is logged, never fatal.
    async fn audit_engine(&self, mut event: Value) {
        let Some(key) = self.cfg.admin_key.as_deref() else {
            return;
        };
        let Some(base) = self.engine_base() else {
            return;
        };
        if let Some(o) = event.as_object_mut() {
            o.retain(|_, v| !v.is_null());
            if let Some(Value::String(by)) = o.get_mut("by")
                && by.trim().is_empty()
            {
                *by = "-".into();
            }
        }
        let r = self
            .http
            .post(format!("{base}/admin"))
            .bearer_auth(key)
            .json(&json!({"op": "support_event", "event": event}))
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => eprintln!(
                "agent: support: the engine's audit refused the event ({})",
                r.status()
            ),
            Err(e) => eprintln!("agent: support: the engine's audit: {e}"),
        }
    }
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Ok => "ok",
        Outcome::Failed => "failed",
        Outcome::Refused => "refused",
    }
}
