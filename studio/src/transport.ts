import type { Selection } from "./memory/types";

// How the pages reach the memory. The pages never fetch a URL themselves: every read and change goes through the
// `Transport` the host gives `MemoryStudio`. `ampup-memory serve --local` serves them with `LocalTransport`
// (same-origin calls to the server's own routes); a hosted console gives one that reaches the memory through its
// own routes, as the signed-in person, and adds what it keeps itself (saved arms, area owners, an org's members).

/** A refused or failed call: the server's status and its `error`, and the whole reply. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    /** The whole reply, for routes that say more than `error`. */
    readonly body: Record<string, unknown> = {},
  ) {
    super(message);
  }
}

/**
 * What a failed reply that says nothing (no JSON `error`) means. A 502, 503 or 504 like that comes from a gateway
 * in front of the server giving up on it (slow, or restarting), not from the server refusing: worth trying again.
 */
export function failedWithout(status: number): string {
  if (status === 504) return "The server took too long to answer (504). Nothing was lost: try again.";
  if (status === 502 || status === 503) {
    return `The server couldn't be reached just now (${status}). Try again in a moment.`;
  }
  return `request failed (${status})`;
}

/** Whether asking again may well work: the server or a gateway in front of it gave up, or the connection dropped. */
export function retryable(e: unknown): boolean {
  if (e instanceof ApiError) return e.status === 429 || e.status >= 500;
  // fetch's own failure: the connection dropped or never opened.
  return e instanceof TypeError;
}

/**
 * The memory's routes the pages call, each taking and answering what memstore's route of the same purpose does:
 *
 * - `browse`  `{op, path?, id?, limit?, after?, days?}`: memstore's `/browse` (the tree, an episode, the
 *             branches, the schema, advice usage).
 * - `search`  `{query}`: the 30 best-matching facts, answered as `{facts}` (memstore's `/search` answers
 *             `{replies: [[...facts]]}` to `{calls: [[query, 30, -1]]}`).
 * - `me`      `{}`: who the memory says the caller is (memstore's `GET /me`).
 * - `lessons` `{space?, person?, key?, since?, limit?}`: what the memory learned (memstore's `/lessons`).
 * - `mcp`     `{method: "tools/list" | "tools/call", params?}`: one JSON-RPC call to memstore's `/mcp`, answered
 *             as its JSON-RPC reply.
 * - `agent`   `{question, tools, model?, maxToolCalls?, history?}`: an agent answers with those tools
 *             (memstore's `/agent/run`, `max_tool_calls`), and its trace.
 * - `admin`   `{op, …}`: an org admin's reads and changes (memstore's `/admin`): `activity` (with `sinceDays`),
 *             `conversation`, `user_facts`, `advice_trail`, `ratify_advice`, `branch_diff`, `merge_branch`,
 *             and `set_activity {on}` (memstore's `set_settings`).
 * - `review`  `{op, …}`: owner review and the entity graph (memstore's `/admin` governance ops), for any member.
 * - `learn`   `{op: "add", items, for?}`: add files, pasted text or an answer to the memory (memstore's `/learn`);
 *             `{op: "status", episode_ids}`: what it learned from them so far (memstore's `/learn/status`).
 *
 * The selection's team and branch go along (`?team=`, `?branch=`); its org is the transport's to place (a
 * console names it in its own path; the local server serves one memory and names none).
 */
export type Area =
  | "browse"
  | "search"
  | "me"
  | "lessons"
  | "mcp"
  | "agent"
  | "admin"
  | "review"
  | "learn";

/**
 * Whether a transport reaches what a group of sections needs: `true`, `false` (the sections are hidden), or
 * `"probe"`: ask the server once (`probe` in `MemoryStudio`) and show them only if it answers, so they appear on
 * their own once the server allows them.
 */
export type Capability = boolean | "probe";

export interface Capabilities {
  /**
   * The org admins' reads (`admin`): People, Not answered, Branches and Owners, each conversation's trace, the
   * advice trail and its ratification, and Search's "by person" facet.
   */
  admin?: Capability;
  /** Owner review and the entity graph (`review`): Review, Graph, and an entity's owner on its page. */
  review?: Capability;
  /** An agent answering with the memory's tools (`agent`): Ask. */
  agent?: boolean;
  /** What the memory learned (`lessons`): Learned. */
  lessons?: Capability;
  /** Reading one of the org's teams' memories (`?team=`): the memory picker. */
  teams?: boolean;
  /** Changing the org's settings (`admin {op: "set_activity"}`): People's activity switch. */
  settings?: boolean;
  /** Adding to the memory by hand and reading back what it learned (`learn`): Add to memory, Remember this. */
  learn?: boolean;
}

/** A set of tools an agent answers with, a model and a cap on its tool calls (Ask). */
export interface Arm {
  id: string;
  name: string;
  tools: string[];
  model: "haiku" | "sonnet" | "opus";
  maxToolCalls: number;
  /** One every memory has, not saved and not deletable. */
  builtin?: boolean;
}

