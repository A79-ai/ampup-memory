//! Owner-reviewed publication. Policies and decisions share the memory ledger, so a concurrent
//! policy change invalidates a publication's compare-and-set. A proposal names immutable commits;
//! an actor string, a branch name or an old approval never grants permission to publish new work.

use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const TABLE: &str = "governance";

/// The prefix of the branches main's extraction holds owned changes on ([`Store::stage_extracted`]).
pub const HELD_PREFIX: &str = "held-";

/// Who proposes what extraction held back for its owners.
pub const EXTRACT_PROPOSER: &str = "memstore:extract";

/// Who decides a proposal in an org that has its reviews decided by a decisions model (its `auto_review`
/// setting, [`Store::auto_review`]).
pub const AUTO_REVIEWER: &str = "memstore:auto-review";

/// A proposal waiting for a decision in an org whose reviews a model decides ([`Store::awaiting_auto_review`]).
#[derive(Clone, Debug)]
pub struct AutoCase {
    pub id: String,
    pub digest: String,
    pub branch: String,
    pub created_by: String,
    /// What publishing it changes, as its owners would read it.
    pub changes: Vec<serde_json::Value>,
    /// The episodes it cites, those still readable on main (a forgotten one is left out).
    pub evidence: Vec<String>,
    /// When the latest of them happened (µs): when what it changes was said.
    pub said_at: i64,
}

/// Main refused a commit: it changes what an owner holds, and no approved proposal publishes exactly it.
#[derive(Debug)]
pub struct OwnerApprovalRequired;

impl std::fmt::Display for OwnerApprovalRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "owner approval required: stage protected changes on a branch and submit a proposal",
        )
    }
}

impl std::error::Error for OwnerApprovalRequired {}

