//! The deployment agent (`memstore agent`): a process of its own beside the engine that keeps the
//! deployment at the version the control plane wants, and runs the few actions it may ask for.
//!
//! The engine never upgrades itself. The agent pulls, outbound like everything else a deployment does:
//! `GET /v1/desired` (a generation: an image tag and digest, feature flags, and the customer's policy)
//! and `GET /v1/actions` (lane B's allowlist), both as signed requests from [`crate::deployment`]. It acts
//! only on what the control plane's own Ed25519 key signed: the key is pinned (at enrollment, or by
//! `MEMSTORE_CONTROL_PLANE_KEY`; trusting whoami's on first use is an explicit opt-in), each payload must name this deployment, be at most
//! ten minutes old, and never take the deployment back a generation.
//!
//! What it may change is fixed here, not by the control plane: the image repository is the agent's own
//! setting (the control plane names a tag and a digest only), the target is one named Deployment's one
//! container, and an action is one of six kinds with typed arguments. What it reports back is a phase or
//! an outcome, a code and counts from closed sets, never text.
//!
//! A third signed answer, `GET /v1/support`, carries a break-glass support session: read-only diagnostics
//! from a fixed list, run against the engine's health, telemetry and metrics endpoints only ([`support`]).
//!
//! Nor does the control plane's signature alone let an image run: before the agent sets
//! `<repository>:<tag>@<digest>`, that digest must carry a cosign signature by a key pinned in the agent
//! ([`crate::cosign`]), so a control plane that is compromised can't run an image we didn't build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Datelike, Timelike, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cosign::{self, Registry, RegistryAuth};
use crate::deployment::{self, Credential};
use crate::telemetry::{DeploymentId, Outbox};

pub mod support;

/// How far a signed payload's `issued_at` may be from our clock, either way.
pub const MAX_SKEW_SECS: i64 = 600;
/// Actions remembered as done (the oldest are forgotten first): far more than the control plane keeps
/// waiting at once.
pub const DONE_MAX: usize = 256;

/// What a signed payload is: the three the control plane signs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Desired,
    Actions,
    /// `GET /v1/support`: the break-glass support session open, if any ([`support`]).
    Support,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Desired => "desired",
            Kind::Actions => "actions",
            Kind::Support => "support",
        }
    }
}

/// A signed answer from the control plane: the payload as the exact JSON string it signed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signed {
    pub payload: String,
    pub signature: String,
}

/// What the control plane's signature covers: the kind and our id, so a payload can't stand in for
/// the other kind or for another deployment's.
pub fn signed_text(kind: Kind, id: &DeploymentId, payload: &str) -> String {
    format!("memstore-{}\n{}\n{payload}", kind.as_str(), id.as_str())
}

#[derive(Deserialize)]
struct Header {
    deployment: String,
    issued_at: DateTime<Utc>,
}

/// Check `signed` was signed by `key` as `kind` for `id`, names `id`, and was issued within
/// [`MAX_SKEW_SECS`] of `now`; then read it as `T`.
pub fn verify<T: DeserializeOwned>(
    key: &VerifyingKey,
    kind: Kind,
    id: &DeploymentId,
    signed: &Signed,
    now: DateTime<Utc>,
) -> Result<T> {
    let sig: [u8; 64] = B64
        .decode(&signed.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("a signature is 64 bytes of base64")?;
    key.verify_strict(
        signed_text(kind, id, &signed.payload).as_bytes(),
        &Signature::from_bytes(&sig),
    )
    .map_err(|_| anyhow::anyhow!("the {} signature doesn't verify", kind.as_str()))?;
    let h: Header = serde_json::from_str(&signed.payload).context("the signed payload")?;
    anyhow::ensure!(
        h.deployment == id.as_str(),
        "the {} payload names another deployment",
        kind.as_str()
    );
    anyhow::ensure!(
        (now - h.issued_at).num_seconds().abs() <= MAX_SKEW_SECS,
        "the {} payload was issued at {}, more than {MAX_SKEW_SECS}s from now",
        kind.as_str(),
        h.issued_at
    );
    Ok(serde_json::from_str(&signed.payload)?)
}

/// What `GET /v1/desired` signs.
#[derive(Debug, Clone, Deserialize)]
pub struct Desired {
    pub deployment: String,
    pub issued_at: DateTime<Utc>,
    /// 0: nothing desired yet.
    pub generation: u64,
    pub image: Option<ImageRef>,
    #[serde(default)]
    pub flags: BTreeMap<String, bool>,
    /// The engine's embeddings backend, an [`Embedding`]; none leaves the deployment's own
    /// (`crate::search::embed`). Read when applied, so one the agent can't use fails the generation.
    #[serde(default)]
    pub embedding: Option<Value>,
    pub policy: Policy,
    /// A customer's admin installed this generation from the console: it goes ahead now, outside the
    /// upgrade windows too. Every other check still applies.
    #[serde(default)]
    pub apply_now: bool,
}

/// Which embeddings the engine uses, as `MEMSTORE_EMBED`, `MEMSTORE_EMBED_MODEL` and
/// `MEMSTORE_EMBED_DIMS`. A model or width left out is the backend's default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Embedding {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub dims: Option<u32>,
}

/// The variables an [`Embedding`] is passed in.
pub const EMBED_VARS: [&str; 3] = [
    "MEMSTORE_EMBED",
    "MEMSTORE_EMBED_MODEL",
    "MEMSTORE_EMBED_DIMS",
];

impl Embedding {
    /// Each of [`EMBED_VARS`] set or removed; an error for a backend or value the engine has no use for.
    pub fn env(v: &Value) -> Result<BTreeMap<String, Option<String>>> {
        let e: Embedding = serde_json::from_value(v.clone())?;
        e.vars()
    }

    fn vars(&self) -> Result<BTreeMap<String, Option<String>>> {
        anyhow::ensure!(
            matches!(self.provider.as_str(), "local" | "openai" | "openrouter"),
            "not an embeddings backend"
        );
        if let Some(m) = &self.model {
            anyhow::ensure!(
                (1..=128).contains(&m.len())
                    && m.bytes().all(|c| c.is_ascii_alphanumeric()
                        || matches!(c, b'/' | b'.' | b'_' | b':' | b'-')),
                "not a model name"
            );
        }
        anyhow::ensure!(
            self.dims.is_none_or(|d| (1..=8192).contains(&d)),
            "not a vector width"
        );
        Ok(EMBED_VARS
            .iter()
            .map(|v| v.to_string())
            .zip([
                Some(self.provider.clone()),
                self.model.clone(),
                self.dims.map(|d| d.to_string()),
            ])
            .collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageRef {
    pub tag: String,
    pub digest: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub pinned: bool,
    /// UTC; none means any time.
    #[serde(default)]
    pub windows: Vec<Window>,
    pub actions_enabled: bool,
    /// Whether the customer allows break-glass support sessions at all. A control plane from before the
    /// setting sends none: on request, as before.
    #[serde(default)]
    pub support: SupportMode,
}

/// The customer's support policy: `off`, no session runs anything here whatever the control plane sends;
/// `on-request` (the default), a session an engineer asked for and one of the customer's own admins approved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportMode {
    Off,
    #[default]
    OnRequest,
}

/// From `start` (UTC, `HH:MM`) on each of `days` (0 is Sunday), for `hours`.
#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub days: Vec<u8>,
    pub start: String,
    pub hours: u32,
}

const WEEK_MINUTES: i64 = 7 * 24 * 60;

impl Window {
    fn start_minute(&self) -> Option<i64> {
        let (h, m) = self.start.split_once(':')?;
        let ok = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
        if !ok(h) || !ok(m) {
            return None;
        }
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    }

