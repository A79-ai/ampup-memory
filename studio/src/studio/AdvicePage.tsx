import "./advice-page.css";
import { useEffect, useMemo, useState } from "react";
import { ApiError, errorText } from "../transport";
import { browse, call } from "../memory/api";
import { when } from "../memory/format";
import { useOption } from "../memory/hashOptions";
import { Markdown } from "../memory/markdown";
import { LessonsFor } from "./Learned";
import { RangeSeg, scopeText, useAdviceScope } from "./adviceScope";
import type { Selection } from "../memory/types";

// Studio's Advice page: each team note agents were given, how often and to how many people, how those sessions
// went, and the sessions themselves (memstore's `/browse {op: "advice_usage"}`). Everyone sees the org's counts;
// the sessions are the reader's own, or everyone's for the org's admins. Admins ratify a note for the whole team
// (`/admin {op: "ratify_advice"}`), and find the evidence and lookup tables under "Details". The note picked
// is in the link (`#/advice?note=<key>`); where a note was learned from, run by run, is on the Learned page.
// Every total here is counted as Learned counts it (adviceScope.tsx): the window and whose own notes, in the link.

interface Session {
  id: string;
  question: string;
  at: number;
  /** True went well, false went wrong, null no outcome. */
  went_well: boolean | null;
  /** Whose word the outcome is: what came of its PRs, the person's (rating, next message), a reward given for the
   * session (an eval's grader, a run's recorder), or the session judge's. */
  by: "pr" | "person" | "reward" | "judge" | null;
  /** Only in an admin's view. */
  user?: string;
  /** Handed the note's learned preferences with what the person asked to be remembered, not served the note. */
  recalled?: boolean;
}
interface Advice {
  key: string;
  category: string;
  situation: string;
  when?: string;
  text: string;
  learned_at: number;
  used: number;
  people: number;
  went_well: number;
  went_wrong: number;
  no_outcome: number;
  last_used_at: number | null;
  ratified: boolean;
  /** Ratified by an admin, not (only) by its evidence. */
  approved: boolean;
  held?: boolean;
  needs_backing?: string;
  /** The reader's own note (its key `mine:`): written for them alone, served to them at once, never ratified. */
  mine?: boolean;
  /** Whose own note it is (an engine that takes `person` says it). */
  owner?: string;
  /** Of `used`, the sessions handed only its learned preferences, at their start. */
  recalled?: number;
  sessions: Session[];
}
interface Usage {
  advice: Advice[];
  /** The sessions are everyone's (the reader is an admin of an org that shows people's activity). */
  every: boolean;
  /** The sessions counted were asked at or after this (µs); 0, all of them. */
  since?: number;
  /** Whose own notes stand in for the reader's (an engine that takes `person` echoes it). */
  person?: string | null;
}

/** Waiting for an admin: used, and neither ratified nor held back. */
const waiting = (a: Advice) => !a.mine && a.used > 0 && !a.ratified && !a.held;

function Bar({ a }: { a: Advice }) {
  const judged = a.went_well + a.went_wrong;
  if (!judged) return <span className="ap-bar ap-bar-empty" aria-hidden />;
  return (
    <span
      className="ap-bar"
      role="img"
      aria-label={`${a.went_well} went well, ${a.went_wrong} went wrong`}
    >
      <i style={{ width: `${(100 * a.went_well) / judged}%` }} />
      <u style={{ width: `${(100 * a.went_wrong) / judged}%` }} />
    </span>
  );
}

function Status({ a }: { a: Advice }) {
  if (a.mine)
    return a.owner ? (
      <span className="ap-badge ap-badge-ok" title={`Learned from ${a.owner}'s own sessions, for them alone`}>
        {a.owner}&apos;s
      </span>
    ) : (
      <span className="ap-badge ap-badge-ok" title="Learned from your own sessions, for you alone">
        Yours
      </span>
    );
  if (a.held) return <span className="ap-badge ap-badge-held">Held back</span>;
  if (a.ratified)
    return (
      <span
        className="ap-badge ap-badge-ok"
        title={a.approved ? "Ratified by an admin" : "Ratified by its outcomes"}
      >
        ✓ Ratified
      </span>
    );
  return (
    <span className="ap-badge" title={a.needs_backing ?? undefined}>
      Not ratified{a.needs_backing ? " · not served" : ""}
    </span>
  );
}

function Outcome({ s }: { s: Session }) {
  if (s.went_well === null) return <span className="ap-muted">No outcome</span>;
  const who = {
    pr: "what came of its PRs",
    person: "the person said",
    reward: "the reward it was given",
    judge: "the session judge",
  }[s.by ?? "judge"];
  return (
    <span className={s.went_well ? "ap-ok" : "ap-bad"} title={`By ${who}`}>
      {s.went_well ? "✓ Went well" : "✗ Went wrong"}
      <span className="ap-muted ap-small"> · {s.by}</span>
    </span>
  );
}

