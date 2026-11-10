//! Workspaces: isolated, disposable memories (`memstore workspace …`), for experiments and anything else
//! that needs a memory of its own, built from scratch, unchanged while it is read, and deleted afterwards.
//!
//! A workspace is an org whose directory entry says it is one (`kind: workspace`). That gives it an org's
//! isolation: its own scopes (`org:<ws>`, and `team:<ws>/<team>` if it has teams), each its own namespace
//! with its own search index under the index location; its own keys; selection the way orgs are selected
//! (`x-memstore-org`, or `?org=` on an MCP URL); and deletion through the org purge, which leaves nothing in
//! the catalog, the bucket or the index location. On top of an org it adds:
//!
//! - **Snapshots** (`memstore workspace snapshot`): the ledger heads of every scope and branch, under one id.
//!   A read pinned to a snapshot (`x-memstore-at`, or `?at=`) sees exactly that memory, whatever is committed
//!   later. Snapshots are rows of the workspace's own `workspace_snapshots` table, so they go with it.
//! - **Limits**: caps on its episodes, facts and index bytes (a commit or index update past one is refused),
//!   and a time to live, after which it is no longer served and `memstore workspace gc` deletes it.
//! - **Serving without sign-in**: a server that identifies no one serves workspaces side by side when started
//!   with `--workspaces` (`MEMSTORE_SERVE_WORKSPACES=1`): a request selects one, and nothing else in the
//!   catalog is reachable that way.
//!
//! Arms of an experiment that share their sources are branches of one workspace: load once on main, then
//! give each arm a branch (`memstore workspace branch`), and snapshot. A read selects a branch with
//! `x-memstore-branch` (or `?branch=`); a snapshot taken with `--branch` reads that branch by default.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arrow_array::{ArrayRef, StringArray};
use iceberg::spec::PrimitiveType;
use iceberg::{NamespaceIdent, TableIdent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::people::directory::{Actor, Change, Directory};
use crate::{DEFAULT_ORG, EPISODES, FACTS, MAIN, Scope, Session, Store};

/// The table in a workspace's own scope that holds its snapshots. Not `snapshots`: a REST catalog (Polaris)
/// reads `<namespace>.snapshots` as the snapshots metadata table of a table named like the namespace.
pub(crate) const SNAPSHOTS: &str = "workspace_snapshots";

/// What makes an org a workspace, as its directory entry keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    /// The schema it was created with (a built-in name or a file), as given; the schema itself is committed
    /// to the workspace's scope like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// When it stops being served and `memstore workspace gc` may delete it (µs since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub limits: Limits,
}

/// Caps on a workspace, over all its scopes. None: no cap.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_episodes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_facts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_index_bytes: Option<u64>,
}

impl Spec {
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

/// A workspace's memory at one moment: the heads of every scope and branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `snap_` and 16 hex digits of the hash of the workspace, the branch and the heads: the same memory
    /// snapshotted again gets the same id.
    pub id: String,
    pub workspace: String,
    /// The branch a read pinned to it reads, unless the read names another.
    pub branch: String,
    /// Per scope (`org:<ws>`, `team:<ws>/<team>`), per branch, its heads. A branch with no commit is absent.
    pub heads: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// µs since the epoch.
    pub taken_at: i64,
    pub actor: String,
    /// Per scope, the index versions its pinned searches use: the newest update of each index that covers
    /// nothing the snapshot doesn't reach, tagged with the snapshot's id so they are kept. A scope missing
    /// here (a snapshot from before indexes were pinned) searches the current indexes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub index: BTreeMap<String, crate::search::IndexPin>,
}

impl Snapshot {
    /// The heads a read of `scope` on `branch` is pinned to. A branch the scope had no commit on yet reads
    /// as empty when it is main; any other branch the snapshot doesn't hold is refused.
    pub fn heads_of(&self, scope: &Scope, branch: &str) -> Result<Vec<String>> {
        let Some(branches) = self.heads.get(&scope.to_string()) else {
            anyhow::bail!("snapshot {} has no scope {scope}", self.id);
        };
        match branches.get(branch) {
            Some(h) => Ok(h.clone()),
            None if branch == MAIN => Ok(vec![]),
            None => anyhow::bail!("snapshot {} has no branch {branch:?} of {scope}", self.id),
        }
    }
}

