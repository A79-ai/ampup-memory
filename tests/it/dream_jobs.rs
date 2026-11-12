//! Dreaming in Kubernetes, one flow per feature, end to end: a real `ampup-memory serve` over HTTP on a local
//! catalog, a stand-in Kubernetes Jobs API and model, and the finite worker each Job runs (the `dream-job`
//! entry point), invoked against its own `Store` the way a worker pod opens the catalog.
//!
//! The fact pass itself (what a dream may read and change) is `access_flow`'s; dreaming over people's notes is
//! `isolation`'s and `offboarding`'s.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use memstore::auth::{self, OidcConfig, TokenAuth};
use memstore::dream_jobs::{Job, Status};
use memstore::server::{Options, serve_with};
use memstore::{Episode, Fact, Principal, Scope, Store};
use serde_json::{Value, json};
use sha2::Digest;

use crate::isolation::{ADMIN_KEY, CLIENT_ID, admin, id_token, post, start_idp};

/// One deployment: a serve with two orgs (alice admin of both, bob a member of acme), the Jobs API Kubernetes
/// would serve, and a model that records every prompt it is sent.
struct Rig {
    _dir: tempfile::TempDir,
    template: std::path::PathBuf,
    store: Store,
    server: String,
    issuer: String,
    alice: String,
    bob: String,
    jobs: Arc<Mutex<HashMap<String, Value>>>,
    deleted: Arc<AtomicUsize>,
    uncertain_post: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
    prompts: Arc<Mutex<Vec<String>>>,
}

const SYNTHESIS: &str = "selected-refund-guide";
const UNSELECTED: &str = "unselected-secret";

