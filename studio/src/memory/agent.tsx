import { useEffect, useRef, useState } from "react";
import { retryable, transport, type Arm } from "../transport";
import { ApiError, call } from "./api";
import { RememberThis } from "./addToMemory";
import { Markdown } from "./markdown";
import type { Selection } from "./types";
import { EmptyState, ErrorState, Icon, Skeleton } from "./ui";
import { isSearch, Why } from "./why";

/** The memstore read tools an arm may give its agent, and what each does for it. */
export const TOOLS: [string, string][] = [
  ["search_memory", "Facts, with the episodes they cite"],
  ["get_episode", "Read a cited episode in full"],
  ["query_memory", "Raw episode search: passages, no facts"],
  ["get_advice", "Advice learned for this kind of task"],
  ["get_brief", "Where a subject stands"],
  ["list_briefs", "The subjects that have a brief"],
  ["list_facts", "Page facts by subject or predicate"],
  ["list_advice", "Page the org's advice"],
];
const MODELS = [
  ["haiku", "Haiku 5.5"],
  ["sonnet", "Sonnet 5.5"],
  ["opus", "Opus 5.5"],
] as const;
type Model = (typeof MODELS)[number][0];

export type { Arm };

/** The arms every org has: memstore's tools, raw episodes alone, and no memory, to compare against. */
const BUILTIN: Arm[] = [
  {
    id: "memstore",
    name: "AmpUp Memory tools",
    tools: ["search_memory", "get_episode", "get_advice", "get_brief"],
    model: "haiku",
    maxToolCalls: 8,
    builtin: true,
  },
  {
    id: "raw",
    name: "Raw episodes",
    tools: ["query_memory", "get_episode"],
    model: "haiku",
    maxToolCalls: 8,
    builtin: true,
  },
  { id: "none", name: "No memory", tools: [], model: "haiku", maxToolCalls: 1, builtin: true },
];

/** One tool call an agent made, as `/agent/run` traces it. */
interface Step {
  tool: string;
  args: Record<string, unknown>;
  ran: boolean;
  error: boolean;
  ms: number;
  chars: number;
  text: string;
}
interface Run {
  answer: string;
  stopped: string;
  model: string;
  steps: Step[];
  tokens: { in: number; out: number };
  cost: number | null;
  ms: number;
}
type Result =
  | { status: "loading" }
  | { status: "ok"; run: Run }
  /** `retry`: the server or a gateway gave up, or the connection dropped; asking again may well work. */
  | { status: "error"; message: string; retry: boolean };
/** One question, and what each arm it ran on answered. */
interface Turn {
  question: string;
  arms: Arm[];
  results: Record<string, Result>;
}

/** Where the org's own arms are kept (`Transport.arms`); without one, an arm made here lasts the page. */
const armStore = () => transport().arms;

/**
 * Ask's agent mode: a question answered by a model that calls the memory's MCP tools, on one or more
 * arms side by side (memstore's `/agent/run`, as the signed-in person). Each arm is a set of tools, a
 * model and a cap on tool calls; the org's own are saved, beside three every org has. Follow-ups carry
 * each arm's own earlier answers.
 */