    fn valid(&self) -> bool {
        self.start_minute().is_some()
            && (1..=24).contains(&self.hours)
            && !self.days.is_empty()
            && self.days.iter().all(|d| *d <= 6)
    }

    /// Whether `now` falls in this window. Minutes into the week (Sunday 00:00 is 0), modulo a week, so
    /// a window that runs past midnight, or from Saturday into Sunday, holds on the far side too.
    pub fn holds(&self, now: DateTime<Utc>) -> bool {
        let Some(start) = self.start_minute() else {
            return false;
        };
        let at = i64::from(now.weekday().num_days_from_sunday()) * 1440
            + i64::from(now.hour()) * 60
            + i64::from(now.minute());
        let len = i64::from(self.hours) * 60;
        self.days.iter().filter(|d| **d <= 6).any(|d| {
            let from = i64::from(*d) * 1440 + start;
            (at - from).rem_euclid(WEEK_MINUTES) < len
        })
    }
}

/// Whether an upgrade may start at `now`: inside any of `windows`, or any time when there are none.
pub fn in_window(windows: &[Window], now: DateTime<Utc>) -> bool {
    windows.is_empty() || windows.iter().any(|w| w.holds(now))
}

/// Whether to act on generation `offered` when `handled` is the newest this agent has acted on: only a
/// newer one. An older one (a replayed payload, a control plane restored from backup) is never applied.
pub fn should_apply(handled: u64, offered: u64) -> bool {
    offered > handled
}

pub use crate::audit::is_code;

fn is_tag(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// An image repository (the agent's own setting): a registry path with no tag or digest of its own.
pub fn check_repository(r: &str) -> Result<()> {
    let last = r.rsplit('/').next().unwrap_or_default();
    anyhow::ensure!(
        !r.is_empty()
            && !r.contains('@')
            && !last.contains(':')
            && r.bytes().all(|c| c.is_ascii_graphic()),
        "an image repository is a registry path without a tag or digest: {r}"
    );
    Ok(())
}

/// The image a generation runs: the agent's repository, the control plane's tag and digest. The digest
/// is what the runtime pulls; the tag is for people reading the Deployment.
pub fn image_of(repository: &str, image: &ImageRef) -> Result<String> {
    anyhow::ensure!(is_tag(&image.tag), "not an image tag");
    anyhow::ensure!(is_digest(&image.digest), "not a sha256 digest");
    check_repository(repository)?;
    Ok(format!("{repository}:{}@{}", image.tag, image.digest))
}

/// The container variable a flag is passed in: `MEMSTORE_FLAG_<NAME>`, `1` or `0`.
pub fn flag_env(name: &str) -> Result<String> {
    anyhow::ensure!(is_code(name), "not a flag name");
    Ok(format!("MEMSTORE_FLAG_{}", name.to_ascii_uppercase()))
}

pub const FLAG_PREFIX: &str = "MEMSTORE_FLAG_";

/// Where the agent got to with a generation, as `POST /v1/desired/status` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    WaitingWindow,
    Applying,
    Healthy,
    RolledBack,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub generation: u64,
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// One of lane B's actions, with its arguments checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Restart,
    Reindex { scope: String },
    DrainSpool,
    ClearSpool,
    RotateKey,
    Diagnostics,
}

/// The action `kind` with `args`, or the code it is refused with: an unknown kind, or arguments other
/// than the kind's own.
pub fn parse_action(kind: &str, args: &Value) -> std::result::Result<Action, &'static str> {
    let none = args.is_null() || args.as_object().is_some_and(|o| o.is_empty());
    let plain = |a: Action| if none { Ok(a) } else { Err("bad_args") };
    match kind {
        "restart" => plain(Action::Restart),
        "drain_spool" => plain(Action::DrainSpool),
        "clear_spool" => plain(Action::ClearSpool),
        "rotate_key" => plain(Action::RotateKey),
        "diagnostics" => plain(Action::Diagnostics),
        "reindex" => {
            let o = args.as_object().ok_or("bad_args")?;
            let scope = o.get("scope").and_then(Value::as_str).ok_or("bad_args")?;
            let ok = (1..=128).contains(&scope.len())
                && scope
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'.' | b'-'));
            if o.len() != 1 || !ok {
                return Err("bad_args");
            }
            Ok(Action::Reindex {
                scope: scope.into(),
            })
        }
        _ => Err("unknown_kind"),
    }
}

/// An action id as the control plane makes them: `act-` and 16 hex digits.
pub fn is_action_id(id: &str) -> bool {
    id.strip_prefix("act-").is_some_and(|h| {
        h.len() == 16
            && h.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// What `GET /v1/actions` signs.
#[derive(Debug, Clone, Deserialize)]
pub struct Actions {
    pub deployment: String,
    pub issued_at: DateTime<Utc>,
    pub actions: Vec<Requested>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Requested {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub args: Value,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Failed,
    Refused,
}

/// An action's result, as `POST /v1/actions/:id/result` takes it: codes and counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<BTreeMap<String, u64>>,
}

impl ActionResult {
    fn of(outcome: Outcome, code: &str) -> Self {
        ActionResult {
            outcome,
            code: Some(code.into()),
            counts: None,
        }
    }
    fn ok(counts: BTreeMap<String, u64>) -> Self {
        ActionResult {
            outcome: Outcome::Ok,
            code: None,
            counts: (!counts.is_empty()).then_some(counts),
        }
    }
}

/// An action this agent took up: its result once it has one, and whether the control plane has it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Done {
    pub id: String,
    /// None: started and never finished (the agent stopped part way); it is reported as failed, not run
    /// again.
    pub result: Option<ActionResult>,
    pub posted: bool,
}

/// An upgrade under way, kept so an agent that restarts part way finishes it (or rolls it back)
/// rather than starting over or forgetting it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlight {
    pub generation: u64,
    pub version: String,
    pub previous: Snapshot,
    pub started: DateTime<Utc>,
    /// Kept before patching, so a restart can distinguish the old healthy workload from the target.
    #[serde(default)]
    pub desired: Option<ManagedState>,
    #[serde(default)]
    pub rollback: Option<Rollback>,
    /// A drift repair must never roll back to the unapproved state it found.
    #[serde(default)]
    pub repair: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rollback {
    pub started: DateTime<Utc>,
    pub code: String,
}

/// Fields this generation owns. Embedding settings omitted by it remain the operator's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedState {
    pub image: String,
    pub env: BTreeMap<String, Option<String>>,
}

impl ManagedState {
    fn matches(&self, actual: &Snapshot) -> bool {
        self.image == actual.image
            && self
                .env
                .iter()
                .all(|(k, v)| actual.flags.get(k) == v.as_ref())
            && actual
                .flags
                .keys()
                .filter(|k| k.starts_with(FLAG_PREFIX))
                .all(|k| self.env.get(k).is_some_and(Option::is_some))
    }

    fn patch_env(&self, actual: &Snapshot) -> BTreeMap<String, Option<String>> {
        let mut env: BTreeMap<_, _> = actual
            .flags
            .keys()
            .filter(|k| k.starts_with(FLAG_PREFIX))
            .map(|k| (k.clone(), None))
            .collect();
        env.extend(self.env.clone());
        env
    }

    fn restore(previous: &Snapshot, changed: Option<&Self>) -> Self {
        let owns =
            |k: &str| k.starts_with(FLAG_PREFIX) || changed.is_none_or(|s| s.env.contains_key(k));
        let mut env: BTreeMap<_, _> = EMBED_VARS
            .iter()
            .filter(|k| owns(k))
            .map(|k| (k.to_string(), None))
            .collect();
        env.extend(
            previous
                .flags
                .iter()
                .filter(|(k, _)| owns(k))
                .map(|(k, v)| (k.clone(), Some(v.clone()))),
        );
        Self {
            image: previous.image.clone(),
            env,
        }
    }
}

/// Drift repair is a local grant; the control plane cannot enable it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum DriftMode {
    #[default]
    Report,
    Repair,
}