/// The directory's view of a workspace: its spec, if `ws` is one.
pub fn spec_of(dir: &Directory, ws: &str) -> Option<Spec> {
    dir.orgs.get(ws).and_then(|o| o.workspace.clone())
}

/// The workspace `ws` as the directory has it now; refused when there is no such workspace (an org that is
/// not a workspace included).
pub async fn get(store: &Store, ws: &str) -> Result<Spec> {
    let dir = Directory::load(store).await?;
    match (dir.orgs.contains_key(ws), spec_of(&dir, ws)) {
        (_, Some(s)) => Ok(s),
        (true, None) => anyhow::bail!("{ws} is an org, not a workspace"),
        (false, None) => anyhow::bail!("no workspace {ws}"),
    }
}

/// `memstore workspace create`: the org, marked as a workspace, with its scope's tables and, given one, its
/// schema committed on main. Refused if the name is taken (by an org or a workspace).
pub async fn create(
    store: &Store,
    actor: &Actor,
    ws: &str,
    spec: Spec,
    schema: Option<&crate::query::schema::Schema>,
) -> Result<Value> {
    anyhow::ensure!(ws != DEFAULT_ORG, "{DEFAULT_ORG} is the catalog's own org");
    crate::people::directory::apply(
        store,
        actor,
        &Change::CreateWorkspace {
            org: ws.into(),
            spec: spec.clone(),
        },
    )
    .await?;
    let scope = Scope::org(ws)?;
    store.ensure_scope(&scope).await?;
    if let Some(s) = schema {
        store
            .set_schema(&scope, MAIN, &actor.user, s, false)
            .await?;
    }
    show(store, ws).await
}

/// `memstore workspace delete`: the org delete, which purges every scope's tables and search indexes, plus
/// its rows in the system ledger. Refused for an org that is not a workspace.
pub async fn delete(
    store: &Store,
    actor: &Actor,
    ws: &str,
    confirm: Option<&str>,
) -> Result<String> {
    let dir = Directory::load(store).await?;
    // A delete that stopped halfway is finished by running it again: the org is gone, its memory not.
    let pending = !dir.orgs.contains_key(ws) && dir.purging.contains(ws);
    if !pending {
        get(store, ws).await?;
    }
    let namespaces = store.org_namespaces(ws).await?;
    crate::people::directory::apply(
        store,
        actor,
        &Change::DeleteOrg {
            org: ws.into(),
            confirm: confirm.map(String::from),
        },
    )
    .await?;
    forget_snapshots(ws);
    Ok(format!(
        "deleted workspace {ws}: {} scopes purged with their search indexes and ledger rows",
        namespaces.len()
    ))
}

/// `memstore workspace gc`: delete every workspace past its time to live. Returns what was deleted.
pub async fn gc(store: &Store, actor: &Actor) -> Result<Vec<String>> {
    let now = crate::now_micros();
    let dir = Directory::load(store).await?;
    let mut out = vec![];
    for (ws, org) in &dir.orgs {
        if org.workspace.as_ref().is_some_and(|s| s.expired(now)) {
            out.push(delete(store, actor, ws, Some(ws)).await?);
        }
    }
    Ok(out)
}

/// `memstore workspace branch`: a branch of every scope of the workspace, from main as it is now.
pub async fn branch(store: &Store, actor: &str, ws: &str, name: &str) -> Result<()> {
    get(store, ws).await?;
    // Every scope of the workspace, not only its org scope: a corpus loaded into a team scope
    // (`team:<ws>/<team>`) takes an arm's schema and writes on the same branch, and a snapshot of the
    // branch covers them all.
    for scope in scopes(store, ws).await? {
        store.create_branch(&scope, name, actor).await?;
    }
    Ok(())
}