export function Agent({ sel }: { sel: Selection }) {
  const [saved, setSaved] = useState<Arm[]>([]);
  const [armsError, setArmsError] = useState<string>();
  const [picked, setPicked] = useState<string[]>(["memstore", "raw"]);
  const [editing, setEditing] = useState<Arm | null>(null);
  const [draft, setDraft] = useState("");
  const [turns, setTurns] = useState<Turn[]>([]);
  const box = useRef<HTMLTextAreaElement>(null);
  // The composer's height, measured (its textarea resizes): the conversation keeps that much room at its end, so
  // the last answer scrolls fully clear of the composer pinned over it.
  const composer = useRef<HTMLFormElement>(null);
  const [composerH, setComposerH] = useState(0);
  useEffect(() => {
    const el = composer.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => setComposerH(el.offsetHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const arms = [...BUILTIN, ...saved];
  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);

  useEffect(() => {
    const store = armStore();
    if (!store) return;
    let live = true;
    store
      .list(sel)
      .then((r) => live && setSaved(r))
      .catch((e: Error) => live && setArmsError(e.message));
    return () => {
      live = false;
    };
  }, [sel.org]);
  // Another memory, another conversation.
  useEffect(() => setTurns([]), [selKey]);
  useEffect(() => box.current?.focus(), []);

  const running = turns.some((t) => Object.values(t.results).some((r) => r.status === "loading"));
  const chosen = arms.filter((a) => picked.includes(a.id));

  /** Runs `arm` on turn `at`'s question, continuing that arm's own conversation, into the turn's results. */
  const runArm = (at: number, question: string, arm: Arm, earlier: Turn[]) => {
    const set = (r: Result) =>
      setTurns((ts) =>
        ts.map((t, i) => (i === at ? { ...t, results: { ...t.results, [arm.id]: r } } : t)),
      );
    set({ status: "loading" });
    // Each arm continues its own conversation: the questions it answered before, with its answers.
    const history = earlier.flatMap((t) => {
      const r = t.results[arm.id];
      return r?.status === "ok" ? [{ question: t.question, answer: r.run.answer }] : [];
    });
    call<Run>(sel, "agent", {
      question,
      tools: arm.tools,
      model: arm.model,
      maxToolCalls: arm.maxToolCalls,
      ...(history.length ? { history: history.slice(-10) } : {}),
    })
      .then((run) => ({ status: "ok", run }) as Result)
      .catch(
        (e: Error) =>
          ({
            status: "error",
            message:
              e instanceof TypeError
                ? "The connection dropped before the run answered. Try again."
                : e.message,
            retry: retryable(e),
          }) as Result,
      )
      .then(set);
  };

  const ask = (question: string) => {
    const at = turns.length;
    const turn: Turn = {
      question,
      arms: chosen,
      results: Object.fromEntries(chosen.map((a) => [a.id, { status: "loading" } as Result])),
    };
    setTurns((t) => [...t, turn]);
    setDraft("");
    for (const arm of chosen) runArm(at, question, arm, turns);
  };

  const save = async (arm: Arm) => {
    const { id, name, tools, model, maxToolCalls } = arm;
    const body = { ...(arm.builtin || !id ? {} : { id }), name, tools, model, maxToolCalls };
    const store = armStore();
    const out = store
      ? await store.save(sel, body)
      : { ...body, id: body.id ?? `page-${Date.now().toString(36)}` };
    setSaved((s) =>
      s.some((a) => a.id === out.id) ? s.map((a) => (a.id === out.id ? out : a)) : [...s, out],
    );
    setPicked((p) => (p.includes(out.id) ? p : [...p.filter((x) => x !== arm.id), out.id]));
    setEditing(null);
  };
  const remove = async (arm: Arm) => {
    await armStore()?.remove(sel, arm.id);
    setSaved((s) => s.filter((a) => a.id !== arm.id));
    setPicked((p) => p.filter((x) => x !== arm.id));
    setEditing(null);
  };

  return (
    <>
      <div className="agent-arms card card-pad">
        <div className="agent-arms-head">
          <div>
            <div className="agent-label">Arms</div>
            <div className="muted agent-hint">
              Each question runs on every arm picked, side by side.
            </div>
          </div>
          <button
            className="btn"
            type="button"
            onClick={() =>
              setEditing({
                id: "",
                name: "",
                tools: ["search_memory"],
                model: "haiku",
                maxToolCalls: 8,
              })
            }
          >
            New arm
          </button>
        </div>
        <div className="agent-arm-list" role="group" aria-label="Arms to run on">
          {arms.map((a) => (
            <div key={a.id} className={`agent-arm ${picked.includes(a.id) ? "on" : ""}`}>
              <label className="agent-arm-pick">
                <input
                  type="checkbox"
                  checked={picked.includes(a.id)}
                  onChange={() =>
                    setPicked((p) =>
                      p.includes(a.id) ? p.filter((x) => x !== a.id) : [...p, a.id],
                    )
                  }
                />
                <span className="agent-arm-name">{a.name}</span>
                <span className="muted agent-arm-meta">
                  {a.tools.length ? `${a.tools.length} tools` : "no tools"} · {modelName(a.model)}
                </span>
              </label>
              <button
                className="btn agent-quiet agent-small"
                type="button"
                aria-label={`Edit ${a.name}`}
                onClick={() => setEditing(a.builtin ? { ...a, name: `${a.name} (copy)` } : a)}
              >
                {a.builtin ? "Copy" : "Edit"}
              </button>
            </div>
          ))}
        </div>
        {armsError && <div className="error-text">Saved arms didn't load: {armsError}</div>}
        {editing && (
          <ArmEditor
            arm={editing}
            onSave={save}
            onDelete={editing.id && !editing.builtin ? () => remove(editing) : undefined}
            onCancel={() => setEditing(null)}
          />
        )}
      </div>

      <div
        className={`agent-convo ${turns.length ? "agent-convo-on" : ""}`}
        style={{ "--agent-composer-h": `${composerH}px` } as React.CSSProperties}
      >
        {turns.length === 0 ? (
          <EmptyState icon="chat" title="Ask the agent anything about the org">
            Pick the arms to compare: each answers with only its own tools, and shows every call it
            made.
          </EmptyState>
        ) : (
          <div className="agent-turns">
            {turns.map((t, i) => (
              <TurnView
                key={i}
                turn={t}
                sel={sel}
                retry={(arm) => runArm(i, t.question, arm, turns.slice(0, i))}
              />
            ))}
          </div>
        )}

        <form
          ref={composer}
          className="ask-form card card-pad agent-composer"
          onSubmit={(e) => {
            e.preventDefault();
            if (draft.trim() && chosen.length && !running) ask(draft.trim());
          }}
        >
          <textarea
            ref={box}
            className="args ask-box"
            rows={2}
            placeholder={
              turns.length ? "Ask a follow-up…" : "What did we decide about who can join the org?"
            }
            aria-label="Question"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (
                e.key === "Enter" &&
                (e.metaKey || e.ctrlKey) &&
                draft.trim() &&
                chosen.length &&
                !running
              )
                ask(draft.trim());
            }}
          />
          <div className="ask-row">
            <span className="muted">
              {chosen.length
                ? `On ${chosen.map((a) => a.name).join(", ")}`
                : "Pick at least one arm"}
            </span>
            {turns.length > 0 && (
              <button
                className="btn agent-quiet"
                type="button"
                disabled={running}
                onClick={() => setTurns([])}
              >
                New conversation
              </button>
            )}
            <span className="muted">⌘↩</span>
            <button
              className="btn btn-primary"
              type="submit"
              disabled={!draft.trim() || !chosen.length || running}
            >
              <Icon name="play" size={13} /> {running ? "Running…" : "Ask"}
            </button>
          </div>
        </form>
      </div>
    </>
  );
}

const modelName = (m: string) => MODELS.find(([id]) => id === m)?.[1] ?? m;

function ArmEditor({
  arm,
  onSave,
  onDelete,
  onCancel,
}: {
  arm: Arm;
  onSave: (a: Arm) => Promise<void>;
  onDelete?: () => Promise<void>;
  onCancel: () => void;
}) {
  const [a, setA] = useState<Arm>(arm);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => setA(arm), [arm]);
  const act = (f: () => Promise<void>) => {
    setBusy(true);
    setError(undefined);
    f()
      .catch((e: Error) => setError(e instanceof ApiError ? e.message : String(e)))
      .finally(() => setBusy(false));
  };
  return (
    <form
      className="agent-editor"
      aria-label={arm.id && !arm.builtin ? `Edit ${arm.name}` : "New arm"}
      onSubmit={(e) => {
        e.preventDefault();
        act(() => onSave(a));
      }}
    >
      <div className="agent-editor-row">
        <label className="agent-field">
          <span>Name</span>
          <input
            className="field"
            required
            maxLength={60}
            value={a.name}
            onChange={(e) => setA({ ...a, name: e.target.value })}
          />
        </label>
        <label className="agent-field">
          <span>Model</span>
          <select
            className="field"
            value={a.model}
            onChange={(e) => setA({ ...a, model: e.target.value as Model })}
          >
            {MODELS.map(([id, label]) => (
              <option key={id} value={id}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <label className="agent-field agent-field-sm">
          <span>Max tool calls</span>
          <input
            className="field"
            type="number"
            min={1}
            max={12}
            value={a.maxToolCalls}
            onChange={(e) =>
              setA({
                ...a,
                maxToolCalls: Math.max(1, Math.min(12, Number(e.target.value) || 1)),
              })
            }
          />
        </label>
      </div>
      <fieldset className="agent-tools">
        <legend className="agent-label">Tools the agent may call</legend>
        {TOOLS.map(([name, what]) => (
          <label key={name} className="agent-tool">
            <input
              type="checkbox"
              checked={a.tools.includes(name)}
              onChange={() =>
                setA({
                  ...a,
                  tools: a.tools.includes(name)
                    ? a.tools.filter((t) => t !== name)
                    : [...a.tools, name],
                })
              }
            />
            <span className="mono">{name}</span>
            <span className="muted">{what}</span>
          </label>
        ))}
      </fieldset>
      {error && <div className="error-text">{error}</div>}
      <div className="ask-row">
        {onDelete && (
          <button
            className="btn agent-danger"
            type="button"
            disabled={busy}
            onClick={() => act(onDelete)}
          >
            Delete
          </button>
        )}
        <button className="btn agent-quiet" type="button" onClick={onCancel}>
          Cancel
        </button>
        <button className="btn btn-primary" type="submit" disabled={busy || !a.name.trim()}>
          Save arm
        </button>
      </div>
    </form>
  );
}

function TurnView({
  turn,
  sel,
  retry,
}: {
  turn: Turn;
  sel: Selection;
  /** Runs an arm of this turn again, after it failed. */
  retry: (arm: Arm) => void;
}) {
  return (
    <div className="agent-turn">
      <div className="agent-question">{turn.question}</div>
      <div className="agent-grid">
        {turn.arms.map((arm) => (
          <ArmResult
            key={arm.id}
            arm={arm}
            result={turn.results[arm.id]!}
            sel={sel}
            retry={() => retry(arm)}
            remember={(answer) => (
              <RememberThis sel={sel} question={turn.question} answer={answer} />
            )}
          />
        ))}
      </div>
    </div>
  );
}

const STOPPED: Record<string, string> = {
  "tool budget": "Stopped at its tool-call cap",
  "time budget": "Stopped at the time limit",
  "model error": "The model failed",
};

function ArmResult({
  arm,
  result,
  sel,
  remember,
  retry,
}: {
  arm: Arm;
  result: Result;
  sel: Selection;
  retry: () => void;
  /** "Remember this" for its answer. */
  remember?: (answer: string) => React.ReactNode;
}) {
  return (
    <section className="card agent-result" aria-label={arm.name}>
      <header className="agent-result-head">
        <div className="agent-result-title">
          <span className="agent-arm-name">{arm.name}</span>
          <span className="muted">{modelName(arm.model)}</span>
        </div>
        <div className="agent-chips">
          {arm.tools.length ? (
            arm.tools.map((t) => (
              <span key={t} className="chip mono">
                {t}
              </span>
            ))
          ) : (
            <span className="chip">no tools</span>
          )}
        </div>
      </header>
      {result.status === "loading" ? (
        <div className="card-pad">
          <Skeleton rows={4} title={false} />
        </div>
      ) : result.status === "error" ? (
        <div className="card-pad">
          <ErrorState
            title="The run failed"
            message={result.message}
            onRetry={result.retry ? retry : undefined}
          />
        </div>
      ) : (
        <>
          <RunView run={result.run} />
          {result.run.answer && remember?.(result.run.answer)}
          <WhyAnswer run={result.run} sel={sel} />
        </>
      )}
    </section>
  );
}

function RunView({ run }: { run: Run }) {
  const ran = run.steps.filter((s) => s.ran).length;
  return (
    <>
      <dl className="agent-stats">
        <div>
          <dt>Tool calls</dt>
          <dd>{ran}</dd>
        </div>
        <div>
          <dt>Time</dt>
          <dd>{(run.ms / 1000).toFixed(1)} s</dd>
        </div>
        <div>
          <dt>Tokens in</dt>
          <dd>{run.tokens.in >= 1000 ? `${(run.tokens.in / 1000).toFixed(1)}k` : run.tokens.in}</dd>
        </div>
        <div>
          <dt>Cost</dt>
          <dd>{run.cost == null ? "—" : `$${run.cost.toFixed(4)}`}</dd>
        </div>
      </dl>
      {run.steps.length > 0 && (
        <details className="agent-trace">
          <summary>
            Trace <span className="muted">{run.steps.length} calls</span>
          </summary>
          <ol>
            {run.steps.map((s, i) => (
              <li key={i} className={s.ran ? (s.error ? "err" : "") : "refused"}>
                <details>
                  <summary>
                    <span className="mono agent-step-tool">{s.tool}</span>
                    <span className="mono muted agent-step-args">{argsText(s.args)}</span>
                    <span className="muted agent-step-meta">
                      {s.ran ? `${s.ms} ms · ${s.chars.toLocaleString()} chars` : "not run"}
                    </span>
                  </summary>
                  <pre className="agent-step-text">{s.text}</pre>
                </details>
              </li>
            ))}
          </ol>
        </details>
      )}
      <div className="card-pad agent-answer">
        {run.stopped !== "answered" && (
          <div className="agent-stopped">{STOPPED[run.stopped] ?? run.stopped}</div>
        )}
        {run.answer ? <Markdown text={run.answer} /> : <span className="muted">No answer.</span>}
      </div>
    </>
  );
}

/** "Why this answer": each fact search the agent made, explained (why.tsx) once opened. */
function WhyAnswer({ run, sel }: { run: Run; sel: Selection }) {
  const searches = run.steps.filter(
    (s) => s.ran && !s.error && isSearch(s.tool) && typeof s.args.query === "string",
  );
  // Explained once opened: a closed one asks memory nothing.
  const [open, setOpen] = useState(false);
  if (!searches.length) return null;
  return (
    <details className="why-answer" onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary>
        Why this answer{" "}
        <span className="muted">
          {searches.length} memory {searches.length === 1 ? "search" : "searches"}
        </span>
      </summary>
      {open &&
        searches.map((s, i) => (
          <Why
            key={i}
            sel={sel}
            args={s.args}
            title={
              searches.length > 1 ? `Search ${i + 1} of ${searches.length}` : "What memory found"
            }
          />
        ))}
    </details>
  );
}

/** A call's arguments in one line: its query or subject first, as an agent would name it. */
function argsText(args: Record<string, unknown>): string {
  const v = args.query ?? args.subject ?? args.episode_id ?? args.question;
  const s = typeof v === "string" ? JSON.stringify(v) : JSON.stringify(v ?? args);
  return s.length > 90 ? `${s.slice(0, 89)}…` : s;
}
