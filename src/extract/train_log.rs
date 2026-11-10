//! The training log (`MEMSTORE_TRAIN_LOG`): every model call memstore makes for a scope, kept whole (the prompt, the
//! reply, what the call cost, what it was for and what it read) with the outcome the caller's own checks gave it, so
//! a smaller model can later be trained on what the big one did, and scored on what its replies came to. Off unless a
//! location is named: the log holds the text of the episodes and runs the calls read, so it is the operator's
//! decision, per deployment, to keep one. [`crate::train_data`] sweeps it of what the memory has since forgotten and
//! exports it as training examples.
//!
//! - **One directory per request.** `<root>/v1/<org>/<role>/<first byte>/<id>/`, where `<id>` is the SHA-256 of the
//!   scope, backend, model and request body as sent (the model-call cache's key, with the scope): the same request
//!   asked again in the same scope, or answered from the cache, is the same directory, written once, and the same
//!   words read in another scope are a record of that scope's own. `record.json` holds the `prompt`
//!   and the request's `params`, the `reply` text, how it `finish`ed, whether its caller could `usable` it, the tokens
//!   and latency, `cached` (the model-call cache answered it), the `role` (what the call was for) and `refs` (what it
//!   read: the episode with its source, ACL and time, or the runs and the situation a note was written from).
//! - **What a request holds, once.** A chat call's `params` are what else it asked of the API (thinking,
//!   temperature); a decisions model's request is its own `prompt` (its state and questions), kept there alone. A
//!   routing call ([`Ctx::routing`]) names each line it asked about by the line's key, its text (a line of an index
//!   thousands of calls show alike) left out, and says so (`prompt_kept: "line keys"`, else `"whole"`): the
//!   question it routed, every line's key and score and the pick are what an audit of routing reads, and the export
//!   leaves such a record out, as its prompt isn't what was sent.
//! - **Outcomes are further files in it.** The caller's checks run after the reply: how many facts the quote rule
//!   kept, what the reconcile step decided, whether a note had its sections, which line the router named. Each is its
//!   own `verdict.<ms>.json`, never a rewrite of the record, so any number of processes share a location.
//! - **Markers say what a call read.** `<root>/v1/<org>/_by/<scope hash>/<kind>/<ref>/<role>.<id>` is a small object
//!   per episode (`kind` `episode`, holding the ACL it had) or run (`run`) a call read, so a forgotten episode's
//!   calls are found without reading the log: [`purge_ref`] removes them, and the sweep in `crate::train_data` finds
//!   the ones whose episode or run is gone by another path (offboarding, retention, a retraction).
//!   A call that read nothing the memory can forget (the router's question) is marked `untracked` with the day it was
//!   made, and the sweep removes it after `MEMSTORE_TRAIN_LOG_UNTRACKED_DAYS` (default 30). A record is written only
//!   once all its markers are.
//! - **Per org.** An org's records sit under its own prefix, so one tenant's data is one prefix to read, hand over or
//!   delete ([`purge_org`]). A call that names no scope, or an org whose name isn't a plain one (lower-case letters,
//!   digits and `_`, as every org's is), isn't kept: it has no tenant's prefix to go under.
//! - **Never in the way.** A write is one small object put, as the model-call cache's is, and a failure is logged once
//!   and costs the call nothing.
//!
//! The location is a directory or an object-store URI, as `MEMSTORE_MODEL_CACHE`'s is.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use futures::TryStreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Episode, Scope};

/// What a path covers; a change to a record's shape that a reader must tell gets a new one.
const RECIPE: &str = "v1";
/// The longest a write may take: the call is waiting on it.
const WRITE_SECS: u64 = 5;
/// Where the markers of what calls read are, under an org.
const BY: &str = "_by";

struct Sink {
    op: opendal::Operator,
    location: String,
    warned: AtomicBool,
}

