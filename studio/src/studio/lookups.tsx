import { when } from "../memory/format";

// Advice lookups (memstore's `crate::advice_lookups`): what each time an agent asked for advice came to, kept on its
// session's row, and the window's sum (`/admin {op: "activity"}`'s `serving`). Studio's Serving pane and a
// session's trace read them.

/** One advice lookup, as its session keeps it. */
export interface Lookup {
  /** When it began (µs). */
  at: number;
  /** `start`, `prompt` (a hook's), `agent` (the agent's own call), or a later moment: `fork`, `stuck`, `done`,
   * `next`. */
  moment: string;
  outcome: string;
  /** The line it was routed to: a situation (`mine:` for the person's own), a compiled skill (`skill:…`). */
  key?: string;
  /** How likely the decisions model found the best line, 0 to 1. */
  score?: number;
  /** How likely it found the advice, read whole, to be for this very task (at a hook's step: how useful). */
  fit?: number;
  /** What the person's own advice came to first, when it served nothing. */
  own?: string;
  error?: string;
  ms: number;
}

/** Lookups and sessions counted together. */
export interface Tally {
  sessions: number;
  sessions_served: number;
  sessions_withheld: number;
  lookups: number;
  served: number;
  outcomes: Record<string, number>;
}

/** The window's sum: when advice was served and when it wasn't. */
export interface Serving extends Tally {
  /** From when lookups are known (µs): sessions kept before count in `sessions` and `sessions_served` only. */
  lookups_since: number | null;
  by_moment: Record<string, Tally>;
  by_caller: Record<string, Tally>;
  by_person: (Tally & { user: string })[];
  by_harness: (Tally & { harness: string })[];
  top_served: { key: string; lookups: number; sessions: number }[];
  top_no_advice: { key: string; lookups: number }[];
  top_off_task: { key: string; lookups: number }[];
}

/** What each outcome means, in the order a reader takes them: advice given first, then why not. */
export const OUTCOMES: { id: string; label: string; about: string; given: boolean }[] = [
  {
    id: "served",
    label: "Team note served",
    about: "A note learned from the team's runs.",
    given: true,
  },
  {
    id: "served_own",
    label: "Own note served",
    about: "A note learned from the person's own sessions.",
    given: true,
  },
  {
    id: "served_procedures",
    label: "Procedures served",
    about: "No note: a compiled skill, or the procedures linked to the kind of task.",
    given: true,
  },
  {
    id: "revisited",
    label: "Note re-read",
    about: "A later moment (a fork, being stuck, done) re-served the note, cut to the moment.",
    given: true,
  },
  {
    id: "step_handed",
    label: "Step lines handed",
    about: "A hook's step: lines of the served note handed on.",
    given: true,
  },
  {
    id: "withheld",
    label: "Held out",
    about: "Found and fit, but the session is in the holdout arm: withheld to compare.",
    given: false,
  },
  {
    id: "again",
    label: "Already served",
    about: "The note the session was served before: nothing new.",
    given: false,
  },
  {
    id: "no_match",
    label: "No kind of task matched",
    about:
      "No line was likely enough for the decisions model (best under 0.6), or the router named none.",
    given: false,
  },
  {
    id: "no_advice",
    label: "Matched, but no note",
    about: "A kind of task AmpUp Memory knows, with no note or procedure to serve yet.",
    given: false,
  },
  {
    id: "off_task",
    label: "Judged off-task",
    about: "Routed, but the advice read whole was judged to be for another task (fit under 0.5).",
    given: false,
  },
  {
    id: "no_lines",
    label: "Nothing to route over",
    about: "No situation or skill learned yet.",
    given: false,
  },
  {
    id: "nothing_new",
    label: "Nothing new",
    about: "A later moment with no note to re-read.",
    given: false,
  },
  {
    id: "step_not_useful",
    label: "Step: not useful",
    about: "A hook's step: lines found, judged not relevant or useful enough to hand on.",
    given: false,
  },
  {
    id: "step_nothing",
    label: "Step: nothing",
    about: "A hook's step: nothing of the note bears on it, or the session had all it hands on.",
    given: false,
  },
  { id: "error", label: "Failed", about: "The lookup failed.", given: false },
];