impl Rig {
    async fn start() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("pod.json");
        std::fs::write(
            &template,
            json!({"spec":{"serviceAccountName":"dream-worker", "automountServiceAccountToken":true,
                "containers":[{"name":"dream", "image":"memstore:test", "envFrom":[{"secretRef":{"name":"model"}}],
                "resources":{"requests":{"cpu":"1", "memory":"2Gi"}}}]}})
            .to_string(),
        )
        .unwrap();
        let jobs = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
        let deleted = Arc::new(AtomicUsize::new(0));
        let uncertain_post = Arc::new(AtomicBool::new(false));
        let unavailable = Arc::new(AtomicBool::new(false));
        let kube_url = kubernetes(
            jobs.clone(),
            deleted.clone(),
            uncertain_post.clone(),
            unavailable.clone(),
        )
        .await;
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let model_url = model(prompts.clone()).await;
        set(&[
            ("AMPUP_MEMORY_DECIDE", "off"),
            ("AMPUP_MEMORY_EXTRACT", "off"),
            ("AMPUP_MEMORY_DREAM", "openai"),
            ("AMPUP_MEMORY_DREAM_URL", &model_url),
            ("AMPUP_MEMORY_DREAM_MODEL", "stand-in"),
            ("AMPUP_MEMORY_DREAM_KEY", "stand-in"),
            ("AMPUP_MEMORY_EMBED", "openai"),
            ("AMPUP_MEMORY_EMBED_URL", &model_url),
            ("AMPUP_MEMORY_EMBED_MODEL", "stand-in-embedding"),
            ("AMPUP_MEMORY_EMBED_DIMS", "4"),
            ("AMPUP_MEMORY_EMBED_KEY", "stand-in"),
            ("AMPUP_MEMORY_EMBED_CACHE", "off"),
            ("AMPUP_MEMORY_MODEL_CACHE", "off"),
            ("AMPUP_MEMORY_DREAM_ON", "org:acme,org:globex"),
            ("AMPUP_MEMORY_DREAM_SCHEDULER", "kubernetes"),
            (
                "AMPUP_MEMORY_DREAM_JOB_TEMPLATE",
                template.to_str().unwrap(),
            ),
            ("AMPUP_MEMORY_DREAM_JOB_NAMESPACE", "test"),
            ("AMPUP_MEMORY_DREAM_JOB_API", &kube_url),
        ]);
        let issuer = start_idp().await;
        let store = crate::local_store(dir.path()).await;
        store.ensure_scope(&Scope::Org).await.unwrap();
        let keys = json!([{"id":"platform", "sha256":auth::hex(&sha2::Sha256::digest(ADMIN_KEY.as_bytes())),
            "user":"svc:platform", "admin":true}]);
        let auth = TokenAuth::new(Some(&keys.to_string()), None)
            .unwrap()
            .allow_local_issuers()
            .with_oidc(
                OidcConfig::discover(&issuer, vec![CLIENT_ID.into()])
                    .await
                    .unwrap(),
            );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_with(
            port,
            Scope::Org,
            vec![],
            Options {
                listener: Some(listener),
                catalog: Some(store.catalog()),
                index_base: Some(store.index_base().into()),
                token_auth: Some(auth),
                ..Default::default()
            },
        ));
        let server = format!("http://127.0.0.1:{port}");
        let up = Instant::now();
        while reqwest::Client::new().get(&server).send().await.is_err() {
            assert!(up.elapsed() < Duration::from_secs(30));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for org in ["acme", "globex"] {
            admin(
                &server,
                json!({"op":"create_org","org":org,"admin":"alice@acme.com"}),
            )
            .await;
            store.ensure_scope(&Scope::org(org).unwrap()).await.unwrap();
        }
        admin(
            &server,
            json!({"op":"invite","org":"acme","members":[{"user":"bob@acme.com"}]}),
        )
        .await;
        let alice = id_token(&issuer, "alice@acme.com");
        let bob = id_token(&issuer, "bob@acme.com");
        Rig {
            _dir: dir,
            template,
            store,
            server,
            issuer,
            alice,
            bob,
            jobs,
            deleted,
            uncertain_post,
            unavailable,
            prompts,
        }
    }

    fn url(&self, org: &str) -> String {
        format!("{}/admin?org={org}", self.server)
    }

    /// An admin call as `token` on `org`: the HTTP status and the answer.
    async fn call(&self, token: &str, org: &str, mut req: Value) -> (u16, Value) {
        req["org"] = json!(org);
        let (status, body) = post(&self.url(org), token, &req).await;
        (
            status,
            serde_json::from_str(&body).unwrap_or(Value::String(body)),
        )
    }

    /// `call`, which must succeed.
    async fn ok(&self, token: &str, org: &str, req: Value) -> Value {
        let (status, body) = self.call(token, org, req.clone()).await;
        assert_eq!(status, 200, "{req}: {body}");
        body
    }

    /// `create_dream`, waiting out the server's first writer lease as a client would.
    async fn create(&self, token: &str, org: &str, request: Value) -> Value {
        let started = Instant::now();
        loop {
            let (status, body) = self
                .call(token, org, json!({"op":"create_dream","request":request}))
                .await;
            if status == 200 {
                return body;
            }
            assert!(
                body.to_string().contains("writer lease")
                    && started.elapsed() < Duration::from_secs(30),
                "{status}: {body}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// What a worker pod does for `job`'s latest execution: open the catalog afresh and run to the end.
    async fn work(&self, org: &str, job: &Job) -> Job {
        let store =
            Store::new(self.store.catalog()).with_index_base(self.store.index_base().to_string());
        let ex = job.executions.last().expect("a launched run");
        tokio::time::timeout(
            Duration::from_secs(45),
            memstore::dream_jobs::kubernetes::worker(
                &store,
                &Scope::org(org).unwrap(),
                &job.id,
                &job.creator,
                &ex.name,
            ),
        )
        .await
        .expect("a finite dream worker must finish")
        .unwrap()
    }

    /// `work`, for a run alice started through the API.
    async fn work_view(&self, org: &str, view: &Value) -> Job {
        let job = memstore::dream_jobs::get(
            &self.store,
            &Scope::org(org).unwrap(),
            &Principal::new("alice@acme.com", &[]),
            view["id"].as_str().unwrap(),
        )
        .await
        .unwrap();
        self.work(org, &job).await
    }

    fn launched(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    fn fail_pod(&self, name: &str) {
        self.jobs.lock().unwrap().get_mut(name).unwrap()["status"] =
            json!({"conditions":[{"type":"Failed","status":"True"}]});
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }

    /// Two facts about one thing that everyone in `org` may read, quoting `marker`: enough for a fact pass to
    /// put something to the model.
    async fn seed_facts(&self, org: &str, marker: &str) {
        let ep = Episode::sourced(&format!("docs:{marker}"), marker.as_bytes(), &["*"])
            .without_text_index();
        let mut rule =
            Fact::new("policy:refund", "rule", "Verify each request").from_episodes(&[&ep]);
        let mut guidance =
            Fact::new("policy:refund", "guidance", "Check previous refunds").from_episodes(&[&ep]);
        rule.quote = Some(marker.into());
        guidance.quote = Some(marker.into());
        self.store
            .commit(&Scope::org(org).unwrap(), "test", &[ep], &[rule, guidance])
            .await
            .unwrap();
    }

    /// A guide alice may read and a document only bob may read, for synthesis to select from.
    async fn seed_sources(&self) {
        let ep = Episode::sourced(
            "docs:refunds",
            SYNTHESIS.as_bytes(),
            &["user:alice@acme.com"],
        )
        .without_text_index();
        let other = Episode::sourced("docs:other", UNSELECTED.as_bytes(), &["user:bob@acme.com"])
            .without_text_index();
        self.store
            .commit(&Scope::org("acme").unwrap(), "test", &[ep, other], &[])
            .await
            .unwrap();
    }
}

fn set(vars: &[(&str, &str)]) {
    // SAFETY: every test here runs in its own process (`crate::own_process`).
    unsafe {
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
    }
}

fn steps(view: &Value) -> Vec<Value> {
    view["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["step"].clone())
        .collect()
}

/// The batch/v1 Jobs API, holding what it is sent. A Job stays pending until a test marks it or deletes it.
async fn kubernetes(
    jobs: Arc<Mutex<HashMap<String, Value>>>,
    deleted: Arc<AtomicUsize>,
    uncertain_post: Arc<AtomicBool>,
    unavailable: Arc<AtomicBool>,
) -> String {
    let kube = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", kube.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (conn, _) = kube.accept().await.unwrap();
            let state = (
                jobs.clone(),
                deleted.clone(),
                uncertain_post.clone(),
                unavailable.clone(),
            );
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let (jobs, deleted, uncertain_post, unavailable) = state.clone();
                        async move {
                            let method = req.method().clone();
                            let path = req.uri().path().to_string();
                            assert!(path.starts_with("/apis/batch/v1/namespaces/test/jobs"));
                            let bytes = req.into_body().collect().await.unwrap().to_bytes();
                            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                            let mut jobs = jobs.lock().unwrap();
                            let name = path.rsplit('/').next().unwrap().to_string();
                            let (status, answer) = if unavailable.load(Ordering::SeqCst) {
                                (503, json!({}))
                            } else {
                                match method {
                                    hyper::Method::POST => {
                                        let name =
                                            body["metadata"]["name"].as_str().unwrap().to_string();
                                        match jobs.entry(name) {
                                            std::collections::hash_map::Entry::Occupied(_) => {
                                                (409, json!({}))
                                            }
                                            std::collections::hash_map::Entry::Vacant(slot) => {
                                                let mut body = body;
                                                body["metadata"]["uid"] =
                                                    json!(format!("uid-{}", slot.key()));
                                                slot.insert(body.clone());
                                                let lost =
                                                    uncertain_post.swap(false, Ordering::SeqCst);
                                                (if lost { 500 } else { 201 }, body)
                                            }
                                        }
                                    }
                                    hyper::Method::GET => jobs
                                        .get(&name)
                                        .map_or((404, json!({})), |v| (200, v.clone())),
                                    hyper::Method::DELETE => {
                                        assert_eq!(
                                            body["preconditions"]["uid"],
                                            jobs[&name]["metadata"]["uid"]
                                        );
                                        assert_eq!(body["propagationPolicy"], "Foreground");
                                        jobs.remove(&name);
                                        deleted.fetch_add(1, Ordering::SeqCst);
                                        (200, json!({}))
                                    }
                                    _ => panic!("unexpected Kubernetes request"),
                                }
                            };
                            Ok::<_, hyper::Error>(
                                hyper::Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(answer.to_string())))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(conn), svc)
                    .await;
            });
        }
    });
    url
}

