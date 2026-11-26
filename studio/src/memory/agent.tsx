import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { retryable, transport, type Arm } from "../transport";
import { browse, call } from "./api";
import { AddToMemoryDialog, RememberThis } from "./addToMemory";
import { advice, Marked, SESSION, tool } from "./ask";
import { entityName, humanize, short, when } from "./format";
import { EntityLink } from "./layouts";
import { Markdown } from "./markdown";
import {
  citeIndex,
  citeParts,
  readSearch,
  resultTeam,
  searchTexts,
  type Cited,
  type ContextMessage,
  type Found,
} from "./searchText";
import { adviceStarters, type AdviceEntry, type Starter } from "./starters";
import type { Fact, Focus, Listing, Selection } from "./types";
import { ErrorState, Icon, Skeleton } from "./ui";
import { isSearch, Why } from "./why";
import "./playground.css";

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
/** The models a run may use: memstore's name for each, what it's called, and the OpenRouter model a run names. */
const MODELS = [
  ["haiku", "Haiku 5.5", "anthropic/claude-haiku-5.5"],
  ["sonnet", "Sonnet 5.5", "anthropic/claude-sonnet-5.5"],
  ["opus", "Opus 5.5", "anthropic/claude-opus-5.5"],
  ["fable", "Fable 5.2", "anthropic/claude-fable-5.2"],
  ["gpt-sol", "GPT-6.2 Sol", "openai/gpt-6.2-sol"],
  ["gpt-terra", "GPT-6.1 Terra", "openai/gpt-6.1-terra"],
  ["deepseek", "DeepSeek V4 Pro", "deepseek/deepseek-v4-pro-0813"],
  ["qwen", "Qwen4 Max", "qwen/qwen4-max"],
] as const;
type Model = (typeof MODELS)[number][0];

export type { Arm };

/** Where this browser keeps whether the reader closed the settings rail. */
const RAIL_KEY = "ampup-memory.playground.rail";

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
  {
    id: "none",
    name: "No memory",
    tools: [],
    model: "haiku",
    maxToolCalls: 1,
    builtin: true,
  },
];
/** The memory presets the settings rail offers as one switch: each a built-in arm's tools. */
const PRESETS: [string, string][] = [
  ["memstore", "Memory tools"],
  ["raw", "Raw episodes"],
  ["none", "No memory"],
];
/** Questions to start from, on an empty conversation, where the memory has learned no advice yet. */
const SUGGESTIONS: [string, string][] = [
  ["user", "What am I working on this week?"],
  ["history", "What did we decide last week?"],
  ["chat", "What changed in sign-in lately?"],
];
/** What an answer's column is keyed by: the conversation's own settings, beside the arms compared with it. */
const PRIMARY = "primary";

/** Whether two arms' names read as the same one. */
const same = (a: string, b: string) => a.trim().toLowerCase() === b.trim().toLowerCase();
const sameTools = (a: string[], b: string[]) =>
  a.length === b.length && a.every((t) => b.includes(t));
