//! A scope's schema changed through its owners' review. A schema change needs every owner in its scope
//! (docs/ownership.md), so where an entity has an owner main refuses one: it is staged on a branch, made from main
//! where it isn't there, and proposed. An owner's approval (an org admin's decides for all of them) publishes it.

use crate::governance::Review;
use crate::query::schema::Schema;
use crate::{MAIN, Principal, Scope, Store};
use anyhow::{Context, Result};

/// The branch a proposed schema is staged on when none is named: `schema/<name>-v<version>`.
pub fn branch_for(s: &Schema) -> String {
    let name: String = s
        .name
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() || "-_.".contains(c) {
            true => c,
            false => '-',
        })
        .take(40)
        .collect();
    format!("schema/{name}-v{}", s.version)
}

/// Whether an entity of `scope` has an owner: then its schema changes only through their review.
pub async fn owned(store: &Store, scope: &Scope) -> Result<bool> {
    Ok(!store.entity_owners(scope).await?.is_empty())
}

/// Commit `schema` on `branch`, made from main first where it doesn't exist (a name the engine keeps for its own
/// is refused). A branch takes any change, a breaking one too: main's rules apply when it is merged. Returns the
/// commit and whether the branch was made.
pub async fn stage(
    store: &Store,
    scope: &Scope,
    branch: &str,
    actor: &str,
    schema: &Schema,
) -> Result<(String, bool)> {
    anyhow::ensure!(branch != MAIN, "stage a schema on a branch, not main");
    let made = !store.branch_heads(scope).await?.contains_key(branch);
    if made {
        store.create_branch(scope, branch, actor).await?;
    }
    let id = store
        .set_schema(scope, branch, actor, schema, false)
        .await?;
    Ok((id, made))
}

/// Stage `schema` on `branch` and propose it to the owners as `who`. It is first checked as main would take it
/// (never backwards; a breaking change only with `allow_breaking` and a higher version), so an approval publishes
/// it rather than fail at the merge. Returns whether the branch was made, and the proposal.
pub async fn propose(
    store: &Store,
    scope: &Scope,
    branch: &str,
    who: &Principal,
    schema: &Schema,
    allow_breaking: bool,
) -> Result<(bool, Review)> {
    anyhow::ensure!(
        owned(store, scope).await?,
        "no entity of {scope} has an owner, so there is no one to review its schema: set it on main"
    );
    store
        .check_schema(scope, MAIN, schema, allow_breaking)
        .await
        .context("a proposal is checked as main would take it once approved")?;
    let (_, made) = stage(store, scope, branch, who.user(), schema).await?;
    let review = store.propose_branch(scope, branch, who).await?;
    Ok((made, review))
}