impl OwnerApprovalRequired {
    /// Whether `e`, or what it wraps, is this refusal.
    pub fn caused(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.is::<Self>())
    }

    /// Whether `e` is this refusal, as itself or as the text the group commit hands a caller.
    pub fn refused(e: &anyhow::Error) -> bool {
        Self::caused(e) || Self::is_message(&format!("{e:#}"))
    }

    /// Whether an error's text (`{e:#}`, its context chain included) ends in this refusal: the group commit hands
    /// each caller its outcome as text.
    pub(crate) fn is_message(e: &str) -> bool {
        let m = Self.to_string();
        e == m || e.ends_with(&format!(": {m}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Owner {
    pub entity: String,
    pub user: String,
    pub revision: String,
    /// When this owner was recorded, µs since the epoch.
    #[serde(default)]
    pub since: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub branch: String,
    pub digest: String,
    pub base: Vec<String>,
    pub heads: Vec<String>,
    pub policy_revision: String,
    pub required_owners: BTreeSet<String>,
    pub changes: Vec<serde_json::Value>,
    pub evidence: BTreeSet<String>,
    pub created_by: String,
    /// When the proposal was recorded (the ledger's time for its row), µs since the epoch.
    #[serde(default)]
    pub created_at: i64,
    /// The `held-` branch this one replaces: what it held, staged again on main as main is now
    /// ([`Store::renew_held`]). That branch is retired: never proposed again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

/// One owner's recorded decision on a proposal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub user: String,
    pub approve: bool,
    /// µs since the epoch.
    pub at: i64,
    /// Why, for a rejection: the proposer reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Decided by an org admin who isn't one of its required owners: it decides for all of them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin: bool,
}

/// An episode a proposal rests on, as the reader may know it. A proposal is shown only to someone
/// who may read all of its evidence, so `readable` is always true here; it is kept for the API's shape.
#[derive(Clone, Debug, Serialize)]
pub struct EvidenceRef {
    pub id: String,
    pub source: String,
    pub occurred_at: i64,
    pub readable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Review {
    pub proposal: Proposal,
    pub approvals: BTreeSet<String>,
    pub rejected: bool,
    pub status: String,
    pub decisions: Vec<Decision>,
    /// Why a proposal is stale: `draft_changed`, `base_changed` or `ownership_changed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<&'static str>,
    /// When it reached main (the merge's ledger time), once published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
    /// The latest of: proposed, a decision, published.
    pub updated_at: i64,
    pub evidence_details: Vec<EvidenceRef>,
}

/// A proposal as the inbox lists it.
#[derive(Clone, Debug, Serialize)]
pub struct ProposalSummary {
    pub id: String,
    pub branch: String,
    pub digest: String,
    pub status: String,
    pub required_owners: BTreeSet<String>,
    pub approvals: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_by: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// The entities its changes name.
    pub entities: Vec<String>,
    pub changes: usize,
    /// Its first fact change (entity, predicate, value), so two proposals on one entity read apart in a list.
    /// The reader may read it: a proposal is listed only to someone who may read all of its evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<&'static str>,
}

/// The entities of a scope and how they connect, as one reader sees them, with open proposals over it.
#[derive(Clone, Debug, Serialize)]
pub struct GraphNode {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub owner: Option<String>,
    /// Facts true now that the reader may read.
    pub facts: usize,
    /// Open proposals (pending, approved or stale) that change it.
    pub pending: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub predicate: String,
    /// The proposal this link exists in only, when it hasn't reached main.
    pub pending: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Event {
    Owner {
        entity: String,
        user: String,
    },
    OwnerRemoved {
        entity: String,
    },
    Proposal {
        proposal: Proposal,
    },
    Decision {
        proposal: String,
        digest: String,
        user: String,
        approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// `user` decided as an org admin, not as a required owner ([`Store::review_proposal`]): the server checked
        /// they administer the org before recording it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        admin: bool,
    },
    /// A decisions model's decision, in an org that has them decide ([`Store::auto_review`]): it stands in for the
    /// owners until one of them decides. `confidence` is how sure the model was of its decision.
    AutoDecision {
        proposal: String,
        digest: String,
        approve: bool,
        model: String,
        confidence: f64,
    },
}

#[derive(Default)]
struct State {
    owners: BTreeMap<String, Owner>,
    proposals: BTreeMap<String, Proposal>,
    approvals: BTreeMap<String, BTreeSet<String>>,
    /// The proposals an org admin approved: approved for every required owner.
    admin_approved: BTreeSet<String>,
    rejected: BTreeSet<String>,
    /// A decisions model's decision on each proposal ([`Event::AutoDecision`]): whether it approved.
    auto: BTreeMap<String, bool>,
    /// Every decision on each proposal, in the order recorded.
    decisions: BTreeMap<String, Vec<Decision>>,
    /// The ledger time of the latest event about each proposal.
    updated: BTreeMap<String, i64>,
    metadata: HashSet<String>,
    /// Open proposals main has moved past without touching what they change ([`Store::moved_past`]).
    clear: HashSet<String>,
}

impl State {
    fn revision(&self) -> Result<String> {
        Ok(auth::hex(&Sha256::digest(serde_json::to_vec(
            &self.owners,
        )?)))
    }

    // Approval records advance main without changing the memory a reviewer saw.
    fn memory_heads(&self, ledger: &Ledger) -> Vec<String> {
        let mut todo = ledger.heads(MAIN);
        let mut heads = BTreeSet::new();
        let mut seen = HashSet::new();
        while let Some(id) = todo.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if self.metadata.contains(&id) {
                if let Some(row) = ledger.rows.iter().find(|r| r.commit_id == id) {
                    todo.extend(row.parents.clone());
                }
            } else {
                heads.insert(id);
            }
        }
        heads.into_iter().collect()
    }

    fn current(&self, ledger: &Ledger, p: &Proposal) -> Result<bool> {
        Ok(self.stale_reason(ledger, p)?.is_none())
    }

    /// Why the proposal no longer describes what would publish, or None while it does.
    fn stale_reason(&self, ledger: &Ledger, p: &Proposal) -> Result<Option<&'static str>> {
        Ok(if p.policy_revision != self.revision()? {
            Some("ownership_changed")
        } else if p.base != self.memory_heads(ledger) && !self.clear.contains(&p.id) {
            Some("base_changed")
        } else if p.heads != sorted(ledger.heads(&p.branch)) {
            Some("draft_changed")
        } else {
            None
        })
    }

    fn accepted(&self, p: &Proposal) -> bool {
        !self.rejected.contains(&p.id)
            && (self
                .approvals
                .get(&p.id)
                .is_some_and(|a| p.required_owners.is_subset(a))
                || self.admin_approved.contains(&p.id)
                || self.auto.get(&p.id) == Some(&true))
    }

    /// Whether the decisions model rejected it and no owner has approved it since: an owner's approval overrides.
    fn auto_rejected(&self, p: &Proposal) -> bool {
        self.auto.get(&p.id) == Some(&false)
            && self.approvals.get(&p.id).is_none_or(|a| a.is_empty())
    }
}

#[derive(Default)]
struct Impact {
    owners: BTreeSet<String>,
    evidence: BTreeSet<String>,
    changes: Vec<serde_json::Value>,
}

/// The owners of the entities a fact version names: its subject, and its value where that is an entity.
fn owners_of(state: &State, f: &Fact) -> BTreeSet<String> {
    [
        Some(f.entity.as_str()),
        Some(f.value.as_str()),
        f.value_ref.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|id| state.owners.get(id).map(|o| o.user.clone()))
    .collect()
}

/// Each episode's ACL in every state the `EPISODES` rows of the commits `within` holds gave it, one per
/// `recorded_at` (rows recorded at the same instant are one state, their ACLs a union, as
/// [`Store::current_episodes`] reads them).
fn acl_states(
    rows: &[RecordBatch],
    within: impl Fn(&str) -> bool,
) -> HashMap<String, Vec<BTreeSet<String>>> {
    let mut states: HashMap<(String, i64), BTreeSet<String>> = HashMap::new();
    for b in rows {
        let (commit, id, acl, at) = (
            strings(b, "commit_id"),
            strings(b, "episode_id"),
            strings(b, "acl"),
            timestamps(b, "recorded_at"),
        );
        for i in 0..b.num_rows() {
            if !within(&commit[i]) {
                continue;
            }
            states
                .entry((id[i].clone(), at[i].unwrap_or(i64::MIN)))
                .or_default()
                .extend(split_list(&acl[i]).iter().map(|a| auth::norm_entry(a)));
        }
    }
    let mut out: HashMap<String, Vec<BTreeSet<String>>> = HashMap::new();
    for ((id, _), acl) in states {
        out.entry(id).or_default().push(acl);
    }
    out
}

/// Who an ACL lets read: its entries but [`FORGOTTEN`], which no one holds.
fn readers(acl: &BTreeSet<String>) -> BTreeSet<&str> {
    acl.iter()
        .map(String::as_str)
        .filter(|a| *a != FORGOTTEN)
        .collect()
}

/// Whether an ACL lets one person read at most: a single `user:`, or no one.
fn at_most_one_reader(acl: &BTreeSet<String>) -> bool {
    match readers(acl).into_iter().collect::<Vec<_>>().as_slice() {
        [] => true,
        [one] => one.starts_with("user:"),
        _ => false,
    }
}

/// The episodes among `states` ([`acl_states`]) that one and the same person at most may read, in every state
/// they have had: whichever of those visibility resolves (a later state's commit withdrawn, say), no one else
/// reads them, and handing one to another single reader isn't private (that reader didn't write it).
/// A fact is read only by those who may read every episode it cites, and a closure ends a fact only for those
/// who may read its episode: what cites one of these reaches one person alone, so it changes nothing the
/// entity's owners or anyone else reads, and needs no owner (the owners couldn't read its evidence to review it
/// either). Letting more than one person read such an episode later is what needs them
/// ([`Store::governance_impact`]).
fn private_episodes(states: &HashMap<String, Vec<BTreeSet<String>>>) -> HashSet<String> {
    states
        .iter()
        .filter(|(_, acls)| {
            let all: BTreeSet<String> = acls.iter().flat_map(|a| a.iter().cloned()).collect();
            at_most_one_reader(&all)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether fact version `f` cites an episode only one person may read ([`private_episodes`]).
fn private_fact(private: &HashSet<String>, f: &Fact) -> bool {
    f.source_episodes.iter().any(|e| private.contains(e))
}

/// Per episode, the owners of the facts among `facts` that cite it. Until producers carry explicit lineage,
/// shared evidence conservatively carries ownership. This also covers copies made by a subject merge racing
/// the first owner assignment. A fact one person alone reads carries none ([`private_episodes`]).
fn evidence_owners<'a>(
    state: &State,
    private: &HashSet<String>,
    facts: impl Iterator<Item = &'a Fact>,
) -> HashMap<String, BTreeSet<String>> {
    let mut out: HashMap<String, BTreeSet<String>> = HashMap::new();
    for f in facts.filter(|f| !private_fact(private, f)) {
        let required = owners_of(state, f);
        for source in &f.source_episodes {
            out.entry(source.clone())
                .or_default()
                .extend(required.clone());
        }
    }
    out
}

/// Who must approve fact version `f`: the owners it names, and those of every fact sharing its evidence. No one
/// for a version only one person reads ([`private_episodes`]).
fn required_for(
    state: &State,
    private: &HashSet<String>,
    evidence: &HashMap<String, BTreeSet<String>>,
    f: &Fact,
) -> BTreeSet<String> {
    if private_fact(private, f) {
        return BTreeSet::new();
    }
    let mut required = owners_of(state, f);
    for source in &f.source_episodes {
        if let Some(owners) = evidence.get(source) {
            required.extend(owners.clone());
        }
    }
    required
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Where a proposal stands ([`Store::status_of`]).
struct Standing {
    status: &'static str,
    rejected: bool,
    stale_reason: Option<&'static str>,
    published_at: Option<i64>,
    updated_at: i64,
}

/// The entities a proposal's changes name, each once, in order of first mention.
fn entities_of(p: &Proposal) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &p.changes {
        if let Some(e) = c["entity"].as_str()
            && !out.iter().any(|x| x == e)
        {
            out.push(e.to_string());
        }
    }
    out
}

/// What a stored governance row names, for the erasures (`crate::offboard`): the proposal it is (`proposal`
/// with `is_proposal`) or decides on, the episodes a proposal cites (its evidence and each fact row's), and the
/// user it names (an owner, a decision's reviewer, a proposal's proposer), and the entity whose ownership it
/// sets or removes. None where the body isn't an event.
pub(crate) struct Names {
    pub proposal: Option<String>,
    pub is_proposal: bool,
    pub evidence: BTreeSet<String>,
    pub user: Option<String>,
    pub entity: Option<String>,
}

pub(crate) fn names(body: &str) -> Option<Names> {
    Some(match serde_json::from_str::<Event>(body).ok()? {
        Event::Owner { user, entity } => Names {
            proposal: None,
            is_proposal: false,
            evidence: BTreeSet::new(),
            user: Some(user),
            entity: Some(entity),
        },
        Event::OwnerRemoved { entity } => Names {
            proposal: None,
            is_proposal: false,
            evidence: BTreeSet::new(),
            user: None,
            entity: Some(entity),
        },
        Event::Proposal { proposal } => {
            let mut evidence = proposal.evidence.clone();
            for c in &proposal.changes {
                if let Some(a) = c["evidence"].as_array() {
                    evidence.extend(a.iter().filter_map(|v| v.as_str()).map(str::to_string));
                }
            }
            Names {
                proposal: Some(proposal.id),
                is_proposal: true,
                evidence,
                user: Some(proposal.created_by),
                entity: None,
            }
        }
        Event::Decision { proposal, user, .. } => Names {
            proposal: Some(proposal),
            is_proposal: false,
            evidence: BTreeSet::new(),
            user: Some(user),
            entity: None,
        },
        // Goes with its proposal; it names no person.
        Event::AutoDecision { proposal, .. } => Names {
            proposal: Some(proposal),
            is_proposal: false,
            evidence: BTreeSet::new(),
            user: None,
            entity: None,
        },
    })
}

pub(crate) fn table_schema() -> Result<Schema> {
    schema(&[
        ("commit_id", true, PrimitiveType::String),
        ("body", true, PrimitiveType::String),
        ("recorded_at", true, PrimitiveType::Timestamptz),
    ])
}

impl Store {
    /// Publication checks cannot use an eventually refreshed table or a cached "missing" result.
    async fn governance_rows(&self, scope: &Scope, name: &str) -> Result<Arc<Vec<RecordBatch>>> {
        let ns = scope.namespace();
        self.forget_missing(&ns, name);
        self.invalidate(&ns, name);
        self.side_rows(&ns, name).await
    }

    async fn governance_state(&self, scope: &Scope, ledger: &Ledger) -> Result<State> {
        let rows = self.governance_rows(scope, TABLE).await?;
        let visible = ledger.visible(MAIN);
        let mut events = HashMap::new();
        for b in rows.iter() {
            for (id, body) in strings(b, "commit_id").into_iter().zip(strings(b, "body")) {
                if visible.contains(&id) {
                    // A row this engine can't read is left out, not a reason to refuse every write to the
                    // scope: nothing it would have granted is granted (a decision it held doesn't count).
                    let Ok(event) = serde_json::from_str::<Event>(&body) else {
                        eprintln!("governance: skipping an unreadable row at commit {id}");
                        continue;
                    };
                    anyhow::ensure!(
                        events.insert(id, event).is_none(),
                        "duplicate governance commit"
                    );
                }
            }
        }
        let mut ordered: Vec<_> = ledger
            .rows
            .iter()
            .filter(|r| events.contains_key(&r.commit_id))
            .collect();
        ordered.sort_by_key(|r| r.recorded_at);
        let mut state = State::default();
        for row in ordered {
            state.metadata.insert(row.commit_id.clone());
            match events.remove(&row.commit_id).unwrap() {
                Event::Owner { entity, user } => {
                    state.owners.insert(
                        entity.clone(),
                        Owner {
                            entity,
                            user,
                            revision: row.commit_id.clone(),
                            since: row.recorded_at,
                        },
                    );
                }
                Event::OwnerRemoved { entity } => {
                    state.owners.remove(&entity);
                }
                Event::Proposal { mut proposal } => {
                    proposal.created_at = row.recorded_at;
                    state.updated.insert(proposal.id.clone(), row.recorded_at);
                    state.proposals.insert(proposal.id.clone(), proposal);
                }
                Event::Decision {
                    proposal,
                    digest,
                    user,
                    approve,
                    note,
                    admin,
                } => {
                    // A decision on a proposal no longer stored (its evidence was erased) or that doesn't fit
                    // its proposal grants nothing and is left out.
                    let Some(p) = state.proposals.get(&proposal) else {
                        continue;
                    };
                    if p.digest == digest && (admin || p.required_owners.contains(&user)) {
                        state.updated.insert(proposal.clone(), row.recorded_at);
                        state
                            .decisions
                            .entry(proposal.clone())
                            .or_default()
                            .push(Decision {
                                user: user.clone(),
                                approve,
                                at: row.recorded_at,
                                note,
                                admin,
                            });
                        if approve {
                            if admin {
                                state.admin_approved.insert(proposal.clone());
                            }
                            state.approvals.entry(proposal).or_default().insert(user);
                        } else {
                            state.rejected.insert(proposal);
                        }
                    } else {
                        eprintln!(
                            "governance: skipping a decision that doesn't fit proposal {proposal}"
                        );
                    }
                }
                Event::AutoDecision {
                    proposal,
                    digest,
                    approve,
                    model,
                    confidence,
                } => {
                    let Some(p) = state.proposals.get(&proposal) else {
                        continue;
                    };
                    if p.digest != digest {
                        eprintln!(
                            "governance: skipping a model's decision that doesn't fit proposal {proposal}"
                        );
                        continue;
                    }
                    state.updated.insert(proposal.clone(), row.recorded_at);
                    state
                        .decisions
                        .entry(proposal.clone())
                        .or_default()
                        .push(Decision {
                            user: AUTO_REVIEWER.into(),
                            approve,
                            at: row.recorded_at,
                            note: Some(format!(
                                "decided by {model}, {:.0}% sure",
                                confidence * 100.0
                            )),
                            admin: false,
                        });
                    state.auto.insert(proposal, approve);
                }
            }
        }
        state.clear = self.moved_past(scope, ledger, &state).await?;
        Ok(state)
    }

    /// The open proposals main has moved past since their base without touching what they change: no commit
    /// since then writes a fact on one of their keys, a version, closure, quote or retraction of one of their
    /// facts, or anything about their evidence, and none changes the schema or withdraws an extractor's facts.
    /// Such a proposal still describes what publishing it changes, so a busy main (extraction commits every few
    /// seconds) doesn't void it. Anything else that moved main since its base makes it stale, as before.
    async fn moved_past(
        &self,
        scope: &Scope,
        ledger: &Ledger,
        state: &State,
    ) -> Result<HashSet<String>> {
        let heads = state.memory_heads(ledger);
        let main = ledger.visible(MAIN);
        let open: Vec<&Proposal> = state
            .proposals
            .values()
            .filter(|p| {
                p.base != heads
                    && !state.rejected.contains(&p.id)
                    && !p.heads.iter().all(|h| main.contains(h))
            })
            .collect();
        let mut out = HashSet::new();
        if open.is_empty() {
            return Ok(out);
        }
        let col = |b: &RecordBatch, name: &str, i: usize| -> Option<String> {
            b.column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .filter(|c| !c.is_null(i))
                .map(|c| c.value(i).to_string())
        };
        let facts: Vec<(String, Fact)> = self
            .governance_rows(scope, FACTS)
            .await?
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .collect();
        let mut side = Vec::new();
        for table in [CLOSURES, QUOTES, RETRACTIONS, EPISODES, SCHEMAS] {
            side.push((table, self.governance_rows(scope, table).await?));
        }
        for p in open {
            let before = ledger.reachable(&p.base);
            let since: HashSet<&String> = main
                .iter()
                .filter(|c| !before.contains(*c) && !state.metadata.contains(*c))
                .collect();
            let keys: HashSet<(&str, &str)> = p
                .changes
                .iter()
                .filter(|c| c["kind"] == "fact")
                .filter_map(|c| Some((c["entity"].as_str()?, c["predicate"].as_str()?)))
                .collect();
            let ids: HashSet<&str> = p
                .changes
                .iter()
                .filter_map(|c| c["id"].as_str().or(c["fact_id"].as_str()))
                .collect();
            // Its evidence, and what the values on its keys cite (what its owners reviewed as "before"): an ACL
            // change or a delete of one of those changes what they read.
            let evidence: HashSet<&str> = p
                .evidence
                .iter()
                .map(String::as_str)
                .chain(
                    facts
                        .iter()
                        .filter(|(_, f)| keys.contains(&(f.entity.as_str(), f.predicate.as_str())))
                        .flat_map(|(_, f)| f.source_episodes.iter().map(String::as_str)),
                )
                .collect();
            let touched_fact = facts.iter().any(|(c, f)| {
                since.contains(c)
                    && (keys.contains(&(f.entity.as_str(), f.predicate.as_str()))
                        || ids.contains(f.fact_id.as_str())
                        || f.source_episodes
                            .iter()
                            .any(|e| evidence.contains(e.as_str())))
            });
            let touched_side = side.iter().any(|(table, rows)| {
                rows.iter().any(|b| {
                    let commits = strings(b, "commit_id");
                    (0..b.num_rows()).any(|i| {
                        since.contains(&commits[i])
                            && match *table {
                                SCHEMAS => true,
                                EPISODES => col(b, "episode_id", i)
                                    .is_some_and(|e| evidence.contains(e.as_str())),
                                _ => {
                                    col(b, "actor_prefix", i).is_some()
                                        || col(b, "fact_id", i)
                                            .is_some_and(|f| ids.contains(f.as_str()))
                                        || col(b, "episode_id", i)
                                            .is_some_and(|e| evidence.contains(e.as_str()))
                                }
                            }
                    })
                })
            });
            if !touched_fact && !touched_side {
                out.insert(p.id.clone());
            }
        }
        Ok(out)
    }

    async fn governance_ledger(&self, scope: &Scope) -> Result<Ledger> {
        let snap = self.ledger_now().await?;
        Ok(Ledger::from_batches(&snap.rows, &scope.namespace()))
    }

    async fn governance_event(
        &self,
        scope: &Scope,
        who: &Principal,
        event: Event,
        expected: Vec<String>,
    ) -> Result<()> {
        self.check_fence()?;
        let ns = scope.namespace();
        let id = uuid::Uuid::new_v4().to_string();
        let table = self.write_target(&ns, TABLE).await?;
        let col = |s: String| -> ArrayRef { Arc::new(StringArray::from(vec![s])) };
        append(
            &*self.catalog(),
            &table,
            vec![
                ("commit_id", col(id.clone())),
                ("body", col(serde_json::to_string(&event)?)),
                ("recorded_at", ts([Some(self.now())])),
            ],
            OnConflict::Recommit,
        )
        .await?;
        self.invalidate(&ns, TABLE);
        self.append_ledger(&id, scope, MAIN, who.user(), &|ledger| {
            anyhow::ensure!(
                sorted(ledger.heads(MAIN)) == expected,
                "memory changed while recording review; retry"
            );
            Ok(ledger.heads(MAIN))
        })
        .await
    }

    /// The first assignment is bootstrapped by an org admin; an existing owner alone can transfer it.
    /// The owner must be an explicit member of the scope. Assignment does not attest existing facts.
    pub async fn set_entity_owner(
        &self,
        scope: &Scope,
        who: &Principal,
        via: &directory::SignIn,
        entity: &str,
        user: &str,
        expected_revision: Option<&str>,
    ) -> Result<Owner> {
        anyhow::ensure!(
            !entity.is_empty() && entity.len() <= 512 && !entity.contains([',', '\n', '\r']),
            "invalid entity id"
        );
        let org = scope
            .org_name()
            .ok_or_else(|| anyhow::anyhow!("ownership requires an organization"))?;
        let dir = directory::Directory::load(self).await?;
        let team = match scope {
            Scope::Team(id) => Some(id.rsplit('/').next().unwrap()),
            _ => None,
        };
        dir.resolve(who.user(), via, Some(org), team)?;
        let user = auth::norm_id(user);
        anyhow::ensure!(
            dir.holds_member(&user, org, team),
            "owner must be a member of this scope"
        );
        self.ensure_scope(scope).await?;
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        match state.owners.get(entity) {
            Some(old) => {
                anyhow::ensure!(
                    old.user == auth::norm_id(who.user()),
                    "only the current owner may transfer ownership"
                );
                anyhow::ensure!(old.user != user, "already the owner");
                anyhow::ensure!(
                    expected_revision == Some(old.revision.as_str()),
                    "ownership revision changed; refresh before transferring"
                );
            }
            None => {
                anyhow::ensure!(expected_revision.is_none(), "entity has no owner revision");
                anyhow::ensure!(
                    dir.is_admin(who.user(), via, org),
                    "only an org admin may assign the first owner"
                );
            }
        }
        self.governance_event(
            scope,
            who,
            Event::Owner {
                entity: entity.into(),
                user,
            },
            sorted(ledger.heads(MAIN)),
        )
        .await?;
        self.entity_owners(scope)
            .await?
            .into_iter()
            .find(|o| o.entity == entity)
            .ok_or_else(|| anyhow::anyhow!("owner was not recorded"))
    }

    /// The owner, or an org admin, removes an entity's owner: its changes publish without review again.
    /// The caller names the ownership revision they saw, so a transfer and a removal can't race.
    pub async fn remove_entity_owner(
        &self,
        scope: &Scope,
        who: &Principal,
        via: &directory::SignIn,
        entity: &str,
        expected_revision: &str,
    ) -> Result<()> {
        let org = scope
            .org_name()
            .ok_or_else(|| anyhow::anyhow!("ownership requires an organization"))?;
        let dir = directory::Directory::load(self).await?;
        let team = match scope {
            Scope::Team(id) => Some(id.rsplit('/').next().unwrap()),
            _ => None,
        };
        dir.resolve(who.user(), via, Some(org), team)?;
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let old = state
            .owners
            .get(entity)
            .ok_or_else(|| anyhow::anyhow!("entity has no owner"))?;
        anyhow::ensure!(
            old.user == auth::norm_id(who.user()) || dir.is_admin(who.user(), via, org),
            "only the owner or an org admin may remove an owner"
        );
        anyhow::ensure!(
            old.revision == expected_revision,
            "ownership revision changed; refresh before removing"
        );
        self.governance_event(
            scope,
            who,
            Event::OwnerRemoved {
                entity: entity.into(),
            },
            sorted(ledger.heads(MAIN)),
        )
        .await
    }

    pub async fn entity_owners(&self, scope: &Scope) -> Result<Vec<Owner>> {
        let ledger = self.governance_ledger(scope).await?;
        Ok(self
            .governance_state(scope, &ledger)
            .await?
            .owners
            .into_values()
            .collect())
    }

    pub(crate) async fn check_owned_subject_merge(
        &self,
        scope: &Scope,
        from: &str,
        into: &str,
    ) -> Result<()> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        anyhow::ensure!(
            !state.owners.contains_key(from) && !state.owners.contains_key(into),
            "owner approval required: protected subject merges need an ownership migration and are not supported yet"
        );
        Ok(())
    }

    /// Conservative impact discovery over the actual immutable rows, never a caller's claimed manifest.
    /// A shared document needs the owners of what it touches (`document_owners` below), an aggregate those of its
    /// subject and of the facts it cites: a scope's advice or compiled procedures don't wait for an owner of an
    /// entity they never name. Schema changes and an extractor's withdrawal, which have no exact list of what they
    /// change, still need every owner in the scope.
    async fn governance_impact(
        &self,
        scope: &Scope,
        commits: &HashSet<String>,
        published: &HashSet<String>,
        state: &State,
    ) -> Result<Impact> {
        let mut out = Impact::default();
        if state.owners.is_empty() {
            return Ok(out);
        }
        let all_owners: BTreeSet<_> = state.owners.values().map(|o| o.user.clone()).collect();
        let owned: HashSet<&str> = state.owners.keys().map(String::as_str).collect();
        // Every table the check reads, fresh, at once: each is a catalog round trip, and this runs inside the ledger's
        // compare-and-set, where every second spent lets another writer's append make the commit try again.
        let started = Instant::now();
        let (
            episode_rows,
            closure_rows,
            aggregate_rows,
            quote_rows,
            retraction_rows,
            schema_rows,
            fact_rows,
        ) = futures::try_join!(
            self.governance_rows(scope, EPISODES),
            self.governance_rows(scope, CLOSURES),
            self.governance_rows(scope, AGGREGATES),
            self.governance_rows(scope, QUOTES),
            self.governance_rows(scope, RETRACTIONS),
            self.governance_rows(scope, SCHEMAS),
            self.governance_rows(scope, FACTS),
        )?;
        let loaded = started.elapsed();
        // Who reads each episode as main stands, and once these commits land.
        let private_before =
            private_episodes(&acl_states(&episode_rows, |c| published.contains(c)));
        let private = private_episodes(&acl_states(&episode_rows, |c| {
            published.contains(c) || commits.contains(c)
        }));
        // Each document version main holds: its key and the entities it is about; and each key's versions.
        let mut published_docs: HashMap<String, (String, Vec<String>)> = HashMap::new();
        let mut versions_of: HashMap<String, Vec<String>> = HashMap::new();
        for b in episode_rows.iter() {
            let (Some(commit), Some(ids), Some(docs)) = (
                text_column(b, "commit_id"),
                text_column(b, "episode_id"),
                text_column(b, "document"),
            ) else {
                continue;
            };
            let about = text_column(b, "about");
            for i in 0..b.num_rows() {
                if docs.is_null(i) || !published.contains(commit.value(i)) {
                    continue;
                }
                let (id, key) = (ids.value(i).to_string(), docs.value(i).to_string());
                let about = about
                    .filter(|a| !a.is_null(i))
                    .map(|a| split_list(a.value(i)))
                    .unwrap_or_default();
                versions_of.entry(key.clone()).or_default().push(id.clone());
                published_docs.insert(id, (key, about));
            }
        }
        let document_ids: HashSet<String> = published_docs.keys().cloned().collect();
        // What these commits reach beyond their own rows: the episodes whose older versions they retire, the episodes
        // they let more than one person read, and the facts their rows name (by id, by a document key, by what an
        // aggregate cites, or by a closure on what they retire or widen).
        let superseded = |b: &RecordBatch, i: usize| {
            b.column_by_name("superseded")
                .and_then(|c| c.as_any().downcast_ref::<arrow_array::BooleanArray>())
                .is_some_and(|c| !c.is_null(i) && c.value(i))
        };
        let mut retired: HashSet<String> = HashSet::new();
        let mut needed: HashSet<String> = HashSet::new();
        for (table, rows) in [
            (CLOSURES, &closure_rows),
            (QUOTES, &quote_rows),
            (RETRACTIONS, &retraction_rows),
        ] {
            for b in rows.iter() {
                let Some(commit) = text_column(b, "commit_id") else {
                    continue;
                };
                let (fid, ep) = (text_column(b, "fact_id"), text_column(b, "episode_id"));
                let at = |c: Option<&StringArray>, i: usize| {
                    c.filter(|c| !c.is_null(i)).map(|c| c.value(i).to_string())
                };
                for i in (0..b.num_rows()).filter(|i| commits.contains(commit.value(*i))) {
                    needed.extend(at(fid, i));
                    if table == RETRACTIONS && superseded(b, i) {
                        retired.extend(at(ep, i));
                    }
                }
            }
        }
        let mut widened: HashSet<String> = HashSet::new();
        let key_parts = |key: &str| -> Vec<String> {
            std::iter::once(key)
                .chain(key.split('/'))
                .map(str::to_string)
                .collect()
        };
        // The episodes these commits write: what they widen, and the document keys they write under.
        let mut written: HashSet<String> = HashSet::new();
        for b in episode_rows.iter() {
            let (Some(commit), Some(ids)) =
                (text_column(b, "commit_id"), text_column(b, "episode_id"))
            else {
                continue;
            };
            let docs = text_column(b, "document");
            for i in (0..b.num_rows()).filter(|i| commits.contains(commit.value(*i))) {
                let id = ids.value(i);
                written.insert(id.to_string());
                if private_before.contains(id) && !private.contains(id) {
                    widened.insert(id.to_string());
                }
                if let Some(key) = docs.filter(|d| !d.is_null(i)).map(|d| d.value(i)) {
                    needed.extend(key_parts(key));
                }
            }
        }
        for e in retired.iter().chain(&widened) {
            if let Some((key, _)) = published_docs.get(e) {
                needed.extend(key_parts(key));
            }
        }
        for b in aggregate_rows.iter() {
            let (Some(commit), Some(episodes)) =
                (text_column(b, "commit_id"), text_column(b, "episodes"))
            else {
                continue;
            };
            let cites = text_column(b, "cites");
            for i in 0..b.num_rows() {
                let c = commit.value(i);
                let reads_retired = !retired.is_empty()
                    && published.contains(c)
                    && episodes.value(i).split(',').any(|e| retired.contains(e));
                if commits.contains(c) || reads_retired {
                    needed.extend(
                        cites
                            .filter(|x| !x.is_null(i))
                            .map(|x| x.value(i))
                            .into_iter()
                            .flat_map(|x| x.split(','))
                            .filter(|id| !id.is_empty())
                            .map(str::to_string),
                    );
                }
            }
        }
        if !retired.is_empty() || !widened.is_empty() {
            for b in closure_rows.iter() {
                let (Some(commit), Some(fid), Some(ep)) = (
                    text_column(b, "commit_id"),
                    text_column(b, "fact_id"),
                    text_column(b, "episode_id"),
                ) else {
                    continue;
                };
                for i in 0..b.num_rows() {
                    let e = ep.value(i);
                    if (retired.contains(e) || widened.contains(e))
                        && published.contains(commit.value(i))
                    {
                        needed.insert(fid.value(i).to_string());
                    }
                }
            }
        }
        // The facts weighed, never the scope's whole history: those these commits write and those naming an owned
        // entity (only they carry ownership to their evidence, [`evidence_owners`]); then every version of a fact
        // weighed or named above, and those citing an episode retired or widened.
        let first = |c: &str, names: [Option<&str>; 3]| {
            commits.contains(c) || names.iter().flatten().any(|n| owned.contains(n))
        };
        let mut decoded = select_facts(&fact_rows, |c, _, names, _| first(c, names))?;
        needed.extend(
            decoded
                .iter()
                .filter(|(c, _)| commits.contains(c))
                .map(|(_, f)| f.fact_id.clone()),
        );
        let reached: HashSet<&str> = retired.iter().chain(&widened).map(String::as_str).collect();
        decoded.extend(select_facts(&fact_rows, |c, id, names, sources| {
            !first(c, names)
                && (needed.contains(id)
                    || (!reached.is_empty() && sources.split(',').any(|e| reached.contains(e))))
        })?);
        if started.elapsed() > Duration::from_secs(1) {
            let rows: usize = fact_rows.iter().map(|b| b.num_rows()).sum();
            eprintln!(
                "publication check in {scope}: tables read in {} ms, {rows} fact rows scanned and {} decoded by {} ms",
                loaded.as_millis(),
                decoded.len(),
                started.elapsed().as_millis()
            );
        }
        let mut facts: HashMap<String, Vec<Fact>> = HashMap::new();
        for (_, f) in &decoded {
            facts.entry(f.fact_id.clone()).or_default().push(f.clone());
        }
        let evidence_owners = evidence_owners(
            state,
            &private,
            decoded
                .iter()
                .filter(|(c, _)| published.contains(c) || commits.contains(c))
                .map(|(_, f)| f),
        );
        let required_for = |f: &Fact| required_for(state, &private, &evidence_owners, f);
        for (commit, f) in &decoded {
            if commits.contains(commit) {
                // Reusing an id under another subject must not hide its protected identity.
                let owners: BTreeSet<_> =
                    facts[&f.fact_id].iter().flat_map(&required_for).collect();
                if !owners.is_empty() {
                    out.owners.extend(owners);
                    out.evidence.extend(f.source_episodes.clone());
                    out.changes.push(serde_json::json!({"kind":"fact", "id":f.fact_id,
                            "entity":f.entity,"predicate":f.predicate,"value":f.value,
                            "valid_from":f.valid_from,"valid_to":f.valid_to,"evidence":f.source_episodes}));
                }
            }
        }
        // Who must approve each version were none of it private: the owners a widening needs.
        let shared_owners = crate::governance::evidence_owners(
            state,
            &HashSet::new(),
            decoded
                .iter()
                .filter(|(c, _)| published.contains(c))
                .map(|(_, f)| f),
        );
        // Who a write must ask, by what it touches. An entity's owner; a fact's owners ([`required_for`]: those of the
        // entities it names and of every fact sharing its evidence); and those an episode carries, the owners of the
        // facts that cite it.
        let entity_owner = |id: &str| state.owners.get(id).map(|o| o.user.clone());
        let fact_owners = |id: &str| -> BTreeSet<String> {
            facts
                .get(id)
                .into_iter()
                .flatten()
                .flat_map(&required_for)
                .collect()
        };
        let carried = |episode: &str| -> BTreeSet<String> {
            evidence_owners.get(episode).cloned().unwrap_or_default()
        };
        // A shared document's owners: those of the entities it is about, of an entity or fact its key names (a brief
        // keyed by its subject, a dream record keyed by the facts it decided on), and of the facts that cite it or any
        // other version of it (a policy page whose facts an owner holds). A document touching none of them (a team's
        // advice, a compiled procedure) changes nothing an owner holds: what a new version says reaches owned entities
        // only through the facts extraction reads from it, which their owners review as any fact
        // ([`Store::stage_extracted`]).
        let document_owners = |episode: &str, key: Option<&str>, about: &[String]| {
            let mut owners: BTreeSet<String> =
                about.iter().filter_map(|a| entity_owner(a)).collect();
            if let Some(key) = key {
                for part in std::iter::once(key).chain(key.split('/')) {
                    owners.extend(entity_owner(part));
                    owners.extend(fact_owners(part));
                }
                for version in versions_of.get(key).into_iter().flatten() {
                    owners.extend(carried(version));
                }
            }
            owners.extend(carried(episode));
            owners
        };
        // An aggregate's owners: its subject's, and those of the facts it rests on (`cites`); a row from before
        // aggregates named them, those its episodes carry.
        let aggregate_owners = |subject: Option<&str>, cites: Option<&str>, episodes: &str| {
            let mut owners: BTreeSet<String> = subject.and_then(entity_owner).into_iter().collect();
            match cites.filter(|c| !c.is_empty()) {
                Some(cites) => {
                    for id in cites.split(',').filter(|id| !id.is_empty()) {
                        owners.extend(fact_owners(id));
                    }
                }
                None => {
                    for e in episodes.split(',').filter(|e| !e.is_empty()) {
                        owners.extend(carried(e));
                    }
                }
            }
            owners
        };
        // What retiring an older version (a superseded retraction) changes on main: the facts citing it are no
        // longer read (a fact is read only with every episode it cites), the facts its closures ended are current
        // again, and the aggregates that read it go.
        let mut citing: HashMap<&str, Vec<&Fact>> = HashMap::new();
        for (_, f) in decoded.iter().filter(|(c, _)| published.contains(c)) {
            for e in &f.source_episodes {
                citing.entry(e.as_str()).or_default().push(f);
            }
        }
        let mut ended: HashMap<String, Vec<String>> = HashMap::new();
        for cb in closure_rows.iter() {
            let (commit, fid, ep) = (
                strings(cb, "commit_id"),
                strings(cb, "fact_id"),
                strings(cb, "episode_id"),
            );
            for k in 0..cb.num_rows() {
                if published.contains(&commit[k]) {
                    ended.entry(ep[k].clone()).or_default().push(fid[k].clone());
                }
            }
        }
        // Each retired episode main's aggregates read, with the owners of the aggregates reading it.
        let mut aggregated: HashMap<String, BTreeSet<String>> = HashMap::new();
        if !retired.is_empty() {
            for b in aggregate_rows.iter() {
                let (Some(commit), Some(episodes)) =
                    (text_column(b, "commit_id"), text_column(b, "episodes"))
                else {
                    continue;
                };
                let (subject, cites) = (text_column(b, "subject"), text_column(b, "cites"));
                for i in 0..b.num_rows() {
                    let read: Vec<&str> = episodes
                        .value(i)
                        .split(',')
                        .filter(|e| retired.contains(*e))
                        .collect();
                    if read.is_empty() || !published.contains(commit.value(i)) {
                        continue;
                    }
                    let owners =
                        aggregate_owners(cell(subject, i), cell(cites, i), episodes.value(i));
                    for e in read {
                        aggregated
                            .entry(e.to_string())
                            .or_default()
                            .extend(owners.iter().cloned());
                    }
                }
            }
        }
        for table in [CLOSURES, QUOTES, RETRACTIONS, SCHEMAS, EPISODES, AGGREGATES] {
            let rows = match table {
                EPISODES => episode_rows.clone(),
                CLOSURES => closure_rows.clone(),
                AGGREGATES => aggregate_rows.clone(),
                QUOTES => quote_rows.clone(),
                RETRACTIONS => retraction_rows.clone(),
                _ => schema_rows.clone(),
            };
            // ACL-only changes to an existing episode remain immediate, including documents.
            // Compare every non-ACL metadata field; sharing a document key alone is insufficient.
            let signature = |b: &RecordBatch, i: usize| {
                let text = |name: &str| {
                    b.column_by_name(name)
                        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                        .filter(|c| !c.is_null(i))
                        .map(|c| c.value(i).to_string())
                };
                let time = b
                    .column_by_name("occurred_at")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>())
                    .filter(|c| !c.is_null(i))
                    .map(|c| c.value(i));
                let indexed = b
                    .column_by_name("index_text")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::BooleanArray>())
                    .filter(|c| !c.is_null(i))
                    .is_none_or(|c| c.value(i));
                (
                    text("episode_id"),
                    text("source"),
                    time,
                    text("about"),
                    indexed,
                    text("document"),
                )
            };
            // Only the episodes these commits write again can be ACL-only changes of what main holds.
            let existing: HashSet<_> = if table == EPISODES {
                let mut out = HashSet::new();
                for b in rows.iter() {
                    let (Some(commit), Some(ids)) =
                        (text_column(b, "commit_id"), text_column(b, "episode_id"))
                    else {
                        continue;
                    };
                    for i in 0..b.num_rows() {
                        if written.contains(ids.value(i)) && published.contains(commit.value(i)) {
                            out.insert(signature(b, i));
                        }
                    }
                }
                out
            } else {
                HashSet::new()
            };
            for b in rows.iter() {
                let Some(ids) = text_column(b, "commit_id") else {
                    continue;
                };
                for (i, id) in (0..b.num_rows())
                    .map(|i| (i, ids.value(i)))
                    .filter(|(_, id)| commits.contains(*id))
                {
                    let id = &id.to_string();
                    let value = |name: &str| -> Option<String> {
                        b.column_by_name(name)
                            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                            .filter(|c| !c.is_null(i))
                            .map(|c| c.value(i).to_string())
                    };
                    let mut owners = BTreeSet::new();
                    match table {
                        CLOSURES | QUOTES | RETRACTIONS => {
                            // A closure from an episode one person alone reads ends the fact for them alone.
                            let private_end = table == CLOSURES
                                && value("episode_id").is_some_and(|e| private.contains(&e));
                            if let Some(fid) = value("fact_id")
                                && !private_end
                            {
                                let versions = facts
                                    .get(&fid)
                                    .ok_or_else(|| anyhow::anyhow!("unknown changed fact {fid}"))?;
                                owners.extend(versions.iter().flat_map(&required_for));
                                if !owners.is_empty() {
                                    out.evidence.extend(
                                        versions.iter().flat_map(|f| f.source_episodes.clone()),
                                    );
                                }
                            }
                            // Source erasure and ACL enforcement remain immediate. Semantic extractor
                            // withdrawal, which has no exact fact list, conservatively needs every owner.
                            if value("actor_prefix").is_some() {
                                owners.extend(all_owners.clone());
                            }
                            // Retiring an older source version is a semantic replacement, not erasure: the owners
                            // of what it changes decide, those of the facts citing it and of the facts it ended
                            // (shared evidence carries ownership, as for any fact). Ended from a version one
                            // person alone reads, a fact comes back for them alone. A document's version also
                            // needs the document's owners, and one an aggregate read those of the aggregates
                            // reading it, as writing them does.
                            if b.column_by_name("superseded")
                                .and_then(|c| {
                                    c.as_any().downcast_ref::<arrow_array::BooleanArray>()
                                })
                                .is_some_and(|c| !c.is_null(i) && c.value(i))
                            {
                                match value("episode_id") {
                                    Some(e) => {
                                        owners.extend(
                                            citing
                                                .get(e.as_str())
                                                .into_iter()
                                                .flatten()
                                                .flat_map(|f| required_for(f)),
                                        );
                                        if !private.contains(&e) {
                                            owners.extend(
                                                ended
                                                    .get(&e)
                                                    .into_iter()
                                                    .flatten()
                                                    .filter_map(|fid| facts.get(fid))
                                                    .flatten()
                                                    .flat_map(&required_for),
                                            );
                                        }
                                        if let Some((key, about)) = published_docs.get(&e) {
                                            owners.extend(document_owners(&e, Some(key), about));
                                        }
                                        owners.extend(
                                            aggregated.get(&e).cloned().unwrap_or_default(),
                                        );
                                    }
                                    None => owners.extend(all_owners.clone()),
                                }
                            }
                            // The closure's episode, and the version a retraction retires, are what the
                            // reviewer reads.
                            if matches!(table, CLOSURES | RETRACTIONS)
                                && !owners.is_empty()
                                && let Some(e) = value("episode_id")
                            {
                                out.evidence.insert(e);
                            }
                        }
                        SCHEMAS => {
                            owners.extend(all_owners.clone());
                        }
                        EPISODES => {
                            let was_document =
                                value("episode_id").is_some_and(|id| document_ids.contains(&id));
                            let document = value("document").is_some() || was_document;
                            // What the document touches: as this row writes it, and as main held it (a row
                            // dropping a version's document identity changes that document).
                            let touched = |e: &str| {
                                let about = value("about")
                                    .as_deref()
                                    .map(split_list)
                                    .unwrap_or_default();
                                let mut owners =
                                    document_owners(e, value("document").as_deref(), &about);
                                if let Some((key, about)) = published_docs.get(e) {
                                    owners.extend(document_owners(e, Some(key), about));
                                }
                                owners
                            };
                            // A document one person at most reads (a session's conversation, a person's own
                            // advice) publishes nothing the owners or anyone else reads, as a fact citing such an
                            // episode doesn't: it needs no owner. Letting others read it later does (below).
                            let private_document =
                                value("episode_id").is_some_and(|e| private.contains(&e));
                            if document
                                && !private_document
                                && !existing.contains(&signature(b, i))
                                && let Some(e) = value("episode_id")
                            {
                                let touched = touched(&e);
                                if !touched.is_empty() {
                                    owners.extend(touched);
                                    out.evidence.insert(e);
                                }
                            }
                            // A document one person at most read, now read by more: what it says reaches others
                            // for the first time, as a new document's would.
                            if document
                                && let Some(e) = value("episode_id")
                                && private_before.contains(&e)
                                && !private.contains(&e)
                            {
                                let touched = touched(&e);
                                if !touched.is_empty() {
                                    owners.extend(touched);
                                    out.evidence.insert(e);
                                }
                            }
                            // Letting more than one person read an episode one person at most read: the facts
                            // citing it and the facts it closed took no owner when they landed, as no one else
                            // read that ([`private_episodes`]); now others would, so those facts' owners decide.
                            // Narrowing it, handing it to another single reader, or forgetting it stays
                            // immediate.
                            if let Some(e) = value("episode_id")
                                && private_before.contains(&e)
                                && !private.contains(&e)
                            {
                                let raw = |f: &Fact| {
                                    crate::governance::required_for(
                                        state,
                                        &HashSet::new(),
                                        &shared_owners,
                                        f,
                                    )
                                };
                                let mut cited: BTreeSet<String> = decoded
                                    .iter()
                                    .filter(|(c, f)| {
                                        published.contains(c) && f.source_episodes.contains(&e)
                                    })
                                    .flat_map(|(_, f)| raw(f))
                                    .collect();
                                for cb in closure_rows.iter() {
                                    let (commit, fid, ep) = (
                                        strings(cb, "commit_id"),
                                        strings(cb, "fact_id"),
                                        strings(cb, "episode_id"),
                                    );
                                    for k in 0..cb.num_rows() {
                                        if published.contains(&commit[k]) && ep[k] == e {
                                            cited.extend(
                                                facts
                                                    .get(&fid[k])
                                                    .into_iter()
                                                    .flatten()
                                                    .flat_map(raw),
                                            );
                                        }
                                    }
                                }
                                if !cited.is_empty() {
                                    owners.extend(cited);
                                    out.evidence.insert(e);
                                }
                            }
                        }
                        AGGREGATES => {
                            let es = value("episodes").unwrap_or_default();
                            owners.extend(aggregate_owners(
                                value("subject").as_deref(),
                                value("cites").as_deref(),
                                &es,
                            ));
                            if !owners.is_empty() {
                                out.evidence.extend(
                                    es.split(',').filter(|e| !e.is_empty()).map(str::to_string),
                                );
                            }
                        }
                        _ => unreachable!(),
                    }
                    if !owners.is_empty() {
                        out.owners.extend(owners);
                        let mut change = serde_json::Map::new();
                        change.insert("kind".into(), table.into());
                        change.insert("commit".into(), id.clone().into());
                        for name in [
                            "fact_id",
                            "episode_id",
                            "source",
                            "document",
                            "body",
                            "subject",
                            "key",
                            "value",
                            "note",
                            "actor_prefix",
                            "quote",
                            "name",
                            "version",
                            "cites",
                            "audience",
                        ] {
                            if let Some(v) = value(name) {
                                change.insert(name.into(), v.into());
                            }
                        }
                        for name in ["valid_to", "as_of"] {
                            if let Some(column) = b.column_by_name(name).and_then(|c| {
                                c.as_any().downcast_ref::<TimestampMicrosecondArray>()
                            }) && !column.is_null(i)
                            {
                                change.insert(name.into(), column.value(i).into());
                            }
                        }
                        out.changes.push(change.into());
                    }
                }
            }
        }
        out.changes.sort_by_key(|v| v.to_string());
        Ok(out)
    }

    async fn governance_readable(
        &self,
        scope: &Scope,
        p: &Proposal,
        who: &Principal,
    ) -> Result<()> {
        let vis = self.visibility(scope, &p.branch).await?;
        anyhow::ensure!(
            p.evidence
                .iter()
                .all(|id| !vis.is_gone(id) && vis.acls.get(id).is_some_and(|acl| who.can_see(acl))),
            "review includes evidence you cannot read"
        );
        // Fail closed: a fact row that cites no episode is readable by no one on main, so a proposal
        // carrying one is shown only to those who must act on it (its owners) and who made it.
        let grounded = p.changes.iter().all(|c| {
            c["kind"] != "fact" || c["evidence"].as_array().is_some_and(|a| !a.is_empty())
        });
        let insider = p.required_owners.contains(&auth::norm_id(who.user()))
            || auth::norm_id(&p.created_by) == auth::norm_id(who.user());
        anyhow::ensure!(
            grounded || insider,
            "review includes a change whose evidence you cannot read"
        );
        Ok(())
    }

    pub async fn propose_branch(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
    ) -> Result<Review> {
        self.propose_as(scope, branch, who, true, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("proposal made without a review"))
    }

    /// [`Store::propose_branch`]; `readable`: whether the proposer must read its evidence. Only the engine skips
    /// it, proposing what it held back ([`Store::stage_extracted`]) as [`EXTRACT_PROPOSER`], which reads
    /// nothing: every reader of the proposal, its owners included, is still checked on reading it.
    async fn propose_as(
        &self,
        scope: &Scope,
        branch: &str,
        who: &Principal,
        readable: bool,
        supersedes: Option<&str>,
    ) -> Result<Option<Review>> {
        anyhow::ensure!(
            !branch.is_empty() && branch != MAIN,
            "proposal requires a draft branch"
        );
        let ledger = self.governance_ledger(scope).await?;
        anyhow::ensure!(ledger.base(branch).is_some(), "no such branch");
        let state = self.governance_state(scope, &ledger).await?;
        let heads = sorted(ledger.heads(branch));
        let main = ledger.visible(MAIN);
        let new: HashSet<_> = ledger
            .reachable(&heads)
            .difference(&main)
            .cloned()
            .collect();
        let impact = self.governance_impact(scope, &new, &main, &state).await?;
        // A branch staged again in place of a held one is proposed even when nothing on it needs an owner now:
        // its proposal is what retires the branch it replaces.
        anyhow::ensure!(
            !impact.owners.is_empty() || supersedes.is_some(),
            "branch has no protected changes"
        );
        let base = state.memory_heads(&ledger);
        let policy_revision = state.revision()?;
        let digest = auth::hex(&Sha256::digest(serde_json::to_vec(&(
            scope.to_string(),
            &base,
            &heads,
            &policy_revision,
            &impact.changes,
            &impact.evidence,
            &impact.owners,
        ))?));
        if let Some(p) = state.proposals.values().find(|p| p.digest == digest) {
            if !readable {
                return Ok(None);
            }
            self.governance_readable(scope, p, who).await?;
            return self
                .review_from(scope, &ledger, &state, p, who)
                .await
                .map(Some);
        }
        let p = Proposal {
            id: uuid::Uuid::new_v4().to_string(),
            branch: branch.into(),
            digest,
            base,
            heads,
            policy_revision,
            required_owners: impact.owners,
            changes: impact.changes,
            evidence: impact.evidence,
            created_by: who.user().into(),
            created_at: self.now(),
            supersedes: supersedes.map(String::from),
        };
        // Readability first: what a conflict would say about the branch is for those who may read it.
        if readable {
            self.governance_readable(scope, &p, who).await?;
        }
        anyhow::ensure!(
            self.branch_diff(scope, branch).await?.conflicts.is_empty(),
            "resolve branch conflicts before proposing"
        );
        self.governance_event(
            scope,
            who,
            Event::Proposal {
                proposal: p.clone(),
            },
            sorted(ledger.heads(MAIN)),
        )
        .await?;
        if !readable {
            return Ok(None);
        }
        self.proposal(scope, &p.id, who).await.map(Some)
    }

    /// Where a proposal stands, from the ledger and the stored events alone: no table is read.
    fn status_of(ledger: &Ledger, state: &State, p: &Proposal) -> Result<Standing> {
        let main = ledger.visible(MAIN);
        let published = p.heads.iter().all(|h| main.contains(h));
        let rejected = state.rejected.contains(&p.id) || state.auto_rejected(p);
        let stale_reason = state.stale_reason(ledger, p)?;
        let status = if published {
            "published"
        } else if rejected {
            "rejected"
        } else if stale_reason.is_some() {
            "stale"
        } else if state.accepted(p) {
            "approved"
        } else {
            "pending_review"
        };
        // When it reached main: the earliest main row that brought a head in (the merge, or the head itself).
        let published_at = published
            .then(|| {
                ledger
                    .rows
                    .iter()
                    .filter(|r| r.branch == MAIN && main.contains(&r.commit_id))
                    .filter(|r| {
                        p.heads.contains(&r.commit_id)
                            || r.parents.iter().any(|x| p.heads.contains(x))
                    })
                    .map(|r| r.recorded_at)
                    .min()
            })
            .flatten();
        let updated_at = state
            .updated
            .get(&p.id)
            .copied()
            .unwrap_or(p.created_at)
            .max(published_at.unwrap_or(i64::MIN));
        Ok(Standing {
            status,
            rejected,
            stale_reason: (status == "stale").then_some(stale_reason).flatten(),
            published_at,
            updated_at,
        })
    }

    /// The review of `p` for a reader who may read it (checked by the caller).
    async fn review_from(
        &self,
        scope: &Scope,
        ledger: &Ledger,
        state: &State,
        p: &Proposal,
        who: &Principal,
    ) -> Result<Review> {
        let s = Self::status_of(ledger, state, p)?;
        Ok(Review {
            proposal: self.with_before(scope, ledger, who, p.clone()).await?,
            approvals: state.approvals.get(&p.id).cloned().unwrap_or_default(),
            rejected: s.rejected,
            status: s.status.into(),
            decisions: state.decisions.get(&p.id).cloned().unwrap_or_default(),
            stale_reason: s.stale_reason,
            published_at: s.published_at,
            updated_at: s.updated_at,
            evidence_details: self.evidence_details(scope, p).await?,
        })
    }

    /// Each fact row with `before`: what main said for its entity and predicate when the owners reviewed it, as
    /// this reader may read it (null when nothing was there, or when its sources are closed to them). That is main
    /// at the proposal's base: a decision needs the base to still be main's, so a pending proposal reads main as
    /// it is now, and a decided one keeps reading what it was decided against, not what it changed main to.
    /// A predicate that holds several values (`many`) is added to, never replaced: `before` is the value itself
    /// when main held it already (unchanged), else null (new), and `held` lists the other values main held.
    /// Computed per reader, never stored: the proposal's rows are the same for everyone, what main holds is not.
    async fn with_before(
        &self,
        scope: &Scope,
        ledger: &Ledger,
        who: &Principal,
        mut p: Proposal,
    ) -> Result<Proposal> {
        let wanted: HashSet<(String, String)> = p
            .changes
            .iter()
            .filter(|c| c["kind"] == "fact")
            .filter_map(|c| {
                Some((
                    c["entity"].as_str()?.to_string(),
                    c["predicate"].as_str()?.to_string(),
                ))
            })
            .collect();
        if wanted.is_empty() {
            return Ok(p);
        }
        let ns = scope.namespace();
        let (vis, rows, closure_rows) = tokio::try_join!(
            self.visibility(scope, MAIN),
            self.rows(&ns, FACTS),
            self.side_rows(&ns, CLOSURES)
        )?;
        // Main at the base: the commits it reached, and its time, past which nothing had ended yet.
        let at_base = ledger.reachable(&p.base);
        let times = ledger.times();
        let base_at = p
            .base
            .iter()
            .filter_map(|c| times.get(c.as_str()).copied())
            .max()
            .unwrap_or(i64::MIN);
        // The closures main had at the base, newest recorded first. One committed later, even with an earlier
        // end, wasn't there when the owners reviewed.
        let mut closed: HashMap<String, Vec<(i64, i64, String)>> = HashMap::new();
        for b in closure_rows.iter() {
            let (commit, fid, ep) = (
                strings(b, "commit_id"),
                strings(b, "fact_id"),
                strings(b, "episode_id"),
            );
            let (end, recorded) = (timestamps(b, "valid_to"), timestamps(b, "recorded_at"));
            for i in 0..b.num_rows() {
                if at_base.contains(&commit[i])
                    && let Some(t) = end[i]
                {
                    closed.entry(fid[i].clone()).or_default().push((
                        recorded[i].unwrap_or(i64::MIN),
                        t,
                        ep[i].clone(),
                    ));
                }
            }
        }
        for v in closed.values_mut() {
            v.sort_by_key(|a| std::cmp::Reverse(a.0));
        }
        // Whether the fact no longer held on main at the base, for this reader: a merge set it aside (it never
        // held, whoever reads), or its stated end or the newest closure they can see came by then (as
        // `Vis::valid_to`, over the closures at the base).
        let ended = |f: &Fact| {
            let list = closed
                .get(&f.fact_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if list.iter().any(|(_, _, e)| vis.merges.contains(e)) {
                return true;
            }
            let end = list
                .iter()
                .find(|(_, _, e)| vis.can_see_episode(e, who))
                .map(|(_, t, _)| *t);
            match (f.valid_to, end) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
            .is_some_and(|t| t <= base_at)
        };
        let schema = vis.schema.clone();
        let many = |entity: &str, predicate: &str| {
            schema.as_deref().is_none_or(|s| {
                s.card(s.entity_type(entity), predicate) == crate::query::schema::Card::Many
            })
        };
        let mut newest: HashMap<(String, String), (i64, String)> = HashMap::new();
        let mut held: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
        for b in rows.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                let key = (f.entity.clone(), f.predicate.clone());
                // `allows` is read now on purpose: what was retracted or forgotten since stays gone, even here.
                if !wanted.contains(&key)
                    || !at_base.contains(&commit)
                    || !vis.allows(&commit, &f, who, None)
                    || ended(&f)
                {
                    continue;
                }
                held.entry(key.clone()).or_default().insert(f.value.clone());
                let e = newest.entry(key).or_insert((i64::MIN, String::new()));
                if f.valid_from >= e.0 {
                    *e = (f.valid_from, f.value.clone());
                }
            }
        }
        for c in p.changes.iter_mut().filter(|c| c["kind"] == "fact") {
            let key = (
                c["entity"].as_str().unwrap_or_default().to_string(),
                c["predicate"].as_str().unwrap_or_default().to_string(),
            );
            if many(&key.0, &key.1) {
                let value = c["value"].as_str().unwrap_or_default().to_string();
                let had = held.get(&key);
                c["before"] = if had.is_some_and(|h| h.contains(&value)) {
                    serde_json::Value::String(value.clone())
                } else {
                    serde_json::Value::Null
                };
                c["many"] = serde_json::Value::Bool(true);
                c["held"] = serde_json::json!(
                    had.into_iter()
                        .flatten()
                        .filter(|v| **v != value)
                        .collect::<Vec<_>>()
                );
            } else {
                c["before"] = newest
                    .get(&key)
                    .map(|(_, v)| serde_json::Value::String(v.clone()))
                    .unwrap_or(serde_json::Value::Null);
            }
        }
        Ok(p)
    }

    /// Each evidence episode's source and time, from the rows the proposal's branch committed.
    async fn evidence_details(&self, scope: &Scope, p: &Proposal) -> Result<Vec<EvidenceRef>> {
        let ns = scope.namespace();
        let vis = self.visibility(scope, &p.branch).await?;
        let mut out: BTreeMap<String, EvidenceRef> = BTreeMap::new();
        for b in self.rows(&ns, EPISODES).await?.rows.iter() {
            let (commit, id, source) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
            );
            let at = timestamps(b, "occurred_at");
            for i in 0..b.num_rows() {
                if vis.committed.contains(&commit[i]) && p.evidence.contains(&id[i]) {
                    out.entry(id[i].clone()).or_insert_with(|| EvidenceRef {
                        id: id[i].clone(),
                        source: source[i].clone(),
                        occurred_at: at[i].unwrap_or_default(),
                        readable: true,
                    });
                }
            }
        }
        Ok(p.evidence
            .iter()
            .map(|id| {
                out.remove(id).unwrap_or_else(|| EvidenceRef {
                    id: id.clone(),
                    source: String::new(),
                    occurred_at: 0,
                    readable: true,
                })
            })
            .collect())
    }

    /// The proposals this reader may see (all of whose evidence they may read), newest activity first;
    /// `entity` keeps only those that change it.
    pub async fn list_proposals(
        &self,
        scope: &Scope,
        who: &Principal,
        entity: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ProposalSummary>> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let mut out = Vec::new();
        for p in state.proposals.values() {
            let entities = entities_of(p);
            if entity.is_some_and(|e| !entities.iter().any(|x| x == e)) {
                continue;
            }
            // A proposal whose evidence the reader can't read is not theirs to know about.
            if self.governance_readable(scope, p, who).await.is_err() {
                continue;
            }
            let s = Self::status_of(&ledger, &state, p)?;
            out.push(ProposalSummary {
                id: p.id.clone(),
                branch: p.branch.clone(),
                digest: p.digest.clone(),
                status: s.status.into(),
                required_owners: p.required_owners.clone(),
                approvals: state.approvals.get(&p.id).cloned().unwrap_or_default(),
                rejected_by: state
                    .decisions
                    .get(&p.id)
                    .and_then(|ds| ds.iter().find(|d| !d.approve))
                    .map(|d| d.user.clone()),
                created_by: p.created_by.clone(),
                created_at: p.created_at,
                updated_at: s.updated_at,
                entities,
                changes: p.changes.len(),
                first: p.changes.iter().find(|c| c["kind"] == "fact").map(|c| {
                    serde_json::json!({"entity": c["entity"], "predicate": c["predicate"],
                        "value": c["value"]})
                }),
                stale_reason: s.stale_reason,
            });
        }
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out.truncate(limit);
        Ok(out)
    }

    /// The entities of main as this reader sees them, linked where a fact names another entity, with
    /// the owners and the open proposals drawn over them.
    pub async fn entity_graph(&self, scope: &Scope, who: &Principal) -> Result<Graph> {
        let ns = scope.namespace();
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let (vis, rows) = tokio::try_join!(self.visibility(scope, MAIN), self.rows(&ns, FACTS))?;
        // Every fact the reader may read, once each, as they see it.
        let mut seen = HashSet::new();
        let mut facts: Vec<Fact> = Vec::new();
        for b in rows.rows.iter() {
            for (commit, f) in decode_facts(b, "source_episodes") {
                if vis.committed.contains(&commit)
                    && vis.allows(&commit, &f, who, None)
                    && seen.insert(f.fact_id.clone())
                {
                    facts.push(vis.annotate(f, who));
                }
            }
        }
        let now = self.now();
        let current = |f: &Fact| f.valid_to.is_none_or(|t| t > now);
        let entities: BTreeSet<String> = facts
            .iter()
            .filter(|f| current(f))
            .flat_map(|f| std::iter::once(f.entity.clone()).chain(f.value_ref.clone()))
            .collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut edges: Vec<GraphEdge> = Vec::new();
        let mut linked: HashSet<(String, String, String)> = HashSet::new();
        for f in facts.iter().filter(|f| current(f)) {
            *counts.entry(f.entity.as_str()).or_default() += 1;
            let to = f
                .value_ref
                .clone()
                .or_else(|| entities.contains(&f.value).then(|| f.value.clone()));
            if let Some(to) = to
                && to != f.entity
                && linked.insert((f.entity.clone(), to.clone(), f.predicate.clone()))
            {
                edges.push(GraphEdge {
                    from: f.entity.clone(),
                    to,
                    predicate: f.predicate.clone(),
                    pending: None,
                });
            }
        }
        // Open proposals the reader may see: what they change, and the links they would add.
        let mut pending: HashMap<String, Vec<String>> = HashMap::new();
        for p in state.proposals.values() {
            if !matches!(
                Self::status_of(&ledger, &state, p)?.status,
                "pending_review" | "approved" | "stale"
            ) || self.governance_readable(scope, p, who).await.is_err()
            {
                continue;
            }
            for e in entities_of(p) {
                pending.entry(e).or_default().push(p.id.clone());
            }
            for c in &p.changes {
                let (Some(from), Some(to)) = (c["entity"].as_str(), c["value"].as_str()) else {
                    continue;
                };
                let predicate = c["predicate"].as_str().unwrap_or_default();
                if entities.contains(to)
                    && from != to
                    && linked.insert((from.into(), to.into(), predicate.into()))
                {
                    edges.push(GraphEdge {
                        from: from.into(),
                        to: to.into(),
                        predicate: predicate.into(),
                        pending: Some(p.id.clone()),
                    });
                }
            }
        }
        let mut all: BTreeSet<String> = entities;
        all.extend(pending.keys().cloned());
        let nodes = all
            .into_iter()
            .map(|id| GraphNode {
                kind: id
                    .split_once(':')
                    .map(|(t, _)| t)
                    .unwrap_or("_")
                    .to_string(),
                owner: state.owners.get(&id).map(|o| o.user.clone()),
                facts: counts.get(id.as_str()).copied().unwrap_or_default(),
                pending: pending.remove(&id).unwrap_or_default(),
                id,
            })
            .collect();
        Ok(Graph { nodes, edges })
    }

    pub async fn proposal(&self, scope: &Scope, id: &str, who: &Principal) -> Result<Review> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let p = state
            .proposals
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no such proposal"))?;
        self.governance_readable(scope, p, who).await?;
        self.review_from(scope, &ledger, &state, p, who).await
    }

    /// Store the authenticated owner's decision. Publishing is a separate idempotent merge, so a lost
    /// HTTP response or a restart after approval can safely retry it through the ordinary merge gate.
    /// `admin`: the caller administers the scope's org (the server checked it). An admin who isn't a required
    /// owner decides for all of them: their approval publishes, their rejection rejects. The decision names them.
    #[allow(clippy::too_many_arguments)]
    pub async fn review_proposal(
        &self,
        scope: &Scope,
        id: &str,
        digest: &str,
        who: &Principal,
        approve: bool,
        note: Option<&str>,
        admin: bool,
    ) -> Result<Review> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let p = state
            .proposals
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no such proposal"))?;
        let user = auth::norm_id(who.user());
        let owner = p.required_owners.contains(&user);
        anyhow::ensure!(
            owner || admin,
            "only a required owner or an org admin may review this proposal"
        );
        let admin = admin && !owner;
        anyhow::ensure!(
            note.is_none_or(|n| n.len() <= 2000),
            "note is too long (2000 bytes at most)"
        );
        anyhow::ensure!(p.digest == digest, "proposal digest changed");
        self.governance_readable(scope, p, who).await?;
        if p.heads.iter().all(|h| ledger.visible(MAIN).contains(h)) {
            return self.review_from(scope, &ledger, &state, p, who).await;
        }
        anyhow::ensure!(
            state.current(&ledger, p)?,
            "proposal is stale; propose the current revision"
        );
        anyhow::ensure!(!state.rejected.contains(id), "proposal was rejected");
        if !approve || !state.approvals.get(id).is_some_and(|a| a.contains(&user)) {
            self.governance_event(
                scope,
                who,
                Event::Decision {
                    proposal: id.into(),
                    digest: digest.into(),
                    user,
                    approve,
                    note: note
                        .filter(|n| !n.trim().is_empty())
                        .map(|n| n.trim().to_string()),
                    admin,
                },
                sorted(ledger.heads(MAIN)),
            )
            .await?;
        }
        let review = self.proposal(scope, id, who).await?;
        if review.status == "approved" {
            self.merge(scope, &review.proposal.branch, who.user())
                .await?;
        }
        self.proposal(scope, id, who).await
    }

    /// Commits `episodes`, `facts` and `closures` on main as `who`, in one commit. Where that changes what an owner
    /// holds (main refuses it, [`OwnerApprovalRequired`]), they are committed on `branch` instead (made if it
    /// isn't there; a retry finds it), proposed by `who` and decided by them at once when they may: a required
    /// owner's approval, or an org admin's (`admin`, checked by the caller), which publishes as soon as it is
    /// enough. Otherwise the proposal waits for its owners. Answers the review where it went through one.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_reviewed(
        &self,
        scope: &Scope,
        who: &Principal,
        admin: bool,
        branch: &str,
        episodes: &[Episode],
        facts: &[Fact],
        closures: &[Closure],
    ) -> Result<Option<Review>> {
        episode_types::check_stored(episodes)?;
        not_held(branch)?;
        anyhow::ensure!(
            branch != MAIN,
            "a reviewed change needs a branch of its own"
        );
        match self
            .commit_full(
                scope,
                MAIN,
                who.user(),
                episodes,
                facts,
                &[],
                closures,
                None,
            )
            .await
        {
            Ok(_) => return Ok(None),
            Err(e) if OwnerApprovalRequired::caused(&e) => {}
            Err(e) => return Err(e),
        }
        // The approval below covers the whole branch, so it must hold only this change: never reuse one.
        let ledger = self.governance_ledger(scope).await?;
        anyhow::ensure!(
            ledger.base(branch).is_none(),
            "branch {branch} already exists; a reviewed change needs a new branch"
        );
        self.make_branch(scope, branch, who.user()).await?;
        self.commit_full(
            scope,
            branch,
            who.user(),
            episodes,
            facts,
            &[],
            closures,
            None,
        )
        .await?;
        let review = self
            .propose_as(scope, branch, who, true, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("proposal made without a review"))?;
        let me = auth::norm_id(who.user());
        if review.status == "published" || !(admin || review.proposal.required_owners.contains(&me))
        {
            return Ok(Some(review));
        }
        let (id, digest) = (review.proposal.id.clone(), review.proposal.digest.clone());
        Ok(Some(
            Box::pin(self.review_proposal(scope, &id, &digest, who, true, None, admin)).await?,
        ))
    }

    /// Publishes what the decisions model approved but didn't reach main (its merge failed after the decision was
    /// recorded), while it is still current and no owner rejected it. Returns how many published.
    pub async fn publish_auto_approved(&self, scope: &Scope) -> Result<usize> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let main = ledger.visible(MAIN);
        let mut n = 0;
        for p in state.proposals.values() {
            if state.auto.get(&p.id) != Some(&true)
                || !state.accepted(p)
                || p.heads.iter().all(|h| main.contains(h))
                || !state.current(&ledger, p)?
            {
                continue;
            }
            match self.merge(scope, &p.branch, AUTO_REVIEWER).await {
                Ok(_) => n += 1,
                Err(e) => eprintln!(
                    "governance: approved {} not published in {scope}: {e:#}",
                    p.id
                ),
            }
        }
        Ok(n)
    }

    /// Proposes again what extraction holds for its owners on a `held-` branch whose proposal went stale (an
    /// owner changed, or main moved over what it changes) before anyone decided it: no person proposed it, so no
    /// one else would, and its change would wait unseen. A branch with a current or published proposal is left
    /// alone, and so is one an owner or the model decided, or one a later branch superseded. A branch main now
    /// conflicts with (another change to the same key published first) is staged again on main as it is
    /// ([`Store::restage_held`]). Returns the branches proposed.
    pub async fn renew_held(&self, scope: &Scope) -> Result<Vec<String>> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let main = ledger.visible(MAIN);
        let retired: HashSet<&str> = state
            .proposals
            .values()
            .filter_map(|p| p.supersedes.as_deref())
            .collect();
        let mut by_branch: BTreeMap<&str, Vec<&Proposal>> = BTreeMap::new();
        for p in state.proposals.values() {
            if p.created_by == EXTRACT_PROPOSER && p.branch.starts_with(HELD_PREFIX) {
                by_branch.entry(&p.branch).or_default().push(p);
            }
        }
        let mut renewed = Vec::new();
        for (branch, ps) in by_branch {
            // Settled: published, rejected, or still current. An approval whose merge never happened and that went
            // stale is not: it is proposed again, for a new decision.
            let settled = retired.contains(branch)
                || ps.iter().any(|p| {
                    p.heads.iter().all(|h| main.contains(h))
                        || state.rejected.contains(&p.id)
                        || state.auto_rejected(p)
                        || state.current(&ledger, p).unwrap_or(true)
                });
            if settled || ledger.base(branch).is_none() {
                continue;
            }
            let r = if self.branch_diff(scope, branch).await?.conflicts.is_empty() {
                self.propose_held(scope, branch, None)
                    .await
                    .map(|_| Some(branch.to_string()))
            } else {
                self.restage_held(scope, branch).await
            };
            match r {
                Ok(Some(b)) => renewed.push(b),
                Ok(None) => {}
                Err(e) => eprintln!("governance: {branch} not proposed again in {scope}: {e:#}"),
            }
        }
        Ok(renewed)
    }

    /// Stages what `held` holds again on a new `held-` branch from main as it is now, for a branch main conflicts
    /// with: for each key both changed, the later statement wins (when its evidence happened). Where `held`'s is
    /// later, the new branch also ends what main holds for the key; where main's is, `held`'s facts for the key are
    /// left out. Its closures of facts main no longer holds are left out too. The new branch is proposed as
    /// superseding `held`, which retires it; when nothing is left, nothing is proposed and `held` stays as it is.
    /// The records of conflicts `held` holds ([`crate::conflicts`]) go with it, each whose two facts the new branch
    /// still reads (one it stages, the other on main or staged too). Returns the new branch.
    async fn restage_held(&self, scope: &Scope, held: &str) -> Result<Option<String>> {
        let ledger = self.governance_ledger(scope).await?;
        let main = ledger.visible(MAIN);
        let own: HashSet<String> = ledger
            .reachable(&ledger.heads(held))
            .difference(&main)
            .cloned()
            .collect();
        let conflicts: HashSet<String> = self
            .branch_diff(scope, held)
            .await?
            .conflicts
            .into_iter()
            .collect();
        let key = |f: &Fact| format!("{}.{}", f.entity, f.predicate);
        let now = self.now();
        let decoded: Vec<(String, Fact)> = self
            .governance_rows(scope, FACTS)
            .await?
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .collect();
        // Main's ends, by fact.
        let mut ended: HashSet<String> = HashSet::new();
        let mut branch_ends: Vec<Closure> = Vec::new();
        let mut closure_commits: HashSet<String> = HashSet::new();
        for b in self.governance_rows(scope, CLOSURES).await?.iter() {
            let (commit, fid, ep) = (
                strings(b, "commit_id"),
                strings(b, "fact_id"),
                strings(b, "episode_id"),
            );
            let end = timestamps(b, "valid_to");
            for i in 0..b.num_rows() {
                closure_commits.insert(commit[i].clone());
                let Some(t) = end[i] else { continue };
                if main.contains(&commit[i]) && t <= now {
                    ended.insert(fid[i].clone());
                } else if own.contains(&commit[i]) {
                    branch_ends.push(Closure {
                        fact_id: fid[i].clone(),
                        valid_to: t,
                        episode_id: ep[i].clone(),
                    });
                }
            }
        }
        let vis = self.visibility(scope, MAIN).await?;
        let current: Vec<&Fact> = decoded
            .iter()
            .filter(|(c, f)| {
                main.contains(c)
                    && !vis.retracted_facts.contains(&f.fact_id)
                    && !ended.contains(&f.fact_id)
                    && f.valid_to.is_none_or(|t| t > now)
            })
            .map(|(_, f)| f)
            .collect();
        let mut facts: Vec<Fact> = decoded
            .iter()
            .filter(|(c, _)| own.contains(c))
            .map(|(_, f)| f.clone())
            .collect();
        // When each side's statement on a conflicting key happened: the latest evidence it cites.
        let mut cited: Vec<String> = facts
            .iter()
            .flat_map(|f| f.source_episodes.clone())
            .collect();
        cited.extend(
            current
                .iter()
                .filter(|f| conflicts.contains(&key(f)))
                .flat_map(|f| f.source_episodes.clone()),
        );
        let times = self.said_at(scope, &cited).await?;
        let said = |f: &Fact| {
            f.source_episodes
                .iter()
                .filter_map(|e| times.get(e).copied())
                .max()
                .unwrap_or(f.valid_from)
        };
        let mut ends: Vec<Closure> = Vec::new();
        for k in &conflicts {
            let ours: Vec<&Fact> = facts.iter().filter(|f| key(f) == *k).collect();
            let theirs: Vec<&&Fact> = current.iter().filter(|f| key(f) == *k).collect();
            let (Some(ours_at), theirs_at) = (
                ours.iter().map(|f| said(f)).max(),
                theirs.iter().map(|f| said(f)).max().unwrap_or(i64::MIN),
            ) else {
                continue;
            };
            if ours_at > theirs_at {
                let by = ours.iter().max_by_key(|f| said(f)).unwrap();
                let (from, episode) = (
                    by.valid_from,
                    by.source_episodes.first().cloned().unwrap_or_default(),
                );
                ends.extend(theirs.iter().map(|f| Closure {
                    fact_id: f.fact_id.clone(),
                    valid_to: from,
                    episode_id: episode.clone(),
                }));
            } else {
                facts.retain(|f| key(f) != *k);
            }
        }
        let current_ids: HashSet<&str> = current.iter().map(|f| f.fact_id.as_str()).collect();
        let kept: HashSet<String> = facts.iter().map(|f| f.fact_id.clone()).collect();
        ends.extend(
            branch_ends
                .into_iter()
                .filter(|c| current_ids.contains(c.fact_id.as_str()) || kept.contains(&c.fact_id)),
        );
        if facts.is_empty() && ends.is_empty() {
            return Ok(None);
        }
        let records = self.held_records(scope, &own, &kept, &current_ids).await?;
        let mut what: Vec<String> = facts
            .iter()
            .map(|f| format!("{}|{}|{}|{}", f.entity, f.predicate, f.value, f.valid_from))
            .chain(
                ends.iter()
                    .map(|c| format!("{}|{}|{}", c.fact_id, c.valid_to, c.episode_id)),
            )
            .collect();
        what.sort();
        what.insert(0, held.to_string());
        let branch = format!(
            "{HELD_PREFIX}{}",
            &auth::hex(&Sha256::digest(what.join(",")))[..12]
        );
        // The engine's extraction wrote it: so do its writes here, which a withdrawal of that extractor takes too.
        let held_vis = self.visibility(scope, held).await?;
        let actor = own
            .iter()
            .find_map(|c| held_vis.actors.get(c).cloned())
            .unwrap_or_else(|| EXTRACT_PROPOSER.to_string());
        if ledger.base(&branch).is_none() {
            self.make_branch(scope, &branch, &actor).await?;
        }
        // Written once: a retry (after a failure between making the branch and writing it) finds it holding its
        // rows, or writes them.
        let ledger = self.governance_ledger(scope).await?;
        let written: HashSet<String> = ledger
            .reachable(&ledger.heads(&branch))
            .difference(&main)
            .cloned()
            .collect();
        // What the branch holds of its own must be what this staging writes, or nothing yet: anything else on it
        // is not proposed as extraction's.
        let on_branch: BTreeSet<&str> = decoded
            .iter()
            .filter(|(c, _)| written.contains(c))
            .map(|(_, f)| f.fact_id.as_str())
            .collect();
        let wanted: BTreeSet<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
        let wrote = !on_branch.is_empty() || closure_commits.iter().any(|c| written.contains(c));
        if !wrote {
            self.commit_full(scope, &branch, &actor, &records, &facts, &[], &ends, None)
                .await?;
        } else {
            anyhow::ensure!(
                on_branch == wanted,
                "{branch} holds other facts than {held} staged again: not proposed"
            );
        }
        self.propose_held(scope, &branch, Some(held)).await?;
        eprintln!(
            "governance: {held} staged again on main as {branch} in {scope}, with {} conflict records",
            records.len()
        );
        Ok(Some(branch))
    }

    /// The records of conflicts committed in `own` (a held branch's own commits) whose two facts are both still
    /// read: one of `kept` (staged again) and the other kept too or `current` on main. Whole, bodies included, to
    /// be committed again with what they name.
    async fn held_records(
        &self,
        scope: &Scope,
        own: &HashSet<String>,
        kept: &HashSet<String>,
        current: &HashSet<&str>,
    ) -> Result<Vec<Episode>> {
        let read = |f: &str| kept.contains(f) || current.contains(f);
        let mut found: HashMap<String, Episode> = HashMap::new();
        for b in self.governance_rows(scope, EPISODES).await?.iter() {
            let (commit, id, source, acl) = (
                strings(b, "commit_id"),
                strings(b, "episode_id"),
                strings(b, "source"),
                strings(b, "acl"),
            );
            let (occurred, document) = (
                timestamps(b, "occurred_at"),
                optional_strings(b, "document"),
            );
            for i in 0..b.num_rows() {
                let Some((x, y)) = document[i]
                    .as_deref()
                    .filter(|_| {
                        own.contains(&commit[i]) && source[i].starts_with(crate::conflicts::SOURCE)
                    })
                    .and_then(crate::conflicts::named)
                else {
                    continue;
                };
                if !(kept.contains(x) || kept.contains(y)) || !read(x) || !read(y) {
                    continue;
                }
                found.entry(id[i].clone()).or_insert_with(|| Episode {
                    episode_id: id[i].clone(),
                    source: source[i].clone(),
                    occurred_at: occurred[i].unwrap_or_default(),
                    acl: split_list(&acl[i]),
                    about: vec![],
                    index_text: false,
                    body: vec![],
                    recorded_at: None,
                    document: document[i].clone(),
                });
            }
        }
        if found.is_empty() {
            return Ok(vec![]);
        }
        let mut bodies = self
            .episode_bodies(scope, found.keys().cloned().collect())
            .await?;
        let mut out: Vec<Episode> = found
            .into_values()
            .filter_map(|mut e| {
                e.body = bodies.remove(&e.episode_id)?;
                Some(e)
            })
            .collect();
        out.sort_by(|a, b| a.episode_id.cmp(&b.episode_id));
        Ok(out)
    }

    /// When each of `ids` (committed on main) was said: its `occurred_at`, but never later than the commit that
    /// brought it in. `occurred_at` is the writer's to set, so a date in the future can't make an episode outrank
    /// what was said after it was written.
    async fn said_at(&self, scope: &Scope, ids: &[String]) -> Result<HashMap<String, i64>> {
        let ledger = self.governance_ledger(scope).await?;
        let times = ledger.times();
        let main = ledger.visible(MAIN);
        let mut out: HashMap<String, i64> = HashMap::new();
        for b in self.governance_rows(scope, EPISODES).await?.iter() {
            let (commit, id) = (strings(b, "commit_id"), strings(b, "episode_id"));
            let at = timestamps(b, "occurred_at");
            for i in 0..b.num_rows() {
                if !main.contains(&commit[i]) || !ids.contains(&id[i]) {
                    continue;
                }
                let Some(&written) = times.get(commit[i].as_str()) else {
                    continue;
                };
                let t = at[i].unwrap_or(written).min(written);
                out.entry(id[i].clone())
                    .and_modify(|e| *e = (*e).min(t))
                    .or_insert(t);
            }
        }
        Ok(out)
    }

    /// Proposes held branch `branch` as extraction, or, when nothing on it needs an owner any more (what it changed
    /// of an owned entity main already changed the same way, or its owner left), merges it: extraction would have
    /// landed it on main itself.
    async fn propose_held(
        &self,
        scope: &Scope,
        branch: &str,
        supersedes: Option<&str>,
    ) -> Result<()> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let main = ledger.visible(MAIN);
        let new: HashSet<String> = ledger
            .reachable(&ledger.heads(branch))
            .difference(&main)
            .cloned()
            .collect();
        let unowned = self
            .governance_impact(scope, &new, &main, &state)
            .await?
            .owners
            .is_empty();
        if unowned && supersedes.is_none() {
            self.merge(scope, branch, EXTRACT_PROPOSER).await?;
            eprintln!("governance: {branch} needs no owner now; merged in {scope}");
            return Ok(());
        }
        let proposer = Principal::new(EXTRACT_PROPOSER, &[]);
        self.propose_as(scope, branch, &proposer, false, supersedes)
            .await?;
        // Recorded to retire what it supersedes; nothing on it waits for anyone.
        if unowned {
            self.merge(scope, branch, EXTRACT_PROPOSER).await?;
            eprintln!("governance: {branch} needs no owner now; merged in {scope}");
        }
        Ok(())
    }

    /// The proposals in `scope` a decisions model is to decide: where its org has `auto_review` on, those that are
    /// current and that neither an owner nor the model has decided yet. Empty elsewhere.
    pub async fn awaiting_auto_review(&self, scope: &Scope) -> Result<Vec<AutoCase>> {
        let Some(org) = scope.org_name() else {
            return Ok(vec![]);
        };
        if !self.directory().await?.dir.auto_review(org) {
            return Ok(vec![]);
        }
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let main = ledger.visible(MAIN);
        let vis = self.visibility(scope, MAIN).await?;
        let retired: HashSet<&str> = state
            .proposals
            .values()
            .filter_map(|p| p.supersedes.as_deref())
            .collect();
        let mut out = Vec::new();
        for p in state.proposals.values() {
            if retired.contains(p.branch.as_str()) {
                continue;
            }
            let decided = state.auto.contains_key(&p.id)
                || state.rejected.contains(&p.id)
                || state.approvals.get(&p.id).is_some_and(|a| !a.is_empty());
            if decided || p.heads.iter().all(|h| main.contains(h)) || !state.current(&ledger, p)? {
                continue;
            }
            out.push(AutoCase {
                id: p.id.clone(),
                digest: p.digest.clone(),
                branch: p.branch.clone(),
                created_by: p.created_by.clone(),
                changes: p.changes.clone(),
                evidence: p
                    .evidence
                    .iter()
                    .filter(|e| !vis.is_gone(e))
                    .cloned()
                    .collect(),
                said_at: 0,
            });
        }
        let ids: Vec<String> = out.iter().flat_map(|c| c.evidence.clone()).collect();
        let times = self.said_at(scope, &ids).await?;
        for c in &mut out {
            c.said_at = c
                .evidence
                .iter()
                .filter_map(|e| times.get(e).copied())
                .max()
                .unwrap_or(0);
        }
        Ok(out)
    }

    /// Records a decisions model's decision on proposal `id` at `digest`, and publishes it at once when it approves.
    /// Only where the org has `auto_review` on, on a current proposal nobody has decided. An owner may still approve
    /// what the model rejected; what it approved is published, and an owner corrects it in memory like any other
    /// fact. Called by the server's review loop alone: no request reaches it. Returns the proposal's status after.
    pub async fn auto_review(
        &self,
        scope: &Scope,
        id: &str,
        digest: &str,
        approve: bool,
        model: &str,
        confidence: f64,
    ) -> Result<&'static str> {
        let org = scope
            .org_name()
            .ok_or_else(|| anyhow::anyhow!("only an org's reviews are decided by a model"))?;
        anyhow::ensure!(
            self.directory().await?.dir.auto_review(org),
            "{org} has its reviews decided by its owners, not a model"
        );
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let p = state
            .proposals
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no such proposal"))?;
        anyhow::ensure!(p.digest == digest, "proposal digest changed");
        let main = ledger.visible(MAIN);
        anyhow::ensure!(
            !p.heads.iter().all(|h| main.contains(h)),
            "proposal is published"
        );
        anyhow::ensure!(
            state.current(&ledger, p)?,
            "proposal is stale; propose the current revision"
        );
        anyhow::ensure!(
            !state.auto.contains_key(id)
                && !state.rejected.contains(id)
                && state.approvals.get(id).is_none_or(|a| a.is_empty()),
            "proposal is already decided"
        );
        let reviewer = Principal::new(AUTO_REVIEWER, &[]);
        self.governance_event(
            scope,
            &reviewer,
            Event::AutoDecision {
                proposal: id.into(),
                digest: digest.into(),
                approve,
                model: model.into(),
                confidence,
            },
            sorted(ledger.heads(MAIN)),
        )
        .await?;
        if !approve {
            return Ok("rejected");
        }
        let branch = p.branch.clone();
        self.merge(scope, &branch, AUTO_REVIEWER).await?;
        Ok("published")
    }

    /// Called inside both ledger CAS loops, after rows are staged but before they become visible.
    /// Whether commits `ids`, appended to main together on its current heads, change nothing an entity owner holds:
    /// then none of them needs a proposal ([`Store::authorize_publication`]). One check for a whole group commit.
    pub(crate) async fn publications_untouched(
        &self,
        scope: &Scope,
        ids: &[String],
        ledger: &Ledger,
    ) -> Result<bool> {
        let state = self.governance_state(scope, ledger).await?;
        if state.owners.is_empty() {
            return Ok(true);
        }
        let main = ledger.visible(MAIN);
        let new: HashSet<String> = ids.iter().cloned().collect();
        let checked = Instant::now();
        let impact = self.governance_impact(scope, &new, &main, &state).await?;
        if checked.elapsed() > Duration::from_secs(1) {
            eprintln!(
                "publication check of {} commits in {scope} took {} ms ({} owned changes)",
                new.len(),
                checked.elapsed().as_millis(),
                impact.changes.len()
            );
        }
        Ok(impact.owners.is_empty())
    }

    pub(crate) async fn authorize_publication(
        &self,
        scope: &Scope,
        commit: &str,
        parents: &[String],
        ledger: &Ledger,
    ) -> Result<()> {
        let state = self.governance_state(scope, ledger).await?;
        if state.owners.is_empty() {
            return Ok(());
        }
        let main = ledger.visible(MAIN);
        let mut new: HashSet<_> = ledger
            .reachable(parents)
            .difference(&main)
            .cloned()
            .collect();
        new.insert(commit.into());
        let checked = Instant::now();
        let impact = self.governance_impact(scope, &new, &main, &state).await?;
        // It runs inside the ledger's compare-and-set: a slow check is a slow (and retried) commit.
        if checked.elapsed() > Duration::from_secs(1) {
            eprintln!(
                "publication check of {} commits in {scope} took {} ms ({} owned changes)",
                new.len(),
                checked.elapsed().as_millis(),
                impact.changes.len()
            );
        }
        if impact.owners.is_empty() {
            return Ok(());
        }
        for p in state.proposals.values() {
            if !state.accepted(p) || !state.current(ledger, p)? {
                continue;
            }
            let allowed: HashSet<_> = ledger
                .reachable(&p.heads)
                .difference(&main)
                .cloned()
                .collect();
            let proposed = self
                .governance_impact(scope, &allowed, &main, &state)
                .await?;
            // The merge commit itself must add no new protected rows (including resolutions).
            if new.iter().all(|c| c == commit || allowed.contains(c))
                && allowed.is_subset(&new)
                && impact.changes == proposed.changes
                && impact.owners == p.required_owners
                && proposed.changes == p.changes
            {
                return Ok(());
            }
        }
        Err(OwnerApprovalRequired.into())
    }

    /// An extraction main refused ([`OwnerApprovalRequired`]): what it read about owned entities waits for their
    /// owners, and the rest lands. The facts and closures of each episode a protected fact cites (shared evidence
    /// carries ownership, as [`Store::authorize_publication`] counts it) go to a branch named for those
    /// episodes, with a proposal for its owners made by [`EXTRACT_PROPOSER`]; the other facts, and every
    /// episode's read, commit to main, so the extractor doesn't read the batch again. The engine's `records` of
    /// what reconcile found ([`crate::conflicts`]) go where the facts they name go: one naming a held fact waits on
    /// the branch with it, so it reaches main only when that fact is published, and never if it is rejected. The
    /// branch is written first: a failure before main's commit leaves the episodes unread, and the next run lands on
    /// the same branch (and the same proposal) instead of a new one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn stage_extracted(
        &self,
        scope: &Scope,
        actor: &str,
        facts: &[Fact],
        extractions: &[Extraction],
        closures: &[Closure],
        records: &[Episode],
        adds: Option<&schema::Schema>,
    ) -> Result<String> {
        let ledger = self.governance_ledger(scope).await?;
        let state = self.governance_state(scope, &ledger).await?;
        let main = ledger.visible(MAIN);
        let decoded: Vec<_> = self
            .governance_rows(scope, FACTS)
            .await?
            .iter()
            .flat_map(|b| decode_facts(b, "source_episodes"))
            .collect();
        let mut versions: HashMap<&str, Vec<&Fact>> = HashMap::new();
        for f in decoded.iter().map(|(_, f)| f).chain(facts) {
            versions.entry(f.fact_id.as_str()).or_default().push(f);
        }
        // The episodes one person alone reads: what only they read needs no owner ([`private_episodes`]).
        let private = private_episodes(&acl_states(
            &self.governance_rows(scope, EPISODES).await?,
            |c| main.contains(c),
        ));
        let evidence = evidence_owners(
            &state,
            &private,
            decoded
                .iter()
                .filter(|(c, _)| main.contains(c))
                .map(|(_, f)| f)
                .chain(facts),
        );
        let protected = |id: &str| {
            versions.get(id).is_some_and(|vs| {
                vs.iter()
                    .any(|f| !required_for(&state, &private, &evidence, f).is_empty())
            })
        };
        // The episodes held back: every one a protected fact or closure cites.
        let mut held: BTreeSet<String> = BTreeSet::new();
        for f in facts.iter().filter(|f| protected(&f.fact_id)) {
            held.extend(f.source_episodes.iter().cloned());
        }
        let ends_shared = |c: &Closure| !private.contains(&c.episode_id) && protected(&c.fact_id);
        for c in closures.iter().filter(|c| ends_shared(c)) {
            held.insert(c.episode_id.clone());
        }
        let waits =
            |f: &Fact| protected(&f.fact_id) || f.source_episodes.iter().any(|e| held.contains(e));
        let ends = |c: &Closure| ends_shared(c) || held.contains(&c.episode_id);
        let (staged, landed): (Vec<Fact>, Vec<Fact>) =
            facts.iter().cloned().partition(|f| waits(f));
        let (staged_ends, landed_ends): (Vec<Closure>, Vec<Closure>) =
            closures.iter().cloned().partition(|c| ends(c));
        anyhow::ensure!(
            !staged.is_empty() || !staged_ends.is_empty(),
            "owner approval required, though no fact of this extraction is protected"
        );
        let held_facts: HashSet<&str> = staged.iter().map(|f| f.fact_id.as_str()).collect();
        let (staged_records, landed_records): (Vec<Episode>, Vec<Episode>) =
            records.iter().cloned().partition(|r| {
                crate::conflicts::record_of(r)
                    .is_some_and(|(a, b)| held_facts.contains(a) || held_facts.contains(b))
            });
        // Named for what it holds (the episodes, and the facts and closures read from them), so a retry of this
        // write finds it again, and a later read of the same episodes that says something else gets its own.
        let mut what: Vec<String> = held.iter().cloned().collect();
        what.extend(
            staged
                .iter()
                .map(|f| format!("{}|{}|{}|{}", f.entity, f.predicate, f.value, f.valid_from)),
        );
        what.extend(
            staged_ends
                .iter()
                .map(|c| format!("{}|{}|{}", c.fact_id, c.valid_to, c.episode_id)),
        );
        let branch = format!(
            "{HELD_PREFIX}{}",
            &auth::hex(&Sha256::digest(what.join(",")))[..12]
        );
        let mut reads: Vec<Extraction> = extractions
            .iter()
            .filter(|x| held.contains(&x.episode_id))
            .cloned()
            .collect();
        if reads.is_empty() {
            reads = extractions.to_vec();
        }
        if ledger.base(&branch).is_none() {
            self.make_branch(scope, &branch, actor).await?;
        }
        // Written once (a second commit would void the proposal made for the first): a retry finds the branch
        // already holding these reads.
        let extractor = &reads[0].extractor;
        let written = self.extracted_on(scope, &branch, extractor).await?;
        if !reads.iter().all(|x| written.contains(&x.episode_id)) {
            anyhow::ensure!(!reads.is_empty(), "nothing extracted");
            self.commit_full(
                scope,
                &branch,
                actor,
                &staged_records,
                &staged,
                &reads,
                &staged_ends,
                adds,
            )
            .await?;
        }
        // A proposal that can't be made now (a conflict with main, say) leaves the branch for an admin in Branches;
        // the read still lands, so the extractor moves on.
        let proposer = Principal::new(EXTRACT_PROPOSER, &[]);
        if let Err(e) = self
            .propose_as(scope, &branch, &proposer, false, None)
            .await
        {
            eprintln!("held {branch} in {scope} for its owners, without a proposal: {e:#}");
        }
        let id = match self
            .commit_full(
                scope,
                MAIN,
                actor,
                &landed_records,
                &landed,
                extractions,
                &landed_ends,
                adds,
            )
            .await
        {
            // The guard holds more than this split counted: the rest waits on the branch too (its proposal made
            // again, the first one stale), its records with it, and main takes the reads alone, which no owner
            // holds.
            Err(e)
                if OwnerApprovalRequired::caused(&e)
                    && !(landed.is_empty() && landed_ends.is_empty()) =>
            {
                self.commit_full(
                    scope,
                    &branch,
                    actor,
                    &landed_records,
                    &landed,
                    extractions,
                    &landed_ends,
                    adds,
                )
                .await?;
                if let Err(e) = self
                    .propose_as(scope, &branch, &proposer, false, None)
                    .await
                {
                    eprintln!("held {branch} in {scope} for its owners, without a proposal: {e:#}");
                }
                self.commit_full(scope, MAIN, actor, &[], &[], extractions, &[], adds)
                    .await?
            }
            r => r?,
        };
        eprintln!(
            "held {} facts, {} closures and {} conflict records of {} episodes in {scope} on {branch} for their owners",
            staged.len(),
            staged_ends.len(),
            staged_records.len(),
            held.len()
        );
        Ok(id)
    }
}

