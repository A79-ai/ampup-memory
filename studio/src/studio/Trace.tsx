import "./studio.css";
import { useEffect, useMemo, useState } from "react";
import { call } from "../memory/api";
import { errorText } from "../transport";
import { when } from "../memory/format";
import type { Selection } from "../memory/types";
import {
  HARNESS,
  KINDS,
  mentions,
  readView,
  shortTool,
  shown,
  steps,
  turns,
  writeView,
  type Kind,
  type Step,
  type Turn,
  type View,
} from "./traceSteps";
import { LookupList, type Lookup } from "./lookups";
import { PrList, type SessionPr } from "./prs";
import { isSearch, searchArgs, Why } from "../memory/why";

// One conversation from the answer log, traced for a quick read (`admin {op:
// "conversation"}`): who asked what, the advice it was served and the ops that fired, the judge's verdict and
// reward, and its transcript split into steps (each person's turn, the assistant's, each tool call with its
// result). The whole conversation is one tree: its sessions (a harness ends one at a compaction or when idle and
// opens the next for the same conversation) as its parts, each person's message heading the turn it led to, and
// the turn's replies and tool calls beneath it. Steps are filtered by kind (advice, memory read or written,
// errors, …) with context kept around each match, a strip of lanes shows the whole run at once, and the filter
// and the step linked to are kept in the link (`#/conversation/<entry>/<view>`).

interface Conversation {
  id: string;
  kind: "session" | "run" | "answer";
  user: string;
  question: string;
  answer?: string;
  goal?: string;
  rating?: string;
  reward?: number;
  reward_reason?: string;
  asked_at: number;
  updated_at: number;
  cited: string[];
  episodes: string[];
  session?: {
    harness?: string | null;
    arm?: string;
    served?: string[];
    /** The version of each note it was served: the episode holding the note's text then. Absent from older
     * engines. */
    served_versions?: Record<string, string>;
    withheld?: string[];
    ended_at?: number | null;
    ended_by?: string | null;
    /** The harness's own session id, shared by every session of one conversation. */
    thread?: string | null;
    fires?: string[];
    /** What each advice lookup in it came to (absent from sessions kept before lookups were). */
    lookups?: Lookup[];
    /** The PRs it opened or pushed to, each with what later came of it (absent from older engines). */
    prs?: SessionPr[];
    verdicts?: {
      outcome?: string;
      reason?: string;
      notes?: Record<string, { helped?: boolean | null; applied?: boolean | null; why?: string }>;
    };
  };
  transcript_text?: string;
  transcript_cut?: boolean;
  /** The sessions of the same conversation (its harness's own session), this one among them, oldest first:
   * absent from older engines and for a session that names none. */
  thread_sessions?: Part[];
}

const LONG = 600;

const ROLE_LABEL: Partial<Record<Kind, string>> = {
  advice: "Advice",
  read: "Memory read",
  write: "Memory write",
  session: "Session",
  error: "Error",
};

/** The view in the link: what follows the entry in `#/conversation/<entry>/<view>`. */
const viewInHash = () => {
  const m = /^#\/(?:conversation|c)\/[^/]*\/(.*)$/.exec(location.hash);
  return decodeURIComponent(m?.[1] ?? "");
};

/** The first and last step of a turn. */
const first = (t: Turn) => t.ask ?? t.steps[0]!;
const lastOf = (t: Turn) => t.steps[t.steps.length - 1] ?? t.ask!;

