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
    // A read is decided by the directory requests are signed in by (`Directory::behind`); a change, by the
    // directory at most the max staleness old.
    let dir = match field("op") {
        "entity_owners" | "list_proposals" | "entity_graph" | "get_proposal" | "list_conflicts" => {
            Directory::behind(store).await?
        }
        _ => Directory::load(store).await?,
    };
    crate::phases::mark("governance directory");
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
            let entity = req["entity"].as_str().filter(|e| !e.is_empty());
            // `built: true` (Studio's Home): listed only once every proposal's branch has its state built, so the
            // list waits for no build; until then `proposals` is null, and the branches are built in the background.
            // Governance read as this server holds it, as the list itself is: Home asks again while they build.
            if req["built"] == true && !store.held(store.proposal_branches_built(&scope)).await? {
                crate::phases::mark("proposals building");
                return Ok(json!({"org": org, "scope": scope.to_string(),
                    "result": {"proposals": null, "building": true}}));
            }
            crate::phases::mark("proposals built");
            let all = proposals_kept(store, &scope, &reader).await?;
            crate::phases::mark("proposals read");
            let listed: Vec<&crate::governance::ProposalSummary> = all
                .iter()
                .filter(|p| entity.is_none_or(|e| p.entities.iter().any(|x| x == e)))
                .take(limit)
                .collect();
            serde_json::to_value(json!({"proposals": listed}))
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
        // And on the next read of the review lists, for whoever changed it first of all.
        crate::kept_answers::changed(&scope.namespace());
    }
    Ok(json!({"org": org, "scope": scope.to_string(), "result": result}))
}

/// Each reader's proposals, by memory state.
static PROPOSALS: std::sync::LazyLock<
    crate::kept_answers::Kept<Vec<crate::governance::ProposalSummary>>,
> = std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

/// Every proposal `reader` may see, newest activity first ([`Store::list_proposals`]), kept by memory state: each
/// list reads the governance tables and builds the visibility of every proposal's branch. Kept by main's state and
/// every other branch's heads, and only main may have moved on for a kept list to answer while the next is made: a
/// branch's change (a draft changed, evidence forgotten there) is listed on the read that follows it.
async fn proposals_kept(
    store: &Store,
    scope: &Scope,
    reader: &Principal,
) -> Result<std::sync::Arc<Vec<crate::governance::ProposalSummary>>> {
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    let vis = store.lagging_visibility(scope, crate::MAIN).await?;
    let mut at = crate::kept_answers::At::of(mark, &vis);
    at.pinned = store
        .branch_heads_held(scope)
        .await?
        .into_iter()
        .filter(|(b, _)| b != crate::MAIN)
        .map(|(b, heads)| format!("{b} {}", heads.join(",")))
        .collect();
    PROPOSALS
        .get(
            &scope.namespace(),
            &crate::kept_answers::reader_key(reader),
            at,
            true,
            || {
                let (store, scope, reader) = (store.clone(), scope.clone(), reader.clone());
                async move {
                    store
                        .held(store.list_proposals(&scope, &reader, None, usize::MAX))
                        .await
                }
            },
        )
        .await
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

/// Each reader's last graph, by scope, reader (user and every group they read as), `limit` and `entity`: never
/// shared between readers.
static GRAPHS: std::sync::LazyLock<crate::kept_answers::Kept<Value>> =
    std::sync::LazyLock::new(|| crate::kept_answers::Kept::new(500));

/// [`graph`], kept by memory state ([`crate::kept_answers`]): building it reads and decodes every fact the reader
/// may read, seconds in a large org. At the same state it is the same graph. Once main moved on, the kept one
/// answers at once while the new one is built in the background for the next read, but never once memory lost
/// something since it was built; only a reader's first graph (or one after a restart or a loss) is built on the
/// request.
async fn graph_kept(
    store: &Store,
    scope: &Scope,
    reader: &Principal,
    max: usize,
    keep: Option<&str>,
) -> Result<Value> {
    let mark = crate::kept_answers::Mark::read(store, scope).await?;
    let vis = store.lagging_visibility(scope, crate::MAIN).await?;
    let key = format!(
        "{}\n{max}\n{}",
        crate::kept_answers::reader_key(reader),
        keep.unwrap_or_default()
    );
    let graph = GRAPHS
        .get(
            &scope.namespace(),
            &key,
            crate::kept_answers::At::of(mark, &vis),
            true,
            || {
                let (store, scope, reader) = (store.clone(), scope.clone(), reader.clone());
                let keep = keep.map(String::from);
                async move { graph(&store, &scope, &reader, max, keep.as_deref()).await }
            },
        )
        .await?;
    Ok((*graph).clone())
}