const modelName = (m: string) => MODELS.find(([id, , or]) => id === m || or === m)?.[1] ?? m;
const toolsText = (n: number) => (n ? `${n} ${n === 1 ? "tool" : "tools"}` : "no tools");

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
/** What the model was given (memstore's `/agent/run` sends it from late 2026; older servers don't). */
interface Context {
  messages: ContextMessage[];
  tools: { function?: { name?: string; description?: string } }[];
}
interface Run {
  context?: Context;
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
/** One question, and what each arm it ran on answered (the conversation's own settings under PRIMARY). */
interface Turn {
  question: string;
  arms: Record<string, Arm>;
  results: Record<string, Result>;
}

/** Where the org's own arms are kept (`Transport.arms`); without one, an arm made here lasts the page. */
const armStore = () => transport().arms;

const DEFAULT: Arm = { ...BUILTIN[0]!, id: PRIMARY, builtin: false };
type Mode = "chat" | "search";

/**
 * The playground: a question answered by a model that calls the memory's MCP tools (Chat), or the memory searched
 * as an agent's search_memory call returns it (Search). Chat's settings, in the rail beside it, are one arm: its
 * tools, model and cap on tool calls, loaded from or saved as one of the org's arms; each question can also run on
 * other arms ("Compare with"), answered in columns side by side. Follow-ups carry each arm's own earlier answers.
 */
export function Agent({
  sel,
  onFocus,
  addToMemory,
  connect,
  teams = [],
  onScope,
}: {
  sel: Selection;
  onFocus?: (f: Focus) => void;
  /** The teams whose memory the reader may ask, besides the org's own: the namespaces a question is scoped to. */
  teams?: string[];
  /** Ask in another team's memory (none: the org's), and open `then` there. */
  onScope?: (team: string | undefined, then?: Focus) => void;
  /** The host's "Add to memory" control, for an empty memory's first step. */
  addToMemory?: React.ReactNode;
  /** Where the host connects a source and an agent (as Home's `connect`). */
  connect?: { sources?: string; agents?: string };
}) {
  const empty = useEmptyMemory(sel);
  const starters = useStarters(sel);
  const [mode, setMode] = useState<Mode>("chat");
  const [saved, setSaved] = useState<Arm[]>([]);
  const [armsError, setArmsError] = useState<string>();
  // The conversation's own settings; `from` is the saved or built-in arm they were loaded from.
  const [cfg, setCfg] = useState<Arm>(DEFAULT);
  const [from, setFrom] = useState<string>("memstore");
  const [compare, setCompare] = useState<string[]>([]);
  const [draft, setDraft] = useState("");
  const [turns, setTurns] = useState<Turn[]>([]);
  const [search, setSearch] = useState<SearchCfg>(SEARCH_DEFAULT);
  const [searched, setSearched] = useState("");
  // The rail shows what the model was given for the latest answer, in place of the settings.
  const [raw, setRaw] = useState(false);
  // The settings rail, open unless this reader closed it (kept in this browser).
  const [rail, setRail] = useState(() => {
    try {
      return localStorage.getItem(RAIL_KEY) !== "closed";
    } catch {
      return true;
    }
  });
  const showRail = (open: boolean) => {
    setRail(open);
    try {
      localStorage.setItem(RAIL_KEY, open ? "open" : "closed");
    } catch {
      // Without storage, the rail is open again on the next visit.
    }
  };
  const box = useRef<HTMLTextAreaElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const root = useFill<HTMLDivElement>();
  const arms = [...BUILTIN, ...saved];
  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);
  const open = (f: Focus) => onFocus?.(f);
  /** Opens `f` in `team`'s memory, where a result the agent found is (none: the one being asked). */
  const openIn = (f: Focus, team?: string) =>
    team && team !== sel.team && onScope ? onScope(team, f) : open(f);

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
  useEffect(() => {
    setTurns([]);
    setSearched("");
  }, [selKey]);
  useEffect(() => box.current?.focus(), [mode]);
  // A new question scrolls the conversation (not the page) to its end.
  useEffect(() => {
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [turns.length]);

  const running = turns.some((t) => Object.values(t.results).some((r) => r.status === "loading"));
  const compared = arms.filter((a) => compare.includes(a.id));

  /** Runs `arm` (under `key`) on turn `at`'s question, continuing that arm's own conversation. */
  const runArm = (at: number, question: string, key: string, arm: Arm, earlier: Turn[]) => {
    const set = (r: Result) =>
      setTurns((ts) =>
        ts.map((t, i) =>
          i === at
            ? {
                ...t,
                arms: { ...t.arms, [key]: arm },
                results: { ...t.results, [key]: r },
              }
            : t,
        ),
      );
    set({ status: "loading" });
    const history = earlier.flatMap((t) => {
      const r = t.results[key];
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
    if (!question.trim() || running) return;
    const at = turns.length;
    const runs: [string, Arm][] = [
      [PRIMARY, cfg],
      ...compared.map((a) => [a.id, a] as [string, Arm]),
    ];
    setTurns((t) => [
      ...t,
      {
        question,
        arms: Object.fromEntries(runs),
        results: Object.fromEntries(runs.map(([k]) => [k, { status: "loading" } as Result])),
      },
    ]);
    setDraft("");
    for (const [key, arm] of runs) runArm(at, question, key, arm, turns);
  };

  /** "Run without memory" on one answer: that question on the No memory arm, in its own column. */
  const withoutMemory = (at: number) => {
    const none = BUILTIN[2]!;
    if (!compare.includes(none.id)) setCompare((c) => [...c, none.id]);
    runArm(at, turns[at]!.question, none.id, none, turns.slice(0, at));
  };

  const load = (arm: Arm) => {
    setCfg({ ...arm, id: PRIMARY, builtin: false });
    setFrom(arm.id);
    setCompare((c) => c.filter((x) => x !== arm.id));
  };
  const loaded = arms.find((a) => a.id === from);
  const dirty =
    !loaded ||
    loaded.model !== cfg.model ||
    loaded.maxToolCalls !== cfg.maxToolCalls ||
    !sameTools(loaded.tools, cfg.tools);

  const save = async (name: string, asNew: boolean) => {
    const was = !asNew && loaded && !loaded.builtin ? loaded : undefined;
    // Two arms of one name read as one in the menu and the columns: a new arm, or a rename, may not take a name
    // already here.
    if (!was || !same(was.name, name)) {
      const taken = arms.find((b) => same(b.name, name) && b !== was);
      if (taken)
        throw new Error(`An arm named “${taken.name}” is already here: name this one apart.`);
    }
    const { tools, model, maxToolCalls } = cfg;
    const body = {
      ...(was ? { id: was.id } : {}),
      name,
      tools,
      model,
      maxToolCalls,
    };
    const store = armStore();
    const out = store
      ? await store.save(sel, body)
      : { ...body, id: body.id ?? `page-${Date.now().toString(36)}` };
    setSaved((s) =>
      s.some((a) => a.id === out.id) ? s.map((a) => (a.id === out.id ? out : a)) : [...s, out],
    );
    setFrom(out.id);
    setCfg((c) => ({ ...c, name: out.name }));
  };
  const remove = async (arm: Arm) => {
    await armStore()?.remove(sel, arm.id);
    setSaved((s) => s.filter((a) => a.id !== arm.id));
    setCompare((c) => c.filter((x) => x !== arm.id));
    load(BUILTIN[0]!);
  };
  const toggleNone = () =>
    setCompare((c) => (c.includes("none") ? c.filter((x) => x !== "none") : [...c, "none"]));

  // The columns: the conversation's own, then each arm compared with it.
  const columns = [PRIMARY, ...compare.filter((id) => arms.some((a) => a.id === id))];
  // The namespace a question is asked in: the org's memory or one of its teams', picked here where there is a choice.
  const branch = sel.branch && sel.branch !== "main" ? ` · ${sel.branch}` : "";
  const scope =
    onScope && (teams.length > 0 || sel.team) ? (
      <>
        <select
          className="pg-scope-pick mono"
          aria-label="Namespace"
          title="The memory a question is asked in: the org's, or one team's"
          value={sel.team ?? ""}
          onChange={(e) => onScope(e.target.value || undefined)}
        >
          <option value="">{sel.org}</option>
          {[...new Set([...(sel.team ? [sel.team] : []), ...teams])].map((t) => (
            <option key={t} value={t}>
              {sel.org} / {t}
            </option>
          ))}
        </select>
        {branch}
      </>
    ) : (
      `${sel.org}${sel.team ? ` / ${sel.team}` : ""}${branch}`
    );
  const composer = (placeholder: string) => (
    <Composer
      boxRef={box}
      draft={draft}
      setDraft={setDraft}
      onSend={() => ask(draft.trim())}
      disabled={running}
      placeholder={placeholder}
      scope={scope}
      left={
        <>
          <AddToMemoryDialog sel={sel} onOpen={onFocus} compact />
          <ArmMenu
            cfg={cfg}
            arms={arms}
            from={from}
            dirty={dirty}
            onModel={(model) => setCfg({ ...cfg, model })}
            onLoad={load}
          />
        </>
      }
      right={<CompareToggle on={compare.includes("none")} onToggle={toggleNone} />}
    />
  );

  return (
    <div className="pg" ref={root}>
      <header className="pg-head">
        <h1 className="pg-title">Playground</h1>
        <p className="pg-sub">
          Chat with your memory as an agent would, or search it. Every answer shows the tool calls
          behind it.
        </p>
      </header>
      <div className={`pg-body ${rail ? "" : "pg-body-full"}`}>
        <section className="pg-main" aria-label="Playground">
          <div className="pg-bar">
            <div className="pg-tabs" role="tablist" aria-label="Playground mode">
              {(
                [
                  ["chat", "Chat", "chat"],
                  ["search", "Search", "search"],
                ] as const
              ).map(([m, label, icon]) => (
                <button
                  key={m}
                  type="button"
                  role="tab"
                  className="pg-tab"
                  aria-selected={mode === m}
                  onClick={() => setMode(m)}
                >
                  <Icon name={icon} size={14} /> {label}
                </button>
              ))}
            </div>
            <div className="pg-bar-actions">
              {!rail && (
                <button
                  type="button"
                  className="pg-link"
                  aria-label={mode === "search" ? "Show search settings" : "Show memory settings"}
                  onClick={() => showRail(true)}
                >
                  <Icon name="panel" size={14} /> Settings
                </button>
              )}
              {mode === "chat" && (
                <button
                  type="button"
                  className="pg-link"
                  aria-pressed={raw && rail}
                  onClick={() => {
                    if (raw && rail) setRaw(false);
                    else {
                      setRaw(true);
                      showRail(true);
                    }
                  }}
                >
                  {raw && rail ? "Hide raw context" : "Raw context"}
                </button>
              )}
              {mode === "chat" && turns.length > 0 && (
                <button
                  type="button"
                  className="pg-link"
                  disabled={running}
                  onClick={() => setTurns([])}
                >
                  Clear chat
                </button>
              )}
              {mode === "search" && searched && (
                <button type="button" className="pg-link" onClick={() => setSearched("")}>
                  Clear results
                </button>
              )}
            </div>
          </div>

          {empty && (mode === "chat" ? turns.length === 0 : !searched) ? (
            <div className="pg-scroll">
              <EmptyPlayground addToMemory={addToMemory} connect={connect} />
            </div>
          ) : mode === "chat" ? (
            turns.length === 0 ? (
              <div className="pg-scroll">
                <Hero title="See what AmpUp Memory can do">
                  {composer("Ask your memory…")}
                  <div className="pg-suggest" aria-label="Questions to start from">
                    {(starters ?? []).map((s) => (
                      <button
                        key={s.question}
                        type="button"
                        className="pg-chip"
                        title={s.situation ? adviceTitle(s) : s.question}
                        onClick={() => ask(s.asks ?? s.question)}
                      >
                        <Icon
                          name={
                            s.situation
                              ? "skill"
                              : (SUGGESTIONS.find(([, q]) => q === s.question)?.[0] ?? "chat")
                          }
                          size={13}
                        />
                        <span className="pg-chip-text">{s.question}</span>
                      </button>
                    ))}
                  </div>
                </Hero>
              </div>
            ) : (
              <>
                <div className="pg-scroll" ref={scroller}>
                  <div
                    className={`pg-cols ${columns.length > 1 ? "pg-cols-many" : ""}`}
                    style={{ "--pg-cols": columns.length } as React.CSSProperties}
                  >
                    {/* Compared, each column says what it is; a row per question keeps the answers side by side. */}
                    {columns.length > 1 &&
                      columns.map((key, c) => {
                        const arm = key === PRIMARY ? cfg : arms.find((a) => a.id === key)!;
                        return (
                          <div
                            key={`h-${key}`}
                            className={`pg-col-head ${c ? "pg-cell-side" : ""}`}
                          >
                            <div>
                              <div className="pg-col-title">
                                {key === PRIMARY
                                  ? dirty
                                    ? "Your settings"
                                    : (loaded?.name ?? "Your settings")
                                  : arm.name}
                              </div>
                              <div className="pg-col-meta">
                                {modelName(arm.model)} · {toolsText(arm.tools.length)}
                                {arm.tools.length === 0 && " · nothing looked up"}
                              </div>
                            </div>
                            {key !== PRIMARY && (
                              <button
                                type="button"
                                className="pg-link"
                                onClick={() => setCompare((cs) => cs.filter((x) => x !== key))}
                              >
                                Hide
                              </button>
                            )}
                          </div>
                        );
                      })}
                    {turns.map((t, i) =>
                      columns.map((key, c) => {
                        const arm =
                          t.arms[key] ?? (key === PRIMARY ? cfg : arms.find((a) => a.id === key)!);
                        const result = t.results[key];
                        return (
                          <div key={`${i}-${key}`} className={`pg-turn ${c ? "pg-cell-side" : ""}`}>
                            <div className="pg-question">{t.question}</div>
                            {result ? (
                              <Answer
                                arm={arm}
                                result={result}
                                sel={sel}
                                question={t.question}
                                onFocus={openIn}
                                retry={() => runArm(i, t.question, key, arm, turns.slice(0, i))}
                                withoutMemory={
                                  key === PRIMARY &&
                                  !t.results.none &&
                                  (t.arms[PRIMARY]?.tools.length ?? 0) > 0
                                    ? () => withoutMemory(i)
                                    : undefined
                                }
                              />
                            ) : (
                              <div className="pg-muted pg-not-run">
                                Not asked on this arm.{" "}
                                <button
                                  type="button"
                                  className="pg-link"
                                  disabled={running}
                                  onClick={() => runArm(i, t.question, key, arm, turns.slice(0, i))}
                                >
                                  Ask it
                                </button>
                              </div>
                            )}
                          </div>
                        );
                      }),
                    )}
                  </div>
                </div>
                <div className="pg-dock">{composer("Ask a follow-up…")}</div>
              </>
            )
          ) : (
            <SearchPane
              sel={sel}
              cfg={search}
              boxRef={box}
              question={searched}
              onSearch={setSearched}
              onFocus={open}
              scope={scope}
            />
          )}
        </section>

        {rail && (
          <aside
            className="pg-rail"
            aria-label={
              mode === "search" ? "Search settings" : raw ? "Raw context" : "Memory settings"
            }
          >
            {mode === "chat" && raw ? (
              <RawContext run={latest(turns)} onClose={() => setRaw(false)} />
            ) : mode === "chat" ? (
              <ChatSettings
                org={sel.org}
                cfg={cfg}
                setCfg={setCfg}
                arms={arms}
                from={from}
                loaded={loaded}
                dirty={dirty}
                compare={compare}
                setCompare={setCompare}
                onLoad={load}
                onReset={() => {
                  load(BUILTIN[0]!);
                  setCompare([]);
                }}
                onSave={save}
                onDelete={loaded && !loaded.builtin ? () => remove(loaded) : undefined}
                armsError={armsError}
                question={draft.trim() || turns.at(-1)?.question || ""}
                onCollapse={() => showRail(false)}
              />
            ) : (
              <SearchSettings
                org={sel.org}
                cfg={search}
                setCfg={setSearch}
                question={searched}
                onReset={() => setSearch(SEARCH_DEFAULT)}
                onCollapse={() => showRail(false)}
              />
            )}
          </aside>
        )}
      </div>
    </div>
  );
}

/**
 * Fills the window down from where the playground starts, where the page scrolls as a whole (a console hosting the
 * pages): the conversation then scrolls inside it, under a composer that stays. Where the pages are their own app,
 * the pane they sit in is already the window's height, and the stylesheet fills it.
 */
function useFill<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    const pane = el?.closest(".pane");
    if (!el || !pane || getComputedStyle(pane).overflowY !== "visible") return;
    const inner = el.closest<HTMLElement>(".pane-inner");
    const fit = () => {
      if (matchMedia("(max-width: 1000px)").matches) {
        el.style.height = "";
        el.style.flex = "";
        if (inner) inner.style.paddingBottom = "";
        return;
      }
      // The pane's own space under it goes; what the host keeps under the pane stays, and is at least a gutter.
      if (inner) inner.style.paddingBottom = "0px";
      const box = el.getBoundingClientRect();
      const top = box.top + window.scrollY;
      const below = document.documentElement.scrollHeight - (box.bottom + window.scrollY);
      // Its own height, not the pane's to share: the pane grows with its content here.
      el.style.flex = "none";
      el.style.height = `${Math.max(520, window.innerHeight - top - Math.max(below, 16))}px`;
    };
    fit();
    // Again once what's above it has settled (the host's bars, fonts), and whenever the window or page changes.
    const later = requestAnimationFrame(fit);
    addEventListener("resize", fit);
    return () => {
      cancelAnimationFrame(later);
      removeEventListener("resize", fit);
      if (inner) inner.style.paddingBottom = "";
    };
  }, []);
  return ref;
}

