import { useEffect, useState } from "react";
import { ApiError, call } from "./api";
import { Markdown } from "./markdown";
import type { Focus, Selection } from "./types";
import { EmptyState, ErrorState, Icon, Skeleton } from "./ui";

type Tool = {
  name: string;
  description?: string;
  inputSchema?: {
    properties?: Record<string, { type?: string; description?: string }>;
    required?: string[];
  };
};

type Run = {
  id: number;
  tool: string;
  args: string;
  ms: number;
  status: "ok" | "error";
  text: string;
};

export async function mcp<T>(sel: Selection, method: string, params?: unknown): Promise<T> {
  const v = await call<{ result?: T; error?: { message?: string } }>(sel, "mcp", {
    method,
    params,
  });
  if (v?.error) throw new ApiError(400, v.error.message ?? JSON.stringify(v.error));
  return v.result as T;
}

/** Arguments to start from: a useful example for the tools we know, else one value per argument. */
function example(t: Tool): unknown {
  switch (t.name) {
    case "search_memory":
      return { query: "", k: 10 };
    case "query_memory":
      return { query: { find: "facts", limit: 20 } };
    case "get_entity":
      return { id: "" };
    case "get_episode":
      return { episode_id: "" };
    case "remember_memory":
      return { source: "ui", body: "" };
  }
  const out: Record<string, unknown> = {};
  for (const [k, p] of Object.entries(t.inputSchema?.properties ?? {})) {
    out[k] =
      p.type === "integer" || p.type === "number"
        ? 0
        : p.type === "object"
          ? {}
          : p.type === "array"
            ? []
            : "";
  }
  return out;
}

/** A tool's answer: JSON pretty-printed, anything else (most answers are markdown) drawn as markdown. */
function Result({ text }: { text: string }) {
  const json = (() => {
    try {
      return JSON.stringify(JSON.parse(text), null, 2);
    } catch {
      return null;
    }
  })();
  return json != null ? (
    <pre className="code">{json}</pre>
  ) : (
    <div className="result-md">
      <Markdown text={text} />
    </div>
  );
}

/** Calls the memory's MCP tools, as an agent would, against the picked namespace and scope. */
/** memstore's tools that change something, by their name without an MCP server's prefix, and what they change.
 * Run from here they act as you, for real: each asks first. */
const WRITES: Record<string, string> = {
  start_session: "opens a real session in your name, which then shows in People",
  end_session: "ends a session and sends it to be judged",
  remember_memory: "writes to your memory, as if you had said it",
  forget_memory: "forgets something from your memory",
};
const writes = (name: string) => WRITES[name.split("__").pop() ?? name];

