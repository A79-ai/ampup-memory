// What memstore's /browse, /me and /search answer (see README, "Browsing memory as a tree").

export type Kind = "folder" | "episode" | "entity" | "skill" | "skill_index";

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
};

export type Listing = { path: string[]; children: Child[]; next: string | null };

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
};

export type EntityView = { path: string[]; entity: string; facts: Fact[] };

/** One step of an AIP step graph (v0.3). */
export type AipStep = { name: string; description?: string; inputs?: { name: string }[]; outputs?: { name: string }[] };

export type Aip = {
  aip?: string;
  name?: string;
  purpose?: string;
  trigger_when?: string[];
  do_not_use_when?: string[];
  steps?: AipStep[];
  source_ids?: string[];
};

/** `skills/<name>`: a compiled skill (with its AIP graph, null before it has one) or a routing index. A memstore from
 * before skills were compiled per scope keeps them per goal, `skills/<goal>/<name>`, and names the goal. */
export type SkillView = {
  path: string[];
  goal?: string;
  name: string;
  kind: "skill" | "skill_index";
  markdown: string;
  episode_id?: string;
  occurred_at?: number;
  aip?: Aip | string | null;
};

/** A branch of the scope, with how many compiled skills the reader may read on it. */
export type Branch = { name: string; skills: number };

export type EpisodeView = {
  episode_id: string;
  source: string;
  path: string[];
  occurred_at: number;
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

/** Which memstore (a `memstore-*` namespace on Kubernetes) and, within it, which org or team. */
export type Selection = { ns?: string; scope?: string; org?: string; team?: string; branch?: string };

export type Namespace = { namespace: string; serving: boolean };

export type PredicateDef = {
  type: string;
  card?: "one" | "many";
  values?: string[];
  to?: string[];
  unit?: string;
  part_of?: boolean;
  description?: string;
};

export type TypeDef = { description?: string; id_prefix?: string; predicates: Record<string, PredicateDef> };

export type ExtractSection = {
  sources?: { prefixes?: string[]; about?: string[]; from?: string; to?: string };
  facts_per_episode?: number;
  aggregates?: {
    subject: string;
    elements?: Record<string, string[]>;
    items?: Record<string, string[]>;
    rules?: string[];
  };
  style?: string;
  outcomes?: boolean | string[];
  writer?: { id: string; names?: string[] };
};

/** A scope's schema (`/browse {"op": "schema"}`): what its memory is typed with and what its `extract` section reads. */
export type SchemaDoc = {
  name: string;
  version: number;
  description?: string;
  mode?: "strict" | "open";
  default_type?: string;
  types: Record<string, TypeDef>;
  metrics?: Record<string, unknown>;
  guidance?: string[];
  extract?: ExtractSection;
};

/** The schema on the branch read (null: untyped) and every version committed there, oldest first. */
export type SchemaView = { schema: SchemaDoc | null; history: SchemaDoc[] };

/** What the inspector shows. */
export type Focus =
  | { kind: "schema" }
  | { kind: "folder"; path: string[] }
  | { kind: "entity"; path: string[] }
  | { kind: "skill"; path: string[] }
  | { kind: "episode"; id: string }
  | { kind: "search"; query: string }
  | { kind: "tools"; tool?: string };