/// `memstore workspace snapshot`: record the heads of every scope and branch of `ws` as a snapshot, and
/// return it. Taken again on an unchanged workspace it is the same snapshot (with `label` added).
pub async fn snapshot(
    store: &Store,
    actor: &str,
    ws: &str,
    branch: &str,
    label: Option<&str>,
) -> Result<Snapshot> {
    get(store, ws).await?;
    let mut heads = BTreeMap::new();
    for scope in scopes(store, ws).await? {
        heads.insert(scope.to_string(), store.branch_heads(&scope).await?);
    }
    anyhow::ensure!(
        branch == MAIN || heads.values().any(|b| b.contains_key(branch)),
        "workspace {ws} has no branch {branch:?}"
    );
    let mut h = Sha256::new();
    h.update(format!(
        "{ws}\n{branch}\n{}",
        serde_json::to_string(&heads)?
    ));
    let id = format!("snap_{}", &crate::auth::hex(&h.finalize())[..16]);
    let have = store.snapshots(ws).await?;
    // Taken before: the same memory, read with the same index versions (a new label is another row).
    if let Some(old) = have.iter().find(|s| s.id == id) {
        if label.is_some()
            && !have
                .iter()
                .any(|s| s.id == id && s.label.as_deref() == label)
        {
            let mut again = old.clone();
            again.label = label.map(String::from);
            again.taken_at = crate::now_micros();
            again.actor = actor.into();
            store.append_snapshot(&again).await?;
        }
        return Ok(old.clone());
    }
    let mut index = BTreeMap::new();
    for (name, branches) in &heads {
        let scope = Scope::parse(name)?;
        let all: Vec<String> = branches.values().flatten().cloned().collect();
        let reach = store.reachable(&scope, &all).await?;
        let (fdir, pdir) = (
            crate::search::dir_uri(store, &scope),
            crate::search::passages::dir_uri(store, &scope),
        );
        let pin = crate::search::IndexPin {
            facts: crate::search::newest_within(&crate::search::coverage(&fdir).await?, &reach),
            passages: crate::search::newest_within(&crate::search::coverage(&pdir).await?, &reach),
        };
        for (dir, c) in [(&fdir, &pin.facts), (&pdir, &pin.passages)] {
            if let Some(c) = c {
                crate::search::tag_versions(dir, c, &id).await?;
            }
        }
        index.insert(name.clone(), pin);
    }
    let snap = Snapshot {
        id,
        workspace: ws.into(),
        branch: branch.into(),
        heads,
        label: label.map(String::from),
        taken_at: crate::now_micros(),
        actor: actor.into(),
        index,
    };
    store.append_snapshot(&snap).await?;
    Ok(snap)
}

/// What a snapshot's pinned searches read from the tables rather than an index: per scope and kind of
/// index, a line for each index that covers less than the snapshot (none at all, or an update from
/// before its last commits). Empty when its indexes cover all of it.
pub async fn unindexed(store: &Store, snap: &Snapshot) -> Result<Vec<String>> {
    let mut out = vec![];
    for (name, pin) in &snap.index {
        let scope = Scope::parse(name)?;
        let wanted: Vec<&String> = snap
            .heads
            .get(name)
            .into_iter()
            .flat_map(|b| b.values().flatten())
            .collect();
        for (kind, c) in [("facts", &pin.facts), ("passages", &pin.passages)] {
            let gap = match c {
                None if wanted.is_empty() => None,
                None => Some("no index".to_string()),
                Some(c) => {
                    let had: Vec<String> = c.heads.values().flatten().cloned().collect();
                    let reach = store.reachable(&scope, &had).await?;
                    let missing = wanted.iter().filter(|h| !reach.contains(**h)).count();
                    (missing > 0).then(|| format!("an index from before {missing} of its heads"))
                }
            };
            if let Some(g) = gap {
                out.push(format!(
                    "{name}: {kind} searches read {g}; the rest from the tables (run `memstore index {name}` before snapshotting)"
                ));
            }
        }
    }
    Ok(out)
}

/// Snapshots read so far, by workspace and id: they never change, so a server reads each once.
type Known = Mutex<HashMap<(String, String), Arc<Snapshot>>>;
static KNOWN: std::sync::LazyLock<Known> = std::sync::LazyLock::new(Default::default);

