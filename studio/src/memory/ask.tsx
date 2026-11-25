import { useEffect, useRef, useState } from "react";
import { humanize, short } from "./format";
import { EntityLink } from "./layouts";
import { Markdown } from "./markdown";
import { readSearch, type Found } from "./searchText";
import { ApiError } from "./api";
import { mcp } from "./tools";
import type { Focus, Selection } from "./types";
import { EmptyState, ErrorState, Icon, Skeleton } from "./ui";

/** What a question is asked of: the facts that match, the advice for it, the episodes the facts cite. */
export type AskParts = { facts: boolean; advice: boolean; episodes: boolean };
export const ALL_PARTS: AskParts = { facts: true, advice: true, episodes: true };

/**
 * One MCP tool call's text, as an agent gets it. A call cut off on the way (502 or 504: the ingress gives a
 * request 60 s, and a cold search can take longer) is made once more.
 */
export async function tool(sel: Selection, name: string, args: Record<string, unknown>): Promise<string> {
  const once = () =>
    mcp<{ content?: { text?: string }[]; isError?: boolean }>(sel, "tools/call", {
      name,
      arguments: args,
    });
  const r = await once().catch((e: unknown) => {
    if (e instanceof ApiError && (e.status === 502 || e.status === 504)) return once();
    throw e;
  });
  const text = (r.content ?? []).map((c) => c.text ?? "").join("\n");
  if (r.isError) throw new Error(text || `${name} failed`);
  return text;
}

const STILL = /still being looked up/i;
export const SESSION = /session_id: ([0-9a-f-]{36})/;
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** get_advice answers later for a new question: ask again with its session until the advice is in. */
export async function advice(sel: Selection, question: string): Promise<string> {
  let text = await tool(sel, "get_advice", { question });
  const session = SESSION.exec(text)?.[1];
  for (let i = 0; i < 10 && session && STILL.test(text); i++) {
    await wait(3000);
    text = await tool(sel, "get_advice", { question, session_id: session });
  }
  return text;
}

type Part<T> =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; data: T; ms: number }
  | { status: "error"; message: string };

/**
 * `#/ask/<parts>/<question>`: a question asked of the memory through its MCP tools, as an agent asks it. Facts are
 * search_memory's; episodes are the ones those facts cite; advice is get_advice's (which opens a session for
 * the question and answers once the advice is looked up). memstore writes no answer here: each part is what
 * the tools return, with links to the entities and episodes in the Explorer.
 */
