//! One-off Kubernetes execution. The API only creates/observes Jobs; the worker opens the shared catalog.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Job, Status, Step};
use crate::people::directory::{Directory, SignIn};
use crate::server::writer::Turn;
use crate::{Principal, Scope, Store};

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Execution {
    pub name: String,
    pub namespace: String,
    pub created_at: i64,
}

/// The API's authorization at submission, never a caller-supplied worker identity or bearer token.
/// Directory membership, disables, closure removals and source ACLs are checked again by the worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Authorization {
    #[serde(default)]
    actor: Option<String>,
    groups: Vec<String>,
    superuser: bool,
    person: bool,
    orgs: Option<BTreeSet<String>>,
    provider_groups: Vec<String>,
    verified_email: bool,
    auth0: bool,
    auth0_org: Option<String>,
    /// An unverified password sign-in's Auth0 account: the job reaches only what an approval let it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unverified: Option<String>,
}

impl Authorization {
    pub(crate) fn new(who: &Principal, via: &SignIn, superuser: bool) -> Self {
        Self {
            actor: Some(who.user().into()),
            groups: who.groups().to_vec(),
            superuser,
            person: via.person,
            orgs: via.orgs.clone(),
            provider_groups: via.groups.clone(),
            verified_email: via.verified_email,
            auth0: via.auth0,
            auth0_org: via.auth0_org.clone(),
            unverified: via.unverified.clone(),
        }
    }

    async fn reader(&self, store: &Store, scope: &Scope, user: &str) -> Result<Principal> {
        let dir = Directory::fresh(store).await?;
        let actor = self.actor.as_deref().unwrap_or(user);
        ensure!(
            !dir.disabled
                .get(&crate::people::directory::user_id(actor))
                .copied()
                .unwrap_or(false),
            "dream creator is disabled"
        );
        let via = SignIn {
            person: self.person,
            orgs: self.orgs.clone(),
            groups: self.provider_groups.clone(),
            verified_email: self.verified_email,
            auth0: self.auth0,
            auth0_org: self.auth0_org.clone(),
            unverified: self.unverified.clone(),
            ..Default::default()
        };
        if !self.superuser {
            let org = scope
                .org_name()
                .context("dream scope must belong to an org")?;
            ensure!(
                dir.is_admin(actor, &via, org),
                "dream creator is no longer an org admin"
            );
            let team = match scope {
                Scope::Team(id) => Some(id.split_once('/').map_or(id.as_str(), |(_, team)| team)),
                _ => None,
            };
            let resolved = dir.resolve(actor, &via, Some(org), team)?;
            ensure!(resolved == *scope, "dream scope is no longer accessible");
        }
        let mut groups = self.groups.clone();
        if let Some(edit) = dir.closure_edits.get(&crate::auth::acl::norm_id(actor)) {
            groups.retain(|g| !edit.remove.contains(g));
            groups.extend(edit.add.clone());
        }
        Ok(Principal::from_parts(user.into(), groups))
    }
}

/// An operator-supplied PodTemplateSpec. Only the image/config/mounts/resources come from here;
/// command, arguments, restart policy, deadlines and run identity are controlled by the launcher.
pub struct Launcher {
    api: String,
    namespace: String,
    token: Option<PathBuf>,
    client: reqwest::Client,
    template: Value,
}

fn name(n: &str) -> Result<()> {
    ensure!(
        !n.is_empty()
            && n.len() <= 63
            && n.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && n.as_bytes()[0].is_ascii_alphanumeric()
            && n.as_bytes()[n.len() - 1].is_ascii_alphanumeric(),
        "invalid Kubernetes name"
    );
    Ok(())
}