/** The empty page's middle: a mark, a line, and what's under it (the composer, the suggestions). */
/**
 * Whether the memory holds nothing yet, as Home counts it (no sources and no entities): `undefined` until the
 * root's counts answer, and `false` when they can't be read, so the playground opens as usual.
 */
function useEmptyMemory(sel: Selection): boolean | undefined {
  const [empty, setEmpty] = useState<boolean>();
  useEffect(() => {
    let live = true;
    setEmpty(undefined);
    browse<Listing>({ op: "ls", path: [] }, sel).then(
      (root) => {
        const count = (name: string) => root.children.find((c) => c.name === name)?.count;
        if (live) setEmpty(count("sources") === 0 && count("entities") === 0);
      },
      () => live && setEmpty(false),
    );
    return () => {
      live = false;
    };
  }, [sel.org, sel.team, sel.branch]);
  return empty;
}

/**
 * The questions an empty conversation offers: from the situations the memory learned advice for, best backed first
 * (`adviceStarters`), filled up with the fixed ones where it has fewer than three. `undefined` until list_advice
 * answers; the fixed ones alone where it can't be read.
 */
function useStarters(sel: Selection): Starter[] | undefined {
  const [starters, setStarters] = useState<Starter[]>();
  useEffect(() => {
    let live = true;
    const fill = (learned: Starter[]) =>
      [...learned, ...SUGGESTIONS.map(([, question]) => ({ question }))].slice(0, 3);
    setStarters(undefined);
    const read = (args: Record<string, unknown>) =>
      tool(sel, "list_advice", { ...args, limit: 200 }).then(
        (text) => (JSON.parse(text || "{}") as { advice?: AdviceEntry[] }).advice ?? [],
      );
    // The best backed rank first, and a note that helped once is backed best: the team's notes that helped give
    // the same three as every note does, where they give three, without reading the reader's own notes (which have
    // no tally) or the rest. Where they don't, every note is read.
    read({ space: "team", min_helped: 1 })
      .then((helped) => {
        const best = adviceStarters(helped);
        return best.length >= 3 ? best : read({ space: "all" }).then((all) => adviceStarters(all));
      })
      .then((learned) => live && setStarters(fill(learned)))
      .catch(() => live && setStarters(fill([])));
    return () => {
      live = false;
    };
  }, [sel.org, sel.team, sel.branch]);
  return starters;
}

/** What a starter from advice says on hover: the situation, and how often its advice helped. */
const adviceTitle = (s: Starter) =>
  `Advice learned for: ${s.situation}` +
  (s.served ? ` · helped ${s.helped ?? 0} of ${s.served} ${s.served === 1 ? "run" : "runs"}` : "");

/** The playground on a memory with nothing in it: nothing to ask yet, and the three ways to put something in. */
function EmptyPlayground({
  addToMemory,
  connect,
}: {
  addToMemory?: React.ReactNode;
  connect?: { sources?: string; agents?: string };
}) {
  return (
    <div className="pg-hero">
      <div className="pg-mark" aria-hidden="true">
        <Icon name="database" size={20} />
      </div>
      <div className="pg-empty-text">
        <h2 className="pg-hero-title">Nothing to ask yet</h2>
        <p className="pg-hint">
          This memory is empty. Put something in it, then chat with it or search it here.
        </p>
      </div>
      <ol className="pg-start">
        <li className="pg-start-card">
          <Icon name="doc" size={16} />
          <h3>Add to memory</h3>
          <p>A note, a document or a decision, kept as an episode and learned from.</p>
          {addToMemory ?? (
            <p className="pg-muted pg-small">
              Agents add with <span className="mono">remember_memory</span>, scripts with{" "}
              <span className="mono">POST /remember</span>.
            </p>
          )}
        </li>
        <li className="pg-start-card">
          <Icon name="database" size={16} />
          <h3>Connect a source</h3>
          <p>Slack, a CRM, meetings or a repository, read on a schedule.</p>
          {connect?.sources ? (
            <a className="btn" href={connect.sources}>
              Connect a source <Icon name="arrow" size={12} />
            </a>
          ) : (
            <p className="pg-muted pg-small">
              Sources are connected where the memory is administered.
            </p>
          )}
        </li>
        <li className="pg-start-card">
          <Icon name="tools" size={16} />
          <h3>Connect an agent</h3>
          <p>Its MCP tools search the memory as it works and leave what it learned.</p>
          {connect?.agents ? (
            <a className="btn" href={connect.agents}>
              Connect an agent <Icon name="arrow" size={12} />
            </a>
          ) : (
            <p className="pg-muted pg-small">
              Point it at this memory's <span className="mono">/mcp</span> endpoint.
            </p>
          )}
        </li>
      </ol>
    </div>
  );
}