export function Ask({
  sel,
  question,
  parts,
  onAsk,
  onFocus,
}: {
  sel: Selection;
  question: string;
  parts: AskParts;
  onAsk: (question: string, parts: AskParts) => void;
  onFocus: (f: Focus) => void;
}) {
  const [draft, setDraft] = useState(question);
  const [pick, setPick] = useState<AskParts>(parts);
  const [found, setFound] = useState<Part<Found>>({ status: "idle" });
  const [tips, setTips] = useState<Part<string>>({ status: "idle" });
  const box = useRef<HTMLTextAreaElement>(null);
  useEffect(() => setDraft(question), [question]);
  useEffect(() => setPick(parts), [parts]);
  useEffect(() => box.current?.focus(), []);

  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);
  const want = JSON.stringify(parts);
  useEffect(() => {
    if (!question) return;
    let live = true;
    const time = async <T,>(set: (p: Part<T>) => void, load: () => Promise<T>) => {
      set({ status: "loading" });
      const t0 = performance.now();
      try {
        const data = await load();
        if (live) set({ status: "ok", data, ms: performance.now() - t0 });
      } catch (e) {
        if (live) set({ status: "error", message: (e as Error).message });
      }
    };
    // Facts and episodes are one search_memory call; advice is its own.
    if (parts.facts || parts.episodes)
      void time(setFound, async () =>
        readSearch(await tool(sel, "search_memory", { query: question, k: 10 })),
      );
    else setFound({ status: "idle" });
    if (parts.advice) void time(setTips, () => advice(sel, question));
    else setTips({ status: "idle" });
    return () => {
      live = false;
    };
    // `sel` and `parts` are read through their keys.
  }, [question, selKey, want]);

  const none = !pick.facts && !pick.advice && !pick.episodes;
  const toggle = (k: keyof AskParts) => setPick((p) => ({ ...p, [k]: !p[k] }));
  const episodes = found.status === "ok" ? [...found.data.sources.values()] : [];

  return (
    <section>
      <div className="head">
        <span className="kind k-chat">
          <Icon name="chat" size={20} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Look up</div>
          <h1 className="title">Look a question up in memory</h1>
          <div className="subtitle">
            Through its MCP tools, as an agent asks: you see what each returns, with links to the
            evidence.
          </div>
        </div>
      </div>
      <form
        className="ask-form card card-pad"
        onSubmit={(e) => {
          e.preventDefault();
          if (draft.trim() && !none) onAsk(draft.trim(), pick);
        }}
      >
        <textarea
          ref={box}
          className="args ask-box"
          rows={3}
          placeholder="What do we know about the AEA deal?"
          aria-label="Question"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && draft.trim() && !none)
              onAsk(draft.trim(), pick);
          }}
        />
        <div className="ask-row">
          <fieldset className="ask-parts">
            <legend className="sr-only">Use</legend>
            {(
              [
                ["facts", "Facts", "search_memory"],
                ["advice", "Advice", "get_advice"],
                ["episodes", "Episodes", "the episodes the facts cite"],
              ] as const
            ).map(([k, label, how]) => (
              <label key={k} className="ask-part" title={how}>
                <input type="checkbox" checked={pick[k]} onChange={() => toggle(k)} /> {label}
              </label>
            ))}
          </fieldset>
          <span className="muted">⌘↩</span>
          <button className="btn btn-primary" type="submit" disabled={!draft.trim() || none}>
            <Icon name="search" size={13} /> Look up
          </button>
        </div>
        {none && <div className="error-text">Pick at least one of facts, advice or episodes.</div>}
      </form>

      {!question ? (
        <EmptyState icon="chat" title="Ask anything about the org">
          Facts come with the quote they rest on, advice with the runs it was learned from, and
          episodes open in the Explorer.
        </EmptyState>
      ) : (
        <>
          {parts.advice && (
            <PartCard title="Advice" tool="get_advice" part={tips} rows={3}>
              {(text) => {
                // The session line is for an agent; the reader gets the advice, and the session apart.
                const session = SESSION.exec(text)?.[1];
                const advice = text.replace(/\n*session_id: .*$/s, "").trim();
                return (
                  <>
                    <Markdown text={advice} />
                    {session && (
                      <div className="muted ask-session">
                        session <span className="mono">{short(session)}</span>
                      </div>
                    )}
                  </>
                );
              }}
            </PartCard>
          )}
          {parts.facts && (
            <PartCard title="Facts" tool="search_memory" part={found} rows={5}>
              {(f) =>
                f.results.length ? (
                  <>
                    <div className="muted ask-head">{f.head}</div>
                    {f.results.map((r) => (
                      <div key={r.entity} className="ask-result">
                        <div className="ask-result-head">
                          <EntityLink entity={r.entity} onFocus={onFocus} />
                          <span className="muted">{r.when}</span>
                        </div>
                        <ul>
                          {r.lines.map((l, i) => (
                            <li key={i}>
                              <Marked line={l} sources={f.sources} onFocus={onFocus} />
                            </li>
                          ))}
                        </ul>
                      </div>
                    ))}
                  </>
                ) : (
                  <Markdown text={f.head || "Nothing matched."} />
                )
              }
            </PartCard>
          )}
          {parts.episodes && (
            <PartCard title="Episodes" tool="cited by search_memory" part={found} rows={3}>
              {() =>
                episodes.length ? (
                  <div className="ask-episodes">
                    {episodes.map((e) => (
                      <button
                        key={e.id}
                        className="chip"
                        onClick={() => onFocus({ kind: "episode", id: e.id })}
                      >
                        <Icon name="episode" size={12} />
                        <span className="chip-type">{humanize(e.type)}</span>
                        <span className="mono">{short(e.id)}</span>
                      </button>
                    ))}
                  </div>
                ) : (
                  <div className="muted">No episode is cited.</div>
                )
              }
            </PartCard>
          )}
        </>
      )}
    </section>
  );
}

function PartCard<T>({
  title,
  tool,
  part,
  rows,
  children,
}: {
  title: string;
  tool: string;
  part: Part<T>;
  rows: number;
  children: (data: T) => React.ReactNode;
}) {
  return (
    <>
      <div className="h-section">
        {title} <span className="count mono">{tool}</span>
        {part.status === "ok" && <span className="count">{(part.ms / 1000).toFixed(1)} s</span>}
      </div>
      <div className="card card-pad ask-card">
        {part.status === "loading" || part.status === "idle" ? (
          <Skeleton rows={rows} title={false} />
        ) : part.status === "error" ? (
          <ErrorState title={`${title}: the call failed`} message={part.message} />
        ) : (
          children(part.data)
        )}
      </div>
    </>
  );
}

/** A fact's line with its markers (`[e3 t14]`) as links to the episode they cite. */
export function Marked({
  line,
  sources,
  onFocus,
}: {
  line: string;
  sources: Found["sources"];
  onFocus: (f: Focus) => void;
}) {
  const out: React.ReactNode[] = [];
  let at = 0;
  for (const m of line.matchAll(/\[(e\d+)(?: [^\]]*)?\]/g)) {
    out.push(line.slice(at, m.index));
    const src = sources.get(m[1]!);
    out.push(
      src ? (
        <button
          key={m.index}
          className="link mono ask-cite"
          title={`${src.type} ${src.id}`}
          onClick={() => onFocus({ kind: "episode", id: src.id })}
        >
          {m[0]}
        </button>
      ) : (
        m[0]
      ),
    );
    at = m.index + m[0].length;
  }
  out.push(line.slice(at));
  return <>{out}</>;
}