fn forget_snapshots(ws: &str) {
    KNOWN.lock().unwrap().retain(|(w, _), _| w != ws);
}

/// Snapshot `id` of workspace `ws`; refused when it has none of that id.
pub async fn find_snapshot(store: &Store, ws: &str, id: &str) -> Result<Arc<Snapshot>> {
    let key = (ws.to_string(), id.to_string());
    if let Some(s) = KNOWN.lock().unwrap().get(&key) {
        return Ok(s.clone());
    }
    let snap = store
        .snapshots(ws)
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .map(Arc::new)
        .with_context(|| format!("workspace {ws} has no snapshot {id}"))?;
    KNOWN.lock().unwrap().insert(key, snap.clone());
    Ok(snap)
}

/// What a read sees: `scope` on `branch` (else main, or the snapshot's branch), pinned to snapshot `at` if
/// given. Only a workspace's scopes have snapshots; any scope may have pins (`crate::pins`: an `at` of `pin_…`),
/// each read on its own branch.
pub async fn read_session(
    store: &Store,
    scope: &Scope,
    branch: Option<&str>,
    at: Option<&str>,
) -> Result<Session> {
    let Some(at) = at else {
        return store.pin(scope, branch.unwrap_or(MAIN)).await;
    };
    if at.starts_with(crate::pins::PREFIX) {
        let pin = store.find_pin(scope, at).await?;
        if let Some(b) = branch
            && b != pin.branch
        {
            return Err(crate::pins::Refused(
                crate::pins::Why::Bad,
                format!("pin {at} holds branch {}, not {b}", pin.branch),
            )
            .into());
        }
        return store
            .pin_at(scope, &pin.branch, &pin.id, &pin.heads, pin.at, None)
            .await;
    }
    let ws = match scope.org_name() {
        Some(o) if o != DEFAULT_ORG => o,
        _ => anyhow::bail!("{scope} is not a workspace's: it has no snapshots"),
    };
    let snap = find_snapshot(store, ws, at).await?;
    let branch = branch.unwrap_or(&snap.branch);
    let heads = snap.heads_of(scope, branch)?;
    let index = snap.index.get(&scope.to_string()).cloned();
    store
        .pin_at(scope, branch, &snap.id, &heads, snap.taken_at, index)
        .await
}

/// A workspace's scopes: its own, and its teams' (deleted teams' too, whose memory stays until the
/// workspace goes).
pub async fn scopes(store: &Store, ws: &str) -> Result<Vec<Scope>> {
    let own = Scope::org(ws)?;
    let mut out = vec![own.clone()];
    for ns in store.org_namespaces(ws).await? {
        if let Some(team) = ns.strip_prefix(&format!("{}__team_", own.namespace())) {
            out.push(Scope::team(ws, team)?);
        }
    }
    Ok(out)
}

/// `memstore workspace list`: every workspace with its spec.
pub async fn list(store: &Store) -> Result<Vec<Value>> {
    let dir = Directory::load(store).await?;
    let now = crate::now_micros();
    Ok(dir
        .orgs
        .iter()
        .filter_map(|(name, o)| {
            let s = o.workspace.as_ref()?;
            Some(json!({"workspace": name, "spec": s, "expired": s.expired(now)}))
        })
        .collect())
}

/// `memstore workspace show`: the spec, each scope's branches and heads and its row counts, the index
/// location and size, and the snapshots.
pub async fn show(store: &Store, ws: &str) -> Result<Value> {
    let spec = get(store, ws).await?;
    let mut out_scopes = serde_json::Map::new();
    for scope in scopes(store, ws).await? {
        let ns = scope.namespace();
        out_scopes.insert(
            scope.to_string(),
            json!({
                "namespace": ns,
                "branches": store.branch_heads(&scope).await?,
                "episodes": store.total_rows(&ns, EPISODES).await?,
                "facts": store.total_rows(&ns, FACTS).await?,
                "index": crate::search::index_uri(store, &scope),
            }),
        );
    }
    let snapshots: Vec<Value> = store
        .snapshots(ws)
        .await?
        .iter()
        .map(|s| json!({"id": s.id, "branch": s.branch, "label": s.label, "taken_at": s.taken_at}))
        .collect();
    Ok(json!({
        "workspace": ws,
        "spec": spec,
        "expired": spec.expired(crate::now_micros()),
        "scopes": out_scopes,
        "index_bytes": index_bytes(store, ws).await?,
        "snapshots": snapshots,
    }))
}