function StepView({
  s,
  n,
  linked,
  onLink,
  sel,
  at,
}: {
  s: Step;
  n: number;
  linked: boolean;
  onLink: (n: number) => void;
  sel: Selection;
  /** When the session ran (µs), for a replayed memory read. */
  at: number;
}) {
  const body = s.result != null ? `${s.text}\n→ ${s.result}` : s.text;
  const [open, setOpen] = useState(body.length <= LONG || linked);
  // A fact search the agent made: its explanation, the recorded query run again with `explain`.
  const args = s.tool && isSearch(s.tool) && s.kind !== "error" ? searchArgs(s.text) : null;
  const [why, setWhy] = useState(false);
  return (
    <li
      className={`st-step st-step-${s.kind} ${linked ? "st-step-linked" : ""}`}
      id={`st-step-${n}`}
    >
      <button
        className="st-step-n st-fig"
        title={`Link to step ${n}`}
        aria-label={`Link to step ${n}`}
        onClick={() => onLink(n)}
      >
        {n}
      </button>
      <div className="st-step-body">
        <div className="st-step-head">
          <span className="st-step-role">
            {s.tool
              ? (ROLE_LABEL[s.kind] ?? "Tool")
              : s.label === HARNESS
                ? "Said by the harness"
                : s.label || s.kind}
          </span>
          {s.tool && <span className="mono st-small st-quiet">{shortTool(s.tool)}</span>}
          {body.length > LONG && (
            <button className="st-link st-small" onClick={() => setOpen(!open)}>
              {open ? "Shorten" : `Show all ${body.length.toLocaleString()} characters`}
            </button>
          )}
          {args && (
            <button className="st-link st-small" aria-expanded={why} onClick={() => setWhy(!why)}>
              {why ? "Hide why" : "Why these results"}
            </button>
          )}
        </div>
        <div className={`st-step-text ${s.tool ? "mono" : ""}`}>
          {open ? body : `${body.slice(0, LONG)}…`}
        </div>
        {args && why && (
          <div className="st-step-why">
            <Why sel={sel} args={args} replay={{ at }} title="Why these results" />
          </div>
        )}
      </div>
    </li>
  );
}

/** The whole run as one strip: a lane per kind that happened, a mark per step; a mark jumps to its step. */
function Lanes({
  all,
  visible,
  onJump,
}: {
  all: Step[];
  visible: boolean[];
  onJump: (i: number) => void;
}) {
  const lanes = KINDS.filter(([k]) => all.some((s) => s.kind === k));
  // The marks stretch with the page's width; the labels beside them don't.
  const W = 1000;
  const lh = 16;
  const x = (i: number) => ((i + 0.5) * W) / all.length;
  const w = Math.max(2, Math.min(8, W / all.length - 1));
  return (
    <div className="st-lanes" role="group" aria-label="Steps by kind">
      <ul className="st-lanes-names" aria-hidden="true">
        {lanes.map(([k, label]) => (
          <li key={k}>{label}</li>
        ))}
      </ul>
      <svg
        viewBox={`0 0 ${W} ${lanes.length * lh}`}
        preserveAspectRatio="none"
        style={{ height: lanes.length * lh }}
      >
        {lanes.map(([k], r) => (
          <line key={k} className="st-lane-rule" x1={0} x2={W} y1={r * lh + 8} y2={r * lh + 8} />
        ))}
        {all.map((s, i) => {
          const r = lanes.findIndex(([k]) => k === s.kind);
          if (r < 0) return null;
          return (
            <rect
              key={i}
              className={`st-lane-mark st-lane-${s.kind} ${visible[i] ? "" : "st-lane-off"}`}
              x={x(i) - w / 2}
              y={r * lh + 2}
              width={w}
              height={12}
              onClick={() => onJump(i)}
            >
              <title>
                Step {i + 1}: {s.tool ? shortTool(s.tool) : s.label || s.kind}
              </title>
            </rect>
          );
        })}
      </svg>
    </div>
  );
}

/** A note the session was served: where the transcript names it, the judge's verdict and why, and the note's
 * text, read when opened: the version it was served (an episode), else the note as it reads now. */
