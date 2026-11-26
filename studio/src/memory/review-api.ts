// Owner review: the transport's `review`, memstore's governance ops on `/admin` (docs/ownership.md). memstore answers `{org, scope, result}`; `result` is what each op
// returns and what the page reads.
import { call } from "./api";
import type { Selection } from "./types";
import type { Graph } from "./graphTypes";

export type { Graph, GraphEdge, GraphNode } from "./graphTypes";

/** Who owns an entity: every change to it waits for them. `since` is µs, as memstore's times are. */
export interface Owner {
  entity: string;
  user: string;
  /** The ledger revision that set it; a transfer or removal names it so two changes can't race. */
  revision: string;
  since?: number;
}

export type ProposalStatus = "pending_review" | "approved" | "published" | "rejected" | "stale";

/** One protected row a proposal would publish, as the engine records it (a fact, a quote, a closure…). */
export interface Change {
  kind: string;
  id?: string;
  entity?: string;
  predicate?: string;
  value?: string | null;
  /**
   * What main said when the owners reviewed it (null: nothing, the row is new). For a predicate that holds
   * several values (`many`), the value itself when main held it already, else null: it adds, never replaces.
   */
  before?: string | null;
  /** The predicate holds several values: this row adds one. */
  many?: boolean;
  /** For `many`: the other values main held. */
  held?: string[];
  valid_from?: number;
  valid_to?: number | null;
  evidence?: string[];
  /** A schema change (`kind` `schemas`): what moving main's schema (`before`) to it (`value`) breaks for facts
   * already written, and what it adds. */
  breaking?: string[];
  adds?: string[];
  [k: string]: unknown;
}

export interface Decision {
  user: string;
  approve: boolean;
  at?: number;
  /** Why, for a rejection. */
  note?: string;
  /** An org admin who isn't a required owner decided it, for every owner. */
  admin?: boolean;
}

/** An episode a proposal rests on; `readable` false means the reader may know it exists, not its words. */
export interface EvidenceRef {
  id: string;
  source?: string;
  occurred_at?: number;
  readable: boolean;
  excerpt?: string;
}

export interface ProposalSummary {
  id: string;
  branch: string;
  digest: string;
  status: ProposalStatus;
  required_owners: string[];
  approvals: string[];
  rejected_by?: string;
  created_by: string;
  created_at?: number;
  updated_at?: number;
  entities: string[];
  changes: number;
  /** Its first fact change, so two proposals on one entity read apart; with none, its schema change
   * (`predicate` `schema`, `value` the schema it moves to). */
  first?: { entity?: string; predicate?: string; value?: string | null };
  stale_reason?: "draft_changed" | "base_changed" | "ownership_changed";
}

/** `get_proposal`: the immutable proposal and where its review stands. */
export interface Review {
  proposal: {
    id: string;
    branch: string;
    /** The exact revision a reviewer approves: scope, base, heads, policy and changes hashed. */
    digest: string;
    base: string[];
    heads: string[];
    policy_revision: string;
    required_owners: string[];
    changes: Change[];
    evidence: string[];
    created_by: string;
    created_at?: number;
  };
  approvals: string[];
  rejected: boolean;
  status: ProposalStatus;
  decisions?: Decision[];
  evidence_details?: EvidenceRef[];
  stale_reason?: ProposalSummary["stale_reason"];
  published_at?: number;
}

/** One side of a conflict: an open fact, where it came from, and what it quotes. */
export interface ConflictSide {
  fact_id: string;
  value: string;
  /** When it became true, µs. */
  since: number;
  since_day: string;
  quote?: string | null;
  episode_id?: string | null;
  /** The account it comes from, when it was given one. */
  source?: string | null;
}

/** Two open facts on one entity's single-valued attribute that disagree, from different sources. */
export interface Conflict {
  entity: string;
  predicate: string;
  owner: string | null;
  /** The caller is the entity's owner or an org admin. */
  can_resolve: boolean;
  /** The older first. */
  facts: [ConflictSide, ConflictSide];
}