export function Tools({
  sel,
  tool,
  onFocus,
  head,
}: {
  sel: Selection;
  tool?: string;
  onFocus: (f: Focus) => void;
  /** Anything to show above the tool list. */
  head?: React.ReactNode;
}) {
  const [tools, setTools] = useState<{ list?: Tool[]; error?: string } | null>(null);
  const [args, setArgs] = useState<Record<string, string>>({});
  const [runs, setRuns] = useState<Run[]>([]);
  // The tool a call is running for: another tool can be run meanwhile.
  const [busy, setBusy] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  // The writing tools the person has said they mean to run.
  const [meant, setMeant] = useState<Set<string>>(new Set());

  useEffect(() => {
    mcp<{ tools: Tool[] }>(sel, "tools/list").then(
      (r) => setTools({ list: r.tools }),
      (e) => setTools({ error: e.message }),
    );
  }, [sel, attempt]);

  const shell = (side: React.ReactNode, body: React.ReactNode) => (
    <>
      <aside className="side" aria-label="Tools">
        {head}
        <div className="side-scroll">
          <div className="side-section">MCP tools</div>
          {side}
          <p className="side-note">
            The tools this server offers you, in the memory picked above. Results are exactly what
            an agent gets.
          </p>
        </div>
      </aside>
      <section className="pane">
        <div className="pane-inner">{body}</div>
      </section>
    </>
  );

  if (!tools) return shell(<Skeleton rows={5} title={false} />, <Skeleton rows={3} />);
  if (tools.error)
    return shell(
      null,
      <ErrorState
        title="Couldn't list the tools"
        message={tools.error}
        onRetry={() => {
          setTools(null);
          setAttempt((n) => n + 1);
        }}
      />,
    );
  const list = tools.list ?? [];
  const current = list.find((t) => t.name === tool) ?? list[0];
  if (!current)
    return shell(
      null,
      <EmptyState icon="tools" title="No tools">
        This server offers no tools to you.
      </EmptyState>,
    );
  const text = args[current.name] ?? JSON.stringify(example(current), null, 2);
  let parsed: unknown;
  let parseError: string | null = null;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    parseError = (e as Error).message;
  }

  const changes = writes(current.name);
  const blocked = !!changes && !meant.has(current.name);
  const run = async () => {
    if (parseError || busy === current.name || blocked) return;
    const tool = current.name;
    const id = Date.now() + Math.random();
    setBusy(tool);
    const t0 = performance.now();
    try {
      const r = await mcp<{ content?: { text?: string }[]; isError?: boolean }>(sel, "tools/call", {
        name: current.name,
        arguments: parsed,
      });
      const out = (r.content ?? []).map((c) => c.text ?? "").join("\n");
      setRuns((rs) => [
        {
          id,
          tool,
          args: text,
          ms: performance.now() - t0,
          status: r.isError ? "error" : "ok",
          text: out,
        },
        ...rs,
      ]);
    } catch (e) {
      setRuns((rs) => [
        {
          id,
          tool,
          args: text,
          ms: performance.now() - t0,
          status: "error",
          text: (e as Error).message,
        },
        ...rs,
      ]);
    } finally {
      setBusy((b) => (b === tool ? null : b));
    }
  };

  const props = Object.keys(current.inputSchema?.properties ?? {}).length;
  // The results are the picked tool's own: each tool keeps its runs while you look at another.
  const mine = runs.filter((r) => r.tool === current.name);
  const ranFor = (name: string) => runs.filter((r) => r.tool === name).length;
  return shell(
    <div className="tree">
      {list.map((t) => (
        <button
          key={t.name}
          className={`tree-row ${t.name === current.name ? "tree-row-on" : ""}`}
          style={{ paddingLeft: 8 }}
          onClick={() => onFocus({ kind: "tools", tool: t.name })}
        >
          <Icon name="tools" size={14} />
          <span className="tree-name mono">{t.name}</span>
          {writes(t.name) && (
            <span className="tree-count" title="Changes something when run">
              writes
            </span>
          )}
          {ranFor(t.name) > 0 && <span className="tree-count">{ranFor(t.name)}</span>}
        </button>
      ))}
    </div>,
    <section>
      <div className="head">
        <span className="kind k-code">
          <Icon name="tools" size={20} />
        </span>
        <div className="head-text">
          <div className="eyebrow">MCP tool</div>
          <h1 className="title mono">{current.name}</h1>
          <div className="subtitle">
            {props} {props === 1 ? "argument" : "arguments"}
          </div>
        </div>
      </div>
      {current.description && (
        <div className="card card-pad">
          <details open={current.description.length < 600}>
            <summary
              className="muted"
              style={{ cursor: "pointer", fontSize: "var(--t-sm)", marginBottom: "var(--s2)" }}
            >
              What it does
            </summary>
            <div className="tool-desc">{current.description}</div>
          </details>
        </div>
      )}
      <div className="h-section">Arguments</div>
      <ArgHints tool={current} />
      <textarea
        className="args"
        spellCheck={false}
        aria-label="Arguments (JSON)"
        value={text}
        onChange={(e) => setArgs((a) => ({ ...a, [current.name]: e.target.value }))}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) run();
        }}
        rows={Math.min(16, Math.max(4, text.split("\n").length + 1))}
      />
      {changes && (
        <label className="tool-writes">
          <input
            type="checkbox"
            checked={meant.has(current.name)}
            onChange={(e) =>
              setMeant((m) => {
                const n = new Set(m);
                if (e.target.checked) n.add(current.name);
                else n.delete(current.name);
                return n;
              })
            }
          />{" "}
          This tool {changes}. Run it anyway.
        </label>
      )}
      <div className="run-row">
        <button
          className="btn btn-primary"
          disabled={busy === current.name || !!parseError || blocked}
          onClick={run}
        >
          <Icon name="play" size={13} /> {busy === current.name ? "Running…" : "Run"}
        </button>
        <span className="muted">⌘↩ to run</span>
        {parseError && <span className="error-text">Not JSON: {parseError}</span>}
        <button
          className="link"
          style={{ marginLeft: "auto" }}
          onClick={() =>
            setArgs((a) => ({ ...a, [current.name]: JSON.stringify(example(current), null, 2) }))
          }
        >
          Reset arguments
        </button>
      </div>
      {mine.length > 0 && (
        <div className="h-section">
          Results <span className="count">{mine.length}</span>
        </div>
      )}
      {mine.map((r) => (
        <div key={r.id} className="card result">
          <div className="result-head">
            <span className="mono">{r.tool}</span>
            <span className={`badge ${r.status === "ok" ? "badge-ok" : "badge-error"}`}>
              {r.status === "ok" ? "ok" : "error"}
            </span>
            <span className="muted">{Math.round(r.ms)} ms</span>
            <button
              className="link"
              style={{ marginLeft: "auto" }}
              onClick={() => setArgs((a) => ({ ...a, [r.tool]: r.args }))}
            >
              Use these arguments
            </button>
          </div>
          <Result text={r.text} />
        </div>
      ))}
    </section>,
  );
}

function ArgHints({ tool }: { tool: Tool }) {
  const props = Object.entries(tool.inputSchema?.properties ?? {});
  if (!props.length) return null;
  const required = new Set(tool.inputSchema?.required ?? []);
  return (
    <ul className="hints">
      {props.map(([k, p]) => (
        <li key={k}>
          <code>{k}</code> <span className="muted">{p.type}</span>
          {required.has(k) && <span className="req"> required</span>}
          {p.description && <span className="muted"> · {p.description}</span>}
        </li>
      ))}
    </ul>
  );
}