function Hero({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="pg-hero">
      <div className="pg-mark" aria-hidden="true">
        <Icon name="chat" size={20} />
      </div>
      <h2 className="pg-hero-title">{title}</h2>
      <div className="pg-hero-body">{children}</div>
    </div>
  );
}

/** The message box: a textarea that grows, a row of controls under it, and the memory it reads beneath. */
function Composer({
  boxRef,
  draft,
  setDraft,
  onSend,
  disabled,
  placeholder,
  scope,
  left,
  right,
  hint,
}: {
  boxRef: React.RefObject<HTMLTextAreaElement>;
  draft: string;
  setDraft: (s: string) => void;
  onSend: () => void;
  disabled?: boolean;
  placeholder: string;
  scope: React.ReactNode;
  left?: React.ReactNode;
  right?: React.ReactNode;
  hint?: React.ReactNode;
}) {
  const grow = (el: HTMLTextAreaElement) => {
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  };
  useEffect(() => {
    if (boxRef.current && !draft) boxRef.current.style.height = "";
  }, [draft]);
  return (
    <form
      className="pg-composer"
      onSubmit={(e) => {
        e.preventDefault();
        if (draft.trim() && !disabled) onSend();
      }}
    >
      <div className="pg-composer-box">
        <textarea
          ref={boxRef}
          className="pg-input"
          rows={1}
          placeholder={placeholder}
          aria-label="Question"
          value={draft}
          onChange={(e) => {
            setDraft(e.target.value);
            grow(e.target);
          }}
          onKeyDown={(e) => {
            // Enter sends; Shift+Enter is a new line.
            if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              if (draft.trim() && !disabled) onSend();
            }
          }}
        />
        <div className="pg-composer-row">
          {left}
          <span className="pg-grow" />
          {right}
          <button
            type="submit"
            className="pg-send"
            aria-label={disabled ? "Running" : "Send"}
            disabled={!draft.trim() || disabled}
          >
            <Icon name="up" size={15} />
          </button>
        </div>
      </div>
      <div className="pg-scope">
        <span className="pg-dot" aria-hidden="true" />
        <span className="mono">{scope}</span>
        {hint && <span className="pg-scope-hint">{hint}</span>}
      </div>
    </form>
  );
}

function CompareToggle({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      className="pg-compare"
      aria-pressed={on}
      title="Send to both: with your memory, and without"
      aria-label="Compare without memory"
      onClick={onToggle}
    >
      <Icon name="compare" size={13} />
      <span className="pg-compare-label">Compare without memory</span>
    </button>
  );
}