static SINK: LazyLock<Option<Sink>> = LazyLock::new(|| {
    let location = std::env::var("MEMSTORE_TRAIN_LOG").ok()?.trim().to_string();
    if location.is_empty() || location.eq_ignore_ascii_case("off") {
        return None;
    }
    match crate::search::embed_cache::operator(&location) {
        Ok(op) => {
            eprintln!("training log: {location}/{RECIPE}");
            Some(Sink {
                op,
                location,
                warned: AtomicBool::new(false),
            })
        }
        Err(e) => {
            eprintln!("training log: none ({e:#}); model calls aren't logged");
            None
        }
    }
});

impl Sink {
    /// Whether the object is there now: written, or already (a record is immutable).
    async fn put(&self, path: &str, body: Vec<u8>) -> bool {
        let w = self.op.write_with(path, body);
        // A record is immutable: where one exists the request was logged before.
        let w = if self.op.info().full_capability().write_with_if_not_exists {
            w.if_not_exists(true)
        } else {
            w
        };
        let r = tokio::time::timeout(std::time::Duration::from_secs(WRITE_SECS), w).await;
        let err = match r {
            Ok(Ok(_)) => return true,
            Ok(Err(e)) if e.kind() == opendal::ErrorKind::ConditionNotMatch => return true,
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer in {WRITE_SECS}s"),
        };
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "training log {}: can't write {path} ({err}); carrying on without (further failures aren't logged)",
                self.location
            );
        }
        false
    }
}

/// Whether calls are being logged.
pub fn enabled() -> bool {
    SINK.is_some()
}

/// The directory a server logs by: an org's calls are kept only where its settings say `train_log`
/// (`Directory::trains`). Unset (`memstore train-log`, tests, a store with no server), every org's are.
static FOLLOW: std::sync::RwLock<Option<Arc<crate::people::directory::Directory>>> =
    std::sync::RwLock::new(None);

/// Log by `dir` from now on: what it says of each org, and for an org it doesn't hold, nothing.
pub fn follow(dir: crate::people::directory::Directory) {
    *FOLLOW.write().unwrap() = Some(Arc::new(dir));
}

/// A directory that can't be read: what was followed stays, and before any, nobody's calls are kept.
pub fn follow_none() {
    let mut f = FOLLOW.write().unwrap();
    if f.is_none() {
        *f = Some(Arc::new(crate::people::directory::Directory::empty()));
    }
}

/// Whether `scope`'s calls are kept: its org turned the log on, or no server is following a directory.
fn keeps(scope: &Scope) -> bool {
    FOLLOW
        .read()
        .unwrap()
        .as_ref()
        .is_none_or(|dir| dir.trains(scope))
}

tokio::task_local! {
    /// What the model calls made inside it are for.
    static CTX: Ctx;
}

struct Inner {
    role: &'static str,
    scope: String,
    /// Its org's path segment; none for an org no prefix can be made for (its calls aren't kept).
    org: Option<String>,
    refs: Value,
    /// The ids of the calls made inside it, for the outcome to name.
    calls: Mutex<Vec<String>>,
    /// Episodes the work found out it reads beyond `refs` (what a prompt shows of other episodes).
    also: Mutex<Vec<String>>,
    /// The lines a routing call asks about, by key ([`Ctx::routing`]).
    lines: Option<Lines>,
}

/// What a routing call asks of each line: `ask` and then the line's text, each question named `line<i>` for the
/// line whose key is `keys[i]`.
struct Lines {
    ask: String,
    keys: Vec<String>,
}

/// What one piece of work asks the model for: its role, its scope and what it reads. Calls made inside [`Ctx::run`]
/// are logged under it, and [`Ctx::verdict`] then says how the work came out.
#[derive(Clone)]
pub struct Ctx(Arc<Inner>);

/// The org a scope belongs to, as a path segment: its name as it is, which a path takes unchanged only for a plain
/// one (every org's is, [`crate::check_name`]); none for any other, so two orgs never share a prefix.
pub(crate) fn org_of(scope: &Scope) -> Option<String> {
    let org = match scope {
        Scope::NamedOrg(o) => o.as_str(),
        Scope::Team(id) => id.split_once('/').map_or(crate::DEFAULT_ORG, |(o, _)| o),
        Scope::Org | Scope::User(_) => crate::DEFAULT_ORG,
    };
    crate::check_name("org", org).ok().map(|_| org.to_string())
}