/** Where a host keeps an org's own arms, beside the ones every memory has. */
export interface ArmStore {
  list(sel: Selection): Promise<Arm[]>;
  /** Saves a new arm (no `id`) or changes one; answers it as saved. */
  save(sel: Selection, arm: Omit<Arm, "id" | "builtin"> & { id?: string }): Promise<Arm>;
  remove(sel: Selection, id: string): Promise<void>;
}

export type AreaOwnerKind = "episode_type" | "category" | "entity_type";

/** Who decides for an area of the memory: a branch's conflicts in it, and the advice learned in it. */
export interface AreaOwner {
  kind: AreaOwnerKind;
  area: string;
  owner: string;
  setBy: string;
  setAt: string;
}

/** Where a host keeps an org's area owners (Owners). Without one, Owners shows the entities' owners alone. */
export interface AreaOwnerStore {
  list(org: string): Promise<AreaOwner[]>;
  /** Sets (or, with `owner` null, removes) an area's owner; answers every owner after. */
  set(org: string, kind: AreaOwnerKind, area: string, owner: string | null): Promise<AreaOwner[]>;
}

/**
 * Where a host keeps a page's last complete answer for the person reading (Home's numbers), so the page shows it at
 * once the next time it opens, marked as updating, while it reads the memory again. `name` carries the answer's
 * shape and its version (`home/1`): a host keeps it as given and answers it back unchanged. The host decides for how
 * long and for whom: it must never answer one person's or one org's to another.
 */
export interface KeptAnswers {
  /** The answer last kept under `name` for this selection, if the host still has it. */
  get(sel: Selection, name: string): unknown;
  /** Keeps `value`, an answer the page worked out completely, in place of the one before. */
  set(sel: Selection, name: string, value: unknown): void;
}

export interface Transport {
  /** One call to the memory (see `Area`). Rejects with an `ApiError` on a refusal. */
  call<T>(sel: Selection, area: Area, body?: Record<string, unknown>): Promise<T>;
  /** Where the slowest pages' last answers are kept between visits; without it, every open reads anew. */
  kept?: KeptAnswers;
  /** What it reaches; what it leaves out is hidden. */
  capabilities: Capabilities;
  /** The org's own saved arms (Ask); without it, the arms every memory has. */
  arms?: ArmStore;
  /** The org's area owners (Owners). */
  owners?: AreaOwnerStore;
  /** The org's members, for the admin's "by person" facet beside who was active. */
  members?(sel: Selection): Promise<string[]>;
  /** An error as the pages say it. A host may act on one first (sign in again on a 401). */
  errorText?(e: unknown): string;
}

let current: Transport | null = null;

/** The transport the pages call through; `MemoryStudio` sets it. */
export function transport(): Transport {
  if (!current)
    throw new Error("no transport: render the pages inside <MemoryStudio transport={…}>");
  return current;
}

/** Sets the transport the pages call through (`MemoryStudio` does, as it renders). */
export function setTransport(t: Transport) {
  current = t;
}

/** An error as the pages say it: the transport's way, else its message. */
export function errorText(e: unknown): string {
  const own = current?.errorText;
  if (own) return own(e);
  return e instanceof Error ? e.message : String(e);
}

/**
 * The calls each probed capability is checked with: a harmless read that only a caller allowed it can make.
 * A reply is a yes; a refusal is a no.
 */
const PROBES: Record<"admin" | "review" | "lessons", [Area, Record<string, unknown>]> = {
  // Each answers without reading every session: the categories' names alone, and the lessons' probe.
  admin: ["admin", { op: "advice_trail", categories_only: true }],
  review: ["review", { op: "entity_owners" }],
  lessons: ["lessons", { probe: true }],
};

/** Each capability resolved to yes or no (a probed one once its probe answers; until then, no). */
export type Resolved = {
  admin: boolean;
  review: boolean;
  agent: boolean;
  lessons: boolean;
  teams: boolean;
  settings: boolean;
};

/** Asks the server about each capability given as `"probe"`. */
export async function probe(t: Transport, sel: Selection): Promise<Resolved> {
  const caps = t.capabilities;
  const one = async (k: keyof typeof PROBES): Promise<boolean> => {
    const c = caps[k];
    if (c !== "probe") return c ?? false;
    const [area, body] = PROBES[k];
    return t.call(sel, area, body).then(
      () => true,
      () => false,
    );
  };
  const [admin, review, lessons] = await Promise.all([one("admin"), one("review"), one("lessons")]);
  return {
    admin,
    review,
    agent: caps.agent ?? false,
    lessons,
    teams: caps.teams ?? false,
    settings: caps.settings ?? false,
  };
}

/** What is known before any probe answers: what is given as true. */
export function given(t: Transport): Resolved {
  const c = t.capabilities;
  return {
    admin: c.admin === true,
    review: c.review === true,
    agent: c.agent === true,
    lessons: c.lessons === true,
    teams: c.teams ?? false,
    settings: c.settings ?? false,
  };
}
