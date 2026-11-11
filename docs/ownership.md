# Entity ownership: first engine increment

This increment adds explicit owner review of draft branches. It is opt-in per entity and organization/team scope. Studio onboarding, automatic proposal creation by background writers, and goal-to-schema planning are subsequent work.

Ownership and decisions live in the engine's `governance` table and use its existing ledger. Both grouped and individual main-branch publications check the current ownership policy inside the ledger compare-and-set loop. Actor strings do not count as approval. Unprotected scopes retain their existing behavior.

## API

These are `POST /admin` operations for signed-in people. Pass `org` and optionally `team`. The ordinary scope resolver applies: an org administrator is not automatically a member of a team, and service keys cannot act as reviewers. Responses have `org`, `scope`, and `result`.

| Operation | Additional fields | Behavior |
| --- | --- | --- |
| `set_entity_owner` | `entity`, `owner`, optional `revision` | An org admin assigns the first owner. The owner must be an explicit member of the scope. Only the current owner can transfer ownership, supplying the current revision; a transfer to the current owner is refused. |
| `remove_entity_owner` | `entity`, `revision` | The owner or an org admin removes the owner, naming the revision they saw; the entity's changes publish without review again. |
| `entity_owners` | None | Returns each entity's owner, revision and `since` (µs), to any member of the scope: who owns what is not filtered by what the reader may read. |
| `list_proposals` | optional `entity`, optional `limit` (≤ 500) | The proposals whose evidence the caller may read, newest activity first: id, branch, digest, status, required owners, approvals, who rejected, who proposed, `created_at`, `updated_at`, the entities changed, how many rows, and `stale_reason`. |
| `propose_branch` | `branch` | Records an immutable review of protected changes on a conflict-free draft branch. Repeating the same proposal returns the existing record, including a rejection. |
| `get_proposal` | `proposal` | Returns the proposal, approvals and status, provided the caller may read its evidence. Each fact row carries `before`: what main says now as this reader may read it, or null (computed per reader, never stored). The review carries `decisions` (user, approve, at, note), `stale_reason` (`draft_changed`, `base_changed`, `ownership_changed`), `published_at`, `updated_at` and `evidence_details` (each episode's source and time). |
| `review_proposal` | `proposal`, `digest`, `decision`: `approve` or `reject`, optional `note` (≤ 2000 bytes) | Records the authenticated owner's decision (a rejection's note is kept with it). The last required approval publishes the reviewed branch through the ordinary merge gate. An org admin who may read the proposal's evidence may decide it too, for all of its owners: their approval publishes it, their rejection rejects it, and the decision carries `admin: true`. |
| `entity_graph` | None | The entities of main as the caller may read them: each with type, owner, how many facts hold now and the open proposals that change it; an edge wherever a fact names another entity, and a `pending` edge for a link an open proposal would add. |

### Facts in conflict

Two open facts of one thing and attribute that holds one value at a time, with different values from different sources, are in conflict ([conflicts](../crates/memstore-core/src/conflicts.rs)): both stay, and where the org or team turns `conflicts` on, a read that serves one says what the other says. Their owners settle them.

| Operation | Additional fields | Behavior |
| --- | --- | --- |
| `add_fact` | `entity`, `predicate`, `value`, optional `note`, `source` (a short label), `valid_from` | An org admin states a fact, from a source of its own (two under one `source` label are one source revising itself). No model reads it. On an owned entity the admin's approval publishes it. Returns `fact_id`, `episode_id` and the `review` it went through, if any. |
| `check_conflicts` | None | An org admin runs the conflict check over the scope now instead of waiting for the next write: every pair of open facts the admin may read, of one entity and a predicate that holds one value (not a schema `list`, `many` or `ref`), with different values from different sources, not yet recorded, is recorded in conflict. Returns `checked`, `recorded`, the pairs, and `marked_in_reads` (whether reads mark them). |
| `list_conflicts` | optional `entity`, optional `mine` | The recorded conflicts between open facts the caller may read, each once, older side first: entity, predicate, owner, `can_resolve` (the caller owns the entity or administers the org), and each side's value, `since`, quote, episode and source. |
| `resolve_conflict` | `facts` (the two ids), and either `keep` (one of them) or `value` | The entity's owner or an org admin settles the attribute: keep one side, or state the value (a new fact). Every other open value of the attribute the caller may read ends, cited by the engine's record of the resolution (fact ids only, so it says nothing to anyone who can't read the facts, and forgetting the resolver doesn't reopen what they settled); ended facts stay in history. A kept value older than one that ends is stated again from now so reads serve it. On an owned entity it goes through review, approved by the one resolving it; where other owners must approve too it waits for them (`status`). |