/// A scope as a path segment: a hash of its name, so no two scopes share one.
fn scope_dir(scope: &str) -> String {
    crate::auth::hex(&Sha256::digest(scope.as_bytes()))[..16].to_string()
}

impl Ctx {
    /// Work in `scope` for `role`, reading `refs` (a JSON object).
    pub fn new(role: &'static str, scope: &Scope, refs: Value) -> Ctx {
        Ctx(Arc::new(Inner {
            role,
            scope: scope.to_string(),
            org: org_of(scope).filter(|_| keeps(scope)),
            refs,
            calls: Mutex::new(vec![]),
            also: Mutex::new(vec![]),
            lines: None,
        }))
    }

    /// Routing work: a decisions model asked of lines (an index of kinds of task) whether `ask`, followed by each
    /// line's text, fits the question. Its records keep each line by its key, not its text ([`by_line_key`]).
    pub fn routing(
        role: &'static str,
        scope: &Scope,
        refs: Value,
        ask: &str,
        keys: Vec<String>,
    ) -> Ctx {
        Ctx(Arc::new(Inner {
            role,
            scope: scope.to_string(),
            org: org_of(scope).filter(|_| keeps(scope)),
            refs,
            calls: Mutex::new(vec![]),
            also: Mutex::new(vec![]),
            lines: Some(Lines {
                ask: ask.to_string(),
                keys,
            }),
        }))
    }

    /// Work on one episode: its id, source, ACL (who may read what the work finds) and time.
    pub fn episode(role: &'static str, scope: &Scope, ep: &Episode) -> Ctx {
        Ctx::new(
            role,
            scope,
            json!({"episode_id": ep.episode_id, "source": ep.source, "acl": ep.acl,
                   "occurred_at": ep.occurred_at}),
        )
    }

    /// Runs `f`, its model calls logged under this context.
    pub async fn run<F: Future>(&self, f: F) -> F::Output {
        CTX.scope(self.clone(), f).await
    }

    /// How the work came out, as the caller's own checks saw it (a JSON object), attached to every call made in it.
    pub async fn verdict(&self, verdict: Value) {
        let (Some(sink), Some(org)) = (SINK.as_ref(), self.0.org.as_deref()) else {
            return;
        };
        let ids = self.0.calls.lock().unwrap().clone();
        let ms = chrono::Utc::now().timestamp_millis();
        for id in ids {
            let record = json!({"v": 1, "id": id, "ts": chrono::Utc::now().to_rfc3339(), "role": self.0.role,
                                "verdict": verdict});
            let at = key(org, self.0.role, &id, &format!("verdict.{ms}.json"));
            sink.put(&at, record.to_string().into_bytes()).await;
        }
    }
}

/// Says the work in progress also reads these episodes (a prompt that shows facts other episodes gave): they are
/// marked like the one it was made for, so forgetting any of them takes the call. A call outside any [`Ctx`] ignores it.
pub fn also_read(episodes: impl IntoIterator<Item = String>) {
    let _ = CTX.try_with(|c| c.0.also.lock().unwrap().extend(episodes));
}

/// Where a call's file is kept.
fn key(org: &str, role: &str, id: &str, file: &str) -> String {
    format!("{RECIPE}/{org}/{role}/{}/{id}/{file}", &id[..2])
}

/// Where the marker of `id` (`<role>.<call>`) is: a call read `kind` `id` in `scope`.
fn marker(org: &str, scope: &str, kind: &str, id: &str, role: &str, call: &str) -> String {
    format!(
        "{RECIPE}/{org}/{BY}/{}/{kind}/{id}/{role}.{call}",
        scope_dir(scope)
    )
}

/// An id a marker's path may hold as it is: what an episode's or a run's id is made of.
fn plain(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// A model call as the extractor made it.
pub(crate) struct Call<'a> {
    pub backend: &'a str,
    pub model: &'a str,
    /// The request body as it went to the API (the cache's key hashes the same text).
    pub body: &'a Value,
    pub prompt: &'a str,
    pub max_tokens: u32,
    pub reply: &'a str,
    pub finish: Option<&'a str>,
    pub usable: bool,
    pub cached: bool,
    pub batch: bool,
    pub tokens: (u64, u64),
    pub latency_ms: u64,
}