/// An OpenAI-compatible model: embeddings, and chat answers that decide nothing. It records every chat prompt.
async fn model(prompts: Arc<Mutex<Vec<String>>>) -> String {
    let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}/v1", model.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (conn, _) = model.accept().await.unwrap();
            let prompts = prompts.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let prompts = prompts.clone();
                        async move {
                            let bytes = req.into_body().collect().await.unwrap().to_bytes();
                            let body: Value = serde_json::from_slice(&bytes).unwrap();
                            let answer = if let Some(inputs) = body["input"].as_array() {
                                let data: Vec<_> = (0..inputs.len())
                                    .map(|i| json!({"index":i,"embedding":[1.0,0.0,0.0,0.0]}))
                                    .collect();
                                json!({"data":data})
                            } else {
                                let prompt =
                                    body["messages"][0]["content"].as_str().unwrap().to_string();
                                let output = if prompt.contains("You are tidying a team's memory.")
                                {
                                    "{\"ends\":[],\"same\":[]}"
                                } else {
                                    "{\"memories\":[]}"
                                };
                                prompts.lock().unwrap().push(prompt);
                                json!({"choices":[{"message":{"content":output},"finish_reason":"stop"}],
                                    "usage":{"prompt_tokens":41,"completion_tokens":5}})
                            };
                            Ok::<_, hyper::Error>(
                                hyper::Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(answer.to_string())))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(conn), svc)
                    .await;
            });
        }
    });
    url
}

