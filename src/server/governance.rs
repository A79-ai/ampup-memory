//! Owner review operations use the signed-in person and the ordinary scope resolver. In particular,
//! an org admin cannot review as an owner, or read a team they do not belong to.

use super::*;

pub(super) async fn handle(
    store: &Store,
    who: &Principal,
    via: &SignIn,
    req: &Value,
) -> Result<Value> {
    use crate::people::directory::{Directory, Refused};
    let field = |k: &str| req[k].as_str().unwrap_or_default();
    let org = field("org");
    crate::check_name("org", org).map_err(|e| Refused(e.to_string()))?;
    let dir = Directory::load(store).await?;
    let scope = dir.resolve(
        who.user(),
        via,
        Some(org),
        req["team"].as_str().filter(|t| !t.is_empty()),
    )?;
    let reader = as_member(who, org);
    let result = match field("op") {
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
        "entity_owners" => serde_json::to_value(store.entity_owners(&scope).await?),
        "list_proposals" => {
            let limit = req["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
            serde_json::to_value(json!({"proposals": store
                .list_proposals(
                    &scope,
                    &reader,
                    req["entity"].as_str().filter(|e| !e.is_empty()),
                    limit,
                )
                .await?}))
        }
        "entity_graph" => serde_json::to_value(store.entity_graph(&scope, &reader).await?),
        "propose_branch" => serde_json::to_value(
            store
                .propose_branch(&scope, field("branch"), &reader)
                .await?,
        ),
        "get_proposal" => {
            serde_json::to_value(store.proposal(&scope, field("proposal"), &reader).await?)
        }
        "review_proposal" => {
            let approve = match field("decision") {
                "approve" => true,
                "reject" => false,
                _ => return Err(Refused("decision is approve or reject".into()).into()),
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
                    )
                    .await?,
            )
        }
        _ => unreachable!(),
    }?;
    Ok(json!({"org": org, "scope": scope.to_string(), "result": result}))
}
