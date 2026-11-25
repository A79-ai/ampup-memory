// What memstore's /browse, /me and /search answer (see README, "Browsing memory as a tree").

export type Kind = "folder" | "episode" | "entity" | "skill" | "skill_index" | "advice";

export type Child = {
  name: string;
  kind: Kind;
  path: string[];
  count?: number;
  latest?: number | null;
  episode_id?: string;
  occurred_at?: number;
  entity?: string;
  /** A compiled skill's line in its goal's routing index: when it applies. */
  when?: string | null;
  /** In `notes/`, `sessions/` and `advice/`: what to show for the name (a note's first line, a session's first
   * request, a piece of advice's situation). */
  title?: string;
  /** A piece of advice's standing: `served`, `needs backing`, `held` or `merged`. */
  status?: string;
  runs?: number;
  learned_at?: number | null;
};

/** `advice/<space>/<category>/<situation>`: one piece of advice, as `/browse` lists it with its note. */
export type AdviceView = {
  path: string[];
  advice: {
    space: string;
    key: string;
    category: string;
    situation: string;
    when?: string | null;
    text?: string;
    runs?: number;
    wrong?: number;
    learned_at?: number | null;
    note_id?: string | null;
    ratified?: boolean;
    needs_backing?: string | null;
    merged_into?: string | null;
  };
  /** Every version of its note the reader may read, newest first (the first is the one above). */
  history?: DocVersion[];
};

/** One version of an engine document (a note, a compiled skill): read in full with the `episode` op. */
export type DocVersion = { episode_id: string; occurred_at: number; title: string };

export type Listing = {
  path: string[];
  children: Child[];
  next: string | null;
  /** What the listed folder is called, where the console names it (a Slack channel's `#name` for its id). */
  title?: string;
};

export type Fact = {
  fact_id: string;
  entity: string;
  predicate: string;
  value: string;
  valid_from: number;
  valid_to: number | null;
  source_episodes: string[];
  quote: string | null;
  score?: number;
  /** The search's mark on a hit that is not a close match; absent when it is one (or from a server before it). */
  weak?: boolean;
};

export type EntityView = { path: string[]; entity: string; facts: Fact[] };

/** One step of an AIP step graph (v0.3). */
export type AipStep = {
  name: string;
  description?: string;
  inputs?: { name: string }[];
  outputs?: { name: string }[];
};

export type Aip = {
  aip?: string;
  name?: string;
  purpose?: string;
  trigger_when?: string[];
  do_not_use_when?: string[];
  steps?: AipStep[];
  source_ids?: string[];
};

/** `skills/<name>`: a compiled skill (with its AIP graph, null before it has one) or a routing index. */
export type SkillView = {
  path: string[];
  name: string;
  kind: "skill" | "skill_index";
  markdown: string;
  episode_id?: string;
  occurred_at?: number;
  aip?: Aip | string | null;
  /** The episodes the skill was compiled from that the reader may read, and how many more they may not. */
  sources?: { episode_id: string; name: string }[];
  sources_hidden?: number;
  /** Every version of the skill the reader may read, newest first (the first is the one shown). */
  history?: DocVersion[];
};

/** A branch of the scope, with how many compiled skills the reader may read on it. */
export type Branch = { name: string; skills: number | null };

export type EpisodeView = {
  episode_id: string;
  source: string;
  path: string[];
  occurred_at: number;
  /** What its type says it is called (a call's recorder title), when it says. */
  title?: string | null;
  about: string[];
  body: string;
  truncated: boolean;
  facts: Fact[];
};

export type Me = {
  user: string;
  groups: string[];
  scope: string | null;
  refused: string | null;
  memberships: Record<string, { teams?: string[] } | unknown>;
};

/** Which memory is read: the org's own or one of its teams', on a branch. */
export type Selection = { org: string; team?: string; branch?: string };

/** What the page shows: the Explorer's focus, a search, the Tools, the schema or the goals. */
export type Focus =
  /** Home: what changed in the memory, what it answered, what needs the reader. */
  | { kind: "home" }
  /** Import: add files (text, PDF, Word, recordings) or pasted text to the memory. */
  | { kind: "import" }
  | { kind: "folder"; path: string[] }
  | { kind: "entity"; path: string[] }
  | { kind: "skill"; path: string[] }
  | { kind: "episode"; id: string }
  | { kind: "search"; query: string }
  | { kind: "tools"; tool?: string }
  | { kind: "schema"; type?: string }
  | { kind: "goals" }
  | { kind: "handover" }
  | { kind: "people"; user?: string }
  | { kind: "advice" }
  /** What it learned, a lesson picked (`lessonId` in studio/Learned.tsx). */
  | { kind: "learned"; lesson?: string }
  | { kind: "serving" }
  | { kind: "branches" }
  | { kind: "owners" }
  | { kind: "gaps" }
  | { kind: "conversation"; id: string }
  | { kind: "ask"; question: string; parts?: string }
  | { kind: "agent" }
  | { kind: "review"; proposal?: string }
  | { kind: "graph"; entity?: string };

/** A predicate as a schema declares it: its value type, cardinality, allowed values or the types it refers to. */
export type Predicate = {
  type: string;
  card?: "one" | "many";
  values?: string[];
  to?: string[];
  unit?: string;
  description?: string;
};
export type TypeDef = { description?: string; predicates?: Record<string, Predicate> };
export type Schema = {
  name?: string;
  version?: number;
  description?: string;
  mode?: "strict" | "open";
  types?: Record<string, TypeDef>;
  /** What the memory extracts with it: which episodes, by their source prefixes. */
  extract?: { sources?: { prefixes?: string[] } };
};

/** A predicate in the org's schema, and where it comes from: the base schema, or the goal whose slice adds it. */
export type OrgPredicate = Predicate & { from: string };
export type OrgType = { description?: string; from: string; predicates: Map<string, OrgPredicate> };