/// The id of a request in a scope: the model-call cache's key with the scope, so the same words read in another
/// scope are another record, whose markers and purges are that scope's own.
fn id_of(scope: &str, backend: &str, model: &str, body: &str) -> String {
    let mut h = Sha256::new();
    for part in [scope, backend, model, body] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    crate::auth::hex(&h.finalize())
}

/// A routing request (`body`, as a decisions model takes it) with each question about a line named by that line's
/// key: `{"line": <key>}` in place of its instructions, which were `ask` followed by the line's text, and `ask` kept
/// once. The question it routed (the state) and everything else stay as sent. None where a question isn't of that
/// form: the request is then kept whole.
fn by_line_key(body: &Value, ask: &str, keys: &[String]) -> Option<Value> {
    let named = |name: &str, q: &Value| -> Option<Value> {
        let key = keys.get(name.strip_prefix("line")?.parse::<usize>().ok()?)?;
        q["instructions"].as_str()?.strip_prefix(ask)?;
        let mut q = q.clone();
        let o = q.as_object_mut()?;
        o.remove("instructions");
        o.insert("line".into(), json!(key));
        Some(q)
    };
    let mut b = body.clone();
    match &mut b["questions"] {
        Value::Object(qs) => {
            for (name, q) in qs.iter_mut() {
                let by_key = named(name, q)?;
                *q = by_key;
            }
        }
        Value::Array(qs) => {
            for q in qs.iter_mut() {
                let by_key = named(q["name"].as_str()?, q)?;
                *q = by_key;
            }
        }
        _ => return None,
    }
    b.as_object_mut()?.insert("ask".into(), json!(ask));
    Some(b)
}

/// Logs one call made inside a [`Ctx`]; one made outside any isn't logged.
pub(crate) async fn record(call: Call<'_>) {
    let Some(sink) = SINK.as_ref() else {
        return;
    };
    // A call that names no scope has no tenant's prefix to go under, so it isn't kept.
    let Ok(ctx) = CTX.try_with(Ctx::clone) else {
        return;
    };
    let Some(org) = ctx.0.org.as_deref() else {
        return;
    };
    let body = call.body.to_string();
    let id = id_of(&ctx.0.scope, call.backend, call.model, &body);
    let (role, scope) = (ctx.0.role, ctx.0.scope.as_str());
    let also = ctx.0.also.lock().unwrap().clone();
    let mut refs = ctx.0.refs.clone();
    if !also.is_empty() {
        refs["also_read"] = json!(also);
    }
    ctx.0.calls.lock().unwrap().push(id.clone());
    // The body without its prompt (its messages, and a system part sent apart for the prompt cache, which
    // `prompt` holds whole): what else was asked of the API (thinking, temperature).
    // A request that is its own prompt (a decisions model's: its state and questions) has nothing else to keep.
    let whole = call.prompt == body;
    let mut params = if whole {
        Value::Null
    } else {
        call.body.clone()
    };
    if let Some(o) = params.as_object_mut() {
        o.remove("messages");
        o.remove("model");
        o.remove("system");
    }
    // A routing call's lines by key: each is a line of an index thousands of calls show alike.
    let lines = ctx
        .0
        .lines
        .as_ref()
        .filter(|_| whole)
        .and_then(|l| by_line_key(call.body, &l.ask, &l.keys));
    let (prompt, kept) = match &lines {
        Some(b) => (b.to_string(), "line keys"),
        None => (call.prompt.to_string(), "whole"),
    };
    let record = json!({
        "v": 1, "id": id, "ts": chrono::Utc::now().to_rfc3339(), "memstore": env!("CARGO_PKG_VERSION"),
        "role": role, "scope": scope, "refs": refs,
        "backend": call.backend, "model": call.model, "batch": call.batch,
        "max_tokens": call.max_tokens, "params": params,
        "prompt": prompt, "prompt_kept": kept, "reply": call.reply,
        "finish": call.finish, "usable": call.usable, "cached": call.cached,
        "tokens_in": call.tokens.0, "tokens_out": call.tokens.1, "latency_ms": call.latency_ms,
    });
    // The markers first: a record whose markers failed to be written could never be found to be forgotten.
    let mut reads: Vec<(&str, String, Value)> = vec![];
    if let Some(e) = refs["episode_id"].as_str() {
        reads.push((
            "episode",
            e.to_string(),
            json!({"scope": scope, "acl": refs["acl"]}),
        ));
    }
    // A run is named by its answer id: a note and a sort read several, a judge one.
    let runs = refs["run_ids"].as_array().into_iter().flatten();
    for r in runs.chain(std::iter::once(&refs["answer_id"])) {
        if let Some(r) = r.as_str() {
            reads.push(("run", r.to_string(), json!({"scope": scope})));
        }
    }
    // What else the prompt showed, readable by everyone who may read the episode it was made for (a fact is shown
    // to a reader only if they may read every episode it cites), so under that episode's ACL.
    for e in &also {
        reads.push((
            "episode",
            e.clone(),
            json!({"scope": scope, "acl": refs["acl"]}),
        ));
    }
    // A call that read nothing the memory can forget (a router's question, a link of situations) is kept for a
    // while only: it is marked with the day it was made, and the sweep removes it once that day is old enough.
    if reads.is_empty() {
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        reads.push(("untracked", day, json!({"scope": scope})));
    }
    // The record only once every marker is there: a record that couldn't be found to be forgotten isn't kept.
    for (kind, rid, body) in reads {
        if !plain(&rid)
            || !sink
                .put(
                    &marker(org, scope, kind, &rid, role, &id),
                    body.to_string().into_bytes(),
                )
                .await
        {
            return;
        }
    }
    sink.put(
        &key(org, role, &id, "record.json"),
        record.to_string().into_bytes(),
    )
    .await;
}