impl Launcher {
    pub fn from_env() -> Result<Self> {
        let path = std::env::var("AMPUP_MEMORY_DREAM_JOB_TEMPLATE").context(
            "configure AMPUP_MEMORY_DREAM_JOB_TEMPLATE to launch dreams as Kubernetes Jobs",
        )?;
        let template = serde_json::from_slice(&std::fs::read(path)?)?;
        let namespace = std::env::var("AMPUP_MEMORY_DREAM_JOB_NAMESPACE")
            .or_else(|_| std::fs::read_to_string(Path::new(SA).join("namespace")))?;
        let override_api = std::env::var("AMPUP_MEMORY_DREAM_JOB_API").ok();
        let api = match override_api {
            Some(api) => api,
            None => {
                let host = std::env::var("KUBERNETES_SERVICE_HOST")
                    .context("dream launcher must run in Kubernetes")?;
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host
                };
                let port =
                    std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
                format!("https://{host}:{port}")
            }
        };
        let local = api.starts_with("http://");
        let token = (!local).then(|| Path::new(SA).join("token"));
        let ca = if local {
            None
        } else {
            Some(std::fs::read(Path::new(SA).join("ca.crt"))?)
        };
        Self::at(&api, namespace.trim(), token, ca.as_deref(), template)
    }

    /// Also used by contract tests with a loopback Kubernetes stand-in.
    pub fn at(
        api: &str,
        namespace: &str,
        token: Option<PathBuf>,
        ca: Option<&[u8]>,
        template: Value,
    ) -> Result<Self> {
        let api = crate::deployment::checked_url(api)?;
        name(namespace)?;
        ensure!(
            template["metadata"].is_null() || template["metadata"].is_object(),
            "template metadata must be an object"
        );
        ensure!(
            template["metadata"]["labels"].is_null() || template["metadata"]["labels"].is_object(),
            "template labels must be an object"
        );
        let containers = template["spec"]["containers"]
            .as_array()
            .context("template needs spec.containers")?;
        ensure!(
            containers.len() == 1
                && containers[0]["image"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
            "dream template needs exactly one container with an image"
        );
        ensure!(
            template["spec"]["serviceAccountName"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "dream template needs a worker serviceAccountName"
        );
        let mut client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15));
        if let Some(ca) = ca {
            client = client.add_root_certificate(reqwest::Certificate::from_pem(ca)?);
        }
        Ok(Self {
            api,
            namespace: namespace.into(),
            token,
            client: client.build()?,
            template,
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        execution: Option<&Execution>,
    ) -> Result<reqwest::RequestBuilder> {
        let suffix = if let Some(ex) = execution {
            name(&ex.name)?;
            ensure!(
                ex.namespace == self.namespace,
                "dream belongs to another Kubernetes namespace"
            );
            format!("/{}", ex.name)
        } else {
            String::new()
        };
        let mut req = self.client.request(
            method,
            format!(
                "{}/apis/batch/v1/namespaces/{}/jobs{suffix}",
                self.api, self.namespace
            ),
        );
        if let Some(path) = &self.token {
            req = req.bearer_auth(std::fs::read_to_string(path)?.trim());
        }
        Ok(req)
    }

    fn manifest(&self, job: &Job, ex: &Execution) -> Value {
        let mut template = self.template.clone();
        template["metadata"]["labels"]["memstore-dream"] = json!(job.id);
        template["metadata"]["labels"]["memstore-execution"] = json!(ex.name);
        let spec = &mut template["spec"];
        spec["restartPolicy"] = json!("Never");
        // The worker needs no Kubernetes API access, whatever the operator's template says.
        spec["automountServiceAccountToken"] = json!(false);
        spec["terminationGracePeriodSeconds"] = json!(30);
        let container = &mut spec["containers"][0];
        container["command"] = json!(["ampup-memory"]);
        container["args"] = json!([
            "dream-job",
            "--scope",
            job.scope,
            "--id",
            job.id,
            "--owner",
            job.creator,
            "--execution",
            ex.name
        ]);
        // Server health checks do not describe a finite worker process.
        for field in [
            "livenessProbe",
            "readinessProbe",
            "startupProbe",
            "lifecycle",
        ] {
            container.as_object_mut().unwrap().remove(field);
        }
        json!({"apiVersion":"batch/v1", "kind":"Job",
            "metadata":{"name":ex.name, "namespace":ex.namespace, "labels":{"app.kubernetes.io/component":"dream", "memstore-dream":job.id}},
            "spec":{"parallelism":1, "completions":1, "backoffLimit":0,
                "activeDeadlineSeconds":job.request.budget_seconds + 300,
                "ttlSecondsAfterFinished":604800, "template":template}})
    }

    async fn get(&self, ex: &Execution) -> Result<Option<Value>> {
        let resp = self.request(reqwest::Method::GET, Some(ex))?.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            resp.status().is_success(),
            "Kubernetes GET Job failed: {}",
            resp.status()
        );
        Ok(Some(resp.json().await?))
    }

    fn verify(v: &Value, job: &Job, ex: &Execution) -> Result<()> {
        ensure!(
            v["metadata"]["name"] == ex.name && v["metadata"]["labels"]["memstore-dream"] == job.id,
            "Kubernetes Job identity does not match this dream"
        );
        Ok(())
    }

    async fn create(&self, job: &Job, ex: &Execution) -> Result<()> {
        let resp = self
            .request(reqwest::Method::POST, None)?
            .json(&self.manifest(job, ex))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::CONFLICT {
            let v = self
                .get(ex)
                .await?
                .context("conflicting Kubernetes Job disappeared")?;
            return Self::verify(&v, job, ex);
        }
        ensure!(
            resp.status().is_success(),
            "Kubernetes create Job failed: {}",
            resp.status()
        );
        Ok(())
    }

    pub(crate) async fn cancel(&self, job: &Job) -> Result<()> {
        let Some(ex) = job.executions.last() else {
            return Ok(());
        };
        let Some(v) = self.get(ex).await? else {
            return Ok(());
        };
        Self::verify(&v, job, ex)?;
        let uid = v["metadata"]["uid"]
            .as_str()
            .context("Kubernetes Job has no UID")?;
        let resp = self.request(reqwest::Method::DELETE, Some(ex))?.json(&json!({
            "apiVersion":"v1", "kind":"DeleteOptions", "propagationPolicy":"Foreground", "preconditions":{"uid":uid}})).send().await?;
        ensure!(
            resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND,
            "Kubernetes delete Job failed: {}; cancellation is recorded; retry cancel_dream",
            resp.status()
        );
        Ok(())
    }

    /// Reconcile infrastructure failures, including OOM/startup/deadline failures where no worker saved a result.
    pub(crate) async fn observe(
        &self,
        store: &Store,
        scope: &Scope,
        who: &Principal,
        job: Job,
    ) -> Result<Job> {
        if matches!(
            job.status,
            Status::Completed | Status::Canceled | Status::Failed
        ) {
            return Ok(job);
        }
        let Some(ex) = job.executions.last() else {
            return Ok(job);
        };
        let failure = |observed: &Option<Value>| -> Result<Option<String>> {
            let error = match observed {
                Some(v) => {
                    Self::verify(v, &job, ex)?;
                    terminal(v).map(|state| {
                        format!("Kubernetes Job ended ({state}) without a completed dream record")
                    })
                }
                None if crate::now_micros() - ex.created_at > 120_000_000 => Some(
                    "Kubernetes Job is missing or its retention expired; explicitly resume the dream".into(),
                ),
                None => None,
            };
            Ok(error)
        };
        if failure(&self.get(ex).await?)?.is_none() {
            return Ok(job);
        }
        let turn = control(store, scope, &job.id).await?;
        let writing = turn.store(store);
        let result = async {
            let mut latest = super::get(&writing, scope, who, &job.id).await?;
            if latest.executions.last().map(|e| &e.name) == Some(&ex.name)
                && matches!(latest.status, Status::Pending | Status::Running)
            {
                // A retry may have recovered an uncertain POST using the SAME execution name while
                // we waited for control. Recheck under the launch/cancel lease before marking failure.
                if let Some(error) = failure(&self.get(ex).await?)? {
                    latest.status = Status::Failed;
                    latest.error = Some(error);
                    latest.ended_at = Some(crate::now_micros());
                    latest = super::save(&writing, scope, who, latest).await?;
                }
            }
            Ok(latest)
        }
        .await;
        turn.finish(&writing).await;
        result
    }

    /// Caller holds the control lease. Persist the execution name before POST, so an uncertain POST can
    /// be recovered with the exact same name. Never replace an active or terminating Kubernetes Job.
    pub(crate) async fn launch(
        &self,
        store: &Store,
        scope: &Scope,
        who: &Principal,
        mut job: Job,
        auth: Authorization,
    ) -> Result<Job> {
        ensure!(
            !matches!(job.status, Status::Completed | Status::Canceled),
            "dream is already terminal"
        );
        let mut reuse = false;
        if let Some(ex) = job.executions.last() {
            match self.get(ex).await? {
                Some(v) => {
                    Self::verify(&v, &job, ex)?;
                    if terminal(&v).is_none() {
                        return Ok(job);
                    }
                }
                None => {
                    reuse = job.status == Status::Pending;
                }
            }
        }
        if !reuse {
            ensure!(job.executions.len() < 32, "maximum 32 executions per dream");
            job.executions.push(Execution {
                name: format!("dream-{}", uuid::Uuid::new_v4().simple()),
                namespace: self.namespace.clone(),
                created_at: crate::now_micros(),
            });
        }
        job.authorization = Some(auth);
        job.status = Status::Pending;
        job.step = Step::Queued;
        job.error = None;
        job.ended_at = None;
        job = super::save(store, scope, who, job).await?;
        let ex = job.executions.last().unwrap();
        if let Err(e) = self.create(&job, ex).await {
            // An unknown POST outcome is not proof that the pod didn't start. Keep it recoverable.
            job.error = Some(format!(
                "launch unconfirmed; retry run_dream: {}",
                crate::server::writer::redacted(&e)
            ));
            // Return the diagnostic, but don't write a stale pending snapshot over a pod's progress.
        }
        Ok(job)
    }
}