function ServedNote({
  sel,
  noteKey,
  verdict,
  version,
  at,
  onStep,
  onEpisode,
}: {
  sel: Selection;
  noteKey: string;
  verdict?: { helped?: boolean | null; applied?: boolean | null; why?: string };
  version?: string;
  at: number[];
  onStep: (i: number) => void;
  onEpisode: (id: string) => void;
}) {
  const [now, setNow] = useState<{ text?: string | null; gone?: boolean; error?: unknown } | null>(
    null,
  );
  const read = () => {
    if (now) return;
    setNow({});
    call<{ notes: { key: string; note?: string | null }[] }>(
      { org: sel.org, team: sel.team },
      "admin",
      { op: "advice_trail", key: noteKey },
    ).then(
      (t) => {
        const n = t.notes.find((x) => x.key === noteKey);
        setNow(n ? { text: n.note ?? null } : { gone: true });
      },
      (error: unknown) => setNow({ error }),
    );
  };
  return (
    <li className="st-served">
      <span className="mono st-small">{noteKey}</span>
      {verdict && (verdict.helped != null || verdict.applied != null) && (
        <span className="st-small">
          {verdict.helped === true && <span className="st-within">helped</span>}
          {verdict.helped === false && <span className="st-out">didn't help</span>}
          {verdict.applied != null && (
            <span className="st-quiet">
              {verdict.helped != null ? " · " : ""}
              {verdict.applied ? "applied" : "not applied"}
            </span>
          )}
        </span>
      )}
      {verdict?.why && <span className="st-small st-quiet">{verdict.why}</span>}
      {at.length > 0 && (
        <span className="st-small st-quiet">
          Named in step{at.length > 1 ? "s" : ""}{" "}
          {at.map((i, j) => (
            <span key={i}>
              {j > 0 && ", "}
              <button className="st-link" onClick={() => onStep(i)}>
                {i + 1}
              </button>
            </span>
          ))}
        </span>
      )}
      {version ? (
        <button className="st-link st-small" onClick={() => onEpisode(version)}>
          Read the version it was served
        </button>
      ) : (
        <details className="st-served-text" onToggle={(e) => e.currentTarget.open && read()}>
          <summary className="st-small">Read the note</summary>
          {!now || (!now.error && !now.gone && now.text === undefined) ? (
            <p className="st-quiet st-small">Reading…</p>
          ) : now.error ? (
            <p className="st-error st-small">{errorText(now.error)}</p>
          ) : now.gone ? (
            <p className="st-quiet st-small">
              It is no longer in the team's advice: reworked or merged into another note since.
            </p>
          ) : now.text ? (
            <>
              <p className="st-quiet st-small">As it reads now; it may have changed since.</p>
              <div className="st-step-text mono">{now.text}</div>
            </>
          ) : (
            <p className="st-quiet st-small">It has no written note, or you can't read it.</p>
          )}
        </details>
      )}
    </li>
  );
}

/** A session of the same conversation, as `/admin {op: "conversation"}` names its thread's sessions. */
interface Part {
  id: string;
  asked_at: number;
  updated_at?: number;
  question?: string;
  reward?: number | null;
  outcome?: string | null;
  ended_by?: string | null;
}

/** A part of the tree: one session's steps, `from` up to `to` in the whole run. */
interface Segment {
  part: Part;
  c?: Conversation;
  error?: unknown;
  from: number;
  to: number;
}

const conversationOf = (sel: Selection, id: string) =>
  call<Conversation>({ org: sel.org, team: sel.team }, "admin", { op: "conversation", id });

/** What a collapsed turn or part held, in a few words. */
function tally(all: Step[], from: number, to: number): string {
  let tools = 0;
  let errors = 0;
  let replies = 0;
  for (let i = from; i < to; i++) {
    const s = all[i]!;
    if (s.kind === "error") errors++;
    else if (s.tool) tools++;
    else if (s.kind === "assistant") replies++;
  }
  return [
    `${to - from} step${to - from === 1 ? "" : "s"}`,
    tools && `${tools} tool call${tools === 1 ? "" : "s"}`,
    replies && `${replies} repl${replies === 1 ? "y" : "ies"}`,
    errors && `${errors} error${errors === 1 ? "" : "s"}`,
  ]
    .filter(Boolean)
    .join(" · ");
}

function Fold({ open, onClick, label }: { open: boolean; onClick: () => void; label: string }) {
  return (
    <button className="st-fold-b" aria-expanded={open} aria-label={label} onClick={onClick}>
      <svg
        className={`st-chev ${open ? "open" : ""}`}
        width="12"
        height="12"
        viewBox="0 0 12 12"
        aria-hidden="true"
      >
        <path d="M4 2.5 7.5 6 4 9.5" />
      </svg>
    </button>
  );
}

export function Trace({
  sel,
  id,
  onUser,
  onEpisode,
  onOpen,
}: {
  sel: Selection;
  id: string;
  onUser: (u: string) => void;
  onEpisode: (id: string) => void;
  /** Opens another session of the same conversation as the one traced. */
  onOpen: (id: string) => void;
}) {
  const [got, setGot] = useState<{ id: string; c?: Conversation; error?: unknown } | null>(null);
  useEffect(() => {
    let live = true;
    conversationOf(sel, id).then(
      (c) => live && setGot({ id, c }),
      (error: unknown) => live && setGot({ id, error }),
    );
    return () => {
      live = false;
    };
  }, [sel.org, sel.team, id]);
  const c = got?.id === id ? got.c : undefined;

  // The other sessions of its conversation (a harness ends one at a compaction or when idle, and opens the next
  // for the same conversation), read so the whole conversation is one tree.
  const parts = useMemo<Part[]>(
    () =>
      c?.thread_sessions && c.thread_sessions.length > 1
        ? [...c.thread_sessions].sort((a, b) => a.asked_at - b.asked_at)
        : [],
    [c],
  );
  const [others, setOthers] = useState<Record<string, { c?: Conversation; error?: unknown }>>({});
  useEffect(() => {
    let live = true;
    setOthers({});
    for (const p of parts) {
      if (p.id === id) continue;
      conversationOf(sel, p.id).then(
        (o) => live && setOthers((m) => ({ ...m, [p.id]: { c: o } })),
        (error: unknown) => live && setOthers((m) => ({ ...m, [p.id]: { error } })),
      );
    }
    return () => {
      live = false;
    };
  }, [parts, sel.org, sel.team, id]);

  const { all, segments } = useMemo(() => {
    const all: Step[] = [];
    const segments: Segment[] = [];
    const list: Part[] = parts.length
      ? parts
      : c
        ? [{ id, asked_at: c.asked_at, question: c.question }]
        : [];
    for (const part of list) {
      const conv = part.id === id ? c : others[part.id]?.c;
      const from = all.length;
      if (conv?.transcript_text) all.push(...steps(conv.transcript_text));
      segments.push({ part, c: conv, error: others[part.id]?.error, from, to: all.length });
    }
    return { all, segments };
  }, [c, parts, others, id]);
  const loadingParts = segments.some((g) => !g.c && !g.error);

  const [view, setView] = useState<View>(() => readView(viewInHash()));
  const [opened, setOpened] = useState<Set<number>>(new Set());
  // Turns and parts folded shut, by their first step.
  const [folded, setFolded] = useState<Set<string>>(new Set());
  const [jump, setJump] = useState<number | null>(null);
  // Read again for another entry, and when the link changes under it (pasted, or back and forward).
  useEffect(() => {
    const read = () => {
      setView(readView(viewInHash()));
      setOpened(new Set());
    };
    read();
    addEventListener("hashchange", read);
    addEventListener("popstate", read);
    return () => {
      removeEventListener("hashchange", read);
      removeEventListener("popstate", read);
    };
  }, [id]);
  // The view is kept in the link without a new history entry for each change, while the link is still this
  // trace's: once it has moved on (back, or a link pasted), it is never written over.
  useEffect(() => {
    const here = `#/conversation/${encodeURIComponent(id)}`;
    // A link from before the names (`#/c/…`) is still this trace's: it is written over in the new form.
    const ours = [here, `#/c/${encodeURIComponent(id)}`].some(
      (p) => location.hash === p || location.hash.startsWith(`${p}/`),
    );
    if (!ours) return;
    const v = writeView(view);
    const h = `${here}${v ? `/${encodeURIComponent(v)}` : ""}`;
    if (h !== location.hash) history.replaceState(history.state, "", h);
  }, [id, view]);
  const change = (v: Partial<View>) => {
    setView((old) => ({ ...old, ...v }));
    setOpened(new Set());
  };
  const visible = useMemo(
    () => shown(all, view.only, view.query, view.context, opened),
    [all, view, opened],
  );
  const toggle = (k: Kind) => {
    const only = new Set(view.only);
    if (only.has(k)) only.delete(k);
    else only.add(k);
    change({ only });
  };
  const fold = (key: string) =>
    setFolded((f) => {
      const n = new Set(f);
      if (n.has(key)) n.delete(key);
      else n.add(key);
      return n;
    });
  // A step is reached through its part and turn: both are unfolded, and the step shown if filtered out.
  const goTo = (i: number) => {
    if (!visible[i]) setOpened((o) => new Set(o).add(i));
    const seg = segments.find((g) => i >= g.from && i < g.to);
    const turn = seg && turns(all, seg.from, seg.to).find((t) => i >= first(t) && i <= lastOf(t));
    setFolded((f) => {
      const n = new Set(f);
      if (seg) n.delete(`p${seg.from}`);
      if (turn) n.delete(`t${first(turn)}`);
      return n;
    });
    setJump(i);
  };
  // A link to a step opens on it, once the steps it counts are read.
  const linked = view.step;
  const [landed, setLanded] = useState<string | null>(null);
  const landKey = `${id}:${linked}`;
  if (linked && !loadingParts && all.length >= linked && landed !== landKey) {
    setLanded(landKey);
    goTo(linked - 1);
  }
  useEffect(() => {
    if (jump == null) return;
    const el = document.getElementById(`st-step-${jump + 1}`);
    el?.scrollIntoView({ block: "center" });
    el?.classList.add("st-step-flash");
    const t = setTimeout(() => el?.classList.remove("st-step-flash"), 1200);
    setJump(null);
    return () => clearTimeout(t);
  }, [jump]);
  const linkStep = (n: number) => setView((old) => ({ ...old, step: n }));

  const count = (k: Kind) => all.filter((s) => s.kind === k).length;
  const verdicts = c?.session?.verdicts;
  const filtered = view.only.size > 0 || view.query.trim() !== "";
  const many = segments.length > 1;
  const at = segments.findIndex((g) => g.part.id === id);
  // The conversation's task: what its first part was asked.
  const task = many ? (segments[0]?.c?.goal ?? segments[0]?.part.question) : undefined;

  /** When the part holding step `i` ran (µs). */
  const segmentAt = (i: number) =>
    segments.find((g) => i >= g.from && i < g.to)?.part.asked_at ?? c?.asked_at ?? 0;

  /** The steps from..to: shown ones, and each run of hidden ones as one line that opens them. */
  const rows = (from: number, to: number) => {
    const out: React.ReactNode[] = [];
    for (let i = from; i < to; ) {
      if (visible[i]) {
        out.push(
          <StepView
            key={i}
            s={all[i]!}
            n={i + 1}
            linked={linked === i + 1}
            onLink={linkStep}
            sel={sel}
            at={segmentAt(i)}
          />,
        );
        i++;
      } else {
        const start = i;
        while (i < to && !visible[i]) i++;
        const end = i;
        out.push(
          <li key={`gap-${start}`} className="st-gap">
            <span>
              {end - start} {end - start === 1 ? "step" : "steps"} hidden
            </span>
            <button
              className="st-link st-small"
              onClick={() =>
                setOpened((o) => {
                  const n = new Set(o);
                  for (let k = start; k < end; k++) n.add(k);
                  return n;
                })
              }
            >
              Show
            </button>
          </li>,
        );
      }
    }
    return out;
  };

  /** One part's turns: each person's message with what it led to beneath it. */
  const turnsOf = (g: Segment) => {
    const ts = turns(all, g.from, g.to);
    if (!ts.length)
      return (
        <p className="st-quiet st-small st-tree-empty">
          {g.error
            ? errorText(g.error)
            : !g.c
              ? "Reading this part…"
              : g.c.kind === "answer"
                ? "A logged answer keeps no transcript: its question and answer are all there is."
                : "No transcript was kept for this part."}
        </p>
      );
    return (
      <ol className="st-turns">
        {ts.map((t) => {
          const lo = first(t);
          const hi = lastOf(t) + 1;
          const any = visible.slice(lo, hi).some(Boolean);
          if (!any)
            return (
              <li key={lo} className="st-gap st-turn-gap">
                <span>A turn hidden ({tally(all, lo, hi)})</span>
              </li>
            );
          const key = `t${lo}`;
          const open = !folded.has(key);
          const ask = t.ask != null ? all[t.ask]! : null;
          return (
            <li key={lo} className={`st-turn ${t.ask == null ? "st-turn-lead" : ""}`}>
              <div className="st-turn-head">
                <Fold
                  open={open}
                  onClick={() => fold(key)}
                  label={open ? "Fold this turn" : "Unfold this turn"}
                />
                {ask ? (
                  <button
                    className={`st-turn-ask ${visible[t.ask!] ? "" : "st-quiet"}`}
                    id={`st-step-${t.ask! + 1}`}
                    onClick={() => linkStep(t.ask! + 1)}
                    title="Link to this message"
                  >
                    <span className="st-turn-n st-fig">{t.ask! + 1}</span>
                    <b>{ask.label || "User"}</b>{" "}
                    {ask.text.length > 240 && !open ? `${ask.text.slice(0, 240)}…` : ask.text}
                  </button>
                ) : (
                  <span className="st-turn-ask st-quiet">
                    {many && g.from > 0
                      ? "Carried on from the part before, with no new message"
                      : "Before the first message in this session"}
                  </span>
                )}
                {!open && <span className="st-quiet st-small">{tally(all, lo, hi)}</span>}
              </div>
              {open && t.steps.length > 0 && <ol className="st-steps">{rows(t.steps[0]!, hi)}</ol>}
            </li>
          );
        })}
      </ol>
    );
  };

  return (
    <div className="studio st-admin">
      <header className="st-banner">
        <div className="st-banner-who">
          <h1>
            {c
              ? c.kind === "run"
                ? "Run"
                : c.kind === "session"
                  ? "Session"
                  : "Answer"
              : "Conversation"}
          </h1>
          {c && (
            <button className="st-link" onClick={() => onUser(c.user)}>
              {c.user}
            </button>
          )}
        </div>
        {c && (
          <dl className="st-banner-facts">
            <div>
              <dt>Started</dt>
              <dd>{when(c.asked_at)}</dd>
            </div>
            {c.session?.harness && (
              <div>
                <dt>In</dt>
                <dd>{c.session.harness}</dd>
              </div>
            )}
            {many && (
              <div>
                <dt>Part</dt>
                <dd className="st-fig">
                  {at + 1} of {segments.length}
                </dd>
              </div>
            )}
            <div>
              <dt>Steps</dt>
              <dd className="st-fig">{all.length}</dd>
            </div>
            {c.session?.arm && (
              <div>
                <dt>Arm</dt>
                <dd>{c.session.arm}</dd>
              </div>
            )}
            {c.reward != null && (
              <div>
                <dt>Reward</dt>
                <dd className="st-fig">{c.reward}</dd>
              </div>
            )}
          </dl>
        )}
      </header>
      {got?.error ? (
        <p className="st-error" role="alert">
          {errorText(got.error)}
        </p>
      ) : !c ? (
        <p className="st-quiet st-pad">Reading the conversation…</p>
      ) : (
        <div className="st-trace-layout">
          <section className="st-trace-main" aria-label="Transcript">
            {task && task !== (c.goal ?? c.question) && (
              <p className="st-trace-q">
                <b>Conversation</b> {task}
              </p>
            )}
            <p className={task ? "st-trace-q st-trace-q-part" : "st-trace-q"}>
              <b>{c.goal ? "Goal" : many ? "This part asked" : "Asked"}</b>{" "}
              {c.goal ??
                (c.question || <span className="st-quiet">nothing new: it carried on</span>)}
            </p>
            {all.length > 0 && (
              <>
                <Lanes all={all} visible={visible} onJump={goTo} />
                <div className="st-trace-filters" role="group" aria-label="Show">
                  {KINDS.filter(([k]) => count(k) > 0).map(([k, label]) => (
                    <button
                      key={k}
                      className={`st-filter st-filter-${k}`}
                      aria-pressed={view.only.has(k)}
                      onClick={() => toggle(k)}
                    >
                      <span className="st-filter-dot" aria-hidden="true" />
                      {label} <span className="st-fig st-quiet">{count(k)}</span>
                    </button>
                  ))}
                  <label className="st-trace-ctx st-small st-quiet">
                    Context{" "}
                    <select
                      value={view.context}
                      onChange={(e) => change({ context: Number(e.target.value) })}
                    >
                      {[0, 1, 2, 3].map((n) => (
                        <option key={n} value={n}>
                          ±{n}
                        </option>
                      ))}
                    </select>
                  </label>
                  <input
                    type="search"
                    placeholder="Search the transcript"
                    aria-label="Search the transcript"
                    value={view.query}
                    onChange={(e) => change({ query: e.target.value })}
                  />
                  {filtered && (
                    <button
                      className="st-link st-small"
                      onClick={() => change({ only: new Set(), query: "" })}
                    >
                      Show all
                    </button>
                  )}
                  <span className="st-tree-acts">
                    <button className="st-link st-small" onClick={() => setFolded(new Set())}>
                      Unfold all
                    </button>
                    <button
                      className="st-link st-small"
                      onClick={() =>
                        setFolded(
                          new Set(
                            segments.flatMap((g) =>
                              turns(all, g.from, g.to).map((t) => `t${first(t)}`),
                            ),
                          ),
                        )
                      }
                    >
                      Fold every turn
                    </button>
                  </span>
                </div>
              </>
            )}
            {c.transcript_cut && (
              <p className="st-quiet st-small">
                Its beginning is cut: the last 200,000 characters.
              </p>
            )}
            {many ? (
              <ol className="st-tree" aria-label="The conversation">
                {segments.map((g, k) => {
                  const key = `p${g.from}`;
                  const open = !folded.has(key);
                  const here = g.part.id === id;
                  const outcome = g.c?.session?.verdicts?.outcome ?? g.part.outcome;
                  const reward = g.c?.reward ?? g.part.reward;
                  return (
                    <li key={g.part.id} className={`st-part ${here ? "st-part-here" : ""}`}>
                      <div className="st-part-head">
                        <Fold
                          open={open}
                          onClick={() => fold(key)}
                          label={open ? "Fold this part" : "Unfold this part"}
                        />
                        <b>
                          Part {k + 1} of {segments.length}
                        </b>
                        <span className="st-quiet st-small">{when(g.part.asked_at)}</span>
                        {outcome && (
                          <span
                            className={`st-small ${outcome === "failed" ? "st-out" : outcome === "succeeded" ? "st-within" : "st-quiet"}`}
                          >
                            {outcome}
                          </span>
                        )}
                        {reward != null && (
                          <span className="st-small st-quiet st-fig">reward {reward}</span>
                        )}
                        <span className="st-quiet st-small">{tally(all, g.from, g.to)}</span>
                        {here ? (
                          <span className="st-part-tag st-small">this session</span>
                        ) : (
                          <button className="st-link st-small" onClick={() => onOpen(g.part.id)}>
                            Open this session
                          </button>
                        )}
                      </div>
                      {open && turnsOf(g)}
                    </li>
                  );
                })}
              </ol>
            ) : (
              segments[0] && turnsOf(segments[0])
            )}
            {loadingParts && many && (
              <p className="st-quiet st-small">Reading the conversation's other parts…</p>
            )}
            {c.answer && (
              <div className="st-outcome">
                <h3>{c.kind === "answer" ? "Answer" : "Outcome"}</h3>
                <p className="st-answer">{c.answer}</p>
              </div>
            )}
          </section>
          <aside className="st-trace-side" aria-label="What it used">
            <section>
              <h3>
                Advice served{" "}
                <span className="st-quiet st-fig">{c.session?.served?.length ?? 0}</span>
              </h3>
              {c.session?.served?.length ? (
                <ul className="st-uses">
                  {c.session.served.map((k) => (
                    <ServedNote
                      key={k}
                      sel={sel}
                      noteKey={k}
                      verdict={verdicts?.notes?.[k]}
                      version={c.session?.served_versions?.[k]}
                      at={mentions(all, k)}
                      onStep={goTo}
                      onEpisode={onEpisode}
                    />
                  ))}
                </ul>
              ) : (
                <p className="st-quiet">None.</p>
              )}
              {(c.session?.withheld?.length ?? 0) > 0 && (
                <>
                  <p className="st-quiet st-small">Withheld, to compare:</p>
                  <ul className="st-uses">
                    {c.session!.withheld!.map((k) => (
                      <li key={k} className="mono st-small st-quiet">
                        {k}
                      </li>
                    ))}
                  </ul>
                </>
              )}
              {c.session?.arm === "holdout" && (
                <p className="st-quiet st-small">
                  Held out: advice routed but not served, to compare.
                </p>
              )}
            </section>
            {c.kind === "session" && (
              <section>
                <h3>
                  Advice lookups{" "}
                  <span className="st-quiet st-fig">{c.session?.lookups?.length ?? 0}</span>
                </h3>
                <LookupList lookups={c.session?.lookups ?? []} compact />
              </section>
            )}
            {c.kind === "session" && (c.session?.prs?.length ?? 0) > 0 && (
              <section>
                <h3>
                  Pull requests <span className="st-quiet st-fig">{c.session!.prs!.length}</span>
                </h3>
                <PrList prs={c.session!.prs!} />
              </section>
            )}
            {(c.reward != null || verdicts?.outcome || c.rating || c.session?.ended_by) && (
              <section>
                <h3>Result</h3>
                {c.reward != null && (
                  <p className="st-reward">
                    <span className="st-fig">{c.reward}</span>
                    {/* -1 to 1, drawn out from 0 in the middle. */}
                    <span className="st-reward-bar" aria-hidden="true">
                      <i
                        className={c.reward < 0 ? "st-reward-neg" : undefined}
                        style={{
                          left: `${50 + Math.min(0, Math.max(-1, c.reward)) * 50}%`,
                          width: `${Math.abs(Math.max(-1, Math.min(1, c.reward))) * 50}%`,
                        }}
                      />
                    </span>
                    <span className="st-small st-quiet">−1 … 1</span>
                  </p>
                )}
                {c.reward_reason && <p className="st-small">{c.reward_reason}</p>}
                <dl className="st-kv st-small">
                  {verdicts?.outcome && (
                    <>
                      <dt>Judged</dt>
                      <dd>
                        {verdicts.outcome}
                        {verdicts.reason && <span className="st-quiet">. {verdicts.reason}</span>}
                      </dd>
                    </>
                  )}
                  {c.rating && (
                    <>
                      <dt>Rated</dt>
                      <dd className={c.rating === "not_helpful" ? "st-out" : undefined}>
                        {c.rating.replace("_", " ")}
                      </dd>
                    </>
                  )}
                  {c.session?.ended_by && (
                    <>
                      <dt>Ended by</dt>
                      <dd>
                        {c.session.ended_by}
                        {c.session.ended_at ? `, ${when(c.session.ended_at)}` : ""}
                      </dd>
                    </>
                  )}
                </dl>
              </section>
            )}
            {(c.session?.fires?.length ?? 0) > 0 && (
              <section>
                <h3>
                  Ops fired <span className="st-quiet st-fig">{c.session!.fires!.length}</span>
                </h3>
                <ul className="st-uses mono st-small">
                  {c.session!.fires!.map((f, i) => (
                    <li key={i}>{f}</li>
                  ))}
                </ul>
              </section>
            )}
            <section>
              <h3>
                Facts cited <span className="st-quiet st-fig">{c.cited.length}</span>
              </h3>
            </section>
            <section>
              <h3>
                Episodes <span className="st-quiet st-fig">{c.episodes.length}</span>
              </h3>
              {c.episodes.length === 0 ? (
                <p className="st-quiet">None.</p>
              ) : (
                <p className="st-chips">
                  {c.episodes.map((e) => (
                    <button key={e} className="st-chip mono" onClick={() => onEpisode(e)}>
                      {e.slice(0, 12)}
                    </button>
                  ))}
                </p>
              )}
            </section>
          </aside>
        </div>
      )}
    </div>
  );
}