/** The composer's model and memory picker: the model, and an arm to load the settings from. */
function ArmMenu({
  cfg,
  arms,
  from,
  dirty,
  onModel,
  onLoad,
}: {
  cfg: Arm;
  arms: Arm[];
  from: string;
  dirty: boolean;
  onModel: (m: Model) => void;
  onLoad: (a: Arm) => void;
}) {
  const [open, setOpen] = useState(false);
  // Placed over the page, so no scrolling area clips it: up from the picker where there is room, else down.
  const [at, setAt] = useState<React.CSSProperties>({});
  const root = useRef<HTMLDivElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const place = () => {
    const r = root.current?.getBoundingClientRect();
    if (r)
      setAt(
        r.top > 380
          ? { left: r.left, bottom: window.innerHeight - r.top + 6 }
          : { left: r.left, top: r.bottom + 6 },
      );
  };
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => {
      if (root.current && !root.current.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    // It follows the picker as the page scrolls or resizes (its own scrolling moves nothing).
    const follow = (e: Event) => {
      if (!(e.target instanceof Node && menu.current?.contains(e.target))) place();
    };
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    addEventListener("resize", follow);
    document.addEventListener("scroll", follow, true);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", esc);
      removeEventListener("resize", follow);
      document.removeEventListener("scroll", follow, true);
    };
  }, [open]);
  const setup = dirty ? "Custom" : (arms.find((a) => a.id === from)?.name ?? "Custom");
  return (
    <div className="pg-menu-root" ref={root}>
      <button
        type="button"
        className="pg-picker"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Model and memory: ${modelName(cfg.model)}, ${setup}`}
        onClick={() => {
          place();
          setOpen((o) => !o);
        }}
      >
        <span className="pg-picker-model">{modelName(cfg.model)}</span>
        <span className="pg-picker-setup">{setup}</span>
        <Icon name="down" size={12} />
      </button>
      {open && (
        <div className="pg-menu" role="menu" style={at} ref={menu}>
          <div className="pg-menu-label">Model</div>
          {MODELS.map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="menuitemradio"
              aria-checked={cfg.model === id}
              className="pg-menu-item"
              onClick={() => {
                onModel(id);
                setOpen(false);
              }}
            >
              <span>{label}</span>
              {cfg.model === id && <Icon name="check" size={13} />}
            </button>
          ))}
          <div className="pg-menu-sep" />
          <div className="pg-menu-label">Memory</div>
          {arms.map((a) => (
            <button
              key={a.id}
              type="button"
              role="menuitemradio"
              aria-checked={!dirty && from === a.id}
              className="pg-menu-item"
              onClick={() => {
                onLoad(a);
                setOpen(false);
              }}
            >
              <span>
                {a.name}
                <span className="pg-menu-sub">
                  {modelName(a.model)} · {toolsText(a.tools.length)}
                </span>
              </span>
              {!dirty && from === a.id && <Icon name="check" size={13} />}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** One arm's answer to one question: its tool calls as rows, the answer, then what it took. */
function Answer({
  arm,
  result,
  sel,
  question,
  retry,
  withoutMemory,
  onFocus,
}: {
  /** Opens a focus, in the team whose memory a result is in where it names one. */
  onFocus: (f: Focus, team?: string) => void;
  arm: Arm;
  result: Result;
  sel: Selection;
  question: string;
  retry: () => void;
  withoutMemory?: () => void;
}) {
  if (result.status === "loading")
    return (
      <div className="pg-answer" aria-busy="true">
        <div className="pg-thinking">
          <span className="pg-pulse" aria-hidden="true" />
          {arm.tools.length ? "Looking it up in memory…" : "Answering…"}
        </div>
        <Skeleton rows={3} title={false} />
      </div>
    );
  if (result.status === "error")
    return (
      <div className="pg-answer">
        <ErrorState
          title="The run failed"
          message={result.message}
          onRetry={result.retry ? retry : undefined}
        />
      </div>
    );
  const { run } = result;
  const ran = run.steps.filter((s) => s.ran).length;
  // The search answers whole (what the model was given), not their traces cut short: a citation may point past the cut.
  const cited = citeIndex(searchTexts(run.steps, run.context?.messages));
  return (
    <div className="pg-answer">
      {run.steps.length > 0 && (
        <div className="pg-steps">
          {run.steps.map((s, i) => (
            <StepRow key={i} step={s} onFocus={onFocus} />
          ))}
        </div>
      )}
      {run.stopped !== "answered" && (
        <div className="pg-stopped">{STOPPED[run.stopped] ?? run.stopped}</div>
      )}
      <div className="pg-md">
        {run.answer ? (
          <Markdown
            text={run.answer}
            cite={(inside) => {
              // A bracket that cites nothing it can open reads as written.
              const parts = citeParts(inside);
              return parts.length ? <Citation parts={parts.map(cited)} onFocus={onFocus} /> : null;
            }}
          />
        ) : (
          <span className="pg-muted">No answer.</span>
        )}
      </div>
      <div className="pg-meta">
        <span>{modelName(run.model || arm.model)}</span>
        <span>
          {ran} tool {ran === 1 ? "call" : "calls"}
        </span>
        <span>{(run.ms / 1000).toFixed(1)} s</span>
        <span>
          {run.tokens.in >= 1000 ? `${(run.tokens.in / 1000).toFixed(1)}k` : run.tokens.in} tokens
          in
        </span>
        {run.cost != null && <span>${run.cost.toFixed(4)}</span>}
        {withoutMemory && (
          <button type="button" className="pg-link pg-meta-action" onClick={withoutMemory}>
            <Icon name="compare" size={12} /> Run without memory
          </button>
        )}
      </div>
      {run.answer && <RememberThis sel={sel} question={question} answer={run.answer} />}
      <WhyAnswer run={run} sel={sel} />
    </div>
  );
}

/**
 * An answer's citation (`[e6 @347, e2 @876]`), one pill per subject it cites: the subject's name, opening it; an
 * episode the search listed where no subject carries the marker; the marker as written where neither is known.
 */
function Citation({
  parts,
  onFocus,
}: {
  parts: Cited[];
  onFocus: (f: Focus, team?: string) => void;
}) {
  const shown = parts.filter(
    (p, i) => !p.entity || parts.findIndex((q) => q.entity === p.entity) === i,
  );
  return (
    <>
      {shown.map((p) =>
        p.entity ? (
          <button
            key={p.marker}
            type="button"
            className="pg-cite"
            title={`${p.entity}${p.team ? ` · in team ${p.team}` : ""}${p.marker !== p.entity ? ` · cited as ${p.marker}` : ""}`}
            onClick={() => onFocus({ kind: "entity", path: entityPath(p.entity!) }, p.team)}
          >
            {entityName(p.entity).name}
          </button>
        ) : p.episode ? (
          <button
            key={p.marker}
            type="button"
            className="pg-cite mono"
            title={`Episode ${p.episode} · cited as ${p.marker}`}
            onClick={() => onFocus({ kind: "episode", id: p.episode! })}
          >
            {short(p.episode)}
          </button>
        ) : (
          <span
            key={p.marker}
            className="pg-cite pg-cite-unknown mono"
            title="Not in a search it made"
          >
            {p.marker}
          </span>
        ),
      )}
    </>
  );
}

const STOPPED: Record<string, string> = {
  "tool budget": "Stopped at its tool-call cap",
  "time budget": "Stopped at the time limit",
  "model error": "The model failed",
};

/** A tool call as one row, as an agent's trace shows it: the tool, what it asked, how it went; open for what it got. */
function StepRow({
  step,
  onFocus,
}: {
  step: Step;
  onFocus: (f: Focus, team?: string) => void;
}) {
  // A search the agent made reads as the facts it found, each linked; what the model got stays one click away.
  const found =
    step.ran && !step.error && step.tool === "search_memory" ? readSearch(step.text) : undefined;
  const hits = found?.results.length ? found : undefined;
  return (
    <details className={`pg-step ${step.ran ? (step.error ? "err" : "") : "refused"}`}>
      <summary>
        <Icon name={isSearch(step.tool) ? "search" : "tools"} size={13} />
        <span className="mono pg-step-tool">{step.tool}</span>
        <span className="pg-step-args">{argsText(step.args)}</span>
        <span className="pg-step-meta">
          {step.ran ? (
            <>
              <Icon name={step.error ? "alert" : "check"} size={12} />
              {hits ? `${hits.results.length} · ` : ""}
              {step.ms} ms
            </>
          ) : (
            "not run"
          )}
        </span>
        <span className="pg-step-chev">
          <Icon name="chevron" size={12} />
        </span>
      </summary>
      {hits ? (
        <div className="pg-step-body">
          <ul className="pg-step-hits">
            {hits.results.map((r, i) => (
              <li key={i}>
                <EntityLink
                  entity={r.entity}
                  onFocus={(f) => onFocus(f, resultTeam(` · ${r.when}`))}
                />
                <span className="pg-step-line">
                  <Marked line={r.lines[0] ?? ""} sources={hits.sources} onFocus={onFocus} />
                </span>
                {r.lines.length > 1 && (
                  <span className="pg-muted pg-small">+{r.lines.length - 1}</span>
                )}
              </li>
            ))}
          </ul>
          <details className="pg-step-raw">
            <summary>
              What the model got{" "}
              <span className="pg-muted">{step.chars.toLocaleString()} characters</span>
            </summary>
            <pre className="pg-step-text">{step.text}</pre>
          </details>
        </div>
      ) : (
        <pre className="pg-step-text">{step.text}</pre>
      )}
    </details>
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

/** The latest answer of the conversation's own settings, for Raw context. */
function latest(turns: Turn[]): Run | undefined {
  for (let i = turns.length - 1; i >= 0; i--) {
    const r = turns[i]!.results[PRIMARY];
    if (r?.status === "ok") return r.run;
  }
  return undefined;
}

const ROLES: Record<string, string> = {
  user: "User",
  assistant: "Assistant",
  tool: "Tool result",
};

/**
 * Raw context: what the model was given for the latest answer, as memstore sent it to the model (its system prompt,
 * the conversation with each tool call and what it returned, cut as the model got it) and the tools it was offered.
 */
function RawContext({ run, onClose }: { run?: Run; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const ctx = run?.context;
  const system = ctx?.messages.find((m) => m.role === "system")?.content ?? "";
  const rest = ctx?.messages.filter((m) => m.role !== "system") ?? [];
  return (
    <>
      <div className="pg-rail-head">
        <h2 className="pg-rail-title">Raw context</h2>
        <div className="pg-rail-actions">
          {ctx && (
            <button
              type="button"
              className="pg-link"
              onClick={() => {
                void navigator.clipboard
                  ?.writeText(JSON.stringify({ ...ctx, answer: run?.answer }, null, 2))
                  .then(() => {
                    setCopied(true);
                    setTimeout(() => setCopied(false), 1500);
                  });
              }}
            >
              {copied ? "Copied" : "Copy"} <Icon name={copied ? "check" : "copy"} size={12} />
            </button>
          )}
          <button
            type="button"
            className="pg-link"
            aria-label="Close raw context"
            onClick={onClose}
          >
            <Icon name="close" size={12} />
          </button>
        </div>
      </div>
      {!run ? (
        <p className="pg-muted pg-small pg-raw-note">
          Ask something: what the model was given for the latest answer shows here. Its system
          prompt, the conversation, what each tool call returned and the tools it was offered.
        </p>
      ) : !ctx ? (
        <p className="pg-muted pg-small pg-raw-note">
          This memory's server doesn't send what the model was given yet: a newer AmpUp Memory does.
        </p>
      ) : (
        <>
          <div className="pg-group">
            <div className="pg-label">System prompt</div>
            <pre className="pg-raw">{system}</pre>
          </div>
          <div className="pg-group">
            <div className="pg-label">Conversation</div>
            {rest.map((m, i) => (
              <div key={i} className={`pg-raw-msg pg-raw-${m.role}`}>
                <div className="pg-raw-role">{ROLES[m.role] ?? m.role}</div>
                {m.content && <pre className="pg-raw">{m.content}</pre>}
                {m.tool_calls?.map((c, j) => (
                  <pre key={j} className="pg-raw pg-raw-call">
                    {`${c.function?.name ?? "tool"}(${c.function?.arguments ?? ""})`}
                  </pre>
                ))}
              </div>
            ))}
            {run.answer && (
              <div className="pg-raw-msg pg-raw-assistant">
                <div className="pg-raw-role">Answer</div>
                <pre className="pg-raw">{run.answer}</pre>
              </div>
            )}
          </div>
          <div className="pg-group">
            <div className="pg-label">Tools offered</div>
            {ctx.tools.length ? (
              ctx.tools.map((t, i) => (
                <div key={i} className="pg-raw-tool">
                  <span className="mono">{t.function?.name}</span>
                  <span className="pg-muted pg-small">{t.function?.description}</span>
                </div>
              ))
            ) : (
              <span className="pg-muted pg-small">None: it answered without memory.</span>
            )}
          </div>
        </>
      )}
    </>
  );
}

/* ---------- The settings rail ---------- */

function RailHead({
  title,
  onReset,
  onCollapse,
}: {
  title: string;
  onReset: () => void;
  onCollapse: () => void;
}) {
  return (
    <div className="pg-rail-head">
      <h2 className="pg-rail-title">{title}</h2>
      <div className="pg-rail-actions">
        <button type="button" className="pg-link" onClick={onReset}>
          Reset <Icon name="refresh" size={12} />
        </button>
        <button
          type="button"
          className="pg-link pg-icon-link"
          aria-label={`Hide ${title.toLowerCase()}`}
          title={`Hide ${title.toLowerCase()}`}
          onClick={onCollapse}
        >
          <Icon name="panel" size={14} />
        </button>
      </div>
    </div>
  );
}

function Stepper({
  label,
  value,
  min,
  max,
  onChange,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  onChange: (n: number) => void;
}) {
  return (
    <div className="pg-row">
      <span>{label}</span>
      <div className="pg-stepper" role="group" aria-label={label}>
        <button
          type="button"
          aria-label={`Fewer: ${label.toLowerCase()}`}
          disabled={value <= min}
          onClick={() => onChange(value - 1)}
        >
          −
        </button>
        <span className="mono" aria-live="polite">
          {value}
        </span>
        <button
          type="button"
          aria-label={`More: ${label.toLowerCase()}`}
          disabled={value >= max}
          onClick={() => onChange(value + 1)}
        >
          +
        </button>
      </div>
    </div>
  );
}

/** Joins lines, dropping an empty line that follows another (a part left out leaves no gap). */
const lines = (ls: string[]) => ls.filter((l, i, all) => l || all[i - 1]).join("\n");

function ChatSettings({
  org,
  cfg,
  setCfg,
  arms,
  from,
  loaded,
  dirty,
  compare,
  setCompare,
  onLoad,
  onReset,
  onSave,
  onDelete,
  armsError,
  question,
  onCollapse,
}: {
  org: string;
  cfg: Arm;
  setCfg: (a: Arm) => void;
  arms: Arm[];
  from: string;
  loaded?: Arm;
  dirty: boolean;
  compare: string[];
  setCompare: (f: (c: string[]) => string[]) => void;
  onLoad: (a: Arm) => void;
  onReset: () => void;
  onSave: (name: string, asNew: boolean) => Promise<void>;
  onDelete?: () => Promise<void>;
  armsError?: string;
  question: string;
  onCollapse: () => void;
}) {
  const preset = PRESETS.find(([id]) =>
    sameTools(BUILTIN.find((a) => a.id === id)!.tools, cfg.tools),
  )?.[0];
  const mine = loaded && !loaded.builtin ? loaded : undefined;
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => setName(mine?.name ?? ""), [mine?.id]);
  const act = (f: () => Promise<void>) => {
    setBusy(true);
    setError(undefined);
    f()
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusy(false));
  };
  return (
    <>
      <RailHead title="Memory settings" onReset={onReset} onCollapse={onCollapse} />
      <div className="pg-group">
        <div className="pg-label">Memory</div>
        <div className="pg-seg" role="radiogroup" aria-label="Memory">
          {PRESETS.map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="radio"
              aria-checked={preset === id}
              onClick={() => {
                const b = BUILTIN.find((a) => a.id === id)!;
                setCfg({
                  ...cfg,
                  tools: b.tools,
                  maxToolCalls: b.maxToolCalls,
                });
              }}
            >
              {label}
            </button>
          ))}
        </div>
        <label className="pg-row">
          <span>Model</span>
          <select
            className="pg-select"
            value={cfg.model}
            onChange={(e) => setCfg({ ...cfg, model: e.target.value as Model })}
          >
            {MODELS.map(([id, label]) => (
              <option key={id} value={id}>
                {label}
              </option>
            ))}
          </select>
        </label>
        <Stepper
          label="Max tool calls"
          value={cfg.maxToolCalls}
          min={1}
          max={12}
          onChange={(n) => setCfg({ ...cfg, maxToolCalls: n })}
        />
      </div>

      <div className="pg-group">
        <div className="pg-label">Tools the agent may call</div>
        <div className="pg-checks">
          {TOOLS.map(([t, what]) => (
            <label key={t} className="pg-check" title={what}>
              <input
                type="checkbox"
                checked={cfg.tools.includes(t)}
                onChange={() =>
                  setCfg({
                    ...cfg,
                    tools: cfg.tools.includes(t)
                      ? cfg.tools.filter((x) => x !== t)
                      : [...cfg.tools, t],
                  })
                }
              />
              <span className="mono">{t}</span>
            </label>
          ))}
        </div>
      </div>

      <div className="pg-group">
        <div className="pg-label">Compare with</div>
        <div className="pg-arms">
          {arms
            .filter((a) => a.id !== from || dirty)
            .map((a) => (
              <div key={a.id} className="pg-arm">
                <label className="pg-arm-pick">
                  <input
                    type="checkbox"
                    checked={compare.includes(a.id)}
                    onChange={() =>
                      setCompare((c) =>
                        c.includes(a.id) ? c.filter((x) => x !== a.id) : [...c, a.id],
                      )
                    }
                  />
                  <span className="pg-arm-name">{a.name}</span>
                  <span className="pg-muted pg-small">
                    {modelName(a.model)} · {toolsText(a.tools.length)}
                  </span>
                </label>
                <button
                  type="button"
                  className="pg-link"
                  aria-label={`Use ${a.name}'s settings`}
                  onClick={() => onLoad(a)}
                >
                  Use
                </button>
              </div>
            ))}
        </div>
        {armsError && <div className="error-text">Saved arms didn't load: {armsError}</div>}
      </div>

      <form
        className="pg-group"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim()) act(() => onSave(name.trim(), !mine || !same(mine.name, name)));
        }}
      >
        <div className="pg-label">Save as an arm</div>
        <div className="pg-save">
          <input
            className="pg-field"
            placeholder="Name these settings"
            aria-label="Arm name"
            maxLength={60}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <button type="submit" className="pg-btn" disabled={busy || !name.trim()}>
            {mine && same(mine.name, name) ? "Save" : "Save new"}
          </button>
        </div>
        {mine && onDelete && (
          <button
            type="button"
            className="pg-link pg-danger"
            disabled={busy}
            onClick={() => act(onDelete)}
          >
            Delete “{mine.name}”
          </button>
        )}
        {error && <div className="error-text">{error}</div>}
      </form>

      <Request
        tabs={[
          [
            "Prompt",
            lines([
              "Give this agent AmpUp Memory.",
              "",
              cfg.tools.length
                ? `Connect the AmpUp Memory MCP server and allow only these tools: ${cfg.tools.join(", ")}.`
                : "Connect no memory tools: it answers from the model alone.",
              `Run it on ${modelName(cfg.model)}, and cap it at ${cfg.maxToolCalls} tool ${cfg.maxToolCalls === 1 ? "call" : "calls"} per answer.`,
              cfg.tools.includes("search_memory")
                ? "Have it call search_memory before it answers a question about the team's work, and cite the episodes the facts rest on."
                : "",
              "",
              "Check what is already set up first: reuse the project's MCP client if it has one.",
            ]),
          ],
          ...code("/agent/run", org, {
            question: question || "What did we decide last week?",
            tools: cfg.tools,
            model: cfg.model,
            max_tool_calls: cfg.maxToolCalls,
          }),
        ]}
      />
    </>
  );
}