export function AdvicePage({
  sel,
  admin,
  onTrace,
  onLearned,
  diagnostics,
}: {
  sel: Selection;
  admin: boolean;
  /** Opens a session's transcript. */
  onTrace: (id: string) => void;
  /** Opens a lesson on Advice's Learned tab (every lesson, by whose): `key`, or `key@owner` for a person's own. */
  onLearned?: (key: string) => void;
  /** The evidence and lookup tables, for admins. */
  diagnostics?: React.ReactNode;
}) {
  // In the link, so Home's "this week" opens it at 7 days, and Learned beside it counts the same window and notes.
  const { range, setRange, days, person } = useAdviceScope(admin);
  const [n, setN] = useState(0);
  const [got, setGot] = useState<{ key: string; data?: Usage; error?: unknown } | null>(null);
  const key = JSON.stringify([sel.org, sel.team, sel.branch, days, person, n]);
  useEffect(() => {
    let live = true;
    browse<Usage>(
      { op: "advice_usage", ...(days ? { days } : {}), ...(person ? { person } : {}) },
      sel,
    ).then(
      (data) => live && setGot({ key, data }),
      (error: unknown) => live && setGot({ key, error }),
    );
    return () => {
      live = false;
    };
  }, [key]);
  const cur = got?.key === key ? got : null;
  const data = cur?.data;
  const [filter, setFilter] = useState<"all" | "waiting">("all");
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useOption("note");
  const [tab, setTab] = useState<DetailTab>("advice");
  const [went, setWent] = useState<"all" | "well" | "wrong">("all");
  const [showDiag, setShowDiag] = useState(false);

  // A person's own notes name them; the reader's own are "yours" (the engine names their owner either way).
  const all = useMemo(
    () =>
      (data?.advice ?? []).map((a) => (a.mine && !data?.person ? { ...a, owner: undefined } : a)),
    [data],
  );
  const queue = all.filter(waiting);
  const list = useMemo(() => {
    const q = query.trim().toLowerCase();
    return all.filter(
      (a) =>
        (filter === "all" || waiting(a)) &&
        (!q || `${a.situation} ${a.category}`.toLowerCase().includes(q)),
    );
  }, [all, filter, query]);
  // The note the link names, opened on a page of its own; none: the list.
  const chosen = picked ? all.find((a) => a.key === picked) : undefined;
  const totals = all.reduce(
    (t, a) => ({
      used: t.used + a.used,
      inUse: t.inUse + (a.used > 0 ? 1 : 0),
      well: t.well + a.went_well,
      judged: t.judged + a.went_well + a.went_wrong,
    }),
    { used: 0, inUse: 0, well: 0, judged: 0 },
  );
  const tooOld =
    cur?.error instanceof ApiError && /unknown op/.test(errorText(cur.error))
      ? "This deployment's engine predates the Advice page: it shows once the engine is updated."
      : null;

  return (
    <div className="studio ap">
      <header className="ap-head">
        <h1>Advice</h1>
        <span className="ap-muted">
          What agents were given, how often, and how it went
          {data && !data.every ? " · the sessions listed are yours" : ""}
        </span>
        <RangeSeg range={range} setRange={setRange} />
      </header>
      {data && (
        <p className="ap-scope ap-muted ap-small">
          {/* The window the server applied (`since`), and whose notes it read (`person`, echoed). */}
          {scopeText(data.person ?? "", data.since ? days : null, "sessions")}
        </p>
      )}

      {cur?.error ? (
        <p className="ap-pad ap-error" role="alert">
          {tooOld ?? errorText(cur.error)}{" "}
          {!tooOld && (
            <button className="ap-btn2" onClick={() => setN((x) => x + 1)}>
              Try again
            </button>
          )}
        </p>
      ) : !cur ? (
        <p className="ap-pad ap-muted">Reading the advice…</p>
      ) : all.length === 0 ? (
        <p className="ap-pad ap-muted">
          No advice learned here yet, or none you may read. Advice is learned from sessions whose outcome is
          known: yours appears here after a few of them.
        </p>
      ) : (
        chosen ? (
          <article className="ap-one">
            <button className="ap-back" onClick={() => setPicked(null)}>
              ← All advice
            </button>
            <Detail
              a={chosen}
              sel={sel}
              admin={admin}
              every={!!data?.every}
              days={days}
              tab={tab}
              setTab={setTab}
              went={went}
              setWent={setWent}
              onTrace={onTrace}
              onLearned={onLearned}
              onChanged={() => setN((x) => x + 1)}
            />
          </article>
        ) : (
        <>
          <div className="ap-tiles">
            <div>
              <dt>Advice used</dt>
              <dd>{totals.used.toLocaleString()} times</dd>
            </div>
            <div>
              <dt>Advice in use</dt>
              <dd>
                {totals.inUse.toLocaleString()}{" "}
                <span className="ap-muted ap-small">of {all.length.toLocaleString()}</span>
              </dd>
            </div>
            <div>
              <dt>Sessions that went well</dt>
              <dd>
                {totals.judged
                  ? `${totals.well.toLocaleString()} of ${totals.judged.toLocaleString()} · ${Math.round((100 * totals.well) / totals.judged)}%`
                  : "—"}
              </dd>
            </div>
            {admin && (
              <button
                className={`ap-tile-btn ${filter === "waiting" ? "on" : ""}`}
                onClick={() => setFilter(filter === "waiting" ? "all" : "waiting")}
                aria-pressed={filter === "waiting"}
              >
                <dt>Waiting for ratification</dt>
                <dd>{queue.length}</dd>
              </button>
            )}
          </div>

          <nav className="ap-list" aria-label="Advice">
            <div className="ap-filter">
              <input
                type="search"
                placeholder="Filter advice"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                aria-label="Filter advice"
              />
              {admin && (
                <div className="ap-chips">
                  <button className={filter === "all" ? "on" : ""} onClick={() => setFilter("all")}>
                    All
                  </button>
                  <button
                    className={filter === "waiting" ? "on" : ""}
                    onClick={() => setFilter("waiting")}
                  >
                    Waiting for ratification ({queue.length})
                  </button>
                </div>
              )}
            </div>
            {list.length === 0 && (
              <p className="ap-pad ap-muted ap-small">
                {filter === "waiting" ? "Nothing is waiting for ratification." : "No match."}
              </p>
            )}
            {list.map((a) => (
              <button
                key={a.key}
                className="ap-item"
                onClick={() => {
                  setPicked(a.key);
                  setTab("advice");
                  setWent("all");
                }}
              >
                <span className="ap-item-top">
                  <b>{a.situation}</b>
                  <span className="ap-num">{a.used}</span>
                </span>
                <span className="ap-item-sub">
                  <span className="ap-muted ap-small">
                    {a.category}
                    {a.mine ? (a.owner ? ` · ${a.owner}'s` : " · yours") : ""}
                    {a.ratified ? " · ✓ ratified" : ""}
                    {a.held ? " · held back" : ""}
                  </span>
                  <Bar a={a} />
                </span>
              </button>
            ))}
          </nav>
        </>
        )
      )}

      {admin && diagnostics && (
        <section className="ap-diag">
          <button
            className="ap-btn2"
            onClick={() => setShowDiag(!showDiag)}
            aria-expanded={showDiag}
          >
            {showDiag ? "Hide the details" : "Details: does advice help, and when is it given"}
          </button>
          {showDiag && <div className="ap-diag-in">{diagnostics}</div>}
        </section>
      )}
    </div>
  );
}