/// The rows of the facts table `rows` that `keep` selects, decoded. `keep` reads each row's commit, fact id, the
/// entities it names (subject, value, value reference) and its evidence (comma-joined) as they lie in the column,
/// so an owner check scans a scope's whole history without decoding it and decodes only the facts it weighs: in a
/// scope of 450k facts, decoding them all on every publication took minutes and gigabytes.
fn select_facts(
    rows: &[RecordBatch],
    keep: impl Fn(&str, &str, [Option<&str>; 3], &str) -> bool,
) -> Result<Vec<(String, Fact)>> {
    let mut out = vec![];
    for b in rows {
        let col = |n: &str| b.column_by_name(n).map(|c| c.as_string::<i32>());
        let (Some(commit), Some(id), Some(entity), Some(value), Some(sources)) = (
            col("commit_id"),
            col("fact_id"),
            col("entity"),
            col("value"),
            col("source_episodes"),
        ) else {
            continue;
        };
        let reference = col("value_ref");
        let mask: arrow_array::BooleanArray = (0..b.num_rows())
            .map(|i| {
                let reference = reference.filter(|r| !r.is_null(i)).map(|r| r.value(i));
                Some(keep(
                    commit.value(i),
                    id.value(i),
                    [Some(entity.value(i)), Some(value.value(i)), reference],
                    sources.value(i),
                ))
            })
            .collect();
        if mask.true_count() == 0 {
            continue;
        }
        let kept = arrow_select::filter::filter_record_batch(b, &mask)?;
        out.extend(decode_facts(&kept, "source_episodes"));
    }
    Ok(out)
}

/// A text column of `b`, if it has one: read in place, a row at a time, never copied whole.
fn text_column<'a>(b: &'a RecordBatch, name: &str) -> Option<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
}

/// Row `i` of text column `c`, if it has the column and the row a value.
fn cell(c: Option<&StringArray>, i: usize) -> Option<&str> {
    c.filter(|c| !c.is_null(i)).map(|c| c.value(i))
}