/// What the agent keeps between rounds, beside the telemetry spool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    /// The newest generation acted on (healthy, rolled back or failed): never acted on again, nor one older.
    #[serde(default)]
    pub generation: u64,
    /// What was said of it, said again each round.
    #[serde(default)]
    pub status: Option<Status>,
    /// The accepted fields of a healthy generation, never replaced by a different same-generation payload.
    #[serde(default)]
    pub applied: Option<ManagedState>,
    #[serde(default)]
    pub in_flight: Option<InFlight>,
    /// The control plane's key, when neither the credential nor the environment pinned it.
    #[serde(default)]
    pub control_plane_key: Option<String>,
    #[serde(default)]
    pub actions: Vec<Done>,
    /// The support session seen open, and the diagnostics run in support sessions.
    #[serde(default)]
    pub support: support::SupportState,
}

impl AgentState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{}", path.display())),
        }
    }

    /// Through a temporary file, so a crash leaves the old state whole.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = deployment::open_private(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("{}", path.display()))
    }

    fn done(&mut self, id: &str) -> Option<&mut Done> {
        self.actions.iter_mut().find(|d| d.id == id)
    }
}

/// The running container as the agent found it: its image and the variables the agent sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub image: String,
    /// `MEMSTORE_FLAG_*` and [`EMBED_VARS`] variables and their values.
    pub flags: BTreeMap<String, String>,
}

/// A rollout's progress, as Kubernetes reports a Deployment's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rollout {
    pub generation: i64,
    pub observed: i64,
    /// Replicas wanted.
    pub wanted: i64,
    pub replicas: i64,
    pub updated: i64,
    pub available: i64,
    /// The platform gave up on it (Kubernetes: `ProgressDeadlineExceeded`).
    pub stalled: bool,
}

impl Rollout {
    /// Every replica runs the new template and is available, and no old one is left.
    pub fn complete(&self) -> bool {
        self.observed >= self.generation
            && self.updated == self.wanted
            && self.available == self.wanted
            && self.replicas == self.wanted
    }
}

/// Where the engine runs. Each method acts on the one workload the agent was configured with.
#[async_trait::async_trait]
pub trait Target: Send + Sync {
    async fn snapshot(&self) -> Result<Snapshot>;
    /// Check every workload this target owns, including cells.
    async fn matches(&self, desired: &ManagedState) -> Result<bool> {
        Ok(desired.matches(&self.snapshot().await?))
    }
    /// Run `image`, setting each variable to its value or removing it (None), in one change.
    async fn set(&self, image: &str, env: &BTreeMap<String, Option<String>>) -> Result<()>;
    /// Apply only this specification's fields; clear extra flags independently on every workload.
    async fn set_managed(&self, desired: &ManagedState) -> Result<()> {
        self.set(&desired.image, &desired.patch_env(&self.snapshot().await?))
            .await
    }
    async fn rollout(&self) -> Result<Rollout>;
    /// Replace every replica, keeping the template.
    async fn restart(&self) -> Result<()>;
    /// The engine container's variables: each name with its literal value (none: set from a Secret, a
    /// ConfigMap or a field). Read for the support `config` diagnostic, which shows a value only for a
    /// fixed set of names ([`support`]).
    async fn env(&self) -> Result<Vec<(String, Option<String>)>> {
        anyhow::bail!("this target doesn't read the engine's variables")
    }
    /// Bring any other Deployments running the same engine (cells) to the engine's version and flags,
    /// where they differ: one created later, or left behind by an agent that didn't know of it.
    async fn sync_also(&self) -> Result<()> {
        Ok(())
    }
}

/// A Kubernetes Deployment's one container, through the API server with the agent's service account.
pub struct Kubernetes {
    api: String,
    token_file: Option<PathBuf>,
    client: reqwest::Client,
    namespace: String,
    deployment: String,
    container: String,
    /// Deployments that run the same engine (a cell's, `crate::server::cell`): each version and flag change
    /// goes to them too, after the engine's own, and a rollout is done only once theirs is (`rollout`).
    /// Snapshots and health are the engine's alone.
    also: Vec<String>,
}

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

fn check_name(what: &str, n: &str) -> Result<()> {
    anyhow::ensure!(
        !n.is_empty()
            && n.len() <= 253
            && n.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.')),
        "a Kubernetes {what} name is lowercase letters, digits, - and .: {n}"
    );
    Ok(())
}