/// An org admin starts a fact pass over the API. The API only records it and launches a Job; the worker the Job
/// runs reconciles the org's facts and saves a durable, count-only result.
#[tokio::test(flavor = "multi_thread")]
async fn an_admin_starts_a_fact_pass_that_runs_in_its_own_job() {
    if !crate::own_process("dream_jobs::an_admin_starts_a_fact_pass_that_runs_in_its_own_job") {
        return;
    }
    let r = Rig::start().await;
    r.seed_facts("acme", "acme-reconciliation-fixture").await;
    let request = json!({"mode":"reconcile","budget_seconds":30});
    let (status, _) = r
        .call(
            &r.bob,
            "acme",
            json!({"op":"create_dream","request":request}),
        )
        .await;
    assert_eq!(status, 403, "a member who isn't an admin starts nothing");
    for injected in [
        json!({"op":"create_dream","namespace":"other","request":request}),
        json!({"op":"create_dream","request":{"mode":"reconcile","image":"evil"}}),
        json!({"op":"create_dream","request":{"mode":"reconcile","budget_seconds":99999}}),
    ] {
        assert_ne!(
            r.call(&r.alice, "acme", injected.clone()).await.0,
            200,
            "{injected}"
        );
    }
    let run = r.create(&r.alice, "acme", request).await;
    assert_eq!(run["status"], "pending");
    assert!(
        run.get("authorization").is_none(),
        "the worker's identity stays out of the API: {run}"
    );
    assert_eq!(r.calls(), 0, "the API must not dream");
    let name = run["executions"][0]["name"].as_str().unwrap();
    let pod = r.jobs.lock().unwrap()[name].clone();
    assert_eq!(pod["spec"]["backoffLimit"], 0);
    assert_eq!(pod["spec"]["activeDeadlineSeconds"], 30 + 300);
    let spec = &pod["spec"]["template"]["spec"];
    assert_eq!(spec["restartPolicy"], "Never");
    assert_eq!(
        spec["automountServiceAccountToken"], false,
        "the worker gets no Kubernetes token, whatever the template says"
    );
    assert_eq!(spec["containers"][0]["args"][0], "dream-job");

    let done = r.work_view("acme", &run).await;
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert_eq!(done.report.as_ref().unwrap()["result"], "done");
    assert!(r.calls() > 0, "the worker runs the fact reconciler");
    assert!(
        r.prompts
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.contains("You are tidying a team's memory."))
    );
    let view = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":run["id"]}))
        .await;
    assert_eq!(view["status"], "completed");
    assert_eq!(view["observation"]["state"], "record_only");
    for step in ["queued", "reconciling", "reconciled", "finished"] {
        assert!(
            steps(&view).contains(&json!(step)),
            "missing {step}: {view}"
        );
    }
    assert!(view.get("authorization").is_none(), "{view}");
    assert_eq!(
        r.call(&r.bob, "acme", json!({"op":"get_dream","id":run["id"]}))
            .await
            .0,
        403
    );
}