/**
 * The request the settings make, as cURL, TypeScript and Python: a POST of `body` to the memory's `path`, as the org's
 * member whose token is `AMPUP_MEMORY_TOKEN`, at `AMPUP_MEMORY_URL` (the memory's address: the console's Setup page
 * names it).
 */
function code(path: string, org: string, body: unknown): [string, string][] {
  const q = `?org=${encodeURIComponent(org)}`;
  const json = JSON.stringify(body, null, 2);
  return [
    [
      "cURL",
      [
        `curl -X POST "$AMPUP_MEMORY_URL${path}${q}" \\`,
        `  -H "Authorization: Bearer $AMPUP_MEMORY_TOKEN" \\`,
        `  -H "Content-Type: application/json" \\`,
        `  -d '${JSON.stringify(body).replace(/'/g, "'\\''")}'`,
      ].join("\n"),
    ],
    [
      "TypeScript",
      [
        `const res = await fetch(\`\${process.env.AMPUP_MEMORY_URL}${path}${q}\`, {`,
        `  method: "POST",`,
        `  headers: {`,
        `    Authorization: \`Bearer \${process.env.AMPUP_MEMORY_TOKEN}\`,`,
        `    "Content-Type": "application/json",`,
        `  },`,
        `  body: JSON.stringify(${json.replace(/\n/g, "\n  ")}),`,
        `});`,
        `const answer = await res.json();`,
      ].join("\n"),
    ],
    [
      "Python",
      [
        "import os",
        "import requests",
        "",
        "res = requests.post(",
        `    f"{os.environ['AMPUP_MEMORY_URL']}${path}",`,
        `    params={"org": ${JSON.stringify(org)}},`,
        `    headers={"Authorization": f"Bearer {os.environ['AMPUP_MEMORY_TOKEN']}"},`,
        `    json=${py(body, "    ")},`,
        ")",
        "answer = res.json()",
      ].join("\n"),
    ],
  ];
}

