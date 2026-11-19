// A conversation's transcript as steps, each with the kind of thing that happened in it, so a trace can be
// filtered to what matters (when advice was served, what memory was read and written, where it went wrong).
// Kept apart from Trace.tsx so the parsing is tested without a page.

/** What happened in a step: who spoke, or what a tool call did. memstore's own calls get kinds of their own. */
export type Kind =
  | "user"
  | "assistant"
  | "advice"
  | "read"
  | "write"
  | "session"
  | "tool"
  | "error"
  | "system"
  | "text";

export interface Step {
  kind: Kind;
  /** The speaker as the transcript names it, or the tool's short name. */
  label: string;
  text: string;
  /** For a tool call: its full name (`mcp__plugin_memstore_memstore__get_advice`) and its result. */
  tool?: string;
  result?: string;
}

/** The filters, in the order they're offered, with what each is called on the page. */
export const KINDS: [Kind, string][] = [
  ["user", "People"],
  ["assistant", "Assistant"],
  ["advice", "Advice"],
  ["read", "Memory reads"],
  ["write", "Memory writes"],
  ["session", "Session"],
  ["tool", "Other tools"],
  ["error", "Errors"],
];

// memstore's tools, by their name without an MCP server's prefix (`mcp__<server>__`).
const MEMSTORE: Record<string, Kind> = {
  start_session: "advice",
  get_advice: "advice",
  list_advice: "read",
  search_memory: "read",
  query_memory: "read",
  get_episode: "read",
  get_brief: "read",
  list_briefs: "read",
  list_facts: "read",
  remember_memory: "write",
  forget_memory: "write",
  end_session: "session",
};

/** A tool call's kind by its name: memstore's calls by what they do, any other tool's "tool". */
export function toolKind(name: string): Kind {
  return MEMSTORE[name.split("__").pop() ?? name] ?? "tool";
}

/** A tool's name to show: `mcp__plugin_memstore_memstore__get_advice` reads `get_advice`. */
export const shortTool = (name: string) => name.split("__").pop() || name;

const ROLE: [RegExp, Kind][] = [
  [/^(user|human|customer|person)$/i, "user"],
  // The memstore client labels a harness's turns with its name (`Claude`, `Codex`, `Gemini CLI`, …).
  [
    /^(assistant|ai|claude|claude code|codex|gemini|gemini cli|cursor|copilot|agent|model|bot)$/i,
    "assistant",
  ],
  [/^(tool|tool[ _]?use|tool[ _]?call|function[ _]?call|call)$/i, "tool"],
  [/^(tool[ _]?result|result|function[ _]?result|observation|output)$/i, "tool"],
  [/^(system|note)$/i, "system"],
];
const roleOf = (label: string): Kind | undefined => ROLE.find(([re]) => re.test(label.trim()))?.[1];

/** A call's step, its kind by its tool, an error once its result says so. */
function call(tool: string, args: string, result?: string): Step {
  const err = result != null && /^error\b/i.test(result.trim());
  return {
    kind: err ? "error" : toolKind(tool),
    label: shortTool(tool),
    tool,
    text: args,
    ...(result != null ? { result } : {}),
  };
}

type Block = Record<string, unknown>;

/** A content block's text (a tool result's content is a string or more blocks). */
function blockText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content))
    return content
      .map((b: Block) => (typeof b?.text === "string" ? b.text : JSON.stringify(b)))
      .join("\n");
  return content == null ? "" : JSON.stringify(content);
}

/** JSON lines of messages (`{role, content}`, or a Claude Code transcript's `{type, message}`): one step per
 * content block, each tool result put on the call it answers. */