/// Bytes the workspace's search indexes take, over all its scopes.
pub async fn index_bytes(store: &Store, ws: &str) -> Result<u64> {
    let mut n = 0;
    for ns in store.org_namespaces(ws).await? {
        n += crate::search::namespace_index_bytes(store, &ns).await?;
    }
    // A scope with no tables yet still has its namespace listed; one whose namespace went (a purge that
    // stopped halfway) may still have an index.
    let own = Scope::org(ws)?.namespace();
    if !store.org_namespaces(ws).await?.contains(&own) {
        n += crate::search::namespace_index_bytes(store, &own).await?;
    }
    Ok(n)
}

/// A write refused because it would pass a cap: the deployment's (`MEMSTORE_MAX_EPISODES`, a trial's)
/// or a workspace's. Its message is for the caller: what the cap is, what is there, and what to do.
#[derive(Debug)]
pub struct CapReached(pub String);

impl std::fmt::Display for CapReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CapReached {}

/// The deployment's episodes as the cap sees them: counted from the catalog (every org's episodes table,
/// seconds on a remote catalog), plus what this process has written since, plus what it has let through
/// and not yet landed. Counted again only when nothing is in flight and the count is a minute old, so a
/// write checks against memory rather than the catalog. (One replica: a trial's engine has one.)
struct CapCount {
    counted: Option<(u64, std::time::Instant)>,
    pending: u64,
}

static CAP_COUNT: std::sync::Mutex<CapCount> = std::sync::Mutex::new(CapCount {
    counted: None,
    pending: 0,
});

/// One check at a time, so writes racing each other can't each pass it and together go past the cap.
static CAP_CHECK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CAP_RECOUNT: std::time::Duration = std::time::Duration::from_secs(60);

/// Room for a commit's episodes under the deployment cap, held until it lands (and then counted) or fails
/// (and then given back). The queued commit owns it: a caller that goes away doesn't give it back early.
pub struct CapReservation {
    episodes: u64,
    landed: bool,
}

impl CapReservation {
    pub fn landed(mut self) {
        self.landed = true;
    }
}

impl Drop for CapReservation {
    fn drop(&mut self) {
        let mut c = CAP_COUNT.lock().unwrap();
        c.pending -= self.episodes;
        if self.landed
            && let Some((n, _)) = &mut c.counted
        {
            *n += self.episodes;
        }
    }
}

/// Refuse a write that would take the deployment past `MEMSTORE_MAX_EPISODES`; else reserve its room.
pub async fn reserve(store: &Store, episodes: usize) -> Result<Option<CapReservation>> {
    let Some(cap) = deployment_cap()? else {
        return Ok(None);
    };
    if episodes == 0 {
        return Ok(None);
    }
    let _check = CAP_CHECK.lock().await;
    let recount = {
        let c = CAP_COUNT.lock().unwrap();
        c.pending == 0 && c.counted.is_none_or(|(_, at)| at.elapsed() >= CAP_RECOUNT)
    };
    if recount {
        let n = store.deployment_episodes().await?;
        CAP_COUNT.lock().unwrap().counted = Some((n, std::time::Instant::now()));
    }
    let mut c = CAP_COUNT.lock().unwrap();
    let have = c.counted.map_or(0, |(n, _)| n) + c.pending;
    if have + episodes as u64 > cap {
        // The count spans every org here: it stays in the log, not in what a caller of one org sees.
        eprintln!(
            "memstore: episode cap {cap} reached: {have} held, a write of {episodes} refused"
        );
        return Err(CapReached(format!(
            "this deployment is capped at {cap} episodes (MEMSTORE_MAX_EPISODES) and has reached it. Reads still work; raise or remove the cap to write more"
        ))
        .into());
    }
    c.pending += episodes as u64;
    Ok(Some(CapReservation {
        episodes: episodes as u64,
        landed: false,
    }))
}