/** `list_conflicts`: a page, how many there are in all, and where the next page starts (none: the last). */
export interface ConflictPage {
  conflicts: Conflict[];
  total: number;
  next: string | null;
}

/** A conflict as a list's row shows it (`view: "rows"`): opening it reads it in full (`review.conflict`). */
export interface ConflictRow {
  entity: string;
  predicate: string;
  /** Its first two values, the older first, cut short. */
  values: string[];
  /** How many values disagree. */
  count: number;
}

/** A page of conflict rows, how many there are in all, and where the next page starts (none: the last). */
export interface ConflictRowPage {
  rows: ConflictRow[];
  total: number;
  next: string | null;
}

/** `check_conflicts`: what it looked at and what it recorded. */
export interface ConflictCheck {
  checked: number;
  recorded: number;
  conflicts: {
    entity: string;
    predicate: string;
    facts: [string, string];
    values: [string, string];
  }[];
  /** False while the org's `conflicts` setting is off: searches and agents don't flag them. */
  marked_in_reads: boolean;
}

/** `resolve_conflict`: the fact kept or added, those ended, and whether it reached main yet. */
export interface Resolution {
  kept: string | null;
  added: string | null;
  ended: string[];
  episode_id?: string | null;
  /** `published`, or a review status while other owners must approve too. */
  status: ProposalStatus | string;
  review?: Review | null;
}

/** `add_fact`: the fact, its episode, and its review (none when it went straight to main). */
export interface AddedFact {
  fact_id: string;
  episode_id: string;
  review: Review | null;
}

export interface NewFact {
  entity: string;
  predicate: string;
  value: string;
  note?: string;
  source?: string;
}

/** What a dream decision did: ended a replaced fact, linked two that say the same, named a thing, or (the
 * server's own) shared with the team what several people said. */
export type DreamKind = "end" | "link" | "name" | "share";
/** `applied`: in force; `waiting`: the dream was unsure and waits for a builder; `undone` / `dismissed`: a builder
 * took it back (the dream won't make it again); `released`: the dream let it go (what it rested on is gone). */
export type DreamStatus = "applied" | "waiting" | "undone" | "dismissed" | "released";

/** A fact a decision rests on, with its part in it (`old`/`by` for an ending, `keep`/`dup` for a link,
 * `alias`/`name` for a name). */
export interface DreamFact {
  role: "old" | "by" | "keep" | "dup" | "alias" | "name";
  id: string;
  entity: string;
  predicate: string;
  value: string;
  valid_from: number;
}

export interface DreamDecision {
  id: string;
  kind: DreamKind;
  status: DreamStatus;
  /** What it did (or, waiting, would do), in plain words. */
  summary: string;
  /** The model's own reason, where one was kept; for a share, how many people said it. */
  why: string | null;
  /** When it was decided, and the pass that decided it (µs). */
  at: number;
  pass: number | null;
  /** The dreamer (`dream:<model>@<version>`), or the server for a share. */
  by: string;
  audience: string;
  facts: DreamFact[];
  /** The builder who last applied, undid or dismissed it. */
  reviewed: { action: "apply" | "undo" | "dismiss"; by: string; at: number } | null;
  can_undo: boolean;
  can_apply: boolean;
  can_dismiss: boolean;
}

export interface DreamPass {
  at: number;
  ended: number;
  linked: number;
  named: number;
  waiting: number;
}

export interface DreamList {
  decisions: DreamDecision[];
  next_page: string | null;
  /** How many the filters keep (a server before it answers none). */
  total?: number;
  /** The scope's decisions in all, whatever the filters; `applied_week` those applied in the last seven days (a
   * server before it answers none). */
  counts: { waiting: number; applied: number; applied_week?: number; history: number };
  /** The newest pass that decided something. */
  last_pass: DreamPass | null;
  /** When a pass last ran at all, a change or not (its last commit), whichever process ran it, and the pass it was
   * (its start, as `last_pass.at` names one; null where its records name none). A server before it answers none. */
  last_run?: { at: number; pass: number | null } | null;
  /** The memory the dream ran over (`org:acme`, `team:acme/eng`). */
  scope?: string;
  dreaming: {
    /** A builder may start a pass here (a dreaming model, and passes run by this server). */
    available: boolean;
    running: boolean;
    /** The last pass this server ran, with its counts (a pass that changed nothing included). */
    last: { at: number; counts: Record<string, number | string> } | null;
    failed: { at: number; error: string } | null;
  };
}