function fromJson(lines: string[]): Step[] {
  const out: Step[] = [];
  const calls = new Map<string, Step>();
  for (const l of lines) {
    let o: Block;
    try {
      o = JSON.parse(l) as Block;
    } catch {
      out.push({ kind: "text", label: "text", text: l });
      continue;
    }
    const m = (o.message as Block | undefined) ?? o;
    const label = String(m.role ?? o.type ?? o.role ?? "text");
    const content = m.content ?? o.content ?? o.text;
    const blocks: Block[] = Array.isArray(content) ? (content as Block[]) : [{ text: content }];
    for (const b of blocks) {
      if (b?.type === "tool_use") {
        const s = call(String(b.name), JSON.stringify(b.input ?? {}));
        calls.set(String(b.id), s);
        out.push(s);
      } else if (b?.type === "tool_result") {
        const r = blockText(b.content);
        const s = calls.get(String(b.tool_use_id));
        if (s) Object.assign(s, call(s.tool!, s.text, b.is_error ? `error: ${r}` : r));
        else out.push({ kind: b.is_error ? "error" : "tool", label: "result", text: r });
      } else {
        const text = typeof b?.text === "string" ? b.text : blockText(b?.text ?? b);
        if (text.trim()) out.push({ kind: roleOf(label) ?? "text", label, text: text.trim() });
      }
    }
  }
  return out;
}

/** The label of a turn the harness wrote in the person's place. */
export const HARNESS = "harness";

/** What a harness says in the person's place (a tool loaded, an interruption, a skill's text, a summary of a
 * conversation continued): no one asked it, so it starts no turn. */
export function harnessSaid(text: string): boolean {
  const t = text.trim();
  return (
    /^tool loaded\.?$/i.test(t) ||
    t.startsWith("[Request interrupted") ||
    t.startsWith("Base directory for this skill:") ||
    t.startsWith("This session is being continued") ||
    t.startsWith("<command-") ||
    t.startsWith("<local-command-") ||
    t.startsWith("Caveat: The messages below")
  );
}

/** The client's heading (`Session <id>` or `Run <id>`, `Task: …`, its advice, `Trace:`) is the session's, shown apart: the steps begin
 * after it. */
function withoutHeader(lines: string[]): string[] {
  if (!/^(Session|Run) \S+\s*$/.test(lines[0] ?? "")) return lines;
  const at = lines.findIndex((l) => l.trim() === "Trace:");
  return at < 0 ? lines : lines.slice(at + 1);
}

/**
 * The transcript as steps. Sessions arrive as the memstore client condenses them: `User: …` and `Claude: …`
 * turns (their text may run over several lines), each tool call as `  tool <name> <args>` with its result on
 * the next line as `    -> <result>` (`-> error: …` when it failed), and `[…]` where a long one was cut. JSON
 * lines of messages are read too, and lines that start with a speaker (`Assistant:`, `[tool] …`); anything else
 * is one step.
 */