/// The CronJob's trigger: a tick records a run and launches one worker, however many trigger pods run it, launches
/// nothing while a pass is active, and holds the daily/new-fact gate. With Kubernetes scheduling, the engine's own
/// timers stand down.
#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_tick_launches_one_worker_and_holds_the_gate() {
    if !crate::own_process("dream_jobs::a_scheduled_tick_launches_one_worker_and_holds_the_gate") {
        return;
    }
    let r = Rig::start().await;
    let acme = Scope::org("acme").unwrap();
    r.seed_facts("acme", "acme-reconciliation-fixture").await;
    assert_eq!(
        memstore::server::maintenance::dream_step(&r.store, &acme, Duration::from_secs(30))
            .await
            .unwrap()["result"],
        "external_scheduler",
        "the engine's nightly step stands down"
    );
    let schedule = |tick: &'static str| {
        memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, tick, 30, false)
    };
    let (first, duplicate) = tokio::join!(schedule("tick-1"), schedule("tick-1"));
    let scheduled = first.unwrap();
    assert_eq!(
        scheduled.id,
        duplicate.unwrap().id,
        "a retried trigger reuses its run"
    );
    assert_eq!(r.launched(), 1, "duplicate trigger pods launch one worker");
    assert_eq!(
        schedule("tick-2").await.unwrap().id,
        scheduled.id,
        "an active pass outlives its tick"
    );
    assert_eq!(r.launched(), 1);

    // The gate (a day since the last pass, 200 new facts) holds the first scheduled pass: no model calls.
    let held = r.work("acme", &scheduled).await;
    assert_eq!(held.status, Status::Completed);
    assert_eq!(held.report.as_ref().unwrap()["result"], "skipped");
    assert_eq!(r.calls(), 0);
    assert_eq!(
        schedule("tick-1").await.unwrap().id,
        scheduled.id,
        "a finished tick is idempotent too"
    );

    // Past the gate, the next tick runs a pass.
    set(&[
        ("AMPUP_MEMORY_DREAM_SECS", "1"),
        ("AMPUP_MEMORY_DREAM_FACTS", "1"),
    ]);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let next = schedule("tick-3").await.unwrap();
    assert_ne!(next.id, scheduled.id);
    assert_eq!(r.launched(), 2);
    let ran = r.work("acme", &next).await;
    assert_eq!(ran.report.as_ref().unwrap()["result"], "done", "{ran:?}");
    assert!(r.calls() > 0);
    let history = r
        .ok(&r.alice, "acme", json!({"op":"list_dreams","summary":true}))
        .await;
    assert!(
        history["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == next.id && j["scheduled"] == true)
    );

    assert!(
        memstore::dream_jobs::kubernetes::schedule(&r.store, &Scope::Org, "tick-4", 30, false)
            .await
            .is_err(),
        "an unnamed default scope never becomes a deployment-wide dream"
    );
    set(&[("AMPUP_MEMORY_DREAM_ON", "org:globex")]);
    assert!(
        schedule("tick-5").await.is_err(),
        "the scheduler's scope list applies"
    );
    set(&[("AMPUP_MEMORY_DREAM_SCHEDULER", "typo")]);
    assert!(
        memstore::dream::internal_scheduler().is_err(),
        "a mistyped scheduler is refused"
    );
}

/// Synthesis over selected sources: the inputs are frozen at creation and read with the creator's permissions, the
/// worker writes candidate memories and then maintains the indexes, and a finished job never runs again.
#[tokio::test(flavor = "multi_thread")]
async fn a_synthesis_job_reads_only_its_selected_sources() {
    if !crate::own_process("dream_jobs::a_synthesis_job_reads_only_its_selected_sources") {
        return;
    }
    let r = Rig::start().await;
    r.seed_sources().await;
    let request =
        json!({"inputs":{"source_prefixes":["docs:"]}, "compact":true, "budget_seconds":30});
    assert_eq!(
        r.call(
            &r.bob,
            "acme",
            json!({"op":"create_dream","request":request})
        )
        .await
        .0,
        403
    );
    let run = r.create(&r.alice, "acme", request).await;
    assert_eq!(
        run["inputs"].as_array().unwrap().len(),
        1,
        "only what alice may read is selected: {run}"
    );
    let done = r.work_view("acme", &run).await;
    assert_eq!(done.status, Status::Completed, "{done:?}");
    {
        let prompts = r.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains(SYNTHESIS));
        assert!(!prompts[0].contains(UNSELECTED));
    }
    let view = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":run["id"]}))
        .await;
    assert_eq!(view["status"], "completed");
    assert_eq!(view["maintenance"]["status"], "completed");
    assert_eq!(view["usage"]["input_tokens"], 41);
    for step in [
        "queued",
        "synthesizing",
        "candidates_saved",
        "indexing_facts",
        "compacting_facts",
        "finished",
    ] {
        assert!(
            steps(&view).contains(&json!(step)),
            "missing {step}: {view}"
        );
    }
    assert_eq!(
        r.call(&r.bob, "acme", json!({"op":"get_dream","id":run["id"]}))
            .await
            .0,
        403
    );
    assert_eq!(
        r.call(&r.alice, "acme", json!({"op":"run_dream","id":run["id"]}))
            .await
            .0,
        403,
        "a completed job never runs again"
    );
}