/// The most episodes this deployment holds, across every org and team (`MEMSTORE_MAX_EPISODES`; unset:
/// no cap). A trial deployment is capped at 1,000.
pub fn deployment_cap() -> Result<Option<u64>> {
    match std::env::var("MEMSTORE_MAX_EPISODES") {
        Err(_) => Ok(None),
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!("MEMSTORE_MAX_EPISODES is a whole number above 0, not {v:?}")
            }),
    }
}

impl Store {
    /// The episodes this deployment holds: every memory namespace's (not the system's), from snapshot
    /// summaries.
    pub async fn deployment_episodes(&self) -> Result<u64> {
        let mut n = 0;
        for ns in self.catalog().list_namespaces(None).await? {
            let ns = ns.to_url_string();
            if ns.starts_with("memory_") && ns != "memory_system" {
                n += self.total_rows(&ns, EPISODES).await?;
            }
        }
        Ok(n)
    }
}

/// Refuse a write that would take the deployment past `MEMSTORE_MAX_EPISODES`, or the workspace `scope`
/// belongs to past its episode or fact cap. Scopes of the default org and of orgs that aren't
/// workspaces have no workspace cap.
/// Per capped workspace: one check at a time, and the episodes and facts let through whose commits haven't
/// settled, so writes racing each other can't each pass the check and together go past the cap.
#[derive(Default)]
struct WsPending {
    check: std::sync::Arc<tokio::sync::Mutex<()>>,
    episodes: u64,
    facts: u64,
}

static WS_PENDING: std::sync::LazyLock<std::sync::Mutex<HashMap<String, WsPending>>> =
    std::sync::LazyLock::new(Default::default);

/// A write's room under its workspace's caps, given back when its commit has landed (the catalog counts it
/// from then on) or failed. The queued commit owns it.
pub struct WsReservation {
    ws: String,
    episodes: u64,
    facts: u64,
}

impl Drop for WsReservation {
    fn drop(&mut self) {
        if let Some(p) = WS_PENDING.lock().unwrap().get_mut(&self.ws) {
            p.episodes -= self.episodes;
            p.facts -= self.facts;
        }
    }
}

pub async fn check_write(
    store: &Store,
    scope: &Scope,
    episodes: usize,
    facts: usize,
) -> Result<Option<WsReservation>> {
    let Some(ws) = scope.org_name().filter(|o| *o != DEFAULT_ORG) else {
        return Ok(None);
    };
    let limits = match store.directory().await {
        Ok(f) => match spec_of(&f.dir, ws) {
            Some(s) => s.limits,
            None => return Ok(None),
        },
        // Caps guard resources, not access: a directory that can't be read doesn't stop writes.
        Err(e) => {
            eprintln!("memstore: workspace limits of {ws} unchecked: {e:#}");
            return Ok(None);
        }
    };
    if limits.max_episodes.is_none() && limits.max_facts.is_none() {
        return Ok(None);
    }
    let check = WS_PENDING
        .lock()
        .unwrap()
        .entry(ws.to_string())
        .or_default()
        .check
        .clone();
    let _check = check.lock().await;
    let (mut have_eps, mut have_facts) = {
        let p = WS_PENDING.lock().unwrap();
        let p = &p[ws];
        (p.episodes, p.facts)
    };
    for ns in store.org_namespaces(ws).await? {
        have_eps += store.total_rows(&ns, EPISODES).await?;
        have_facts += store.total_rows(&ns, FACTS).await?;
    }
    for (what, have, adding, cap) in [
        ("episodes", have_eps, episodes as u64, limits.max_episodes),
        ("facts", have_facts, facts as u64, limits.max_facts),
    ] {
        if let Some(cap) = cap
            && have + adding > cap
        {
            return Err(CapReached(format!(
                "workspace {ws} is capped at {cap} {what}: it has {have}, and this write adds {adding}"
            ))
            .into());
        }
    }
    let mut p = WS_PENDING.lock().unwrap();
    let p = p.get_mut(ws).expect("entered above");
    p.episodes += episodes as u64;
    p.facts += facts as u64;
    Ok(Some(WsReservation {
        ws: ws.to_string(),
        episodes: episodes as u64,
        facts: facts as u64,
    }))
}