/** A JSON value as a Python literal, indented from `at`. */
function py(v: unknown, at: string): string {
  const next = `${at}    `;
  if (v === null || v === undefined) return "None";
  if (v === true) return "True";
  if (v === false) return "False";
  if (typeof v === "number" || typeof v === "string") return JSON.stringify(v);
  if (Array.isArray(v)) return v.length ? `[${v.map((x) => py(x, next)).join(", ")}]` : "[]";
  const entries = Object.entries(v as Record<string, unknown>);
  if (!entries.length) return "{}";
  return `{\n${entries.map(([k, x]) => `${next}${JSON.stringify(k)}: ${py(x, next)},`).join("\n")}\n${at}}`;
}

/** A request the settings make, to copy: tabs of text. */
function Request({ tabs }: { tabs: [string, string][] }) {
  const [at, setAt] = useState(0);
  const [copied, setCopied] = useState(false);
  const text = tabs[at]?.[1] ?? "";
  return (
    <div className="pg-group pg-request">
      <div className="pg-rail-head">
        <div className="pg-label">Request</div>
        <button
          type="button"
          className="pg-link"
          onClick={() => {
            void navigator.clipboard?.writeText(text).then(() => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            });
          }}
        >
          {copied ? "Copied" : "Copy"} <Icon name={copied ? "check" : "copy"} size={12} />
        </button>
      </div>
      <div className="pg-code">
        <div className="pg-code-tabs" role="tablist" aria-label="Request format">
          {tabs.map(([label], i) => (
            <button
              key={label}
              type="button"
              role="tab"
              aria-selected={i === at}
              onClick={() => setAt(i)}
            >
              {label}
            </button>
          ))}
        </div>
        <pre className="pg-code-text">{text}</pre>
      </div>
    </div>
  );
}

/* ---------- Search ---------- */

type SearchCfg = {
  k: number;
  facts: boolean;
  advice: boolean;
  episodes: boolean;
};
const SEARCH_DEFAULT: SearchCfg = {
  k: 10,
  facts: true,
  advice: false,
  episodes: true,
};

type Part<T> =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; data: T; ms: number }
  | { status: "error"; message: string };

function SearchSettings({
  org,
  cfg,
  setCfg,
  question,
  onReset,
  onCollapse,
}: {
  org: string;
  cfg: SearchCfg;
  setCfg: (c: SearchCfg) => void;
  question: string;
  onReset: () => void;
  onCollapse: () => void;
}) {
  const q = question || "What did we decide last week?";
  return (
    <>
      <RailHead title="Search settings" onReset={onReset} onCollapse={onCollapse} />
      <div className="pg-group">
        <Stepper
          label="Results"
          value={cfg.k}
          min={1}
          max={30}
          onChange={(k) => setCfg({ ...cfg, k })}
        />
      </div>
      <div className="pg-group">
        <div className="pg-label">Include in results</div>
        <div className="pg-checks">
          {(
            [
              ["facts", "Facts", "search_memory"],
              ["episodes", "Source episodes", "the episodes the facts cite"],
              ["advice", "Advice", "get_advice"],
            ] as const
          ).map(([key, label, how]) => (
            <label key={key} className="pg-check" title={how}>
              <input
                type="checkbox"
                checked={cfg[key]}
                onChange={() => setCfg({ ...cfg, [key]: !cfg[key] })}
              />
              <span>{label}</span>
            </label>
          ))}
        </div>
      </div>
      <Request
        tabs={[
          [
            "Prompt",
            lines([
              "Add AmpUp Memory search to this project.",
              "",
              "Connect the AmpUp Memory MCP server and call its search_memory tool with the user's question.",
              `Ask for the ${cfg.k} best facts (k: ${cfg.k}); each comes with the quote it rests on and the episodes that say it.`,
              cfg.advice
                ? "Also call get_advice with the same question, for what the team learned about this kind of task."
                : "",
              "",
              "Wrap it in one function, searchMemory(query), that returns the facts. Show me the call site before wiring it in.",
            ]),
          ],
          ...code("/mcp", org, {
            jsonrpc: "2.0",
            id: 1,
            method: "tools/call",
            params: { name: "search_memory", arguments: { query: q, k: cfg.k, format: "json" } },
          }),
        ]}
      />
    </>
  );
}

/** One of search_memory's hits as JSON: a fact, with its score and who wrote it. */
type Hit = Fact & { written_by?: unknown; facts?: Hit[] };

/** search_memory with `format: "json"`: the facts that match, best first, each once. */
async function searchHits(sel: Selection, query: string, k: number): Promise<Hit[]> {
  const text = await tool(sel, "search_memory", { query, k, format: "json" });
  // A list of hits, or (from a server that also answers passages) `{hits, passages}`.
  const answered = JSON.parse(text || "[]") as Hit[] | { hits?: Hit[] };
  const hits = Array.isArray(answered) ? answered : (answered.hits ?? []);
  // Each hit is one scored row. The facts nested under it (no score of their own) stay with it, as its related
  // facts, unless they are a hit themselves or already related to an earlier one.
  const seen = new Set(hits.map((h) => h.fact_id));
  const out: Hit[] = [];
  for (const h of hits) {
    if (out.some((o) => o.fact_id === h.fact_id)) continue;
    const facts = (h.facts ?? []).filter((f) => !seen.has(f.fact_id));
    for (const f of facts) seen.add(f.fact_id);
    out.push({ ...h, facts });
  }
  return out;
}

const entityPath = (entity: string) => {
  const i = entity.indexOf(":");
  return i < 0 ? ["entities", "_", entity] : ["entities", entity.slice(0, i), entity.slice(i + 1)];
};
const writer = (w: unknown) =>
  typeof w === "string" ? w : ((w as { label?: string } | null)?.label ?? undefined);

/** A score as a bar of twelve, against the best score shown (or 1, when scores are fractions). */
function Score({ value, top }: { value: number; top: number }) {
  const on = Math.round((Math.max(0, value) / top) * 12);
  return (
    <span className="pg-score" title={`Relevance ${value.toFixed(3)}`}>
      <span className="pg-score-bar" aria-hidden="true">
        {Array.from({ length: 12 }, (_, i) => (
          <span key={i} className={i < on ? "on" : ""} />
        ))}
      </span>
      <span className="mono">{value.toFixed(2)}</span>
    </span>
  );
}