/// What a marker says: a call (`role`, `call`) of `org` read `kind` `id` in `scope`.
#[derive(Clone, Debug)]
pub struct Marker {
    pub org: String,
    pub scope: Scope,
    pub kind: String,
    pub id: String,
    pub role: String,
    pub call: String,
    /// The ACL an episode had when the call read it.
    pub acl: Vec<String>,
}

fn sink() -> Result<&'static Sink> {
    SINK.as_ref()
        .context("no training log: MEMSTORE_TRAIN_LOG isn't set")
}

/// Every object under `prefix`, recursively.
async fn list(sink: &Sink, prefix: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut it = sink
        .op
        .lister_with(prefix)
        .recursive(true)
        .await
        .with_context(|| format!("list {prefix}"))?;
    while let Some(e) = it.try_next().await? {
        if e.metadata().is_file() {
            out.push(e.path().to_string());
        }
    }
    Ok(out)
}

/// The markers of `org` (every org with `None`), for the sweep.
pub async fn markers(org: Option<&str>) -> Result<Vec<Marker>> {
    let sink = sink()?;
    let prefix = match org {
        Some(o) => {
            crate::check_name("org", o)?;
            format!("{RECIPE}/{o}/{BY}/")
        }
        None => format!("{RECIPE}/"),
    };
    let mut out = vec![];
    for path in list(sink, &prefix).await? {
        // v1/<org>/_by/<scope>/<kind>/<ref>/<role>.<call>
        let p: Vec<&str> = path.split('/').collect();
        if p.len() != 7 || p[2] != BY {
            continue;
        }
        let Some((role, call)) = p[6].split_once('.') else {
            continue;
        };
        let body: Value = match sink.op.read(&path).await {
            Ok(b) => serde_json::from_slice(&b.to_vec()).unwrap_or_default(),
            Err(_) => continue,
        };
        let Some(scope) = body["scope"].as_str().and_then(|s| Scope::parse(s).ok()) else {
            continue;
        };
        out.push(Marker {
            org: p[1].to_string(),
            scope,
            kind: p[4].to_string(),
            id: p[5].to_string(),
            role: role.to_string(),
            call: call.to_string(),
            acl: body["acl"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(String::from))
                .collect(),
        });
    }
    Ok(out)
}