fn terminal(v: &Value) -> Option<&str> {
    if v["status"]["active"].as_u64().unwrap_or(0) > 0
        || v["status"]["terminating"].as_u64().unwrap_or(0) > 0
    {
        return None;
    }
    v["status"]["conditions"].as_array()?.iter().find_map(|c| {
        let kind = c["type"].as_str()?;
        (c["status"] == "True" && matches!(kind, "Complete" | "Failed")).then_some(kind)
    })
}

/// How long a recorded run may wait for its first launch (`AMPUP_MEMORY_DREAM_LAUNCH_GRACE_SECS`, default 300):
/// the API waits up to 90 s for the launch lease.
fn unlaunched_grace_micros() -> i64 {
    std::env::var("AMPUP_MEMORY_DREAM_LAUNCH_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(300)
        .saturating_mul(1_000_000)
}

pub(crate) async fn control(store: &Store, scope: &Scope, id: &str) -> Result<Turn> {
    Turn::acquire(
        store,
        &format!("dream-control:{scope}:{id}"),
        chrono::Utc::now() + chrono::Duration::seconds(90),
    )
    .await
}

/// A finite CronJob trigger. It records one scope's fact pass, launches a separate worker pod and
/// exits. Its Kubernetes Job name is an idempotency key shared by duplicate trigger pods.
pub async fn schedule(
    store: &Store,
    scope: &Scope,
    tick: &str,
    budget_seconds: u64,
    compact: bool,
) -> Result<Job> {
    ensure!(
        scope.org_name().is_some(),
        "a scheduler needs an explicit org/team scope"
    );
    ensure!(
        !tick.is_empty()
            && tick.len() <= 128
            && tick
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid schedule tick"
    );
    let on = crate::dream::On::from_env()
        .context("AMPUP_MEMORY_DREAM_ON must enable the scheduled scope")?;
    ensure!(
        on.covers(scope),
        "this scope is not enabled for scheduled dreaming"
    );
    let launcher = Launcher::from_env()?;
    let model =
        crate::extract::Extractor::dream_from_env()?.context("no dreaming model is configured")?;
    let owner = Principal::new("memstore:dream-scheduler", &[]);
    let turn = Turn::acquire(
        store,
        &format!("dream-schedule:{scope}"),
        chrono::Utc::now() + chrono::Duration::seconds(90),
    )
    .await?;
    let writing = turn.store(store);
    let result = async {
        let existing = super::list(&writing, scope, &owner)
            .await?
            .into_iter()
            .find(|j| j.schedule_key.as_deref() == Some(tick));
        let job = if let Some(job) = existing {
            if matches!(job.status, Status::Pending | Status::Running) && !job.executions.is_empty()
            {
                // A retried trigger must observe a worker that already failed, rather than silently
                // treating an unrecorded pod failure as permission to launch a replacement execution.
                launcher.observe(&writing, scope, &owner, job).await?
            } else {
                job
            }
        } else {
            // A trigger finishes before its worker. Cross-check the durable run, not just CronJob Forbid.
            for active in super::list_for_admin(&writing, scope, &owner).await? {
                if active.request.mode == super::Mode::Reconcile
                    && matches!(active.status, Status::Pending | Status::Running)
                {
                    let creator = Principal::new(&active.creator, &[]);
                    let observed = if active.executions.is_empty() {
                        active
                    } else {
                        launcher.observe(&writing, scope, &creator, active).await?
                    };
                    if matches!(observed.status, Status::Pending | Status::Running) {
                        if observed.creator == owner.user() && observed.executions.is_empty() {
                            let auth = Authorization::new(&owner, &SignIn::default(), true);
                            let lock = control(&writing, scope, &observed.id).await?;
                            let controlled = lock.store(&writing);
                            let result = launcher
                                .launch(&controlled, scope, &owner, observed, auth)
                                .await;
                            lock.finish(&controlled).await;
                            return result;
                        }
                        // A manual run recorded but never launched (its API process died or lost the launch
                        // lease) would otherwise hold every later tick back forever. Past the time a launch
                        // takes, fail it and let this tick record its own pass.
                        if observed.executions.is_empty()
                            && crate::now_micros() - observed.created_at > unlaunched_grace_micros()
                        {
                            let lock = control(&writing, scope, &observed.id).await?;
                            let controlled = lock.store(&writing);
                            let failed = async {
                                let mut latest =
                                    super::get(&controlled, scope, &creator, &observed.id).await?;
                                if latest.executions.is_empty()
                                    && matches!(latest.status, Status::Pending | Status::Running)
                                {
                                    latest.status = Status::Failed;
                                    latest.error = Some(
                                        "never launched; a later scheduled pass superseded it"
                                            .into(),
                                    );
                                    latest.ended_at = Some(crate::now_micros());
                                    latest =
                                        super::save(&controlled, scope, &creator, latest).await?;
                                }
                                anyhow::Ok(latest)
                            }
                            .await;
                            lock.finish(&controlled).await;
                            if matches!(failed?.status, Status::Failed) {
                                continue;
                            }
                        }
                        return Ok(observed);
                    }
                }
            }
            super::record(
                &writing,
                scope,
                &owner,
                model.model(),
                super::Request {
                    mode: super::Mode::Reconcile,
                    inputs: Default::default(),
                    instructions: String::new(),
                    compact,
                    budget_seconds,
                },
                vec![],
                Some(tick.into()),
            )
            .await?
        };
        if matches!(
            job.status,
            Status::Completed | Status::Canceled | Status::Failed
        ) {
            return Ok(job);
        }
        let auth = Authorization::new(&owner, &SignIn::default(), true);
        let lock = control(&writing, scope, &job.id).await?;
        let controlled = lock.store(&writing);
        let result = launcher.launch(&controlled, scope, &owner, job, auth).await;
        lock.finish(&controlled).await;
        result
    }
    .await;
    turn.finish(&writing).await;
    result
}

/// The finite worker command. No HTTP server, scheduler or unrelated extraction loops are started.
pub async fn worker(
    store: &Store,
    scope: &Scope,
    id: &str,
    owner: &str,
    execution: &str,
) -> Result<Job> {
    eprintln!("dream {id}: starting Kubernetes execution {execution}");
    // A duplicate pod must not fail early and make Kubernetes kill the legitimate worker. Wait for
    // its whole bounded execution; the Job's activeDeadlineSeconds remains the outer hard limit.
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(3900);
    let turn = Turn::acquire(store, &format!("dream-worker:{scope}:{id}"), deadline).await?;
    let writing = turn.store(store);
    let result = async {
        // Catalog-level access is operator authority. Restore only the identity approved in the record.
        writing.invalidate(crate::SYSTEM_NS, "ledger");
        writing.invalidate(&scope.namespace(), crate::QA_LOG);
        let raw = crate::qa_log::list(&writing, scope, None).await?.into_iter()
            .find(|r| r.answer_id == id && r.asker == owner && r.kind.as_deref() == Some(super::KIND))
            .context("dream worker record is unavailable")?;
        let job: Job = serde_json::from_str(raw.answer.as_deref().context("dream record is empty")?)?;
        ensure!(job.executions.last().is_some_and(|e| e.name == execution), "stale dream execution");
        let authorization = job.authorization.as_ref().context("dream has no approved worker identity")?;
        let who = authorization.reader(&writing, scope, owner).await?;
        let current = super::get(&writing, scope, &who, id).await?;
        if matches!(current.status, Status::Completed | Status::Canceled | Status::Failed) { return Ok(current); }
        let model = crate::extract::Extractor::dream_from_env()?.context("no dreaming model configured in worker")?;
        let work = super::run(&writing, scope, &who, id, &model, || async {
            let deadline = chrono::Utc::now() + chrono::Duration::seconds(job.request.budget_seconds as i64);
            let turn = Turn::acquire(&writing, &format!("index:{scope}"), deadline).await?;
            let indexed = turn.store(&writing);
            let active = || async {
                indexed.check_fence()?;
                let reader = authorization.reader(&indexed, scope, owner).await?;
                ensure!(super::get(&indexed, scope, &reader, id).await?.status != Status::Canceled, "dream was canceled");
                anyhow::Ok(())
            };
            let compacted = async {
                active().await?;
                super::progress(&indexed, scope, &who, id, Step::IndexingFacts).await?;
                let (facts, _) = crate::search::update(&indexed, scope).await?;
                active().await?;
                super::progress(&indexed, scope, &who, id, Step::IndexingPassages).await?;
                let (passages, _, _) = crate::search::passages::update(&indexed, scope).await?;
                active().await?;
                super::progress(&indexed, scope, &who, id, Step::CompactingFacts).await?;
                let fact_tidy = crate::search::tidy_night(&indexed, scope).await?;
                active().await?;
                super::progress(&indexed, scope, &who, id, Step::CompactingPassages).await?;
                let passage_tidy = crate::search::passages::tidy_night(&indexed, scope).await?;
                let counts = |(merged, compacted, dropped)| json!({"indexes_merged":merged, "parts_compacted":compacted, "duplicates_dropped":dropped});
                Ok(json!({"indexed_fact_rows":facts, "indexed_passages":passages, "facts":counts(fact_tidy), "passages":counts(passage_tidy)}))
            }.await;
            turn.finish(&indexed).await;
            compacted
        });
        // Poll the monitor as a future beside the work. Awaiting catalog reads inside a select branch
        // would stop polling work while it might own the very catalog/cache lock the monitor needs.
        let monitor = async {
            let mut poll = tokio::time::interval(Duration::from_secs(2));
            loop {
                poll.tick().await;
                writing.check_fence()?;
                let reader = authorization.reader(&writing, scope, owner).await?;
                let latest = super::get(&writing, scope, &reader, id).await?;
                ensure!(latest.executions.last().is_some_and(|e| e.name == execution), "stale dream execution");
                if latest.status == Status::Canceled { return Ok(latest); }
            }
        };
        tokio::select! { result = work => result, result = monitor => result }
    }.await;
    turn.finish(&writing).await;
    result
}