/// Cancelling: a pending run never starts work, and a launched one is cancelled durably even while Kubernetes is
/// down, its Job deleted (with the Job's UID as a precondition) once Kubernetes answers again.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_a_run_even_while_kubernetes_is_down() {
    if !crate::own_process("dream_jobs::cancelling_stops_a_run_even_while_kubernetes_is_down") {
        return;
    }
    let r = Rig::start().await;
    r.seed_facts("acme", "acme-reconciliation-fixture").await;
    // Recorded, never launched; cancelling it needs no launcher configuration.
    let pending = memstore::dream_jobs::create(
        &r.store,
        &Scope::org("acme").unwrap(),
        &Principal::new("alice@acme.com", &[]),
        "stand-in",
        serde_json::from_value(json!({"mode":"reconcile"})).unwrap(),
    )
    .await
    .unwrap();
    let template = std::fs::read(&r.template).unwrap();
    std::fs::remove_file(&r.template).unwrap();
    let canceled = r
        .ok(
            &r.alice,
            "acme",
            json!({"op":"cancel_dream","id":pending.id}),
        )
        .await;
    std::fs::write(&r.template, template).unwrap();
    assert_eq!(canceled["status"], "canceled");
    assert_eq!(
        r.call(&r.alice, "acme", json!({"op":"run_dream","id":pending.id}))
            .await
            .0,
        403,
        "nor resumed"
    );

    let run = r
        .create(
            &r.alice,
            "acme",
            json!({"mode":"reconcile","budget_seconds":30}),
        )
        .await;
    assert_eq!(
        r.call(&r.bob, "acme", json!({"op":"cancel_dream","id":run["id"]}))
            .await
            .0,
        403
    );
    r.unavailable.store(true, Ordering::SeqCst);
    let stale = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":run["id"]}))
        .await;
    assert_eq!(stale["status"], "pending");
    assert_eq!(
        stale["observation"]["state"], "unavailable",
        "an outage is not proof the work failed"
    );
    assert_ne!(
        r.call(
            &r.alice,
            "acme",
            json!({"op":"cancel_dream","id":run["id"]})
        )
        .await
        .0,
        200,
        "the Job's deletion is unconfirmed"
    );
    let view = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":run["id"]}))
        .await;
    assert_eq!(
        view["status"], "canceled",
        "the cancellation is durable through the outage"
    );
    r.unavailable.store(false, Ordering::SeqCst);
    r.ok(
        &r.alice,
        "acme",
        json!({"op":"cancel_dream","id":run["id"]}),
    )
    .await;
    assert_eq!(r.deleted.load(Ordering::SeqCst), 1);
    assert_eq!(r.launched(), 0);
    r.ok(
        &r.alice,
        "acme",
        json!({"op":"cancel_dream","id":run["id"]}),
    )
    .await;
    let worker = r.work_view("acme", &run).await;
    assert_eq!(
        worker.status,
        Status::Canceled,
        "a worker that starts late does no work"
    );
    assert_eq!(r.calls(), 0);
}

/// A worker pod that dies is observed and recorded as failed. An admin resumes the run in a new Job (once, however
/// often asked), a launch whose answer was lost is never launched twice, and a trigger never restarts a failed
/// scheduled worker on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_run_resumes_in_a_new_job() {
    if !crate::own_process("dream_jobs::a_failed_run_resumes_in_a_new_job") {
        return;
    }
    let r = Rig::start().await;
    r.seed_facts("acme", "acme-reconciliation-fixture").await;
    let request = json!({"mode":"reconcile","budget_seconds":30});
    let run = r.create(&r.alice, "acme", request.clone()).await;
    r.fail_pod(run["executions"][0]["name"].as_str().unwrap());
    let failed = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":run["id"]}))
        .await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(
        r.call(&r.bob, "acme", json!({"op":"run_dream","id":run["id"]}))
            .await
            .0,
        403
    );
    let resumed = r
        .ok(&r.alice, "acme", json!({"op":"run_dream","id":run["id"]}))
        .await;
    assert_eq!(resumed["executions"].as_array().unwrap().len(), 2);
    let again = r
        .ok(&r.alice, "acme", json!({"op":"run_dream","id":run["id"]}))
        .await;
    assert_eq!(
        again["executions"], resumed["executions"],
        "a resume while the Job is pending launches nothing"
    );
    assert_eq!(r.launched(), 2);
    let done = r.work_view("acme", &resumed).await;
    assert_eq!(done.status, Status::Completed, "{done:?}");

    // Kubernetes accepted the Job but its answer was a failure: the recorded name makes the resume idempotent.
    r.uncertain_post.store(true, Ordering::SeqCst);
    let uncertain = r.create(&r.alice, "acme", request).await;
    assert!(
        uncertain["error"]
            .as_str()
            .unwrap()
            .contains("launch unconfirmed"),
        "{uncertain}"
    );
    let resumed = r
        .ok(
            &r.alice,
            "acme",
            json!({"op":"run_dream","id":uncertain["id"]}),
        )
        .await;
    assert_eq!(resumed["executions"], uncertain["executions"]);
    assert_eq!(r.launched(), 3);
    r.ok(
        &r.alice,
        "acme",
        json!({"op":"cancel_dream","id":uncertain["id"]}),
    )
    .await;

    let acme = Scope::org("acme").unwrap();
    let scheduled =
        memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, "tick-1", 30, false)
            .await
            .unwrap();
    r.fail_pod(&scheduled.executions[0].name);
    let before = r.launched();
    let retried = memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, "tick-1", 30, false)
        .await
        .unwrap();
    assert_eq!(retried.status, Status::Failed);
    assert_eq!(
        retried.executions.len(),
        1,
        "a trigger retry never silently restarts a failed worker"
    );
    assert_eq!(r.launched(), before);
}