A review UI can be built on these tools alone.

For example, an org admin assigns `policy:refund-window` to a support owner. A writer stages a new fact on a draft branch and submits that branch. Normal main reads continue to see the published fact. The owner reviews the proposal digest and approves it; all required owners must agree before publication. A changed draft, base memory, or ownership policy requires a new proposal. Approval retries after successful publication are idempotent.

## Scope and current limits

- Targets are exact entity IDs. Owning a schema type, all instances of a type, and groups as reviewers are not included yet.
- Facts naming a protected entity, incoming references, explicit fact closures, quotes and semantic retractions are checked. Direct protected writes fail with an owner-approval error; background writers do not yet turn these refusals into proposals automatically.
- Facts sharing evidence with protected facts conservatively require the same owners, including copies under another entity ID. Direct merges of protected subject IDs are refused until ownership migration is supported.
- A shared versioned document requires the owners of what it touches: the entities it is about, an entity or fact its key names (a brief keyed by its subject, a dream record keyed by the facts it decided on), and the facts citing it or another version of it (a policy page whose facts an owner holds). An aggregate requires its subject's owner and the owners of the facts it cites. Retiring a version requires the owners of the facts citing it or ended by it, of its document, and of the aggregates that read it. A document touching no owned entity (a team's advice, a compiled procedure, a dream record of unowned facts) publishes without review: what its text says reaches an owned entity only through the facts extraction reads from it, which wait for their owners as any fact does. A new document's subject is known only from its `about`, its key and what cites it; its text is not read.
- Schema changes and an extractor's withdrawal (`retract-facts --extractor`), which carry no exact list of what they change, still require every owner in their scope.
- The publication check reads the scope's facts column by column and decodes only the facts it weighs: those a commit writes, those naming an owned entity, and the versions and citing facts its rows reach. It never decodes a scope's whole history.
- Raw source evidence remains separate from approval of structured knowledge. This increment does not claim that arbitrary source text has been owner-approved or add pending-review indicators to every agent response.
- Source deletion and ACL-only changes remain immediate. Existing document metadata is compared before treating an update as ACL-only; replacing or removing its document identity still requires review.
- Initial owner assignment protects future changes. It does not attest the existing baseline. An org admin may remove an owner (`remove_entity_owner`), which is also how an unavailable owner is recovered from: the admin removes them and assigns another.
- Proposal conflicts must be resolved on the draft before proposing. Review records are durable. They do not replace source ACLs or grant the owner access to evidence they cannot otherwise read.
- Review records are memory too, and go the way the memory they are built on goes. A proposal's rows quote the proposed values, so a forget or a purge that removes an episode removes every proposal citing it as evidence, with the decisions on it (`tests/it` `offboarding` and `retention`). Forgetting a person removes the decisions they made, the proposals they made with every decision on those, and the ownership they hold: an entity they own loses its whole ownership history, so no earlier owner comes back, and it is unowned until an admin assigns another. The governance table is compacted and retained with the scope's other tables.
- A stored governance row this engine can't read, or a decision whose proposal is gone, is left out and grants nothing; it never blocks the scope's writes.

## Verification

The requested integration flows are in `tests/it/governance.rs`, registered in the existing integration-test binary. They use authenticated requests to a real server backed by a local persisted catalog. Run with `--features dylib` as required by `CLAUDE.md`. No model is needed for these explicit-write and approval cases; disable extraction during these tests with `AMPUP_MEMORY_EXTRACT=off`.

The first checkpoint is these flows plus the existing authorization regressions. The integration cases cover server teardown and restart against the persisted catalog, including a proposal with one of two approvals. Automatic ingestion/extraction-to-proposal, deterministic publication-race coverage, complete writer/read-path coverage, and the Studio and planner checkpoints remain required before declaring the full implementation plan complete.