const BY_ID = new Map(OUTCOMES.map((o) => [o.id, o]));

export const outcomeLabel = (id: string) => BY_ID.get(id)?.label ?? id.replace(/_/g, " ");
export const given = (id: string) => BY_ID.get(id)?.given ?? false;

export const MOMENTS: Record<string, string> = {
  start: "Session start",
  prompt: "A prompt (hook)",
  agent: "The agent's own ask",
  fork: "At a fork",
  stuck: "Stuck",
  done: "Before done",
  next: "A step done",
  outside: "Outside a session",
};

export const pct = (n: number, of: number) => (of ? `${Math.round((100 * n) / of)}%` : "—");
const p2 = (p?: number) => (p == null ? "" : p.toFixed(2));

/** The outcome that most often kept advice from being given, of a tally's lookups. */
export function topMiss(t: Tally): string | null {
  const miss = Object.entries(t.outcomes)
    .filter(([o]) => !given(o))
    .sort((a, b) => b[1] - a[1])[0];
  return miss ? `${outcomeLabel(miss[0])} (${pct(miss[1], t.lookups)})` : null;
}

/** A session's lookups as a narrow list, for a trace's side: what each came to, its line, and its scores. */
function LookupItems({ lookups }: { lookups: Lookup[] }) {
  return (
    <ul className="st-uses st-lookup-items">
      {lookups.map((l, i) => (
        <li key={`${l.at}-${i}`}>
          <div>
            <span className="st-quiet">{MOMENTS[l.moment] ?? l.moment}:</span>{" "}
            <span className={given(l.outcome) ? "st-within" : undefined}>
              {outcomeLabel(l.outcome)}
            </span>
          </div>
          {l.key && <div className="mono st-small">{l.key}</div>}
          <div className="st-quiet st-small st-fig">
            {[
              l.score != null && `match ${p2(l.score)}`,
              l.fit != null && `fit ${p2(l.fit)}`,
              `${l.ms} ms`,
              when(l.at),
            ]
              .filter(Boolean)
              .join(" · ")}
          </div>
          {l.own && l.own !== "no_lines" && (
            <div className="st-quiet st-small">own notes: {outcomeLabel(l.own)}</div>
          )}
          {l.error && <div className="st-out st-small">{l.error}</div>}
        </li>
      ))}
    </ul>
  );
}

/** A session's lookups, newest last: a table, or (`compact`, a trace's side) a list. */
export function LookupList({ lookups, compact }: { lookups: Lookup[]; compact?: boolean }) {
  if (!lookups.length) {
    return (
      <p className="st-quiet st-small">
        None recorded (sessions before AmpUp Memory kept lookups have none).
      </p>
    );
  }
  if (compact) return <LookupItems lookups={lookups} />;
  return (
    <div className="st-table-wrap">
      <table className="st-table st-lookups">
        <thead>
          <tr>
            <th scope="col">When</th>
            <th scope="col">Moment</th>
            <th scope="col">Came to</th>
            <th scope="col">Line</th>
            <th scope="col" className="st-num" title="How likely the best line was (bar 0.6)">
              Match
            </th>
            <th
              scope="col"
              className="st-num"
              title="How well the advice fit this very task (bar 0.5); at a hook's step, how useful (bar 0.4)"
            >
              Fit
            </th>
            <th scope="col" className="st-num">
              ms
            </th>
          </tr>
        </thead>
        <tbody>
          {lookups.map((l, i) => (
            <tr key={`${l.at}-${i}`}>
              <td className="st-quiet st-nowrap">{when(l.at)}</td>
              <td className="st-nowrap">{MOMENTS[l.moment] ?? l.moment}</td>
              <td className={given(l.outcome) ? "st-within" : undefined}>
                {outcomeLabel(l.outcome)}
                {l.own && l.own !== "no_lines" && (
                  <div className="st-quiet st-small">own notes: {outcomeLabel(l.own)}</div>
                )}
                {l.error && <div className="st-out st-small">{l.error}</div>}
              </td>
              <td className="mono st-small">{l.key ?? "—"}</td>
              <td className="st-num st-fig">{p2(l.score)}</td>
              <td className="st-num st-fig">{p2(l.fit)}</td>
              <td className="st-num st-fig st-quiet">{l.ms}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
