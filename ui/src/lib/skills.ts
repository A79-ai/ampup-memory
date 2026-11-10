import "server-only";

// `skills/` for a memstore whose /browse predates it (it answers `{"op": "branches"}` with "unknown op"), such as
// an experiment's namespace pinned to an older build: the same tree, read through its MCP `skills` tool, which
// reads the same documents with the caller's access on the selected branch. Such a memstore can't name its
// branches, so the page asks for one by name.

/** A /browse call and a `skills` tool call on the memstore and selection being read. */
export type Calls = {
  browse: (body: unknown) => Promise<Response>;
  skills: (args: Record<string, string>) => Promise<Response>;
};

const LIMIT = 200;
const MAX_LIMIT = 1000;

// Whether each memstore's /browse predates `skills/`, once known (by namespace).
const legacy = new Map<string, boolean>();

async function isLegacy(ns: string, calls: Calls): Promise<boolean> {
  const known = legacy.get(ns);
  if (known !== undefined) return known;
  const r = await calls.browse({ op: "branches" });
  if (r.status >= 500) return false;
  const v = await r.json().catch(() => ({}));
  const old = r.status === 400 && /unknown op/.test(String(v?.error ?? ""));
  legacy.set(ns, old);
  return old;
}

/** The `skills` tool's text, or null when it refuses (no such skill, or one the caller may not read). */
async function skillsText(calls: Calls, args: Record<string, string>): Promise<string | null> {
  const r = await calls.skills(args);
  const v = await r.json().catch(() => ({}));
  if (!r.ok || v?.error) throw new Error(v?.error?.message ?? v?.error ?? `skills answered ${r.status}`);
  if (v?.result?.isError) return null;
  return v?.result?.content?.[0]?.text ?? "";
}

type Goal = { skills: Map<string, string>; index: string[] };

/** Each goal's routing indexes the caller may read, from the tool's answer with no arguments: one section per
 * index, `# Skills for goal \`<goal>\`` and a line per skill, `- \`<slug>\`: <when> (\`<AIP graph key>\`)`. */
function goals(text: string): Map<string, Goal> {
  const out = new Map<string, Goal>();
  let at: Goal | null = null;
  for (const line of text.split("\n")) {
    const head = /^# Skills for goal `([^`]+)`/.exec(line);
    if (head) {
      at = out.get(head[1]) ?? { skills: new Map(), index: [] };
      out.set(head[1], at);
    } else if (line.startsWith("# ")) {
      at = null; // another section (the skills learned from runs)
    }
    if (!at) continue;
    at.index.push(line);
    const item = /^- `([^`]+)`: (.*?)(?: \(`[^`]+`\))?$/.exec(line);
    if (item && !at.skills.has(item[1])) at.skills.set(item[1], item[2].trim());
  }
  return out;
}

const json = (v: unknown, status = 200) => Response.json(v, { status });
const notFound = () => json({ error: "nothing there (or not visible)" }, 404);

/** The answer to `req`, with `skills/` filled in from the `skills` tool where memstore's /browse can't. */
export async function withSkills(
  r: Response,
  req: { op?: unknown; path?: unknown; limit?: unknown; after?: unknown },
  ns: string,
  calls: Calls,
): Promise<Response> {
  const path = Array.isArray(req.path) ? (req.path as unknown[]).map(String) : [];
  const op = req.op ?? "ls";
  if (op === "branches") {
    if (r.status !== 400 || !(await isLegacy(ns, calls))) return r;
    return json({ branches: null });
  }
  if (op !== "ls") return r;
  const root = path.length === 0 && r.ok;
  if (!root && !(path[0] === "skills" && r.status === 404)) return r;
  if (!(await isLegacy(ns, calls))) return r;

  if (path.length === 3) {
    const [, goal, name] = path;
    if (name === "index") {
      const g = goals((await skillsText(calls, {})) ?? "").get(goal);
      if (!g) return notFound();
      return json({ path, goal, name, kind: "skill_index", markdown: g.index.join("\n").trim() + "\n" });
    }
    const text = await skillsText(calls, { goal, skill: name });
    if (text == null) return notFound();
    const cut = text.lastIndexOf("\n## AIP step graph\n\n");
    const markdown = cut < 0 ? text : text.slice(0, cut + 1);
    const graph = cut < 0 ? "" : text.slice(cut + "\n## AIP step graph\n\n".length);
    let aip: unknown = null;
    if (graph && graph !== "(no AIP graph yet)") {
      try {
        aip = JSON.parse(graph);
      } catch {
        aip = graph;
      }
    }
    return json({ path, goal, name, kind: "skill", markdown, aip });
  }

  const all = goals((await skillsText(calls, {})) ?? "");
  if (root) {
    const listing = await r.json();
    const count = [...all.values()].reduce((n, g) => n + g.skills.size, 0);
    if (count === 0) return json(listing);
    const children = [...listing.children, { name: "skills", kind: "folder", path: ["skills"], count }];
    children.sort((a: { name: string }, b: { name: string }) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    return json({ ...listing, children });
  }
  let children: { name: string; [k: string]: unknown }[];
  if (path.length === 1) {
    children = [...all].map(([g, v]) => ({ name: g, kind: "folder", path: ["skills", g], count: v.skills.size }));
  } else if (path.length === 2 && all.has(path[1])) {
    const g = all.get(path[1])!;
    children = [
      { name: "index", kind: "skill_index", path: [...path, "index"], count: g.skills.size },
      ...[...g.skills].map(([slug, when]) => ({ name: slug, kind: "skill", path: [...path, slug], when })),
    ];
  } else {
    return notFound();
  }
  // A page by name, as memstore pages.
  children.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const after = typeof req.after === "string" ? req.after : null;
  const limit = Math.min(Math.max(Number(req.limit) || LIMIT, 1), MAX_LIMIT);
  const rest = after == null ? children : children.filter((c) => c.name > after);
  const page = rest.slice(0, limit);
  return json({ path, children: page, next: rest.length > limit ? page.at(-1)!.name : null });
}
