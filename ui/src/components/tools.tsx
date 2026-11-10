"use client";

import { useEffect, useState } from "react";
import { ApiError } from "./api";
import type { Focus, Selection } from "./types";

type Tool = {
  name: string;
  description?: string;
  inputSchema?: { properties?: Record<string, { type?: string; description?: string }>; required?: string[] };
};

type Run = { tool: string; args: string; ms: number; status: "ok" | "error"; text: string };

async function mcp<T>(sel: Selection, method: string, params?: unknown): Promise<T> {
  const r = await fetch("/api/mcp", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ns: sel.ns, sel, method, params }),
  });
  const v = await r.json().catch(() => ({}));
  if (!r.ok) throw new ApiError(r.status, v?.error ?? r.statusText);
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
    out[k] = p.type === "integer" || p.type === "number" ? 0 : p.type === "object" ? {} : p.type === "array" ? [] : "";
  }
  return out;
}

/** Results are JSON as text: pretty-printed when they parse. */
function pretty(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

/** Calls the memory's MCP tools, as an agent would, against the picked namespace and scope. */
export function Tools({ sel, tool, onFocus }: { sel: Selection; tool?: string; onFocus: (f: Focus) => void }) {
  const [tools, setTools] = useState<{ list?: Tool[]; error?: string } | null>(null);
  const [args, setArgs] = useState<Record<string, string>>({});
  const [runs, setRuns] = useState<Run[]>([]);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    mcp<{ tools: Tool[] }>(sel, "tools/list").then(
      (r) => setTools({ list: r.tools }),
      (e) => setTools({ error: e.message }),
    );
  }, [sel]);

  if (!tools) return <div className="pad muted">Loading tools…</div>;
  if (tools.error) return <div className="pad error">{tools.error}</div>;
  const list = tools.list ?? [];
  const current = list.find((t) => t.name === tool) ?? list[0];
  if (!current) return <div className="pad muted">This memstore offers no tools to this caller.</div>;
  const text = args[current.name] ?? JSON.stringify(example(current), null, 2);
  let parsed: unknown;
  let parseError: string | null = null;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    parseError = (e as Error).message;
  }

  const run = async () => {
    if (parseError) return;
    setBusy(true);
    const t0 = performance.now();
    try {
      const r = await mcp<{ content?: { text?: string }[]; isError?: boolean }>(sel, "tools/call", {
        name: current.name,
        arguments: parsed,
      });
      const out = (r.content ?? []).map((c) => c.text ?? "").join("\n");
      setRuns((rs) => [
        { tool: current.name, args: text, ms: performance.now() - t0, status: r.isError ? "error" : "ok", text: out },
        ...rs,
      ]);
    } catch (e) {
      setRuns((rs) => [
        { tool: current.name, args: text, ms: performance.now() - t0, status: "error", text: (e as Error).message },
        ...rs,
      ]);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="tools">
      <aside className="side tool-list">
        {list.map((t) => (
          <button
            key={t.name}
            className={`tree-row ${t.name === current.name ? "tree-row-on" : ""}`}
            onClick={() => onFocus({ kind: "tools", tool: t.name })}
          >
            <span className="mono">{t.name}</span>
          </button>
        ))}
        <p className="muted tool-note">
          The MCP tools this memstore offers this caller, in the scope picked above. Results are what an agent
          gets.
        </p>
      </aside>
      <section className="pane">
        <h2 className="title mono">{current.name}</h2>
        {current.description && (
          <details className="desc" open={current.description.length < 400}>
            <summary className="muted">Description</summary>
            <pre className="body">{current.description}</pre>
          </details>
        )}
        <h3>Arguments</h3>
        <ArgHints tool={current} />
        <textarea
          className="args mono"
          spellCheck={false}
          value={text}
          onChange={(e) => setArgs((a) => ({ ...a, [current.name]: e.target.value }))}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) run();
          }}
          rows={Math.min(16, Math.max(4, text.split("\n").length + 1))}
        />
        <div className="run-row">
          <button className="run" disabled={busy || !!parseError} onClick={run}>
            {busy ? "Running…" : "Run"}
          </button>
          <span className="muted">⌘↩ to run</span>
          {parseError && <span className="error">Not JSON: {parseError}</span>}
          <button className="link" onClick={() => setArgs((a) => ({ ...a, [current.name]: JSON.stringify(example(current), null, 2) }))}>
            reset
          </button>
        </div>
        {runs.length > 0 && <h3>Results</h3>}
        {runs.map((r, i) => (
          <div key={runs.length - i} className="result">
            <div className="result-head">
              <span className="mono">{r.tool}</span>
              <span className={r.status === "ok" ? "current" : "error"}>{r.status}</span>
              <span className="muted">{Math.round(r.ms)} ms</span>
              <button className="link" onClick={() => setArgs((a) => ({ ...a, [r.tool]: r.args }))}>
                use these arguments
              </button>
            </div>
            <pre className="body">{pretty(r.text)}</pre>
          </div>
        ))}
      </section>
    </div>
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
          <span className="mono">{k}</span> <span className="muted">{p.type}</span>
          {required.has(k) && <span className="req"> required</span>}
          {p.description && <span className="muted"> · {p.description}</span>}
        </li>
      ))}
    </ul>
  );
}
