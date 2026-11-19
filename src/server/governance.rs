//! Owner review operations use the signed-in person and the ordinary scope resolver. An org admin may decide any
//! proposal in their org for its owners (the decision names them as an admin), but still reads no team they do not
//! belong to, and no proposal whose evidence they may not read.

use super::*;

pub(super) async fn handle(
    store: &Store,
    who: &Principal,
    via: &SignIn,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::{Directory, Invalid};
    let field = |k: &str| req[k].as_str().unwrap_or_default();
    let org = field("org");
    crate::check_name("org", org).map_err(|e| Invalid(e.to_string()))?;
    let dir = Directory::load(store).await?;
    let scope = dir.resolve(
        who.user(),
        via,
        Some(org),
        req["team"].as_str().filter(|t| !t.is_empty()),
    )?;
    let reader = as_member(who, org);
    let admin = dir.is_admin(who.user(), via, org);
    let result = match field("op") {
        "add_fact" | "check_conflicts" | "list_conflicts" | "resolve_conflict" => {
            Ok(super::conflict_ops::handle(store, &reader, admin, &scope, req).await?)
        }
        "set_entity_owner" => serde_json::to_value(
            store
                .set_entity_owner(
                    &scope,
                    who,
                    via,
                    field("entity"),
                    field("owner"),
                    req["revision"].as_str(),
                )
                .await?,
        ),
        "remove_entity_owner" => {
            store
                .remove_entity_owner(&scope, who, via, field("entity"), field("revision"))
                .await?;
            Ok(json!({}))
        }
        "entity_owners" => serde_json::to_value(store.entity_owners_held(&scope).await?),
        "list_proposals" => {
            let limit = req["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
            let listed = store.list_proposals(
                &scope,
                &reader,
                req["entity"].as_str().filter(|e| !e.is_empty()),
                limit,
            );
            serde_json::to_value(json!({"proposals": store.held(listed).await?}))
        }
        "entity_graph" => {
            // At most `limit` entities (default 300, the most a page draws), keeping `entity` among them.
            let max = req["limit"].as_u64().unwrap_or(300).clamp(1, 2000) as usize;
            let keep = req["entity"].as_str().filter(|e| !e.is_empty());
            Ok(graph_kept(store, &scope, &reader, max, keep).await?)
        }
        "propose_branch" => serde_json::to_value(
            store
                .propose_branch(&scope, field("branch"), &reader)
                .await?,
        ),
        "get_proposal" => serde_json::to_value(
            store
                .held(store.proposal(&scope, field("proposal"), &reader))
                .await?,
        ),
        "review_proposal" => {
            let approve = match field("decision") {
                "approve" => true,
                "reject" => false,
                _ => return Err(Invalid("decision is approve or reject".into()).into()),
            };
            // A rejection ends nothing, so it takes no expiry.
            let expires_at = match approve {
                true => Some(super::conflict_ops::expiry(req)?),
                false => None,
            };
            serde_json::to_value(
                store
                    .review_proposal(
                        &scope,
                        field("proposal"),
                        field("digest"),
                        &reader,
                        approve,
                        req["note"].as_str(),
                        admin,
                        expires_at,
                    )
                    .await?,
            )
        }
        _ => unreachable!(),
    }?;
    // What an action changed (an owner, a proposal, a decision, a fact) shows on the next graph read, built then.
    if !matches!(
        field("op"),
        "entity_owners" | "list_proposals" | "entity_graph" | "get_proposal" | "list_conflicts"
    ) {
        let ns = format!("{}\n", scope.namespace());
        GRAPHS.lock().unwrap().retain(|k, _| !k.starts_with(&ns));
    }
    Ok(json!({"org": org, "scope": scope.to_string(), "result": result}))
}

/// The entity graph of `scope` as `reader` sees it ([`Store::entity_graph`]), each node named where its id says
/// nothing (a meeting by its call's title).
async fn graph(
    store: &Store,
    scope: &Scope,
    reader: &Principal,
    max: usize,
    keep: Option<&str>,
) -> Result<Value> {
    let graph = store
        .held(store.entity_graph(scope, reader, max, keep))
        .await?;
    let graph = serde_json::to_value(graph)?;
    // The episodes, every one the reader may read, only when a node needs naming, on main's last state.
    let mut ids = vec![];
    crate::names::ids_in(&graph, &mut ids);
    if ids.is_empty() {
        return Ok(graph);
    }
    let session = store.pin_lagging(scope, crate::MAIN).await?;
    let eps = crate::browse::episodes(store, &session, reader).await?;
    crate::names::attach(store, &session, reader, graph, &eps).await
}

/// Readers whose graphs are kept, at most.
const MAX_GRAPHS: usize = 500;

struct KeptGraph {
    /// Main's heads it was built at.
    heads: Vec<String>,
    at: std::time::Instant,
    graph: Value,
    refreshing: bool,
}

/// Each reader's last graph, by scope, reader (user and tokens), `limit` and `entity`: never shared between readers.
static GRAPHS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, KeptGraph>>> =
    std::sync::LazyLock::new(Default::default);

/// [`graph`], kept: building it reads and decodes every fact the reader may read, seconds in a large org. At the
/// same heads of main it is the same graph. Once main moved on, the kept one answers at once while the new one is
/// built in the background for the next read; only a reader's first graph (or one after a restart) is built on
/// the request.
async fn graph_kept(
    store: &Store,
    scope: &Scope,
    reader: &Principal,
    max: usize,
    keep: Option<&str>,
) -> Result<Value> {
    let heads = store
        .lagging_visibility(scope, crate::MAIN)
        .await?
        .heads()
        .to_vec();
    let key = format!(
        "{}\n{}\n{}\n{max}\n{}",
        scope.namespace(),
        reader.user(),
        reader.tokens().join(","),
        keep.unwrap_or_default()
    );
    {
        let mut kept = GRAPHS.lock().unwrap();
        if let Some(g) = kept.get_mut(&key) {
            if g.heads != heads && !g.refreshing {
                g.refreshing = true;
                let (store, scope, reader, key, heads) = (
                    store.clone(),
                    scope.clone(),
                    reader.clone(),
                    key.clone(),
                    heads.clone(),
                );
                let keep = keep.map(String::from);
                tokio::spawn(async move {
                    let built = graph(&store, &scope, &reader, max, keep.as_deref()).await;
                    let mut kept = GRAPHS.lock().unwrap();
                    match built {
                        Ok(graph) => keep_graph(&mut kept, key, heads, graph),
                        Err(e) => {
                            eprintln!("entity graph of {scope}: {e:#}");
                            if let Some(g) = kept.get_mut(&key) {
                                g.refreshing = false;
                            }
                        }
                    }
                });
            }
            return Ok(g.graph.clone());
        }
    }
    let built = graph(store, scope, reader, max, keep).await?;
    keep_graph(&mut GRAPHS.lock().unwrap(), key, heads, built.clone());
    Ok(built)
}

fn keep_graph(
    kept: &mut HashMap<String, KeptGraph>,
    key: String,
    heads: Vec<String>,
    graph: Value,
) {
    if kept.len() >= MAX_GRAPHS && !kept.contains_key(&key) {
        // The least recently built goes.
        if let Some(oldest) = kept
            .iter()
            .min_by_key(|(_, g)| g.at)
            .map(|(k, _)| k.clone())
        {
            kept.remove(&oldest);
        }
    }
    kept.insert(
        key,
        KeptGraph {
            heads,
            at: std::time::Instant::now(),
            graph,
            refreshing: false,
        },
    );
}