/// A manual run recorded but never launched (its API process died, or it never got the launch lease) would hold
/// every later tick back. Past the launch grace, the next tick fails it and runs its own pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_never_launched_is_superseded_by_the_next_tick() {
    if !crate::own_process("dream_jobs::a_run_that_never_launched_is_superseded_by_the_next_tick") {
        return;
    }
    let r = Rig::start().await;
    set(&[("AMPUP_MEMORY_DREAM_LAUNCH_GRACE_SECS", "2")]);
    let acme = Scope::org("acme").unwrap();
    let orphan = memstore::dream_jobs::create(
        &r.store,
        &acme,
        &Principal::new("alice@acme.com", &[]),
        "stand-in",
        serde_json::from_value(json!({"mode":"reconcile"})).unwrap(),
    )
    .await
    .unwrap();
    let young = memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, "tick-1", 30, false)
        .await
        .unwrap();
    assert_eq!(
        young.id, orphan.id,
        "within the grace, the run may still be launching"
    );
    assert_eq!(r.launched(), 0);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let tick = memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, "tick-2", 30, false)
        .await
        .unwrap();
    assert_ne!(tick.id, orphan.id);
    assert_eq!(r.launched(), 1, "the tick launched its own pass");
    let orphan = r
        .ok(&r.alice, "acme", json!({"op":"get_dream","id":orphan.id}))
        .await;
    assert_eq!(orphan["status"], "failed");
    assert!(
        orphan["error"].as_str().unwrap().contains("never launched"),
        "{orphan}"
    );
    assert_eq!(r.work("acme", &tick).await.status, Status::Completed);
}

