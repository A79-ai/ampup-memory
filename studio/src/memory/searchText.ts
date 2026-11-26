// No imports, so the tests (NodeNext, no JSX) can load it.

/** search_memory's answer, read back: one block per entity, and the episodes its markers (`[e1 m0]`) name. */
export type Found = {
  head: string;
  results: { entity: string; when: string; lines: string[] }[];
  sources: Map<string, { id: string; type: string }>;
};
export function readSearch(text: string): Found {
  const lines = text.split("\n");
  const sources = new Map<string, { id: string; type: string }>();
  for (const l of lines) {
    const m = /^(e\d+) ([0-9a-f]{16,}) (\S+)\s*$/.exec(l.trim());
    if (m) sources.set(m[1]!, { id: m[2]!, type: m[3]! });
  }
  const results: Found["results"] = [];
  for (const l of lines) {
    const start = /^\d+\. (\S+) · ([^[]+?)(?: \[e\d+\])?\s*$/.exec(l);
    if (start) results.push({ entity: start[1]!, when: start[2]!.trim(), lines: [] });
    else if (results.length && /^\s{2,}\S/.test(l))
      results[results.length - 1]!.lines.push(l.trim());
  }
  return { head: lines[0] ?? "", results, sources };
}

/**
 * A citation an answer makes (`[e6 @347]`, or a subject by its id, `[topic:memstore_cli]`), read: the marker, and
 * what it points at where a search shows it: the entity, the team whose memory its result is in (`in team:acme/eng`
 * reads `eng`), and the episode.
 */
export type Cited = { marker: string; entity?: string; team?: string; episode?: string };

/** The team a result's first line says it is in (`· in team:acme/eng` → `eng`), if any. */
export const resultTeam = (line: string): string | undefined =>
  /· in team:(?:[^/\s]+\/)?([^\s·[]+)/.exec(line)?.[1];

/** A subject's id as a citation names it (`topic:memstore_cli`, `deal:42`): a type, a colon, an id with no spaces. */
export const ENTITY_ID = /^[a-z][a-z0-9_]*:[A-Za-z0-9][\w.\-/@#]*$/;

/**
 * The parts of one citation's brackets that cite something: `[e6 @347, e2 @876]` is two markers, and
 * `[topic:memstore_cli; memstore-connectors#29 notes]` one subject (the words after it cite nothing it can open).
 */
export const citeParts = (inside: string): string[] =>
  inside
    .split(/[,;]\s*/)
    .map((p) => p.trim())
    .filter((p) => /^e\d+\b/.test(p) || ENTITY_ID.test(p));

/** A marker as two citations of one quote write it alike: its episode, and the place in it where it names one
 * (`@120` an offset, `t192` a turn, `m<ts>` a message, `p3` a paragraph, a record's field). */
const markerKey = (part: string) => {
  const m = /^(e\d+)(?:\s+(\S+))?/.exec(part.trim());
  return m ? (m[2] ? `${m[1]} ${m[2]}` : m[1]!) : part.trim();
};

/**
 * What an answer's citations point at, from the search_memory answers the run got (in order): each marker by the
 * entity whose result carries it (on a fact's line, or its result's first line, which names the episode its
 * unmarked facts come from), the exact marker (`e6 @347`) first and the episode's alone (`e6`) else, and the
 * episode each names where the answer lists it. A later search's numbering wins where two use the same one.
 */
export function citeIndex(searches: string[]): (part: string) => Cited {
  type At = { entity: string; team?: string };
  const exact = new Map<string, At>();
  const byEpisode = new Map<string, At>();
  const episodes = new Map<string, string>();
  // Each subject a result was about, with its team: what a citation naming the subject itself opens.
  const subjects = new Map<string, At>();
  for (const text of searches) {
    for (const [e, src] of readSearch(text).sources) episodes.set(e, src.id);
    // This search's own numbering: an episode another search numbered alike no longer answers to it.
    const mine = new Map<string, At>();
    let at: At | undefined;
    for (const line of text.split("\n")) {
      const start = /^\d+\. (\S+) · /.exec(line);
      if (start) {
        at = { entity: start[1]!, team: resultTeam(line) };
        if (!subjects.has(at.entity)) subjects.set(at.entity, at);
      }
      else if (line.trim() && !/^\s{2,}\S/.test(line)) at = undefined;
      if (!at) continue;
      for (const m of line.matchAll(/\[(e\d+)([^\]]*)\]/g))
        for (const part of citeParts(m[1]! + m[2]!)) {
          exact.set(markerKey(part), at);
          const e = part.split(/\s/)[0]!;
          if (!mine.has(e)) mine.set(e, at);
        }
    }
    for (const [e, a] of mine) byEpisode.set(e, a);
  }
  return (part) => {
    // A subject named by its id: itself, in the team its result was in where a search showed it.
    if (ENTITY_ID.test(part.trim())) {
      const id = part.trim();
      const team = subjects.get(id)?.team;
      return { marker: id, entity: id, ...(team ? { team } : {}) };
    }
    const key = markerKey(part);
    const e = key.split(" ")[0]!;
    const at = exact.get(key) ?? byEpisode.get(e);
    return {
      marker: key,
      entity: at?.entity,
      ...(at?.team ? { team: at.team } : {}),
      episode: episodes.get(e),
    };
  };
}

/** What a run gave the model, as `/agent/run` sends it (`context.messages`), as far as this reads it. */
export type ContextMessage = {
  role: string;
  content?: string | null;
  tool_call_id?: string;
  tool_calls?: { id?: string; function?: { name?: string; arguments?: string } }[];
};

/**
 * The search_memory answers a run got, whole, for `citeIndex`: from what the model was given where the run sends
 * it, since a step's trace keeps only the start of a long answer and the model cites markers past it; else from
 * the steps. Each tool answer goes with its call by id, else by order.
 */
export function searchTexts(
  steps: { tool: string; ran: boolean; error?: unknown; text: string }[],
  messages?: ContextMessage[],
): string[] {
  const fromSteps = steps
    .filter((s) => s.ran && !s.error && s.tool === "search_memory")
    .map((s) => s.text);
  if (!messages?.length) return fromSteps;
  const named = new Map<string, string>();
  const pending: string[] = [];
  const whole: string[] = [];
  for (const m of messages) {
    for (const c of m.tool_calls ?? []) {
      const name = c.function?.name ?? "";
      if (c.id) named.set(c.id, name);
      pending.push(name);
    }
    if (m.role !== "tool") continue;
    const byId = m.tool_call_id ? named.get(m.tool_call_id) : undefined;
    const name = byId ?? pending[0];
    if (byId !== undefined) pending.splice(pending.indexOf(byId), 1);
    else pending.shift();
    if (name === "search_memory" && m.content) whole.push(m.content);
  }
  return whole.length ? whole : fromSteps;
}