type Answer<T> = { org: string; scope: string; result: T };

async function op<T>(sel: Selection, body: Record<string, unknown>): Promise<T> {
  const r = await call<Answer<T>>(sel, "review", body);
  return r.result;
}

export const review = {
  owners: (sel: Selection) => op<Owner[]>(sel, { op: "entity_owners" }),
  setOwner: (sel: Selection, entity: string, owner: string, revision?: string) =>
    op<Owner>(sel, { op: "set_entity_owner", entity, owner, revision }),
  removeOwner: (sel: Selection, entity: string, revision: string) =>
    op<unknown>(sel, { op: "remove_entity_owner", entity, revision }),
  proposals: (sel: Selection, entity?: string) =>
    op<{ proposals: ProposalSummary[] }>(sel, { op: "list_proposals", entity }).then(
      (r) => r.proposals,
    ),
  proposal: (sel: Selection, proposal: string) => op<Review>(sel, { op: "get_proposal", proposal }),
  decide: (
    sel: Selection,
    proposal: string,
    digest: string,
    decision: "approve" | "reject",
    note?: string,
  ) => op<Review>(sel, { op: "review_proposal", proposal, digest, decision, note }),
  /** A stale proposal, proposed again from its branch as it stands now. */
  repropose: (sel: Selection, branch: string) => op<Review>(sel, { op: "propose_branch", branch }),
  /** At most `limit` entities, keeping `entity` among them; `total` is how many there are in all. */
  graph: (sel: Selection, opts: { limit?: number; entity?: string } = {}) =>
    op<Graph>(sel, { op: "entity_graph", ...opts }),
  /** An admin's fact, as a person would state it; on an owned entity it goes through review. */
  addFact: (sel: Selection, f: NewFact) => op<AddedFact>(sel, { op: "add_fact", ...f }),
  /** An admin's check, now rather than on the next schedule. */
  checkConflicts: (sel: Selection) => op<ConflictCheck>(sel, { op: "check_conflicts" }),
  /** A page of conflicts (`limit`, at most 200), the next after `after` (the last page's `next`), with the total. */
  conflicts: (
    sel: Selection,
    opts: { entity?: string; mine?: boolean; limit?: number; after?: string } = {},
  ) =>
    op<{ conflicts: Conflict[]; total?: number; next?: string | null }>(sel, {
      op: "list_conflicts",
      ...opts,
    }).then(
      // A server before pages answers every conflict, with no total.
      (r): ConflictPage => ({
        conflicts: r.conflicts,
        total: r.total ?? r.conflicts.length,
        next: r.next ?? null,
      }),
    ),
  /**
   * A page of conflicts as a list shows them: only what a row shows. A server before `view` answers them in full,
   * which is read the same way.
   */
  conflictRows: (
    sel: Selection,
    opts: { mine?: boolean; limit?: number; after?: string } = {},
  ) =>
    op<{
      conflicts: (ConflictRow | Conflict)[];
      total?: number;
      next?: string | null;
    }>(sel, { op: "list_conflicts", view: "rows", ...opts }).then(
      (r): ConflictRowPage => ({
        rows: r.conflicts.map((c) =>
          "values" in c
            ? c
            : {
                entity: c.entity,
                predicate: c.predicate,
                values: c.facts.slice(0, 2).map((f) => f.value),
                count: c.facts.length,
              },
        ),
        total: r.total ?? r.conflicts.length,
        next: r.next ?? null,
      }),
    ),
  /** How many conflicts there are (only those on what the caller owns, with `mine`). */
  conflictCount: (sel: Selection, opts: { mine?: boolean } = {}) =>
    op<{ total?: number; conflicts?: unknown[] }>(sel, {
      op: "list_conflicts",
      view: "count",
      // A server before `view` answers one conflict in full beside the total.
      limit: 1,
      ...opts,
    }).then((r) => r.total ?? r.conflicts?.length ?? 0),
  /** One thing's conflict over one attribute, in full: what a row opens to. None once it is settled. */
  conflict: (sel: Selection, entity: string, predicate: string) =>
    op<{ conflicts: Conflict[] }>(sel, {
      op: "list_conflicts",
      entity,
      predicate,
      limit: 200,
    }).then((r) => r.conflicts.find((c) => c.predicate === predicate) ?? null),
  /** Keep one of the two facts, or state the right value: exactly one of `keep` and `value`. */
  resolve: (sel: Selection, facts: [string, string], how: { keep: string } | { value: string }) =>
    op<Resolution>(sel, { op: "resolve_conflict", facts, ...how }),
  /**
   * The dream's decisions (an org admin's), newest first: those resting on a fact about `entity`, and decided at
   * `since` (µs) or later, where given.
   */
  dream: (
    sel: Selection,
    opts: {
      status?: DreamStatus | DreamStatus[];
      kind?: DreamKind;
      entity?: string;
      since?: number;
      limit?: number;
      after?: string;
    } = {},
  ) => op<DreamList>(sel, { op: "dream_decisions", ...opts }),
  undoDream: (sel: Selection, id: string) => op<DreamDecision>(sel, { op: "undo_dream_decision", id }),
  applyDream: (sel: Selection, id: string) =>
    op<DreamDecision>(sel, { op: "apply_dream_decision", id }),
  dismissDream: (sel: Selection, id: string) =>
    op<DreamDecision>(sel, { op: "dismiss_dream_decision", id }),
  /** A pass over this memory now, in the background. */
  dreamNow: (sel: Selection) =>
    op<{ started: boolean; running: boolean }>(sel, { op: "dream_now" }),
};