/// Run history: summaries carry counts, never inputs, outputs or the starter's identity; pages and status filters
/// hold; malformed requests are refused, and so is anyone who isn't an admin.
#[tokio::test(flavor = "multi_thread")]
async fn admins_page_through_count_only_history() {
    if !crate::own_process("dream_jobs::admins_page_through_count_only_history") {
        return;
    }
    let r = Rig::start().await;
    r.seed_sources().await;
    let first = r
        .create(
            &r.alice,
            "acme",
            json!({"inputs":{"source_prefixes":["docs:"]}, "budget_seconds":30}),
        )
        .await;
    let second = r
        .create(
            &r.alice,
            "acme",
            json!({"mode":"reconcile","budget_seconds":30}),
        )
        .await;
    r.ok(
        &r.alice,
        "acme",
        json!({"op":"cancel_dream","id":second["id"]}),
    )
    .await;
    let page = r
        .ok(
            &r.alice,
            "acme",
            json!({"op":"list_dreams","limit":1,"summary":true}),
        )
        .await;
    assert_eq!(page["data"][0]["id"], second["id"], "newest first");
    assert_eq!(page["next_page"], second["id"]);
    for field in ["inputs", "outputs", "authorization", "request", "timeline"] {
        assert!(
            page["data"][0].get(field).is_none(),
            "a summary carries no {field}"
        );
    }
    let next = r
        .ok(
            &r.alice,
            "acme",
            json!({"op":"list_dreams","limit":1,"summary":true,"after":second["id"]}),
        )
        .await;
    assert_eq!(next["data"][0]["id"], first["id"]);
    assert_eq!(next["data"][0]["input_count"], 1);
    assert!(next["next_page"].is_null(), "{next}");
    let full = r.ok(&r.alice, "acme", json!({"op":"list_dreams"})).await;
    assert!(
        full["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j.get("authorization").is_none())
    );
    let canceled = r
        .ok(
            &r.alice,
            "acme",
            json!({"op":"list_dreams","summary":true,"statuses":["canceled"]}),
        )
        .await;
    assert_eq!(canceled["data"].as_array().unwrap().len(), 1);
    assert_eq!(canceled["data"][0]["id"], second["id"]);
    for malformed in [
        json!({"op":"list_dreams","limit":"10"}),
        json!({"op":"list_dreams","limit":0}),
        json!({"op":"list_dreams","summary":"true"}),
        json!({"op":"list_dreams","statuses":[]}),
        json!({"op":"list_dreams","after":"drm_unknown"}),
    ] {
        assert_ne!(
            r.call(&r.alice, "acme", malformed.clone()).await.0,
            200,
            "{malformed}"
        );
    }
    assert_eq!(
        r.call(&r.bob, "acme", json!({"op":"list_dreams"})).await.0,
        403
    );
    let (status, _) = post(
        &r.url("acme"),
        "",
        &json!({"op":"list_dreams","org":"acme"}),
    )
    .await;
    assert_eq!(status, 401);
}

/// One org's dreaming never reaches another's: the same admin, catalog and tick key give each org its own run,
/// each worker's model sees only its org's facts, and neither org's runs or history read through the other.
#[tokio::test(flavor = "multi_thread")]
async fn each_orgs_dreaming_stays_in_that_org() {
    if !crate::own_process("dream_jobs::each_orgs_dreaming_stays_in_that_org") {
        return;
    }
    let r = Rig::start().await;
    set(&[
        ("AMPUP_MEMORY_DREAM_SECS", "1"),
        ("AMPUP_MEMORY_DREAM_FACTS", "1"),
    ]);
    r.seed_facts("acme", "acme-only-fixture").await;
    r.seed_facts("globex", "globex-only-fixture").await;
    let (acme, globex) = (Scope::org("acme").unwrap(), Scope::org("globex").unwrap());
    let (a, g) = tokio::join!(
        memstore::dream_jobs::kubernetes::schedule(&r.store, &acme, "same-tick", 30, false),
        memstore::dream_jobs::kubernetes::schedule(&r.store, &globex, "same-tick", 30, false),
    );
    let (a, g) = (a.unwrap(), g.unwrap());
    assert_ne!(a.id, g.id);
    assert_eq!(
        (a.scope.as_str(), g.scope.as_str()),
        ("org:acme", "org:globex")
    );
    assert_eq!(r.launched(), 2);
    assert_eq!(r.work("acme", &a).await.report.unwrap()["result"], "done");
    let after_acme = r.calls();
    assert!(after_acme > 0);
    assert_eq!(r.work("globex", &g).await.report.unwrap()["result"], "done");
    {
        let prompts = r.prompts.lock().unwrap();
        let (acme_prompts, globex_prompts) = prompts.split_at(after_acme);
        assert!(!globex_prompts.is_empty());
        assert!(
            acme_prompts
                .iter()
                .all(|p| p.contains("acme-only-fixture") && !p.contains("globex-only-fixture"))
        );
        assert!(
            globex_prompts
                .iter()
                .all(|p| p.contains("globex-only-fixture") && !p.contains("acme-only-fixture"))
        );
    }

    // alice administers both, and still reads acme's run only through acme.
    let (status, body) = r
        .call(&r.alice, "globex", json!({"op":"get_dream","id":a.id}))
        .await;
    assert_eq!(status, 403, "{body}");
    for (org, own, other) in [("acme", &a.id, &g.id), ("globex", &g.id, &a.id)] {
        let history = r
            .ok(&r.alice, org, json!({"op":"list_dreams","summary":true}))
            .await;
        let ids: Vec<_> = history["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].clone())
            .collect();
        assert!(
            ids.contains(&json!(own)) && !ids.contains(&json!(other)),
            "{org}: {ids:?}"
        );
    }
    // bob, a member of acme alone, administers neither and reads neither.
    for (org, id) in [("acme", &a.id), ("globex", &g.id)] {
        assert_eq!(
            r.call(&r.bob, org, json!({"op":"get_dream","id":id}))
                .await
                .0,
            403
        );
    }
    // An admin of another org alone can't start acme's dreaming.
    admin(
        &r.server,
        json!({"op":"create_org","org":"initech","admin":"gary@globex.com"}),
    )
    .await;
    let gary = id_token(&r.issuer, "gary@globex.com");
    let (status, _) = r
        .call(
            &gary,
            "acme",
            json!({"op":"create_dream","request":{"mode":"reconcile"}}),
        )
        .await;
    assert_eq!(status, 403);
}
