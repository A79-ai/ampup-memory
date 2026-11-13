//! Advice's admin ops that only the server doing the background writing runs (`hold_advice`, `ratify_advice` and
//! `learn_advice`: each writes the advice map, which a learn rewrites from what it read, so they run where learning does, under its
//! claim), asked of a server that doesn't. That is every serving pod of a deployment that splits serving from
//! writing (`AMPUP_MEMORY_ROLE=serve`, a cell's pods among them, whose worker holds the cell's lease), and any server while
//! the lease is elsewhere. An admin reaches only the serving pods, so the request is queued for the writer instead of
//! refused: a document of the scope (`advice-requests/<id>`) that no reader reads, committed as `advice:queued:<asker>`.
//!
//! The writer looks every [`every`] (`AMPUP_MEMORY_ADVICE_REQUESTS_SECS`, default 30) for commits of that actor in the
//! ledger, which every server reads anyway, so it reads only the scopes a request was queued in. It applies each
//! request there, oldest first, and marks it applied by giving it to no one (an applied request is gone from the
//! scope's documents). One that fails is said in the log and marked too, so a request that can't be applied isn't tried
//! again every round; one the writer stopped writing during is left for the next holder.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{AclTarget, Episode, FORGOTTEN, MAIN, Scope, Store};

/// Where queued requests are kept, a document each: outside `advice/`, which holds the notes.
pub const PREFIX: &str = "advice-requests/";
/// Who may read a queued request: no one (no sign-in names an address under `.invalid`). The writer reads it from
/// storage, and a document one person at most reads publishes nothing an entity's owners would review.
const ACL: &str = "user:advice-requests@memstore.invalid";
/// The actor queuing commits are made as, the asker's id after it: what the writer finds in the ledger.
pub const QUEUED: &str = "advice:queued:";
/// The actor marking requests applied: not [`QUEUED`], so marking them isn't found as a new request.
const APPLIED: &str = "advice:applied";
/// Each request's source, its id after it: what marks it applied ([`AclTarget::Source`]).
const SOURCE: &str = "advice:request:";

/// One queued op, as the admin asked it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// `hold` (or release, `hold: false`), `ratify` (or withdraw, `ratify: false`) or `learn`.
    pub op: String,
    /// Who asked.
    pub by: String,
    /// When (µs).
    pub at: i64,
    /// The audience the notes were learned for.
    pub acl: Vec<String>,
    /// `hold`: the notes, and whether to hold them or serve them again.
    #[serde(default)]
    pub keys: BTreeSet<String>,
    #[serde(default)]
    pub hold: bool,
    /// `ratify`: the notes, and whether to ratify them or withdraw it.
    #[serde(default)]
    pub ratify: bool,
    /// `learn`: as the admin op takes them.
    #[serde(default)]
    pub min_wrong: usize,
    #[serde(default)]
    pub resort: bool,
    #[serde(default)]
    pub rewrite: BTreeSet<String>,
    #[serde(default)]
    pub force: bool,
}

/// How often the writer looks for queued requests: `AMPUP_MEMORY_ADVICE_REQUESTS_SECS`, default 30.
pub fn every() -> Duration {
    let secs = std::env::var("AMPUP_MEMORY_ADVICE_REQUESTS_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30);
    Duration::from_secs(secs.max(1))
}

/// Queues `r` in `scope` for the writer. Its id.
pub async fn queue(store: &Store, scope: &Scope, r: &Request) -> Result<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let doc = Episode::sourced(&format!("{SOURCE}{id}"), &serde_json::to_vec(r)?, &[ACL])
        .version_of(&format!("{PREFIX}{id}"));
    store
        .commit(scope, &format!("{QUEUED}{}", r.by), &[doc], &[])
        .await?;
    Ok(id)
}

/// The scopes' namespaces a request was queued in since `since` (µs), and the newest such queuing's time.
pub async fn queued_since(store: &Store, since: i64) -> Result<(BTreeSet<String>, i64)> {
    store.committed_by(QUEUED, since).await
}

/// The requests queued in `scope` and not applied yet, oldest first, each with its id.
pub async fn pending(store: &Store, scope: &Scope) -> Result<Vec<(String, Request)>> {
    // Read fresh: the request was committed by another server, which this one's cached tables may not show yet.
    store.invalidate(crate::SYSTEM_NS, "ledger");
    store.invalidate(&scope.namespace(), crate::EPISODES);
    let session = store.pin(scope, MAIN).await?;
    let mut out = vec![];
    for key in session.engine_documents(PREFIX) {
        let Some(body) = session.engine_document(store, &key).await? else {
            continue;
        };
        match serde_json::from_slice::<Request>(&body) {
            Ok(r) => out.push((key[PREFIX.len()..].to_string(), r)),
            Err(e) => eprintln!("advice request {key} in {scope} isn't one: {e}"),
        }
    }
    out.sort_by_key(|(_, r)| r.at);
    Ok(out)
}

/// Marks requests `ids` of `scope` applied: given to no one, so they leave the scope's documents.
pub async fn applied(store: &Store, scope: &Scope, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let targets: Vec<AclTarget> = ids
        .iter()
        .map(|id| AclTarget::Source(format!("{SOURCE}{id}")))
        .collect();
    store
        .change_acl_all(scope, APPLIED, &targets, &[FORGOTTEN.to_string()])
        .await?;
    Ok(())
}