/** A note's tabs: its text (first), the sessions it was served in, and the runs it was learned from. */
type DetailTab = "sessions" | "advice" | "learned";

function Detail({
  a,
  sel,
  admin,
  every,
  days,
  tab,
  setTab,
  went,
  setWent,
  onTrace,
  onLearned,
  onChanged,
}: {
  a: Advice;
  sel: Selection;
  admin: boolean;
  every: boolean;
  /** The window the page counts (null: all), so the note's trails count what its tiles do. */
  days: number | null;
  tab: DetailTab;
  setTab: (t: DetailTab) => void;
  went: "all" | "well" | "wrong";
  setWent: (w: "all" | "well" | "wrong") => void;
  onTrace: (id: string) => void;
  onLearned?: (key: string) => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<{ key: string; text: string; error?: boolean } | null>(null);
  const sessions = a.sessions.filter(
    (s) => went === "all" || (went === "well" ? s.went_well === true : s.went_well === false),
  );
  const ratify = async (on: boolean) => {
    setBusy(true);
    try {
      const r = await call<{ queued?: string }>({ org: sel.org, team: sel.team }, "admin", {
        op: "ratify_advice",
        keys: [a.key],
        ratify: on,
      });
      if (r.queued) {
        setSaid({
          key: a.key,
          text: on
            ? "Ratified: it applies within a minute."
            : "Withdrawn: it applies within a minute.",
        });
        setTimeout(onChanged, 45_000);
      } else {
        setSaid({ key: a.key, text: on ? "Ratified for the team." : "Ratification withdrawn." });
        onChanged();
      }
    } catch (e) {
      setSaid({ key: a.key, text: errorText(e), error: true });
    } finally {
      setBusy(false);
    }
  };
  const note = said?.key === a.key ? said : null;
  return (
    <article className="ap-detail">
      <div className="ap-muted ap-small ap-caps">{a.category}</div>
      <div className="ap-title">
        <h2>{a.situation}</h2>
        <Status a={a} />
        {admin && !a.mine && (
          <div className="ap-actions">
            {!a.ratified || a.held ? (
              <button className="ap-btn" disabled={busy} onClick={() => ratify(true)}>
                Ratify for the team
              </button>
            ) : a.approved ? (
              <button className="ap-btn2" disabled={busy} onClick={() => ratify(false)}>
                Withdraw ratification
              </button>
            ) : null}
          </div>
        )}
      </div>
      {note && (
        <p className={note.error ? "ap-error" : "ap-said"} role="status">
          {note.text}
        </p>
      )}
      {a.mine && (
        <p className="ap-muted ap-small">
          {a.owner
            ? `Learned from ${a.owner}'s own sessions and given to them alone, from the next one on.`
            : "Learned from your own sessions and given to you alone, from the next one on."}
          {a.recalled
            ? ` ${a.recalled} of its ${a.used} ${a.used === 1 ? "use was" : "uses were"} its preferences, handed when a session started.`
            : ""}
        </p>
      )}
      {admin && !a.mine && !a.ratified && !a.held && (
        <p className="ap-muted ap-small">
          {a.needs_backing
            ? "Not served yet: it waits until people's outcomes back it. Ratifying serves it to the whole team now."
            : "Served, not yet proven by outcomes. Ratifying marks it right for the team and keeps it served."}
        </p>
      )}
      <dl className="ap-tiles ap-tiles-4">
        <div>
          <dt>Used</dt>
          <dd>{a.used.toLocaleString()} times</dd>
        </div>
        <div>
          <dt>By</dt>
          <dd>
            {a.people} {a.people === 1 ? "person" : "people"}
          </dd>
        </div>
        <div>
          <dt>Went well</dt>
          <dd className="ap-ok">{a.went_well.toLocaleString()}</dd>
        </div>
        <div>
          <dt>Went wrong</dt>
          <dd className="ap-bad">{a.went_wrong.toLocaleString()}</dd>
        </div>
      </dl>
      <div className="ap-tabs" role="tablist">
        <button
          role="tab"
          aria-selected={tab === "advice"}
          className={tab === "advice" ? "on" : ""}
          onClick={() => setTab("advice")}
        >
          The advice
        </button>
        <button
          role="tab"
          aria-selected={tab === "sessions"}
          className={tab === "sessions" ? "on" : ""}
          onClick={() => setTab("sessions")}
        >
          {every ? "Sessions" : "Your sessions"} ({a.sessions.length})
        </button>
        <button
          role="tab"
          aria-selected={tab === "learned"}
          className={tab === "learned" ? "on" : ""}
          onClick={() => setTab("learned")}
        >
          Learned from
        </button>
      </div>
      {tab === "learned" ? (
        <LessonsFor
          sel={sel}
          adviceKey={a.key}
          mine={!!a.mine}
          owner={a.owner}
          days={days}
          onLesson={(id) => onLearned?.(id)}
          onTrace={onTrace}
        />
      ) : tab === "advice" ? (
        <div className="ap-note">
          <p className="ap-muted ap-small">
            Learned {when(a.learned_at)}
            {a.when ? ` · for: ${a.when}` : ""}
            {" · "}
            <button className="ap-link" onClick={() => setTab("learned")}>
              The runs it was learned from →
            </button>
          </p>
          <Markdown text={a.text.replace(/ ?<!--i:[0-9a-f]+-->/g, "")} />
        </div>
      ) : (
        <>
          <div className="ap-chips">
            {(["all", "wrong", "well"] as const).map((w) => (
              <button key={w} className={went === w ? "on" : ""} onClick={() => setWent(w)}>
                {w === "all" ? "All" : w === "well" ? "Went well" : "Went wrong"}
              </button>
            ))}
          </div>
          {sessions.length === 0 ? (
            <p className="ap-muted ap-pad-y">
              {a.sessions.length === 0
                ? every
                  ? "No session used it in this range."
                  : `It was used ${a.used} times in this range, none of them in your sessions.`
                : "None in this filter."}
            </p>
          ) : (
            // One row per session: what was asked (two lines at most), who and when under it, and how
            // it went. A list rather than a table, so it fits the column at any width.
            <ul className="ap-sessions">
              {sessions.map((s) => (
                <li key={s.id}>
                  <button className="ap-session" onClick={() => onTrace(s.id)}>
                    <span className="ap-session-q">“{s.question || "(no request recorded)"}”</span>
                    <span className="ap-muted ap-small">
                      {every ? `${s.user} · ` : ""}
                      {when(s.at)}
                      {s.recalled ? " · its preferences, at the start" : ""}
                    </span>
                  </button>
                  <span className="ap-session-out">
                    <Outcome s={s} />
                  </span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </article>
  );
}