/// Refuse an index update of a workspace scope whose workspace's indexes are at their byte cap already.
pub async fn check_index(store: &Store, scope: &Scope) -> Result<()> {
    let Some(ws) = scope.org_name().filter(|o| *o != DEFAULT_ORG) else {
        return Ok(());
    };
    let cap = match store.directory().await {
        Ok(f) => spec_of(&f.dir, ws).and_then(|s| s.limits.max_index_bytes),
        Err(_) => None,
    };
    if let Some(cap) = cap {
        let have = index_bytes(store, ws).await?;
        anyhow::ensure!(
            have < cap,
            "workspace {ws} is capped at {cap} index bytes and its indexes take {have}: not indexing {scope}"
        );
    }
    Ok(())
}

/// Parse a time to live: a number and a unit (`30m`, `12h`, `7d`).
pub fn parse_ttl(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = n
        .parse()
        .with_context(|| format!("time to live {s:?}: a number and m, h or d"))?;
    let secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => anyhow::bail!("time to live {s:?}: a number and m, h or d"),
    };
    Ok(std::time::Duration::from_secs(n * secs))
}

impl Store {
    /// The rows a table holds now, from its current snapshot's summary (no data read); 0 without a table.
    pub async fn total_rows(&self, ns: &str, name: &str) -> Result<u64> {
        Ok(self.snapshot_totals(ns, name).await?.0)
    }

    /// A table's rows and the bytes of its data files, from its current snapshot's summary (no data read);
    /// zeros when it has none.
    pub async fn snapshot_totals(&self, ns: &str, name: &str) -> Result<(u64, u64)> {
        let ident = TableIdent::new(NamespaceIdent::new(ns.into()), name.into());
        if !self.has_table(&ident).await? {
            return Ok((0, 0));
        }
        let table = self.catalog().load_table(&ident).await?;
        let total = |key: &str| {
            table
                .metadata()
                .current_snapshot()
                .and_then(|s| {
                    s.summary()
                        .additional_properties
                        .get(key)
                        .and_then(|n| n.parse().ok())
                })
                .unwrap_or(0)
        };
        Ok((total("total-records"), total("total-files-size")))
    }

    /// The snapshots of workspace `ws`, oldest first; none before the first.
    pub async fn snapshots(&self, ws: &str) -> Result<Vec<Snapshot>> {
        let ns = Scope::org(ws)?.namespace();
        let ident = TableIdent::new(NamespaceIdent::new(ns.clone()), SNAPSHOTS.into());
        if !self.has_table(&ident).await? {
            return Ok(vec![]);
        }
        let table = self.catalog().load_table(&ident).await?;
        let mut out = vec![];
        for b in crate::scan_rows(&table, None, None).await? {
            for body in crate::strings(&b, "body") {
                out.push(serde_json::from_str::<Snapshot>(&body)?);
            }
        }
        out.sort_by_key(|s| s.taken_at);
        Ok(out)
    }

    async fn append_snapshot(&self, snap: &Snapshot) -> Result<()> {
        let ns = Scope::org(&snap.workspace)?.namespace();
        self.ensure_table(
            &ns,
            SNAPSHOTS,
            crate::schema(&[
                ("snapshot_id", true, PrimitiveType::String),
                ("body", true, PrimitiveType::String),
            ])?,
        )
        .await?;
        let table = self.table(&ns, SNAPSHOTS).await?;
        let s = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        crate::append(
            &*self.catalog(),
            &table,
            vec![
                ("snapshot_id", s(&snap.id)),
                ("body", s(&serde_json::to_string(snap)?)),
            ],
            crate::OnConflict::Recommit,
        )
        .await
    }
}