impl Kubernetes {
    /// In the cluster: `KUBERNETES_SERVICE_HOST`/`_PORT`, and the service account's token and CA. The
    /// namespace defaults to the service account's own.
    pub fn in_cluster(
        namespace: Option<String>,
        deployment: String,
        container: String,
    ) -> Result<Self> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .context("KUBERNETES_SERVICE_HOST: the agent runs in the cluster")?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let ca = std::fs::read(Path::new(SA_DIR).join("ca.crt")).context("the cluster CA")?;
        let namespace = match namespace {
            Some(n) => n,
            None => std::fs::read_to_string(Path::new(SA_DIR).join("namespace"))
                .context("the service account's namespace")?
                .trim()
                .to_string(),
        };
        Self::at(
            &format!("https://{host}:{port}"),
            Some(Path::new(SA_DIR).join("token")),
            Some(&ca),
            namespace,
            deployment,
            container,
        )
    }

    /// The API server at `api` (https, or http to this machine only), with a bearer token read from
    /// `token_file` on every call (projected tokens are rotated), trusting `ca_pem` as well.
    pub fn at(
        api: &str,
        token_file: Option<PathBuf>,
        ca_pem: Option<&[u8]>,
        namespace: String,
        deployment: String,
        container: String,
    ) -> Result<Self> {
        let api = deployment::checked_url(api)?;
        check_name("namespace", &namespace)?;
        check_name("Deployment", &deployment)?;
        check_name("container", &container)?;
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30));
        if let Some(pem) = ca_pem {
            b = b.add_root_certificate(
                reqwest::Certificate::from_pem(pem).context("the cluster CA")?,
            );
        }
        Ok(Kubernetes {
            api,
            token_file,
            client: b.build()?,
            namespace,
            deployment,
            container,
            also: vec![],
        })
    }

    /// Also patch these Deployments, the same way.
    pub fn with_also(mut self, also: Vec<String>) -> Result<Self> {
        for d in &also {
            check_name("Deployment", d)?;
        }
        self.also = also;
        Ok(self)
    }

    fn url(&self, deployment: &str) -> String {
        format!(
            "{}/apis/apps/v1/namespaces/{}/deployments/{deployment}",
            self.api, self.namespace
        )
    }

    fn request(
        &self,
        method: reqwest::Method,
        deployment: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let mut r = self.client.request(method, self.url(deployment));
        if let Some(f) = &self.token_file {
            let t = std::fs::read_to_string(f).context("the service account token")?;
            r = r.bearer_auth(t.trim());
        }
        Ok(r)
    }

    async fn get(&self) -> Result<Value> {
        self.get_of(&self.deployment).await
    }

    async fn get_of(&self, deployment: &str) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::GET, deployment)?
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "GET Deployment {}/{deployment}: {status}",
            self.namespace
        );
        Ok(resp.json().await?)
    }

    /// A strategic merge patch: containers merge by name, and env by name, so only this container's
    /// image and these variables change (each set to its value, or removed).
    fn image_patch(&self, image: &str, env: &BTreeMap<String, Option<String>>) -> Value {
        let mut c = json!({"name": self.container, "image": image});
        if !env.is_empty() {
            c["env"] = env
                .iter()
                .map(|(n, v)| match v {
                    Some(v) => json!({"name": n, "value": v}),
                    None => json!({"name": n, "$patch": "delete"}),
                })
                .collect();
        }
        json!({"spec": {"template": {"spec": {"containers": [c]}}}})
    }

    /// `d`'s engine container: its image and the variables the agent sets.
    fn snapshot_of(&self, d: &Value) -> Result<Snapshot> {
        let c = d["spec"]["template"]["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"] == self.container.as_str())
            .with_context(|| format!("no container {} in the Deployment", self.container))?;
        let flags = c["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let n = e["name"].as_str()?;
                (n.starts_with(FLAG_PREFIX) || EMBED_VARS.contains(&n))
                    .then(|| (n.to_string(), e["value"].as_str().unwrap_or("").into()))
            })
            .collect();
        Ok(Snapshot {
            image: c["image"].as_str().unwrap_or_default().into(),
            flags,
        })
    }

    async fn patch(&self, deployment: &str, patch: &Value) -> Result<()> {
        let resp = self
            .request(reqwest::Method::PATCH, deployment)?
            .header("content-type", "application/strategic-merge-patch+json")
            .body(patch.to_string())
            .send()
            .await?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "PATCH Deployment {}/{deployment}: {status}",
            self.namespace
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl Target for Kubernetes {
    async fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_of(&self.get().await?)
    }

    async fn matches(&self, desired: &ManagedState) -> Result<bool> {
        if !desired.matches(&self.snapshot().await?) {
            return Ok(false);
        }
        for name in &self.also {
            if !desired.matches(&self.snapshot_of(&self.get_of(name).await?)?) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn sync_also(&self) -> Result<()> {
        if self.also.is_empty() {
            return Ok(());
        }
        let engine = self.snapshot().await?;
        for d in &self.also {
            let cell = self.snapshot_of(&self.get_of(d).await?)?;
            if cell.image == engine.image && cell.flags == engine.flags {
                continue;
            }
            let mut env: BTreeMap<String, Option<String>> =
                cell.flags.keys().map(|k| (k.clone(), None)).collect();
            for (k, v) in &engine.flags {
                env.insert(k.clone(), Some(v.clone()));
            }
            self.patch(d, &self.image_patch(&engine.image, &env))
                .await?;
            eprintln!(
                "agent: {d} brought to the engine's version: {} -> {}",
                cell.image, engine.image
            );
        }
        Ok(())
    }

    async fn set(&self, image: &str, env: &BTreeMap<String, Option<String>>) -> Result<()> {
        let patch = self.image_patch(image, env);
        self.patch(&self.deployment, &patch).await?;
        for d in &self.also {
            self.patch(d, &patch).await?;
        }
        Ok(())
    }

    async fn set_managed(&self, desired: &ManagedState) -> Result<()> {
        for name in std::iter::once(&self.deployment).chain(self.also.iter()) {
            let actual = self.snapshot_of(&self.get_of(name).await?)?;
            if !desired.matches(&actual) {
                self.patch(
                    name,
                    &self.image_patch(&desired.image, &desired.patch_env(&actual)),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The engine's rollout, and each cell's: the first not done is the answer (a cell's surge pod left Pending
    /// with its old pod still serving the old version was once called healthy, 10-26), stalled if any is. A
    /// cell whose template runs another image than the engine's isn't done either.
    async fn rollout(&self) -> Result<Rollout> {
        let engine = self.get().await?;
        let image = self.snapshot_of(&engine)?.image;
        let mut r = rollout_of(&engine);
        for name in &self.also {
            let d = self.get_of(name).await?;
            let mut cell = rollout_of(&d);
            if self.snapshot_of(&d)?.image != image {
                cell.updated = 0;
            }
            let stalled = r.stalled || cell.stalled;
            if r.complete() && !cell.complete() {
                r = cell;
            }
            r.stalled = stalled;
        }
        Ok(r)
    }

    async fn env(&self) -> Result<Vec<(String, Option<String>)>> {
        let d = self.get().await?;
        let c = d["spec"]["template"]["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"] == self.container.as_str())
            .with_context(|| format!("no container {} in the Deployment", self.container))?;
        Ok(c["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let n = e["name"].as_str()?.to_string();
                Some((n, e["value"].as_str().map(String::from)))
            })
            .collect())
    }

    async fn restart(&self) -> Result<()> {
        // What `kubectl rollout restart` does.
        let patch = json!({"spec": {"template": {"metadata": {"annotations": {
            "kubectl.kubernetes.io/restartedAt": Utc::now().to_rfc3339(),
        }}}}});
        self.patch(&self.deployment, &patch).await
    }
}

/// A Deployment's rollout, as its status says.
fn rollout_of(d: &Value) -> Rollout {
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    let stalled = d["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["type"] == "Progressing" && c["reason"] == "ProgressDeadlineExceeded");
    Rollout {
        generation: n(&d["metadata"]["generation"]),
        observed: n(&d["status"]["observedGeneration"]),
        wanted: d["spec"]["replicas"].as_i64().unwrap_or(1),
        replicas: n(&d["status"]["replicas"]),
        updated: n(&d["status"]["updatedReplicas"]),
        available: n(&d["status"]["availableReplicas"]),
        stalled,
    }
}

/// Cloud Run: not yet.
pub struct CloudRun;
/// Snowflake (SPCS): not yet.
pub struct Snowflake;

macro_rules! not_yet {
    ($t:ty, $name:literal) => {
        #[async_trait::async_trait]
        impl Target for $t {
            async fn snapshot(&self) -> Result<Snapshot> {
                anyhow::bail!(concat!("the ", $name, " target isn't built yet"))
            }
            async fn set(&self, _: &str, _: &BTreeMap<String, Option<String>>) -> Result<()> {
                anyhow::bail!(concat!("the ", $name, " target isn't built yet"))
            }
            async fn rollout(&self) -> Result<Rollout> {
                anyhow::bail!(concat!("the ", $name, " target isn't built yet"))
            }
            async fn restart(&self) -> Result<()> {
                anyhow::bail!(concat!("the ", $name, " target isn't built yet"))
            }
        }
    };
}
// TODO: Cloud Run (a new revision of the service with the image and env) and Snowflake (ALTER SERVICE
// with a new specification) targets.
not_yet!(CloudRun, "Cloud Run");
not_yet!(Snowflake, "Snowflake");

/// The agent's settings.
#[derive(Debug, Clone)]
pub struct Config {
    pub credential: PathBuf,
    /// Where [`AgentState`] is kept.
    pub state: PathBuf,
    /// The engine's telemetry mirror, when the agent shares its volume: `drain_spool` and `clear_spool`
    /// act on the spool beside it. Without it they are refused as unsupported.
    pub telemetry_mirror: Option<PathBuf>,
    /// `MEMSTORE_CONTROL_PLANE_KEY`.
    pub control_plane_key: Option<String>,
    /// With no key pinned, take the one `/v1/whoami` gives and keep it (`MEMSTORE_AGENT_TOFU=1`). Off:
    /// the agent refuses to start without a pinned key.
    pub trust_on_first_use: bool,
    pub image_repository: String,
    /// The embeddings backends the control plane may choose (`MEMSTORE_AGENT_EMBED_PROVIDERS`). Empty, as by
    /// default: it may choose none, since a switch to an API sends the deployment's text to that vendor (and
    /// `MEMSTORE_EMBED_KEY`, which names no vendor, with it).
    pub embed_providers: Vec<String>,
    /// Report drift by default; repair only when the deployment's operator opts in.
    pub drift_mode: DriftMode,
    /// The engine's health: `{"ok": true}` (`/healthz`), or what `/telemetry` would send, whose state
    /// must say `ok` (with `health_token`, an admin key).
    pub health_url: String,
    pub health_token: Option<String>,
    pub poll: Duration,
    pub rollout_timeout: Duration,
    /// How long a rolled-out engine has to answer healthy.
    pub health_timeout: Duration,
    /// How often a rollout and the engine's health are looked at while waiting.
    pub check_every: Duration,
    /// The key an image's cosign signature must verify with (`MEMSTORE_AGENT_COSIGN_KEY`). Without one,
    /// no upgrade is applied, unless [`Config::allow_unsigned_images`].
    pub cosign_key: Option<cosign::PublicKey>,
    /// Apply images with no signature check when no key is pinned (`MEMSTORE_AGENT_ALLOW_UNSIGNED=1`).
    /// Warned of at start and on each upgrade.
    pub allow_unsigned_images: bool,
    /// How the registry is read for signatures (`MEMSTORE_AGENT_REGISTRY_AUTH`; default by its host).
    pub registry_auth: Option<RegistryAuth>,
    /// The registry's API at another address than `https://<host>` (tests, a mirror of the same
    /// repository).
    pub registry_api: Option<String>,
    /// The GCE metadata server (`GCE_METADATA_HOST`; default `metadata.google.internal`).
    pub metadata_host: Option<String>,
    /// Where the `sha256-<hex>.sig` signatures are, when cosign kept them apart from the images
    /// (`COSIGN_REPOSITORY` when signing; `MEMSTORE_AGENT_SIGNATURE_REPOSITORY` here). Default: the image
    /// repository.
    pub signature_repository: Option<String>,
    /// The engine, for support diagnostics (`MEMSTORE_AGENT_ENGINE_URL`). Default: the origin of
    /// [`Config::health_url`].
    pub engine_url: Option<String>,
    /// The operator's admin key for the engine (`MEMSTORE_AGENT_ADMIN_KEY_FILE`): support diagnostics read
    /// `/telemetry` and `/metrics` with it, and support events are written to the engine's audit with it.
    /// Without it, [`Config::health_token`] is used for the reads, and no audit is written.
    pub admin_key: Option<String>,
    /// How often the support session is looked at while one is open (5 s); otherwise every round.
    pub support_poll: Duration,
}

impl Config {
    /// The state file's default: beside the telemetry spool (beside the mirror, else the credential).
    pub fn default_state(credential: &Path, mirror: Option<PathBuf>) -> PathBuf {
        let o = Outbox::at(credential.into(), mirror);
        o.spool().with_file_name("agent.state.json")
    }
}

/// The agent: its settings, its target, the pinned key, and what it keeps.
pub struct Agent {
    cfg: Config,
    target: Box<dyn Target>,
    key: VerifyingKey,
    /// Where the engine's images and their signatures are.
    registry: Registry,
    state: AgentState,
    http: reqwest::Client,
    /// From the last desired state that verified; no action runs before one says so.
    actions_enabled: bool,
    /// From the same; none until one has verified in this process, and only a verified `OnRequest`
    /// lets a support command run (fail closed).
    support_mode: Option<SupportMode>,
    rounds: u64,
    /// The control plane's key changed under us: nothing more is acted on.
    halted: bool,
}

/// How often (in rounds) whoami's key is compared with the pinned one again.
const KEY_CHECK_ROUNDS: u64 = 20;

/// A key's SHA-256 fingerprint (of its 32 bytes, hex), for an operator to compare.
pub fn fingerprint(key_b64: &str) -> String {
    use sha2::Digest;
    match B64.decode(key_b64) {
        Ok(b) => deployment::hex(&sha2::Sha256::digest(&b)),
        Err(_) => "not-base64".into(),
    }
}

fn check_same(pinned: &str, told: &str) -> Result<()> {
    anyhow::ensure!(
        pinned == told,
        "the control plane's key isn't the one pinned for it (pinned sha256 {}, /v1/whoami gave sha256 {}); \
         refusing to act on anything it signs",
        fingerprint(pinned),
        fingerprint(told)
    );
    Ok(())
}

/// The key `/v1/whoami` gives, if it answers.
async fn whoami_key(cred: &Credential) -> Option<String> {
    match cred.call("GET", "/v1/whoami", &json!({})).await {
        Ok((200, body)) => body["control_plane_key"].as_str().map(String::from),
        Ok((status, _)) => {
            eprintln!("agent: whoami answered {status}");
            None
        }
        Err(e) => {
            eprintln!("agent: whoami: {e:#}");
            None
        }
    }
}

impl Agent {
    /// Load the credential and state, and pin the control plane's key: from the credential (saved at
    /// enrollment) or `MEMSTORE_CONTROL_PLANE_KEY`. With neither, the agent refuses to start, unless
    /// the operator opted in to trusting `/v1/whoami`'s key on first use ([`Config::trust_on_first_use`]):
    /// then that key is kept and its fingerprint logged for them to check. Whoami's key must agree with
    /// the pinned one; a different key stops the agent.
    pub async fn start(cfg: Config, target: Box<dyn Target>) -> Result<Self> {
        check_repository(&cfg.image_repository)?;
        let cred = Credential::load(&cfg.credential)?;
        let mut state = AgentState::load(&cfg.state)?;
        if let (Some(env), Some(c)) = (&cfg.control_plane_key, &cred.control_plane_key) {
            anyhow::ensure!(
                env == c,
                "MEMSTORE_CONTROL_PLANE_KEY ({}) isn't the key pinned in the credential ({})",
                fingerprint(env),
                fingerprint(c)
            );
        }
        // A key an earlier start trusted on first use counts only while that opt-in stands.
        let pinned = cfg
            .control_plane_key
            .clone()
            .or_else(|| cred.control_plane_key.clone())
            .or_else(|| {
                cfg.trust_on_first_use
                    .then(|| state.control_plane_key.clone())
                    .flatten()
            });
        anyhow::ensure!(
            pinned.is_some() || cfg.trust_on_first_use,
            "no control plane key: the credential doesn't pin one (enrolled before keys were given) and \
             MEMSTORE_CONTROL_PLANE_KEY isn't set. Set it to the key your control plane publishes, or \
             re-enroll; --trust-control-plane-key-on-first-use (MEMSTORE_AGENT_TOFU=1) takes the key \
             /v1/whoami gives instead"
        );
        let told = whoami_key(&cred).await;
        let key = match (pinned, told) {
            (Some(p), Some(t)) => {
                check_same(&p, &t)?;
                p
            }
            (Some(p), None) => p,
            (None, Some(t)) => {
                deployment::public_key(&t)?;
                eprintln!(
                    "agent: WARNING: trusting the control plane's key on first use, as /v1/whoami gave it: \
                     sha256 fingerprint {}. Check it against the one your control plane publishes.",
                    fingerprint(&t)
                );
                state.control_plane_key = Some(t.clone());
                state.save(&cfg.state)?;
                t
            }
            (None, None) => {
                anyhow::bail!("no control plane key: /v1/whoami gave none to trust on first use")
            }
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()?;
        let signatures = cfg
            .signature_repository
            .as_deref()
            .unwrap_or(&cfg.image_repository);
        check_repository(signatures)?;
        let registry = Registry::new(
            signatures,
            cfg.registry_api.as_deref(),
            cfg.registry_auth,
            cfg.metadata_host.clone(),
        )?;
        match (&cfg.cosign_key, cfg.allow_unsigned_images) {
            (Some(k), _) => eprintln!(
                "agent: images must carry a cosign signature by the pinned key (sha256 fingerprint {})",
                k.fingerprint()
            ),
            (None, true) => eprintln!(
                "agent: WARNING: --allow-unsigned-images (MEMSTORE_AGENT_ALLOW_UNSIGNED=1) is on and no cosign \
                 key is pinned: images the control plane names are run WITHOUT a signature check. Set \
                 MEMSTORE_AGENT_COSIGN_KEY to your signing key's public key."
            ),
            (None, false) => eprintln!(
                "agent: no cosign key pinned (MEMSTORE_AGENT_COSIGN_KEY): no upgrade will be applied; \
                 each is reported failed (signature_unverifiable). Actions still run."
            ),
        }
        Ok(Agent {
            key: deployment::public_key(&key)?,
            registry,
            cfg,
            target,
            state,
            http,
            actions_enabled: false,
            support_mode: None,
            rounds: 0,
            halted: false,
        })
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Poll for good: a round every `poll`, give or take a fifth, so a fleet doesn't call in step. While a
    /// support session is open, the session alone is looked at every `support_poll` in between.
    pub async fn run(mut self) -> Result<()> {
        use rand::Rng;
        let mut next = tokio::time::Instant::now();
        loop {
            if tokio::time::Instant::now() >= next {
                if let Err(e) = self.tick().await {
                    eprintln!("agent: {e:#}");
                }
                let f = rand::thread_rng().gen_range(0.8..1.2);
                next = tokio::time::Instant::now() + self.cfg.poll.mul_f64(f);
            } else if let Err(e) = self.support_round().await {
                eprintln!("agent: support: {e:#}");
            }
            let wait = next.saturating_duration_since(tokio::time::Instant::now());
            let wait = if self.state.support.session.is_some() {
                wait.min(self.cfg.support_poll)
            } else {
                wait
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One round: finish an upgrade left part way, look at the desired state, then at the actions.
    pub async fn tick(&mut self) -> Result<()> {
        let cred = Credential::load(&self.cfg.credential)?;
        self.rounds += 1;
        if self.rounds.is_multiple_of(KEY_CHECK_ROUNDS)
            && let Some(t) = whoami_key(&cred).await
        {
            let pinned = B64.encode(self.key.to_bytes());
            if let Err(e) = check_same(&pinned, &t) {
                eprintln!("agent: {e:#}");
                self.halted = true;
            }
        }
        anyhow::ensure!(
            !self.halted,
            "halted: the control plane's key changed; restart the agent once the key is sorted out"
        );
        if let Some(f) = self.state.in_flight.clone() {
            self.finish(&cred, f).await?;
        }
        if let Err(e) = self.desired(&cred).await {
            eprintln!("agent: desired state: {e:#}");
        }
        // Before any generation has been accepted, retain the bootstrap behavior for cells. Once
        // enrolled in desired state, reconciliation must never copy a drifted engine to its cells.
        if self.state.generation == 0
            && self.state.in_flight.is_none()
            && let Err(e) = self.target.sync_also().await
        {
            eprintln!("agent: cells: {e:#}");
        }
        if self.actions_enabled
            && let Err(e) = self.actions(&cred).await
        {
            eprintln!("agent: actions: {e:#}");
        }
        if let Err(e) = self.support(&cred).await {
            eprintln!("agent: support: {e:#}");
        }
        Ok(())
    }

    async fn fetch<T: DeserializeOwned>(&self, cred: &Credential, kind: Kind) -> Result<T> {
        let path = format!("/v1/{}", kind.as_str());
        let (status, body) = cred.call("GET", &path, &json!({})).await?;
        anyhow::ensure!(status == 200, "{path} answered {status}");
        let signed: Signed = serde_json::from_value(body).context("not a signed answer")?;
        verify(&self.key, kind, &cred.deployment, &signed, Utc::now())
    }

    async fn report(&self, cred: &Credential, s: &Status) {
        match cred
            .call(
                "POST",
                "/v1/desired/status",
                &serde_json::to_value(s).unwrap(),
            )
            .await
        {
            Ok((200, _)) => {}
            Ok((status, _)) => eprintln!("agent: status refused ({status})"),
            Err(e) => eprintln!("agent: status: {e:#}"),
        }
    }

    async fn desired(&mut self, cred: &Credential) -> Result<()> {
        let d: Desired = self.fetch(cred, Kind::Desired).await?;
        self.actions_enabled = d.policy.actions_enabled;
        if self.support_mode != Some(d.policy.support) {
            eprintln!(
                "agent: support policy is now {}",
                match d.policy.support {
                    SupportMode::Off => "off: support sessions run nothing here",
                    SupportMode::OnRequest => "on request: a session your admins approve",
                }
            );
        }
        self.support_mode = Some(d.policy.support);
        if d.generation == 0 {
            return Ok(());
        }
        let same = d.generation == self.state.generation;
        if !should_apply(self.state.generation, d.generation) {
            if d.generation < self.state.generation {
                eprintln!(
                    "agent: generation {} is older than {}, ignored",
                    d.generation, self.state.generation
                );
                return Ok(());
            }
            let Some(s) = self.state.status.clone() else {
                return Ok(());
            };
            if s.phase != Phase::Healthy {
                self.report(cred, &s).await;
                return Ok(());
            }
            if let Some(applied) = self.state.applied.clone() {
                return self.reconcile(cred, &d.policy, s, applied).await;
            }
            // An older state file has no accepted spec. Bootstrap it from this signed generation,
            // with the same field validation and local embedding policy as a new generation.
        }
        let status = |phase, code: Option<&str>| Status {
            generation: d.generation,
            phase,
            version: d
                .image
                .as_ref()
                .map(|i| i.tag.clone())
                .filter(|t| is_tag(t)),
            code: code.map(String::from),
        };
        let image = match d
            .image
            .as_ref()
            .map(|i| image_of(&self.cfg.image_repository, i))
        {
            Some(Ok(i)) => i,
            bad => {
                let code = if bad.is_none() {
                    "no_image"
                } else {
                    "bad_image"
                };
                return self.conclude(cred, status(Phase::Failed, Some(code))).await;
            }
        };
        let mut wanted = BTreeMap::new();
        for (name, on) in &d.flags {
            match flag_env(name) {
                Ok(n) => wanted.insert(n, Some(if *on { "1" } else { "0" }.to_string())),
                Err(_) => {
                    return self
                        .conclude(cred, status(Phase::Failed, Some("bad_flag")))
                        .await;
                }
            };
        }
        match d.embedding.as_ref().map(Embedding::env) {
            None => {}
            Some(Ok(env))
                if env["MEMSTORE_EMBED"]
                    .as_ref()
                    .is_some_and(|p| self.cfg.embed_providers.contains(p)) =>
            {
                wanted.extend(env)
            }
            Some(Ok(_)) => {
                return self
                    .conclude(cred, status(Phase::Failed, Some("embedding_not_allowed")))
                    .await;
            }
            Some(Err(_)) => {
                return self
                    .conclude(cred, status(Phase::Failed, Some("bad_embedding")))
                    .await;
            }
        }
        if !d.policy.windows.iter().all(Window::valid) {
            return self
                .conclude(cred, status(Phase::Failed, Some("bad_policy")))
                .await;
        }
        let managed = ManagedState {
            image: image.clone(),
            env: wanted.clone(),
        };
        if same {
            self.state.applied = Some(managed.clone());
            self.state.save(&self.cfg.state)?;
            return self
                .reconcile(cred, &d.policy, status(Phase::Healthy, None), managed)
                .await;
        }
        if !d.apply_now && !in_window(&d.policy.windows, Utc::now()) {
            self.report(cred, &status(Phase::WaitingWindow, None)).await;
            return Ok(());
        }
        if let Err(code) = self.check_image(&image).await {
            let s = status(Phase::Failed, Some(code));
            if matches!(code, "signature_missing" | "signature_invalid") {
                return self.conclude(cred, s).await;
            }
            self.report(cred, &s).await;
            return Ok(());
        }
        let previous = match self.target.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                // Nothing changed yet: said, not kept, so the next round tries again.
                eprintln!("agent: reading the target: {e:#}");
                self.report(cred, &status(Phase::Failed, Some("target_unreadable")))
                    .await;
                return Ok(());
            }
        };
        self.report(cred, &status(Phase::Applying, None)).await;
        let f = InFlight {
            generation: d.generation,
            version: d.image.as_ref().map(|i| i.tag.clone()).unwrap_or_default(),
            previous,
            started: Utc::now(),
            desired: Some(managed),
            rollback: None,
            repair: false,
        };
        self.state.in_flight = Some(f.clone());
        self.state.save(&self.cfg.state)?;
        self.finish(cred, f).await
    }

    async fn check_image(&self, image: &str) -> std::result::Result<(), &'static str> {
        let digest = image.rsplit_once('@').map(|(_, d)| d).ok_or("bad_image")?;
        match &self.cfg.cosign_key {
            Some(key) => self
                .registry
                .verify(&self.cfg.image_repository, digest, key)
                .await
                .map_err(|r| {
                    eprintln!("agent: image signature: {r:?}");
                    r.code()
                }),
            None if self.cfg.allow_unsigned_images => {
                eprintln!(
                    "agent: WARNING: applying {image} WITHOUT a signature check (MEMSTORE_AGENT_ALLOW_UNSIGNED=1)"
                );
                Ok(())
            }
            None => Err("signature_unverifiable"),
        }
    }

    /// Compare actual state on every round, without reinterpreting an accepted generation's fields.
    async fn reconcile(
        &mut self,
        cred: &Credential,
        policy: &Policy,
        mut status: Status,
        desired: ManagedState,
    ) -> Result<()> {
        let matches = match self.target.matches(&desired).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("agent: checking drift: {e:#}");
                status.phase = Phase::Failed;
                status.code = Some("target_unreadable".into());
                self.report(cred, &status).await;
                return Ok(());
            }
        };
        if matches {
            match self.target.rollout().await {
                Ok(r) if r.complete() && self.healthy().await => {}
                Ok(r) if r.complete() => {
                    status.phase = Phase::Failed;
                    status.code = Some("health_failed".into());
                }
                Ok(r) if r.stalled => {
                    status.phase = Phase::Failed;
                    status.code = Some("rollout_stalled".into());
                }
                Ok(_) => {
                    status.phase = Phase::Applying;
                    status.code = Some("rollout_pending".into());
                }
                Err(_) => {
                    status.phase = Phase::Failed;
                    status.code = Some("target_unreadable".into());
                }
            }
            self.report(cred, &status).await;
            return Ok(());
        }
        status.phase = Phase::Failed;
        status.code = Some("drift_detected".into());
        if self.cfg.drift_mode == DriftMode::Report {
            self.report(cred, &status).await;
            return Ok(());
        }
        // A past install-now request does not grant future repairs a maintenance-window exemption.
        if !policy.windows.iter().all(Window::valid) {
            status.code = Some("bad_policy".into());
            self.report(cred, &status).await;
            return Ok(());
        }
        if !in_window(&policy.windows, Utc::now()) {
            status.phase = Phase::WaitingWindow;
            self.report(cred, &status).await;
            return Ok(());
        }
        // An operator may have narrowed this local grant since the generation was installed.
        if let Some(Some(provider)) = desired.env.get("MEMSTORE_EMBED")
            && !self.cfg.embed_providers.contains(provider)
        {
            status.code = Some("embedding_not_allowed".into());
            self.report(cred, &status).await;
            return Ok(());
        }
        if let Err(code) = self.check_image(&desired.image).await {
            status.code = Some(code.into());
            self.report(cred, &status).await;
            return Ok(());
        }
        let f = InFlight {
            generation: status.generation,
            version: status.version.clone().unwrap_or_default(),
            previous: self.target.snapshot().await?,
            desired: Some(desired),
            started: Utc::now(),
            rollback: None,
            repair: true,
        };
        self.state.in_flight = Some(f.clone());
        self.state.save(&self.cfg.state)?;
        status.phase = Phase::Applying;
        status.code = Some("drift_repair".into());
        self.report(cred, &status).await;
        self.finish(cred, f).await
    }

    /// Resume the persisted phase, including a crash before its first patch or during rollback.
    async fn finish(&mut self, cred: &Credential, mut f: InFlight) -> Result<()> {
        let status = |phase, code: Option<&str>| Status {
            generation: f.generation,
            phase,
            version: Some(f.version.clone()).filter(|t| is_tag(t)),
            code: code.map(String::from),
        };
        if f.repair && self.cfg.drift_mode != DriftMode::Repair {
            self.state.in_flight = None;
            return self
                .conclude(cred, status(Phase::Failed, Some("repair_disabled")))
                .await;
        }
        if f.rollback.is_none() {
            let failure = if let Some(desired) = &f.desired {
                let applied = match self.target.matches(desired).await {
                    Ok(true) => Ok(()),
                    Ok(false) => self.target.set_managed(desired).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = applied {
                    eprintln!("agent: applying generation {}: {e:#}", f.generation);
                    Some("patch_failed")
                } else {
                    let deadline =
                        f.started + chrono::Duration::from_std(self.cfg.rollout_timeout)?;
                    match self.wait_rollout(deadline).await {
                        Err(code) => Some(code),
                        Ok(()) if !self.wait_healthy().await => Some("health_failed"),
                        Ok(()) if !self.target.matches(desired).await.unwrap_or(false) => {
                            Some("drift_detected")
                        }
                        Ok(()) => None,
                    }
                }
            } else {
                // Old state files did not keep the intended image. Never certify the old healthy
                // workload as the new version after a crash before the patch.
                Some("interrupted")
            };
            let Some(code) = failure else {
                self.state.applied = f.desired.clone();
                self.state.in_flight = None;
                return self.conclude(cred, status(Phase::Healthy, None)).await;
            };
            if f.repair {
                self.state.in_flight = None;
                return self.conclude(cred, status(Phase::Failed, Some(code))).await;
            }
            f.rollback = Some(Rollback {
                started: Utc::now(),
                code: code.into(),
            });
            self.state.in_flight = Some(f.clone());
            self.state.save(&self.cfg.state)?;
        }
        let rollback = f.rollback.as_ref().expect("rollback phase recorded");
        self.report(cred, &status(Phase::Applying, Some("rollback_in_progress")))
            .await;
        let previous = ManagedState::restore(&f.previous, f.desired.as_ref());
        let restored = match self.target.matches(&previous).await {
            Ok(true) => Ok(()),
            Ok(false) => self.target.set_managed(&previous).await,
            Err(e) => Err(e),
        };
        let code = if let Err(e) = restored {
            eprintln!("agent: rolling back: {e:#}");
            Some("rollback_failed")
        } else {
            let deadline = rollback.started + chrono::Duration::from_std(self.cfg.rollout_timeout)?;
            match self.wait_rollout(deadline).await {
                Err("rollout_timeout") => Some("rollback_timeout"),
                Err(_) => Some("rollback_stalled"),
                Ok(()) if !self.wait_healthy().await => Some("rollback_unhealthy"),
                Ok(()) if !self.target.matches(&previous).await.unwrap_or(false) => {
                    Some("rollback_drift")
                }
                Ok(()) => None,
            }
        };
        self.state.in_flight = None;
        let s = match code {
            Some(code) => status(Phase::Failed, Some(code)),
            None => status(Phase::RolledBack, Some(&rollback.code)),
        };
        self.conclude(cred, s).await
    }

    async fn wait_rollout(&self, deadline: DateTime<Utc>) -> std::result::Result<(), &'static str> {
        loop {
            match self.target.rollout().await {
                Ok(r) if r.complete() => return Ok(()),
                Ok(r) if r.stalled => return Err("rollout_stalled"),
                Ok(_) => {}
                Err(e) => eprintln!("agent: rollout: {e:#}"),
            }
            if Utc::now() >= deadline {
                return Err("rollout_timeout");
            }
            tokio::time::sleep(self.cfg.check_every).await;
        }
    }

    async fn wait_healthy(&self) -> bool {
        let until = tokio::time::Instant::now() + self.cfg.health_timeout;
        loop {
            if self.healthy().await {
                return true;
            }
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(self.cfg.check_every).await;
        }
    }

    /// The engine's own word: `/healthz` answers `{"ok": true}`; `/telemetry` (with an admin key) says
    /// what the exporter would send, and its state must be `ok` (no condition such as a blocked directory).
    pub async fn healthy(&self) -> bool {
        let mut req = self.http.get(&self.cfg.health_url);
        if let Some(t) = &self.cfg.health_token {
            req = req.bearer_auth(t);
        }
        let Ok(resp) = req.send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        let v: Value = resp.json().await.unwrap_or_default();
        match v["would_send"].as_array() {
            Some(sent) => sent.iter().any(|e| {
                e["payload"]["channel"] == "state" && e["payload"]["body"]["health"] == "ok"
            }),
            None => v["ok"] == true,
        }
    }

    /// The last word on a generation: kept, so it is never acted on again and is said again each round.
    async fn conclude(&mut self, cred: &Credential, s: Status) -> Result<()> {
        self.state.generation = self.state.generation.max(s.generation);
        self.state.status = Some(s.clone());
        if s.phase != Phase::Healthy {
            self.state.applied = None;
        }
        self.state.save(&self.cfg.state)?;
        self.report(cred, &s).await;
        Ok(())
    }

    async fn actions(&mut self, cred: &Credential) -> Result<()> {
        let a: Actions = self.fetch(cred, Kind::Actions).await?;
        for r in a.actions {
            if !is_action_id(&r.id) {
                eprintln!("agent: an action id that isn't one, skipped");
                continue;
            }
            // Delivered again: its result never reached the control plane. Say it again, never run it again.
            if let Some(d) = self.state.done(&r.id) {
                let result = d
                    .result
                    .clone()
                    .unwrap_or_else(|| ActionResult::of(Outcome::Failed, "interrupted"));
                d.result = Some(result.clone());
                self.post(cred, &r.id, &result).await?;
                continue;
            }
            self.state.actions.push(Done {
                id: r.id.clone(),
                result: None,
                posted: false,
            });
            let over = self.state.actions.len().saturating_sub(DONE_MAX);
            self.state.actions.drain(..over);
            self.state.save(&self.cfg.state)?;
            let result = if r.expires_at < Utc::now() {
                ActionResult::of(Outcome::Refused, "expired")
            } else {
                match parse_action(&r.kind, &r.args) {
                    Ok(act) => self.run_action(&cred.deployment, act).await,
                    Err(code) => ActionResult::of(Outcome::Refused, code),
                }
            };
            if let Some(d) = self.state.done(&r.id) {
                d.result = Some(result.clone());
            }
            self.state.save(&self.cfg.state)?;
            self.post(cred, &r.id, &result).await?;
        }
        Ok(())
    }

    async fn post(&mut self, cred: &Credential, id: &str, r: &ActionResult) -> Result<()> {
        let (status, _) = match cred
            .call(
                "POST",
                &format!("/v1/actions/{id}/result"),
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
        // 404: the control plane has its result already (or it lapsed); either way, nothing more to say.
        if status == 200 || status == 404 {
            if let Some(d) = self.state.done(id) {
                d.posted = true;
            }
            self.state.save(&self.cfg.state)?;
        } else {
            eprintln!("agent: the result of {id} was refused ({status})");
        }
        Ok(())
    }

    fn outbox(&self) -> Option<Outbox> {
        self.cfg
            .telemetry_mirror
            .clone()
            .map(|m| Outbox::at(self.cfg.credential.clone(), Some(m)))
    }

    async fn run_action(&mut self, id: &DeploymentId, act: Action) -> ActionResult {
        let counts = |kv: &[(&str, u64)]| kv.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        match act {
            Action::Restart => match self.target.restart().await {
                Ok(()) => ActionResult::ok(BTreeMap::new()),
                Err(e) => {
                    eprintln!("agent: restart: {e:#}");
                    ActionResult::of(Outcome::Failed, "restart_failed")
                }
            },
            // The engine has no admin endpoint that reindexes a scope; a reindex is `memstore index` on
            // the engine's own side.
            Action::Reindex { .. } => ActionResult::of(Outcome::Refused, "unsupported"),
            Action::DrainSpool => match self.outbox() {
                None => ActionResult::of(Outcome::Refused, "unsupported"),
                Some(o) => match o.flush().await {
                    Ok(sent) => ActionResult::ok(counts(&[
                        ("sent", sent as u64),
                        ("pending", o.pending().unwrap_or(0) as u64),
                    ])),
                    Err(e) => {
                        eprintln!("agent: drain_spool: {e:#}");
                        ActionResult {
                            counts: Some(counts(&[("pending", o.pending().unwrap_or(0) as u64)])),
                            ..ActionResult::of(Outcome::Failed, "drain_failed")
                        }
                    }
                },
            },
            Action::ClearSpool => match self.outbox() {
                None => ActionResult::of(Outcome::Refused, "unsupported"),
                Some(o) => match o.clear() {
                    Ok(n) => ActionResult::ok(counts(&[("cleared", n as u64)])),
                    Err(e) => {
                        eprintln!("agent: clear_spool: {e:#}");
                        ActionResult::of(Outcome::Failed, "clear_failed")
                    }
                },
            },
            Action::RotateKey => match deployment::rotate(&self.cfg.credential).await {
                Ok(()) => ActionResult::ok(BTreeMap::new()),
                Err(e) => {
                    eprintln!("agent: rotate_key for {}: {e:#}", id.as_str());
                    ActionResult::of(Outcome::Failed, "rotate_failed")
                }
            },
            Action::Diagnostics => {
                let mut c: BTreeMap<String, u64> = counts(&[
                    ("generation", self.state.generation),
                    ("health_ok", u64::from(self.healthy().await)),
                    ("actions_remembered", self.state.actions.len() as u64),
                ]);
                if let Some(o) = self.outbox() {
                    c.insert("spool_pending".into(), o.pending().unwrap_or(0) as u64);
                }
                if let Ok(r) = self.target.rollout().await {
                    let n = |v: i64| v.max(0) as u64;
                    c.insert("replicas_wanted".into(), n(r.wanted));
                    c.insert("replicas_updated".into(), n(r.updated));
                    c.insert("replicas_available".into(), n(r.available));
                }
                ActionResult::ok(c)
            }
        }
    }
}