/** One fact found, opened: what it says, the quote it rests on, when it holds, who wrote it, its episodes. */
function HitDialog({
  hit,
  top,
  onClose,
  onFocus,
}: {
  hit: Hit;
  top: number;
  onClose: () => void;
  onFocus: (f: Focus) => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (d && !d.open) d.showModal();
  }, []);
  const go = (f: Focus) => {
    onClose();
    onFocus(f);
  };
  const n = entityName(hit.entity);
  const by = writer(hit.written_by);
  return (
    <dialog
      ref={ref}
      className="pg-dialog"
      aria-label="Fact"
      onClose={onClose}
      onClick={(e) => e.target === e.currentTarget && onClose()}
    >
      <div className="pg-dialog-head">
        <h2 className="pg-rail-title">Fact</h2>
        <button type="button" className="pg-link" aria-label="Close" onClick={onClose}>
          <Icon name="close" size={12} />
        </button>
      </div>
      <div className="pg-dialog-chips">
        {hit.score != null && <Score value={hit.score} top={top} />}
        <EntityLink entity={hit.entity} onFocus={go} />
      </div>
      <p className="pg-dialog-text">
        <strong>{n.name}</strong> {humanize(hit.predicate)}: {hit.value}
      </p>
      {hit.quote && <blockquote className="pg-dialog-quote">{hit.quote}</blockquote>}
      <dl className="pg-dialog-rows">
        <div>
          <dt>True since</dt>
          <dd>{when(hit.valid_from)}</dd>
        </div>
        {hit.valid_to != null && (
          <div>
            <dt>Until</dt>
            <dd>{when(hit.valid_to)}</dd>
          </div>
        )}
        {by && (
          <div>
            <dt>Written by</dt>
            <dd>{by}</dd>
          </div>
        )}
        <div>
          <dt>Fact ID</dt>
          <dd className="mono">{hit.fact_id}</dd>
        </div>
      </dl>
      {!!hit.facts?.length && (
        <div className="pg-dialog-eps">
          <div className="pg-label">Related facts · {hit.facts.length}</div>
          <ul className="pg-related">
            {hit.facts.map((f) => (
              <li key={f.fact_id} title={f.quote ?? undefined}>
                <span className="pg-hit-subject">{entityName(f.entity).name}</span>{" "}
                {humanize(f.predicate)}: {f.value}
              </li>
            ))}
          </ul>
        </div>
      )}
      {hit.source_episodes.length > 0 && (
        <div className="pg-dialog-eps">
          <div className="pg-label">Source episodes</div>
          <div className="pg-episodes">
            {hit.source_episodes.map((id) => (
              <button
                key={id}
                type="button"
                className="chip"
                onClick={() => go({ kind: "episode", id })}
              >
                <Icon name="episode" size={12} />
                <span className="mono">{short(id)}</span>
              </button>
            ))}
          </div>
        </div>
      )}
      <div className="pg-dialog-foot">
        <button type="button" className="pg-btn" onClick={onClose}>
          Close
        </button>
        <button
          type="button"
          className="pg-btn pg-btn-primary"
          onClick={() => go({ kind: "entity", path: entityPath(hit.entity) })}
        >
          Open {n.type || "subject"} <Icon name="arrow" size={12} />
        </button>
      </div>
    </dialog>
  );
}

/** Search: the memory's search_memory as an agent gets it, with the episodes its facts cite and, if asked, advice. */
function SearchPane({
  sel,
  cfg,
  boxRef,
  question,
  onSearch,
  onFocus,
  scope,
}: {
  sel: Selection;
  cfg: SearchCfg;
  boxRef: React.RefObject<HTMLTextAreaElement>;
  question: string;
  onSearch: (q: string) => void;
  onFocus: (f: Focus) => void;
  scope: React.ReactNode;
}) {
  const [draft, setDraft] = useState(question);
  const [found, setFound] = useState<Part<Hit[]>>({ status: "idle" });
  const [opened, setOpened] = useState<Hit | null>(null);
  const [tips, setTips] = useState<Part<string>>({ status: "idle" });
  const [nonce, setNonce] = useState(0);
  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);
  const want = JSON.stringify(cfg);
  useEffect(() => {
    if (!question) {
      setFound({ status: "idle" });
      setTips({ status: "idle" });
      return;
    }
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
    if (cfg.facts || cfg.episodes) void time(setFound, () => searchHits(sel, question, cfg.k));
    else setFound({ status: "idle" });
    if (cfg.advice) void time(setTips, () => advice(sel, question));
    else setTips({ status: "idle" });
    return () => {
      live = false;
    };
    // `sel` and `cfg` are read through their keys.
  }, [question, selKey, want, nonce]);

  const send = () => {
    const q = draft.trim();
    if (!q) return;
    if (q === question) setNonce((n) => n + 1);
    else onSearch(q);
  };
  const composer = (
    <Composer
      boxRef={boxRef}
      draft={draft}
      setDraft={setDraft}
      onSend={send}
      placeholder="Search your memory…"
      scope={scope}
    />
  );
  if (!question)
    return (
      <div className="pg-scroll">
        <Hero title="Search across your memory">
          {composer}
          <p className="pg-hint">
            Facts that match, with the quote and episodes behind each: what search_memory returns to
            an agent.
          </p>
        </Hero>
      </div>
    );

  const hits = found.status === "ok" ? found.data : [];
  const episodes = [
    ...new Set(hits.flatMap((h) => [h, ...(h.facts ?? [])].flatMap((f) => f.source_episodes))),
  ];
  const count = hits.length;
  // Scores that are fractions are shown against 1; others against the best one here.
  const top = Math.max(1, ...hits.map((h) => h.score ?? 0));
  return (
    <>
      <div className="pg-scroll">
        <div className="pg-results">
          <div className="pg-results-head">
            <h2 className="pg-results-title">Results</h2>
            {found.status === "ok" && (
              <span className={`pg-pill ${count ? "ok" : ""}`}>
                {count} {count === 1 ? "match" : "matches"} · {Math.round(found.ms)} ms
              </span>
            )}
          </div>
          {cfg.advice && (
            <Block title="Advice" tool="get_advice" part={tips} rows={3}>
              {(text) => {
                // The session line is for an agent; the reader gets the advice, and the session apart.
                const session = SESSION.exec(text)?.[1];
                const body = text.replace(/\n*session_id: .*$/s, "").trim();
                return (
                  <>
                    <Markdown text={body} />
                    {session && (
                      <div className="pg-muted pg-small">
                        session <span className="mono">{short(session)}</span>
                      </div>
                    )}
                  </>
                );
              }}
            </Block>
          )}
          {cfg.facts && (
            <Block title="Facts" tool="search_memory" part={found} rows={5}>
              {(hs) =>
                hs.length ? (
                  <ul className="pg-hits">
                    {hs.map((h) => {
                      const n = entityName(h.entity);
                      return (
                        <li key={h.fact_id}>
                          <button type="button" className="pg-hit" onClick={() => setOpened(h)}>
                            <Icon name="entity" size={13} />
                            <span className="pg-hit-text">
                              <span className="pg-hit-subject">{n.name}</span>{" "}
                              {humanize(h.predicate)}: {h.value}
                            </span>
                            {n.type && <span className="pg-hit-type mono">{n.type}</span>}
                            {!!h.facts?.length && (
                              <span
                                className="pg-hit-more mono"
                                title={`${h.facts.length} related facts`}
                              >
                                +{h.facts.length}
                              </span>
                            )}
                            {h.score != null && <Score value={h.score} top={top} />}
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                ) : (
                  <div className="pg-muted">
                    No matches: try a different question, or fewer words.
                  </div>
                )
              }
            </Block>
          )}
          {cfg.episodes && (
            <Block title="Source episodes" tool="cited by search_memory" part={found} rows={2}>
              {() =>
                episodes.length ? (
                  <div className="pg-episodes">
                    {episodes.map((id) => (
                      <button
                        key={id}
                        type="button"
                        className="chip"
                        onClick={() => onFocus({ kind: "episode", id })}
                      >
                        <Icon name="episode" size={12} />
                        <span className="mono">{short(id)}</span>
                      </button>
                    ))}
                  </div>
                ) : (
                  <div className="pg-muted">No episode is cited.</div>
                )
              }
            </Block>
          )}
        </div>
      </div>
      <div className="pg-dock">{composer}</div>
      {opened && (
        <HitDialog hit={opened} top={top} onClose={() => setOpened(null)} onFocus={onFocus} />
      )}
    </>
  );
}

function Block<T>({
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
    <section className="pg-block" aria-label={title}>
      <div className="pg-block-head">
        <span className="pg-label">{title}</span>
        <span className="mono pg-muted pg-small">{tool}</span>
      </div>
      {part.status === "loading" || part.status === "idle" ? (
        <Skeleton rows={rows} title={false} />
      ) : part.status === "error" ? (
        <ErrorState title={`${title}: the call failed`} message={part.message} />
      ) : (
        children(part.data)
      )}
    </section>
  );
}