export function steps(t: string): Step[] {
  const lines = t.split("\n");
  const json = lines.filter((l) => l.trim().startsWith("{"));
  if (json.length && json.length >= lines.filter((l) => l.trim()).length * 0.8) {
    const out = fromJson(json);
    if (out.length) return out;
  }
  const head = /^\s*(?:\[([A-Za-z _]{2,24})\]|([A-Za-z][A-Za-z ]{1,23})\s*:)\s?(.*)$/;
  const out: Step[] = [];
  const last = () => out[out.length - 1];
  for (const l of withoutHeader(lines)) {
    // A call is indented, but the trace's first line can lose its indent: there it is a call when its
    // arguments are JSON.
    const tool = /^\s+tool (\S+)(?: (.*))?$/.exec(l) ?? /^tool (\S+)(?: (\{.*))?$/.exec(l);
    if (tool) {
      out.push(call(tool[1]!, tool[2] ?? ""));
      continue;
    }
    const res = /^\s+-> ?(.*)$/.exec(l);
    const prev = last();
    if (res && prev?.tool && prev.result == null) {
      Object.assign(prev, call(prev.tool, prev.text, res[1] ?? ""));
      continue;
    }
    if (l.trim() === "[…]") {
      out.push({ kind: "system", label: "cut", text: "The middle of this conversation was cut." });
      continue;
    }
    const m = head.exec(l);
    const label = m ? (m[1] ?? m[2])! : "";
    const kind = m ? roleOf(label) : undefined;
    if (kind === "user" && harnessSaid(m![3] ?? ""))
      out.push({ kind: "system", label: HARNESS, text: m![3] ?? "" });
    else if (kind) out.push({ kind, label: label.trim(), text: m![3] ?? "" });
    else if (prev && !prev.tool && prev.label !== "cut") prev.text += `\n${l}`;
    else if (l.trim()) out.push({ kind: "text", label: "", text: l });
  }
  return out
    .map((s) => ({ ...s, text: s.text.trim() }))
    .filter((s) => s.text || s.tool || s.result);
}

/** A turn of the conversation: a person's message (`ask`, the index of its step; none for the steps before the
 * first one, as in a session that began mid-conversation) and the steps it led to, by index. */
export interface Turn {
  ask: number | null;
  steps: number[];
}

/** The steps as turns, each person's message heading what followed it until the next. */
export function turns(all: Step[], from = 0, to = all.length): Turn[] {
  const out: Turn[] = [];
  for (let i = from; i < to; i++) {
    if (all[i]!.kind === "user") out.push({ ask: i, steps: [] });
    else {
      if (!out.length) out.push({ ask: null, steps: [] });
      out[out.length - 1]!.steps.push(i);
    }
  }
  return out;
}

/** Which steps to show for the kinds picked and the words searched: every step when neither is set, else each
 * step that matches, `context` steps either side of it, and the steps opened by hand. */
export function shown(
  all: Step[],
  only: ReadonlySet<Kind>,
  query: string,
  context: number,
  opened: ReadonlySet<number> = new Set(),
): boolean[] {
  const q = query.trim().toLowerCase();
  if (!only.size && !q) return all.map(() => true);
  const hit = all.map(
    (s) =>
      (!only.size || only.has(s.kind)) &&
      (!q || `${s.label}\n${s.text}\n${s.result ?? ""}`.toLowerCase().includes(q)),
  );
  return all.map((_, i) => {
    if (opened.has(i)) return true;
    for (let d = -context; d <= context; d++) if (hit[i + d]) return true;
    return false;
  });
}

/** Where a served note shows in the steps: the first step that names its key, and the later ones that do. Advice
 * a hook served before the transcript (a session's start) names no step. */
export function mentions(all: Step[], key: string): number[] {
  const out: number[] = [];
  all.forEach((s, i) => {
    if (`${s.text}\n${s.result ?? ""}`.includes(key)) out.push(i);
  });
  return out;
}

/** A trace's view as its link keeps it (`#/conversation/<entry>/<view>`): `show=advice,error&ctx=1&q=…`. */
export interface View {
  only: Set<Kind>;
  context: number;
  query: string;
  /** The step linked to (1-based), opened and scrolled to. */
  step?: number;
}

const known = new Set(KINDS.map(([k]) => k));

export function readView(s: string): View {
  const p = new URLSearchParams(s);
  const only = new Set(
    (p.get("show") ?? "").split(",").filter((k): k is Kind => known.has(k as Kind)),
  );
  const ctx = Number(p.get("ctx") ?? 1);
  const step = Number(p.get("step"));
  return {
    only,
    context: Number.isInteger(ctx) && ctx >= 0 && ctx <= 5 ? ctx : 1,
    query: p.get("q") ?? "",
    ...(Number.isInteger(step) && step > 0 ? { step } : {}),
  };
}

export function writeView(v: View): string {
  const p = new URLSearchParams();
  if (v.step) p.set("step", String(v.step));
  if (!v.only.size && !v.query) return p.toString();
  if (v.only.size)
    p.set(
      "show",
      KINDS.filter(([k]) => v.only.has(k))
        .map(([k]) => k)
        .join(","),
    );
  p.set("ctx", String(v.context));
  if (v.query) p.set("q", v.query);
  return p.toString();
}