/// Removes a call: its directory (record and outcomes) and the marker `m`. The call's other markers, under what else it
/// read, are left for the sweep, which removes a marker whose call is gone.
pub async fn purge_call(m: &Marker) -> Result<()> {
    let sink = sink()?;
    let dir = format!(
        "{RECIPE}/{}/{}/{}/{}/",
        m.org,
        m.role,
        &m.call[..2.min(m.call.len())],
        m.call
    );
    sink.op.delete_with(&dir).recursive(true).await?;
    sink.op
        .delete(&marker(
            &m.org,
            &m.scope.to_string(),
            &m.kind,
            &m.id,
            &m.role,
            &m.call,
        ))
        .await?;
    Ok(())
}

/// Whether the call a marker names still has its record.
pub async fn call_exists(m: &Marker) -> Result<bool> {
    let sink = sink()?;
    let rec = format!(
        "{RECIPE}/{}/{}/{}/{}/record.json",
        m.org,
        m.role,
        &m.call[..2.min(m.call.len())],
        m.call
    );
    Ok(sink.op.exists(&rec).await?)
}

/// Removes every call that read `kind` `id` (an `episode` or a `run`) in `scope`, as soon as the memory forgets
/// it. Returns how many calls went. Without a log, nothing.
pub async fn purge_ref(scope: &Scope, kind: &str, id: &str) -> Result<usize> {
    if !enabled() || !plain(id) {
        return Ok(0);
    }
    let Some(org) = org_of(scope) else {
        return Ok(0);
    };
    let sink = sink()?;
    let prefix = format!(
        "{RECIPE}/{org}/{BY}/{}/{kind}/{id}/",
        scope_dir(&scope.to_string())
    );
    let mut n = 0;
    for path in list(sink, &prefix).await? {
        let Some((role, call)) = path.rsplit('/').next().and_then(|f| f.split_once('.')) else {
            continue;
        };
        let m = Marker {
            org: org.clone(),
            scope: scope.clone(),
            kind: kind.into(),
            id: id.into(),
            role: role.into(),
            call: call.into(),
            acl: vec![],
        };
        purge_call(&m).await?;
        n += 1;
    }
    Ok(n)
}

/// Removes everything an org's calls logged: one prefix. Without a log, nothing.
pub async fn purge_org(org: &str) -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    crate::check_name("org", org)?;
    sink()?
        .op
        .delete_with(&format!("{RECIPE}/{org}/"))
        .recursive(true)
        .await?;
    Ok(())
}

/// A call's files as listed: its record, and its outcomes by file name.
type Files = (Option<Value>, Vec<(String, Value)>);

/// A logged call: its record, and its outcomes in the order they were written.
pub struct Logged {
    pub record: Value,
    pub verdicts: Vec<Value>,
}

/// Every call logged for `scope`, with its outcomes.
pub async fn calls(scope: &Scope) -> Result<Vec<Logged>> {
    let Some(org) = org_of(scope) else {
        return Ok(vec![]);
    };
    let sink = sink()?;
    let want = scope.to_string();
    let mut by_dir: std::collections::BTreeMap<String, Files> = Default::default();
    for path in list(sink, &format!("{RECIPE}/{org}/")).await? {
        let p: Vec<&str> = path.split('/').collect();
        // v1/<org>/<role>/<xx>/<id>/<file>
        if p.len() != 6 || p[2] == BY {
            continue;
        }
        let Ok(b) = sink.op.read(&path).await else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&b.to_vec()) else {
            continue;
        };
        let e = by_dir.entry(p[..5].join("/")).or_default();
        if p[5] == "record.json" {
            e.0 = Some(v);
        } else if p[5].starts_with("verdict.") {
            e.1.push((p[5].to_string(), v));
        }
    }
    Ok(by_dir
        .into_values()
        .filter_map(|(r, mut v)| {
            let record = r.filter(|r| r["scope"].as_str() == Some(want.as_str()))?;
            v.sort_by(|a, b| a.0.cmp(&b.0));
            Some(Logged {
                record,
                verdicts: v.into_iter().map(|(_, v)| v["verdict"].clone()).collect(),
            })
        })
        .collect())
}