export const STATUS_WORD: Record<ProposalStatus, string> = {
  pending_review: "needs review",
  approved: "approved",
  published: "published",
  rejected: "rejected",
  stale: "stale",
};

/** A badge class per status: green for what reached main, red for a rejection, amber for stale. */
export const STATUS_BADGE: Record<ProposalStatus, string> = {
  pending_review: "badge-accent",
  approved: "badge-ok",
  published: "badge-ok",
  rejected: "badge-error",
  stale: "rv-badge-warn",
};

/** The proposals waiting on this person: required of them, not yet decided by them. */
export const needsMe = (ps: ProposalSummary[], me: string | undefined) =>
  me
    ? ps.filter(
        (p) =>
          p.status === "pending_review" &&
          p.required_owners.includes(me) &&
          !p.approvals.includes(me),
      )
    : [];

/** A line for a proposal, from what it changes: the engine keeps rows, not titles. */
export function titleOf(p: {
  entities: string[];
  changes: number;
  first?: { predicate?: string; value?: string | null };
}): string {
  const [first, ...rest] = p.entities;
  if (!first && p.first?.predicate === "schema") return `Schema change to ${p.first.value}`;
  if (!first) return `${p.changes} ${p.changes === 1 ? "change" : "changes"}`;
  const where = rest.length ? `${first} and ${rest.length} more` : first;
  return `${p.changes} ${p.changes === 1 ? "change" : "changes"} to ${where}`;
}

/** An entity id as the Explorer addresses it. */
export const entityFocus = (entity: string) => {
  const i = entity.indexOf(":");
  return {
    kind: "entity" as const,
    path: i < 0 ? ["entities", entity] : ["entities", entity.slice(0, i), entity.slice(i + 1)],
  };
};

export const typeOf = (entity: string) => entity.slice(0, Math.max(0, entity.indexOf(":")));
