import "./studio.css";
import { Fragment, useCallback, useEffect, useState } from "react";
import { ApiError, errorText, transport, type AreaOwner, type AreaOwnerKind } from "../transport";
import { call } from "../memory/api";
import { when } from "../memory/format";
import { useChoice, useOption } from "../memory/hashOptions";
import { EPISODE_TYPES } from "../memory/inspector";
import { Markdown } from "../memory/markdown";
import { review, type Owner as EntityOwner } from "../memory/review-api";
import type { Selection } from "../memory/types";
import {
  LookupList,
  MOMENTS,
  OUTCOMES,
  outcomeLabel,
  pct as share,
  topMiss,
  type Lookup,
  type Serving,
  type Tally,
} from "./lookups";

// Studio's admin panes on the org's memory (the transport's `admin`, memstore's /admin): People (each
// person's activity), Advice (where each note came from, was served and was validated) and Branches (how a
// branch and main moved apart, and its merge). memstore withholds the words of anything the admin can't
// read, and refuses People while the org's `activity` setting is off.

function admin<T>(sel: Selection, body: Record<string, unknown>): Promise<T> {
  // The team read, never the branch: a branch's diff and merge name theirs in the body.
  const team = body.op !== "set_activity" ? sel.team : undefined;
  return call<T>({ org: sel.org, team }, "admin", body);
}

/** One read, refreshed when its key changes. */
function useAdmin<T>(sel: Selection, body: Record<string, unknown>) {
  const key = JSON.stringify([sel.org, sel.team, body]);
  const [got, setGot] = useState<{ key: string; data?: T; error?: unknown } | null>(null);
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    admin<T>(sel, body).then(
      (data) => live && setGot({ key, data }),
      (error: unknown) => live && setGot({ key, error }),
    );
    return () => {
      live = false;
    };
  }, [key, n]);
  const reload = useCallback(() => setN((x) => x + 1), []);
  const cur = got?.key === key ? got : null;
  return { data: cur?.data, error: cur?.error, loading: !cur, reload };
}

function Sheet({
  title,
  facts,
  children,
  tools,
}: {
  title: string;
  facts?: [string, React.ReactNode][];
  children: React.ReactNode;
  tools?: React.ReactNode;
}) {
  return (
    <div className="studio st-admin">
      <header className="st-banner">
        <div className="st-banner-who">
          <h1>{title}</h1>
        </div>
        {facts && (
          <dl className="st-banner-facts">
            {facts.map(([k, v]) => (
              <div key={k}>
                <dt>{k}</dt>
                <dd className="st-fig">{v}</dd>
              </div>
            ))}
          </dl>
        )}
        {tools}
      </header>
      {children}
    </div>
  );
}

function Failed({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  return (
    <p className="st-error" role="alert">
      {errorText(error)}{" "}
      <button className="st-btn" onClick={onRetry}>
        Try again
      </button>
    </p>
  );
}

const ago = (us: number | null | undefined) => (us ? when(us) : "—");

/* ---------- Owners ---------- */

type OwnerKind = AreaOwnerKind;
type Owner = AreaOwner;
const KIND_LABEL: Record<OwnerKind, string> = {
  episode_type: "Episode type",
  category: "Advice category",
  entity_type: "Entity type",
};

/**
 * The org's area owners, where the host keeps them (`Transport.owners`), and a change to them. `kept` is false
 * where it keeps none: there are none to show or set.
 */
function useOwners(org: string) {
  const store = transport().owners;
  const [owners, setOwners] = useState<Owner[] | null>(store ? null : []);
  useEffect(() => {
    if (!store) return;
    let live = true;
    store.list(org).then(
      (r) => live && setOwners(r),
      () => live && setOwners([]),
    );
    return () => {
      live = false;
    };
  }, [org, store]);
  const change = async (kind: OwnerKind, area: string, owner: string | null) => {
    if (!store) return;
    setOwners(await store.set(org, kind, area, owner));
  };
  const of = (kind: OwnerKind, area: string) =>
    owners?.find((o) => o.kind === kind && o.area === area)?.owner;
  return { owners, change, of, kept: Boolean(store) };
}

function OwnerHint({ who }: { who?: string }) {
  return who ? <span className="st-quiet"> · owner {who}</span> : null;
}

export function Owners({ sel }: { sel: Selection }) {
  const { owners, change, kept } = useOwners(sel.org);
  const [kind, setKind] = useState<OwnerKind>("category");
  const [area, setArea] = useState("");
  const [owner, setOwner] = useState("");
  const [said, setSaid] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // What memstore already names, to pick from: its advice categories and the schema's types.
  // Only the categories' names: the whole trail reads every session. A server before `categories_only` sends the
  // trail, whose notes name them.
  const trail = useAdmin<{ categories?: string[]; notes?: Note[] }>(sel, {
    op: "advice_trail",
    categories_only: true,
  });
  const categories = [
    ...new Set(trail.data?.categories ?? (trail.data?.notes ?? []).map((n) => n.category)),
  ].sort();
  const suggestions =
    kind === "episode_type" ? EPISODE_TYPES.map(([k]) => k) : kind === "category" ? categories : [];

  const act = async (k: OwnerKind, a: string, o: string | null) => {
    setBusy(true);
    setSaid(null);
    try {
      await change(k, a, o);
      if (o) {
        setArea("");
        setOwner("");
      }
    } catch (e) {
      setSaid(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  // A host that keeps no area owners (the local server): the entities' owners, which the memory keeps itself.
  if (!kept) {
    return (
      <Sheet title="Owners">
        <EntityOwners sel={sel} />
      </Sheet>
    );
  }

  return (
    <Sheet title="Owners" facts={owners ? [["Owned areas", owners.length]] : undefined}>
      <p className="st-lede st-quiet">
        An area's owner decides for an area of the memory: the conflicts on a branch that diverged
        from main, and the advice learned in it. Studio names them beside both. An entity's owner
        approves each change to it before it reaches main: those are below.
      </p>
      <form
        className="st-owner-form"
        onSubmit={(e) => {
          e.preventDefault();
          if (area.trim() && owner.trim()) void act(kind, area.trim(), owner.trim());
        }}
      >
        <label>
          <span>Area</span>
          <select value={kind} onChange={(e) => setKind(e.target.value as OwnerKind)}>
            {(Object.keys(KIND_LABEL) as OwnerKind[]).map((k) => (
              <option key={k} value={k}>
                {KIND_LABEL[k]}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>Name</span>
          <input
            list="st-owner-areas"
            value={area}
            required
            maxLength={128}
            placeholder={
              kind === "episode_type" ? "sales_call" : kind === "category" ? "dev-ops" : "deal"
            }
            onChange={(e) => setArea(e.target.value)}
          />
          <datalist id="st-owner-areas">
            {suggestions.map((s) => (
              <option key={s} value={s} />
            ))}
          </datalist>
        </label>
        <label>
          <span>Owner</span>
          <input
            type="email"
            value={owner}
            required
            placeholder="dana@acme.com"
            onChange={(e) => setOwner(e.target.value)}
          />
        </label>
        <button className="st-btn st-btn-strong" disabled={busy}>
          Set owner
        </button>
      </form>
      {said && (
        <p className="st-error" role="alert">
          {said}
        </p>
      )}
      {owners === null ? (
        <p className="st-quiet st-pad">Reading owners…</p>
      ) : owners.length === 0 ? (
        <p className="st-quiet st-pad">No area has an owner yet.</p>
      ) : (
        <div className="st-table-wrap st-pad-x">
          <table className="st-table">
            <thead>
              <tr>
                <th scope="col">Area</th>
                <th scope="col">Kind</th>
                <th scope="col">Owner</th>
                <th scope="col">Set by</th>
                <th scope="col" className="st-act">
                  <span className="st-sr">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {owners.map((o) => (
                <tr key={`${o.kind}/${o.area}`}>
                  <td className="mono">{o.area}</td>
                  <td>{KIND_LABEL[o.kind]}</td>
                  <td>{o.owner}</td>
                  <td className="st-quiet">
                    {o.setBy}, {new Date(o.setAt).toLocaleDateString()}
                  </td>
                  <td className="st-act">
                    <button
                      className="st-btn"
                      disabled={busy}
                      aria-label={`Remove ${o.owner} as owner of ${o.area}`}
                      onClick={() => void act(o.kind, o.area, null)}
                    >
                      Remove
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <EntityOwners sel={sel} />
    </Sheet>
  );
}

/** The entities that have an owner, as Review's rail lists them: assigned, handed on and removed there. */
function EntityOwners({ sel }: { sel: Selection }) {
  const [owners, setOwners] = useState<EntityOwner[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { org, team } = sel;
  useEffect(() => {
    let live = true;
    review.owners({ org, team } as Selection).then(
      (o) => live && setOwners([...o].sort((a, b) => a.entity.localeCompare(b.entity))),
      (e: unknown) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [org, team]);
  return (
    <section className="st-subsection" aria-labelledby="st-owned-entities">
      <h2 className="st-subhead" id="st-owned-entities">
        Owned entities {owners && <span className="st-quiet st-num">{owners.length}</span>}
      </h2>
      <p className="st-lede st-quiet">
        A change to an owned entity waits in Review until its owner approves it. Owners are
        assigned, handed on and removed in <a href="#/r/">Review</a> and on each entity&rsquo;s
        page.
      </p>
      {error ? (
        <p className="st-error st-pad">{error}</p>
      ) : owners === null ? (
        <p className="st-quiet st-pad">Reading owners…</p>
      ) : owners.length === 0 ? (
        <p className="st-quiet st-pad">No entity has an owner yet.</p>
      ) : (
        <div className="st-table-wrap st-pad-x">
          <table className="st-table">
            <thead>
              <tr>
                <th scope="col">Entity</th>
                <th scope="col">Owner</th>
                <th scope="col">Since</th>
              </tr>
            </thead>
            <tbody>
              {owners.map((o) => {
                const i = o.entity.indexOf(":");
                const href = `#/e/entities/${encodeURIComponent(o.entity.slice(0, i))}/${encodeURIComponent(o.entity.slice(i + 1))}`;
                return (
                  <tr key={o.entity}>
                    <td className="mono">
                      <a href={href}>{o.entity}</a>
                    </td>
                    <td>{o.user}</td>
                    <td className="st-quiet">
                      {o.since ? new Date(o.since / 1000).toLocaleDateString() : "—"}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/* ---------- People ---------- */

interface Person {
  user: string;
  sessions: number;
  runs: number;
  answers: number;
  helpful: number;
  not_helpful: number;
  facts: number;
  episodes: number;
  advice: number;
  last_active: number;
}
interface Entry {
  id: string;
  kind: "session" | "run" | "answer";
  user: string;
  asked_at: number;
  updated_at: number;
  cited: number | string[];
  episodes: string[];
  episodes_hidden: number;
  withheld?: boolean;
  /** A session or run that kept a transcript (`/admin {op: "conversation"}` reads it). */
  transcript?: boolean;
  question?: string;
  answer?: string;
  answer_cut?: boolean;
  goal?: string;
  rating?: string;
  reward?: number;
  reward_reason?: string;
  session?: {
    harness?: string;
    arm?: string;
    served?: string[];
    withheld?: string[];
    ended_at?: number | null;
    /** The harness's own session id, shared by every session of one conversation. */
    thread?: string | null;
    verdicts?: { notes?: Record<string, { helped?: boolean | null }> } | null;
    /** What each advice lookup in it came to (absent from sessions kept before lookups were). */
    lookups?: Lookup[];
  };
}
interface Activity {
  people: Person[];
  entries: Entry[];
  more: boolean;
  /** When advice was served and when it wasn't, over the window (absent from older engines). */
  serving?: Serving;
}

const WINDOWS = [7, 30, 90] as const;

/** A session a harness opened before anything was asked, with nothing kept yet: the start hook opens one as the
 * harness starts, and its first prompt names it. Most end empty, out of the activity; until then they'd show as rows
 * with no question and nothing to open. */
const unasked = (e: Entry) =>
  e.kind === "session" && !e.withheld && !e.question?.trim() && !e.transcript;

/** The entries worth a row: all but the [unasked] sessions. */
const asked = (entries: Entry[]) => entries.filter((e) => !unasked(e));

/** Where each entry stands in its conversation, among the entries read: its part and the conversation's first
 * question, for a session that is one of several (a harness ends one at a compaction or when idle, and opens the
 * next for the same conversation). */
function threads(entries: Entry[]): Map<string, { k: number; n: number; task: string }> {
  const by = new Map<string, Entry[]>();
  for (const e of entries) {
    const t = e.session?.thread;
    if (t) by.set(t, [...(by.get(t) ?? []), e]);
  }
  const out = new Map<string, { k: number; n: number; task: string }>();
  for (const parts of by.values()) {
    if (parts.length < 2) continue;
    parts.sort((a, b) => a.asked_at - b.asked_at);
    const task = parts.find((p) => p.goal || p.question);
    parts.forEach((p, i) =>
      out.set(p.id, { k: i + 1, n: parts.length, task: task?.goal ?? task?.question ?? "" }),
    );
  }
  return out;
}

/** An entry's place in its conversation, under its question. */
function PartOf({ part }: { part?: { k: number; n: number; task: string } }) {
  if (!part) return null;
  return (
    <div className="st-small st-quiet">
      Part {part.k} of {part.n} of one conversation
      {part.k > 1 && part.task
        ? `: ${part.task.slice(0, 120)}${part.task.length > 120 ? "…" : ""}`
        : ""}
    </div>
  );
}

export function People({
  sel,
  user,
  onUser,
  onEpisode,
  onTrace,
}: {
  sel: Selection;
  user?: string;
  onUser: (u: string | undefined) => void;
  onEpisode: (id: string) => void;
  onTrace: (id: string) => void;
}) {
  const [days, setDays] = useChoice("days", WINDOWS, 30);
  const { data, error, loading, reload } = useAdmin<Activity>(sel, {
    op: "activity",
    sinceDays: days,
    ...(user ? { user } : {}),
    limit: 100,
  });
  const [busy, setBusy] = useState(false);
  const off = error instanceof ApiError && error.status === 403 && /activity/i.test(error.message);

  // Where the host lets the org's settings change (not a local server, which keeps none).
  const switchable = transport().capabilities.settings === true;
  const turn = async (on: boolean) => {
    setBusy(true);
    try {
      await admin(sel, { op: "set_activity", on });
      reload();
    } finally {
      setBusy(false);
    }
  };

  const window = (
    <div className="st-seg" role="group" aria-label="Window">
      {WINDOWS.map((d) => (
        <button
          key={d}
          className={`st-seg-b ${d === days ? "on" : ""}`}
          aria-pressed={d === days}
          onClick={() => setDays(d)}
        >
          {d} days
        </button>
      ))}
    </div>
  );

  if (off) {
    return (
      <Sheet title="People">
        <div className="st-empty">
          <h2>Activity is off for {sel.org}</h2>
          <p className="st-quiet">
            The org keeps people's questions to themselves: admins see no one's sessions, runs or
            answers here. Turn it on to see who uses the memory and what it gave them.
          </p>
          {switchable && (
            <button className="st-btn st-btn-strong" disabled={busy} onClick={() => turn(true)}>
              Turn activity on
            </button>
          )}
        </div>
      </Sheet>
    );
  }

  const people = data?.people ?? [];
  const total = (k: keyof Person) => people.reduce((n, p) => n + (p[k] as number), 0);
  // A column no one in the window has any of says nothing: it is left out, and named under the table.
  const cols: {
    label: string;
    has: (p: Person) => number;
    cell: (p: Person) => React.ReactNode;
  }[] = [
    { label: "Sessions", has: (p) => p.sessions, cell: (p) => p.sessions },
    { label: "Runs", has: (p) => p.runs, cell: (p) => p.runs },
    { label: "Answers", has: (p) => p.answers, cell: (p) => p.answers },
    {
      label: "Helpful / not",
      has: (p) => p.helpful + p.not_helpful,
      cell: (p) => (
        <>
          {p.helpful} /{" "}
          <span className={p.not_helpful ? "st-out" : undefined}>{p.not_helpful}</span>
        </>
      ),
    },
    { label: "Facts", has: (p) => p.facts, cell: (p) => p.facts },
    { label: "Episodes", has: (p) => p.episodes, cell: (p) => p.episodes },
    { label: "Advice", has: (p) => p.advice, cell: (p) => p.advice },
  ];
  const shownCols = cols.filter((c) => people.some((p) => c.has(p) > 0));
  const hiddenCols = cols.filter((c) => !shownCols.includes(c));
  const tools = (
    <>
      {window}
      {switchable && (
        <button
          className="st-btn"
          disabled={busy}
          title="Admins stop seeing anyone's activity; the answer log keeps it"
          onClick={() => turn(false)}
        >
          Turn activity off
        </button>
      )}
    </>
  );

  return (
    <Sheet
      title={user ? user : "People"}
      facts={
        data && !user
          ? [
              ["Active", people.length] as [string, number],
              ["Sessions", total("sessions")] as [string, number],
              ["Runs", total("runs")] as [string, number],
              ["Answers", total("answers")] as [string, number],
              ["Rated unhelpful", total("not_helpful")] as [string, number],
            ].filter(([k, n]) => n > 0 || k === "Active" || k === "Sessions")
          : undefined
      }
      tools={tools}
    >
      {user && (
        <p className="st-crumb">
          <button className="st-link" onClick={() => onUser(undefined)}>
            ← Everyone
          </button>
        </p>
      )}
      {error && !off ? (
        <Failed error={error} onRetry={reload} />
      ) : loading ? (
        <p className="st-quiet st-pad">Reading the answer log…</p>
      ) : !user ? (
        people.length === 0 ? (
          <p className="st-quiet st-pad">No one used the memory in the last {days} days.</p>
        ) : (
          <div className="st-table-wrap st-pad-x">
            <table className="st-table">
              <thead>
                <tr>
                  <th scope="col">Person</th>
                  {shownCols.map((c) => (
                    <th key={c.label} scope="col" className="st-num">
                      {c.label}
                    </th>
                  ))}
                  <th scope="col">Last active</th>
                </tr>
              </thead>
              <tbody>
                {people.map((p) => (
                  <tr key={p.user}>
                    <td>
                      <button className="st-name-btn" onClick={() => onUser(p.user)}>
                        {p.user}
                      </button>
                    </td>
                    {shownCols.map((c) => (
                      <td key={c.label} className="st-num st-fig">
                        {c.cell(p)}
                      </td>
                    ))}
                    <td className="st-quiet">{ago(p.last_active)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            {hiddenCols.length > 0 && (
              <p className="st-quiet st-small">
                Not shown, as no one has any in the last {days} days:{" "}
                {hiddenCols.map((c) => c.label.toLowerCase()).join(", ")}.
              </p>
            )}
          </div>
        )
      ) : (
        <Entries
          entries={asked(data?.entries ?? [])}
          more={Boolean(data?.more)}
          onEpisode={onEpisode}
          onTrace={onTrace}
        />
      )}
    </Sheet>
  );
}

/** The notes an entry was served, as a count: its keys and the judge's verdicts on hover, how many helped beside
 * it; a holdout session's withheld notes instead, marked so. */
function AdviceCell({ e }: { e: Entry }) {
  const served = e.session?.served ?? [];
  const withheld = e.session?.withheld ?? [];
  const notes = e.session?.verdicts?.notes ?? {};
  if (!served.length && !withheld.length) return <span className="st-quiet">—</span>;
  if (!served.length)
    return (
      <span className="st-quiet" title={`Withheld (holdout): ${withheld.join(", ")}`}>
        held out
      </span>
    );
  const helped = served.filter((k) => notes[k]?.helped === true).length;
  const title = served
    .map((k) => {
      const h = notes[k]?.helped;
      return `${k}${h === true ? " (helped)" : h === false ? " (didn't help)" : ""}`;
    })
    .join("\n");
  return (
    <span title={title}>
      {served.length}
      {helped > 0 && <span className="st-within"> · {helped} helped</span>}
    </span>
  );
}

function Entries({
  entries,
  more,
  onEpisode,
  onTrace,
}: {
  entries: Entry[];
  more: boolean;
  onEpisode: (id: string) => void;
  onTrace: (id: string) => void;
}) {
  const [open, setOpen] = useOption("open");
  const parts = threads(entries);
  if (!entries.length) return <p className="st-quiet st-pad">Nothing in this window.</p>;
  return (
    <div className="st-table-wrap st-pad-x">
      <table className="st-table">
        <thead>
          <tr>
            <th scope="col">When</th>
            <th scope="col">Kind</th>
            <th scope="col">Question</th>
            <th scope="col" className="st-num">
              Cited
            </th>
            <th scope="col" className="st-num">
              Episodes
            </th>
            <th scope="col" className="st-num">
              Advice
            </th>
            <th scope="col">Rating</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((e) => {
            const cited = Array.isArray(e.cited) ? e.cited.length : e.cited;
            const isOpen = open === e.id;
            return (
              <Fragment key={e.id}>
                <tr>
                  <td className="st-quiet st-nowrap">{ago(e.updated_at || e.asked_at)}</td>
                  <td>
                    {e.kind}
                    {e.session?.harness && <span className="st-quiet"> · {e.session.harness}</span>}
                    {e.transcript && (
                      <>
                        {" "}
                        <button className="st-link st-small" onClick={() => onTrace(e.id)}>
                          Trace
                        </button>
                      </>
                    )}
                  </td>
                  <td>
                    {e.withheld ? (
                      <span
                        className="st-quiet"
                        title="Kept to its asker, or resting on episodes you can't read"
                      >
                        Withheld
                      </span>
                    ) : (
                      <button
                        className="st-name-btn st-q"
                        aria-expanded={isOpen}
                        onClick={() => setOpen(isOpen ? null : e.id)}
                      >
                        {e.question || "(no question)"}
                      </button>
                    )}
                    <PartOf part={parts.get(e.id)} />
                  </td>
                  <td className="st-num st-fig">{cited}</td>
                  <td className="st-num st-fig">
                    {e.episodes.length}
                    {e.episodes_hidden > 0 && (
                      <span className="st-quiet" title="Episodes you can't read">
                        {" "}
                        +{e.episodes_hidden}
                      </span>
                    )}
                  </td>
                  <td className="st-num st-fig">
                    <AdviceCell e={e} />
                  </td>
                  <td className={e.rating === "not_helpful" ? "st-out" : undefined}>
                    {e.rating?.replace("_", " ") ?? (e.reward != null ? `reward ${e.reward}` : "—")}
                  </td>
                </tr>
                {isOpen && (
                  <tr className="st-detail">
                    <td colSpan={7}>
                      <div className="st-detail-in">
                        {e.goal && (
                          <p>
                            <b>Goal</b> {e.goal}
                          </p>
                        )}
                        {e.answer && (
                          <p className="st-answer">
                            {e.answer}
                            {e.answer_cut ? "…" : ""}
                          </p>
                        )}
                        {e.session?.served?.length ? (
                          <p>
                            <b>Advice served</b> {e.session.served.join(", ")}
                          </p>
                        ) : null}
                        {e.kind === "session" && (
                          <>
                            <p>
                              <b>Advice lookups</b>{" "}
                              <span className="st-quiet">
                                each time its agent asked, and what it came to
                              </span>
                            </p>
                            <LookupList lookups={e.session?.lookups ?? []} />
                          </>
                        )}
                        {e.episodes.length > 0 && (
                          <p className="st-chips">
                            <b>Episodes</b>
                            {e.episodes.map((id) => (
                              <button
                                key={id}
                                className="st-chip mono"
                                onClick={() => onEpisode(id)}
                              >
                                {id.slice(0, 12)}
                              </button>
                            ))}
                          </p>
                        )}
                      </div>
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      {more && <p className="st-quiet st-pad">Showing the newest 100.</p>}
    </div>
  );
}

/* ---------- Serving ---------- */

/** One line of a Serving table: sessions, how many were served a note, lookups, the share that gave advice, and
 * what most often didn't. */
function TallyCells({ t }: { t: Tally }) {
  const miss = t.lookups ? topMiss(t) : null;
  return (
    <>
      <td className="st-num st-fig">{t.sessions || "—"}</td>
      <td className="st-num st-fig">
        {t.sessions ? (
          <>
            {t.sessions_served}{" "}
            <span className="st-quiet">({share(t.sessions_served, t.sessions)})</span>
          </>
        ) : (
          "—"
        )}
      </td>
      <td className="st-num st-fig">{t.lookups}</td>
      <td className="st-num st-fig">{share(t.served, t.lookups)}</td>
      <td className="st-small">{miss ?? <span className="st-quiet">—</span>}</td>
    </>
  );
}

const TALLY_HEAD = (
  <>
    <th scope="col" className="st-num">
      Sessions
    </th>
    <th scope="col" className="st-num" title="Sessions served at least one note">
      Served a note
    </th>
    <th scope="col" className="st-num">
      Lookups
    </th>
    <th scope="col" className="st-num" title="Lookups that gave the agent advice">
      Advice given
    </th>
    <th scope="col">Most often not, because</th>
  </>
);

/** A share as a hairline bar, beside its figure. */
function Bar({ n, of }: { n: number; of: number }) {
  const w = of ? Math.max(1, Math.round((100 * n) / of)) : 0;
  return (
    <span className="st-share" aria-hidden="true">
      <i style={{ width: `${w}%` }} />
    </span>
  );
}

/** A ranked list of advice lines: the most served, the most matched with no note, the most judged off-task. */
function KeyTable({
  title,
  about,
  rows,
  sessions,
}: {
  title: string;
  about: string;
  rows: { key: string; lookups: number; sessions?: number }[];
  sessions?: boolean;
}) {
  return (
    <section className="st-subsection">
      <h2 className="st-subhead">
        {title} <span className="st-quiet st-fig">{rows.length}</span>
      </h2>
      <p className="st-lede st-quiet st-small">{about}</p>
      {rows.length === 0 ? (
        <p className="st-quiet st-pad">None in this window.</p>
      ) : (
        <div className="st-table-wrap st-pad-x">
          <table className="st-table">
            <thead>
              <tr>
                <th scope="col">Advice line</th>
                {sessions && (
                  <th scope="col" className="st-num">
                    Sessions
                  </th>
                )}
                <th scope="col" className="st-num">
                  Lookups
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.key}>
                  <td className="mono st-small">{r.key}</td>
                  {sessions && <td className="st-num st-fig">{r.sessions ?? "—"}</td>}
                  <td className="st-num st-fig">{r.lookups}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/** `#/l/`: when the memory serves advice and when it doesn't. Each time an agent asks (a session's start, a prompt
 * a hook routes, a later moment, a hook's step) its session keeps what the ask came to; this is their sum over the
 * window, in all and by caller, moment, person and harness, with the lines served most and the ones that most often
 * served nothing. */
export function AdviceServing({ sel, onUser }: { sel: Selection; onUser: (u: string) => void }) {
  const [days, setDays] = useChoice("days", WINDOWS, 7);
  const { data, error, loading, reload } = useAdmin<Activity>(sel, {
    op: "activity",
    sinceDays: days,
    limit: 1,
  });
  const off = error instanceof ApiError && error.status === 403 && /activity/i.test(error.message);
  const s = data?.serving;
  const window = (
    <div className="st-seg" role="group" aria-label="Window">
      {WINDOWS.map((d) => (
        <button
          key={d}
          className={`st-seg-b ${d === days ? "on" : ""}`}
          aria-pressed={d === days}
          onClick={() => setDays(d)}
        >
          {d} days
        </button>
      ))}
    </div>
  );
  if (off) {
    return (
      <Sheet title="Serving">
        <div className="st-empty">
          <h2>Activity is off for {sel.org}</h2>
          <p className="st-quiet">
            Serving reads people's sessions, which the org keeps to themselves. Turn activity on in
            People to see when advice is served.
          </p>
        </div>
      </Sheet>
    );
  }
  const outcomes = s
    ? OUTCOMES.filter((o) => (s.outcomes[o.id] ?? 0) > 0).concat(
        Object.keys(s.outcomes)
          .filter((id) => !OUTCOMES.some((o) => o.id === id))
          .map((id) => ({ id, label: outcomeLabel(id), about: "", given: false })),
      )
    : [];
  const callers = s ? Object.entries(s.by_caller) : [];
  const moments = s ? Object.entries(s.by_moment).sort((a, b) => b[1].lookups - a[1].lookups) : [];
  return (
    <Sheet
      title="Serving"
      facts={
        s
          ? [
              ["Lookups", s.lookups],
              ["Advice given", share(s.served, s.lookups)],
              [
                "Sessions served a note",
                `${s.sessions_served} of ${s.sessions} (${share(s.sessions_served, s.sessions)})`,
              ],
              ["Held out", s.sessions_withheld],
            ]
          : undefined
      }
      tools={window}
    >
      {error ? (
        <Failed error={error} onRetry={reload} />
      ) : loading ? (
        <p className="st-quiet st-pad">Reading the answer log…</p>
      ) : !s ? (
        <p className="st-quiet st-pad">
          This memory's engine doesn't record advice lookups yet: update it to see them here.
        </p>
      ) : (
        <>
          <p className="st-lede st-quiet st-small">
            Each time an agent asks for advice (a session's start, a prompt a hook routes, a later
            moment, a hook's step) its session keeps what the ask came to.
            {s.lookups_since != null && (
              <>
                {" "}
                Lookups are known since {when(s.lookups_since)}; sessions before that count only in
                sessions served.
              </>
            )}
          </p>
          <section className="st-subsection">
            <h2 className="st-subhead">
              What each lookup came to <span className="st-quiet st-fig">{s.lookups}</span>
            </h2>
            {s.lookups === 0 ? (
              <p className="st-quiet st-pad">No lookups recorded in this window.</p>
            ) : (
              <div className="st-table-wrap st-pad-x">
                <table className="st-table">
                  <thead>
                    <tr>
                      <th scope="col">Came to</th>
                      <th scope="col">What it means</th>
                      <th scope="col" className="st-num">
                        Lookups
                      </th>
                      <th scope="col" className="st-num">
                        Share
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {outcomes.map((o) => {
                      const n = s.outcomes[o.id] ?? 0;
                      return (
                        <tr key={o.id}>
                          <td className={o.given ? "st-within" : undefined}>
                            {o.given ? "Given: " : "Not given: "}
                            {o.label}
                          </td>
                          <td className="st-quiet st-small">{o.about}</td>
                          <td className="st-num st-fig">{n}</td>
                          <td className="st-num st-fig st-nowrap">
                            <Bar n={n} of={s.lookups} /> {share(n, s.lookups)}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </section>
          <section className="st-subsection">
            <h2 className="st-subhead">People and programs</h2>
            <p className="st-lede st-quiet st-small">
              Programs are sessions a program ran (a <code>claude -p</code> or SDK run: a reviewer,
              a grader, an eval), as its hook names them.
            </p>
            <div className="st-table-wrap st-pad-x">
              <table className="st-table">
                <thead>
                  <tr>
                    <th scope="col">Caller</th>
                    {TALLY_HEAD}
                  </tr>
                </thead>
                <tbody>
                  {callers.map(([k, t]) => (
                    <tr key={k}>
                      <td>{k === "people" ? "People" : "Programs"}</td>
                      <TallyCells t={t} />
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
          {moments.length > 0 && (
            <section className="st-subsection">
              <h2 className="st-subhead">When in a session</h2>
              <div className="st-table-wrap st-pad-x">
                <table className="st-table">
                  <thead>
                    <tr>
                      <th scope="col">Moment</th>
                      <th scope="col" className="st-num">
                        Lookups
                      </th>
                      <th scope="col" className="st-num">
                        Advice given
                      </th>
                      <th scope="col">Most often not, because</th>
                    </tr>
                  </thead>
                  <tbody>
                    {moments.map(([m, t]) => (
                      <tr key={m}>
                        <td>{MOMENTS[m] ?? m}</td>
                        <td className="st-num st-fig">{t.lookups}</td>
                        <td className="st-num st-fig">{share(t.served, t.lookups)}</td>
                        <td className="st-small">{topMiss(t) ?? "—"}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </section>
          )}
          <section className="st-subsection">
            <h2 className="st-subhead">
              By person <span className="st-quiet st-fig">{s.by_person.length}</span>
            </h2>
            <div className="st-table-wrap st-pad-x">
              <table className="st-table">
                <thead>
                  <tr>
                    <th scope="col">Person</th>
                    {TALLY_HEAD}
                  </tr>
                </thead>
                <tbody>
                  {s.by_person.map((t) => (
                    <tr key={t.user}>
                      <td>
                        <button className="st-name-btn" onClick={() => onUser(t.user)}>
                          {t.user}
                        </button>
                      </td>
                      <TallyCells t={t} />
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
          <section className="st-subsection">
            <h2 className="st-subhead">
              By harness <span className="st-quiet st-fig">{s.by_harness.length}</span>
            </h2>
            <div className="st-table-wrap st-pad-x">
              <table className="st-table">
                <thead>
                  <tr>
                    <th scope="col">Harness</th>
                    {TALLY_HEAD}
                  </tr>
                </thead>
                <tbody>
                  {s.by_harness.map((t) => (
                    <tr key={t.harness}>
                      <td className="mono st-small">{t.harness}</td>
                      <TallyCells t={t} />
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
          <KeyTable
            title="Served most"
            about="The advice sessions were served most: a note's sessions count since sessions began, its lookups since lookups were kept."
            rows={s.top_served}
            sessions
          />
          <KeyTable
            title="Matched, but no note"
            about="Kinds of task asks were routed to that had nothing to serve: where a note would be served if one were learned."
            rows={s.top_no_advice}
          />
          <KeyTable
            title="Judged off-task"
            about="Advice routed to, then judged to be for another task when read whole: often a neighbouring task's note."
            rows={s.top_off_task}
          />
        </>
      )}
    </Sheet>
  );
}

/* ---------- Not answered ---------- */

/**
 * Why an entry counts as not answered: rated unhelpful, or answered without a fact from the memory
 * (unless the asker rated it helpful).
 */
function gapOf(e: Entry): "unhelpful" | "went wrong" | "no facts" | null {
  if (e.rating === "not_helpful") return "unhelpful";
  // A session or run whose outcome was scored below zero: it went wrong for its asker.
  if (e.reward != null && e.reward < 0) return "went wrong";
  if (e.rating === "helpful") return null;
  const cited = Array.isArray(e.cited) ? e.cited.length : e.cited;
  return e.kind === "answer" && cited === 0 && e.episodes.length === 0 && !e.episodes_hidden
    ? "no facts"
    : null;
}

export function NotAnswered({
  sel,
  onUser,
  onEpisode,
  onTrace,
}: {
  sel: Selection;
  onUser: (u: string) => void;
  onEpisode: (id: string) => void;
  onTrace: (id: string) => void;
}) {
  const [days, setDays] = useChoice("days", WINDOWS, 30);
  const { data, error, loading, reload } = useAdmin<Activity>(sel, {
    op: "activity",
    sinceDays: days,
    limit: 500,
  });
  const [open, setOpen] = useOption("open");
  const fetched = data?.entries.length ?? 0;
  const all = asked(data?.entries ?? []);
  const gaps = all.map((e) => ({ e, why: gapOf(e) })).filter((g) => g.why);
  const parts = threads(all);
  const unhelpful = gaps.filter((g) => g.why !== "no facts").length;
  const answers = all.filter((e) => e.kind === "answer").length;
  const sessions = all.length - answers;
  const off = error instanceof ApiError && error.status === 403 && /activity/i.test(error.message);
  return (
    <Sheet
      title="Not answered"
      facts={
        data
          ? [
              ["Sessions and runs", sessions],
              ["Answers", answers],
              ["Unhelpful or went wrong", unhelpful],
              ["Answered with no facts", gaps.length - unhelpful],
            ]
          : undefined
      }
      tools={
        <div className="st-seg" role="group" aria-label="Window">
          {WINDOWS.map((d) => (
            <button
              key={d}
              className={`st-seg-b ${d === days ? "on" : ""}`}
              aria-pressed={d === days}
              onClick={() => setDays(d)}
            >
              {d} days
            </button>
          ))}
        </div>
      }
    >
      <p className="st-lede st-quiet">
        Questions the memory didn't answer well: the asker rated it unhelpful, a session or run went
        wrong (the judge found it failed, or its reward is below zero), or a logged answer used no
        fact or episode from the memory. Sessions are judged by how they went, not by what they
        cited. Each is a gap to fill, with a source to connect or a goal to extract for.
      </p>
      {off ? (
        <p className="st-quiet st-pad">
          Activity is off for {sel.org}, so its questions stay with their askers. Turn it on in
          People.
        </p>
      ) : error ? (
        <Failed error={error} onRetry={reload} />
      ) : loading ? (
        <p className="st-quiet st-pad">Reading the answer log…</p>
      ) : gaps.length === 0 ? (
        <p className="st-quiet st-pad">
          {all.length
            ? [
                sessions &&
                  (sessions === 1
                    ? "The one session or run didn't go wrong and wasn't rated unhelpful."
                    : `None of the ${sessions} sessions and runs went wrong or was rated unhelpful.`),
                answers &&
                  (answers === 1
                    ? "The one logged answer used the memory."
                    : `Every one of the ${answers} logged answers used the memory.`),
              ]
                .filter(Boolean)
                .join(" ")
            : `No questions in the last ${days} days.`}
          {data?.more && ` Only the newest ${fetched} in the last ${days} days were checked.`}
        </p>
      ) : (
        <div className="st-table-wrap st-pad-x">
          {data?.more && (
            <p className="st-quiet st-small">
              Only the newest {fetched} in the last {days} days were checked.
            </p>
          )}
          <table className="st-table">
            <thead>
              <tr>
                <th scope="col">When</th>
                <th scope="col">Asked by</th>
                <th scope="col">Question</th>
                <th scope="col">Why</th>
              </tr>
            </thead>
            <tbody>
              {gaps.map(({ e, why }) => {
                const isOpen = open === e.id;
                return (
                  <Fragment key={e.id}>
                    <tr>
                      <td className="st-quiet st-nowrap">{ago(e.updated_at || e.asked_at)}</td>
                      <td>
                        <button className="st-link" onClick={() => onUser(e.user)}>
                          {e.user}
                        </button>
                      </td>
                      <td>
                        {e.withheld ? (
                          <span
                            className="st-quiet"
                            title="Kept to its asker, or resting on episodes you can't read"
                          >
                            Withheld
                          </span>
                        ) : (
                          <button
                            className="st-name-btn st-q"
                            aria-expanded={isOpen}
                            onClick={() => setOpen(isOpen ? null : e.id)}
                          >
                            {e.question || "(no question)"}
                          </button>
                        )}
                        <PartOf part={parts.get(e.id)} />
                      </td>
                      <td className={why === "no facts" ? "st-watch" : "st-out"}>
                        {why === "unhelpful"
                          ? "Rated unhelpful"
                          : why === "went wrong"
                            ? "Went wrong"
                            : "No facts used"}
                      </td>
                    </tr>
                    {isOpen && (
                      <tr className="st-detail">
                        <td colSpan={4}>
                          <div className="st-detail-in">
                            {e.answer ? (
                              <p className="st-answer">
                                {e.answer}
                                {e.answer_cut ? "…" : ""}
                              </p>
                            ) : (
                              <p className="st-quiet">No answer was logged.</p>
                            )}
                            {e.reward_reason && <p className="st-quiet">{e.reward_reason}</p>}
                            {e.transcript && (
                              <p>
                                <button className="st-link" onClick={() => onTrace(e.id)}>
                                  Trace the conversation
                                </button>
                              </p>
                            )}
                            {e.episodes.length > 0 && (
                              <p className="st-chips">
                                <b>Episodes</b>
                                {e.episodes.map((id) => (
                                  <button
                                    key={id}
                                    className="st-chip mono"
                                    onClick={() => onEpisode(id)}
                                  >
                                    {id.slice(0, 12)}
                                  </button>
                                ))}
                              </p>
                            )}
                          </div>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
          {data?.more && (
            <p className="st-quiet st-pad">Read from the newest 500 entries in this window.</p>
          )}
        </div>
      )}
    </Sheet>
  );
}

/* ---------- Advice ---------- */

interface Use {
  id: string;
  user?: string | null;
  question?: string | null;
  at?: number | null;
  reward?: number | null;
  applied?: boolean | null;
  helped?: boolean | null;
  outcome?: string | null;
  /** How the session went by what the person said (their rating, or their next messages), apart from the judge. */
  checked?: boolean | null;
}
/** A note's tally, as serving and ratification read it: each person's latest judged session with it. */
interface Evidence {
  served: number;
  applied: number;
  helped: number;
  not_helped: number;
  served_succeeded: number;
  served_failed: number;
  withheld: number;
  withheld_succeeded: number;
  withheld_failed: number;
  checked_served_succeeded: number;
  checked_served_failed: number;
  checked_withheld_succeeded: number;
  checked_withheld_failed: number;
  /** Success rate served less success rate withheld, once both sides have enough sessions (by the judge). */
  value?: number | null;
  /** The same by the outcomes people gave. */
  checked_value?: number | null;
  /** Whether people's outcomes prove it (null until both sides have enough of them). */
  proven?: boolean | null;
  ratified?: boolean;
}
interface Item {
  id: string;
  section: string;
  text: string;
  served: number;
  helped: number;
  harmed: number;
}
interface Note {
  key: string;
  category: string;
  situation: string;
  learned_from: Use[];
  served_in: Use[];
  validated_in: Use[];
  /** The sessions it was withheld from (the holdout); absent from older engines. */
  withheld_in?: Use[];
  people: number;
  /** The note's text, where one is written and the reader may read it; a bare situation has none. */
  note?: string | null;
  /** What the note was last written from: its runs, and how many of them went wrong. */
  written?: { runs: number; wrong: number } | null;
  /** Each item of the note, with the judged sessions it was shown in and helped or harmed in. */
  items?: Item[];
  /** An admin held it back from everyone. */
  held?: boolean;
  /** Absent from older engines, and for a note no judged session has used. */
  evidence?: Evidence | null;
}
interface AdviceTrail {
  notes: Note[];
  map: boolean;
  evidence_bar?: number;
  proof_sessions?: number;
}

type Impact = "hurting" | "proven" | "judged" | "early" | "unserved" | "none";
const IMPACTS: { id: Impact; title: string; about: string }[] = [
  {
    id: "hurting",
    title: "Hurting",
    about:
      "Sessions went worse with it than without it, the judge found it didn't help more often than it helped, one of its items did more harm than good, or agents mostly didn't use it where it was served.",
  },
  {
    id: "proven",
    title: "Helping, proven",
    about:
      "People's own outcomes (their ratings and next messages) went better in the sessions it was served in than in the holdout.",
  },
  {
    id: "judged",
    title: "Helping, by the judge",
    about:
      "The judge found it helped more often than not. Not proven yet: the judge misses most failures, so this waits on people's outcomes.",
  },
  {
    id: "early",
    title: "Too early to tell",
    about: "Served, but no judged session says whether it helps yet.",
  },
  {
    id: "unserved",
    title: "Not served yet",
    about: "No session has been served it.",
  },
  {
    id: "none",
    title: "No note yet",
    about:
      "Situations AmpUp Memory has seen but not written a note for: one gets a note once enough of its runs went wrong, from enough people.",
  },
];

/** Served to at least this many people, a note applied in fewer than half of them is served where it doesn't fit. */
const MISFIRE_MIN = 3;
const pct = (ok: number, bad: number) =>
  ok + bad ? `${Math.round((100 * ok) / (ok + bad))}% of ${ok + bad}` : "—";
const points = (v: number) => `${v > 0 ? "+" : ""}${Math.round(v * 100)} pts`;

/** Where a note stands, and the reason, from the server's tally (the one serving and ratification go by). */
function impact(n: Note): { is: Impact; why: string } {
  const e = n.evidence;
  if (!n.note) return { is: "none", why: "" };
  const harmed = (n.items ?? []).filter((i) => i.harmed > i.helped);
  if (!e || e.served + e.withheld === 0) {
    return n.served_in.length
      ? { is: "early", why: "Served, no judged session yet." }
      : { is: "unserved", why: "" };
  }
  if (e.proven === false) {
    return { is: "hurting", why: "People's outcomes: no better served than withheld." };
  }
  if (e.checked_value != null && e.checked_value < 0) {
    return {
      is: "hurting",
      why: `People's outcomes ${points(e.checked_value)} against the holdout.`,
    };
  }
  if (e.value != null && e.value < 0) {
    return { is: "hurting", why: `Success ${points(e.value)} against the holdout (judge).` };
  }
  if (e.not_helped > e.helped) {
    return { is: "hurting", why: `The judge: didn't help ${e.not_helped}, helped ${e.helped}.` };
  }
  if (harmed.length) {
    return {
      is: "hurting",
      why: `${harmed.length} item${harmed.length > 1 ? "s" : ""} harmed more than helped.`,
    };
  }
  // Served where it doesn't fit: an agent shown it mostly didn't use it, so it costs attention for nothing.
  if (e.served >= MISFIRE_MIN && e.applied * 2 < e.served) {
    return {
      is: "hurting",
      why: `Applied in ${e.applied} of ${e.served}: mostly served where it doesn't fit.`,
    };
  }
  if (e.proven === true) {
    return {
      is: "proven",
      why: `People's outcomes ${points(e.checked_value ?? 0)} against the holdout.`,
    };
  }
  if (e.helped > e.not_helped) {
    return { is: "judged", why: `The judge: helped ${e.helped} of ${e.served}.` };
  }
  if (e.served === 0) {
    return { is: "early", why: "Only withheld so far (the holdout)." };
  }
  return { is: "early", why: "Served, but nothing yet says it helps or hurts." };
}

export function Advice({ sel, onUser }: { sel: Selection; onUser: (u: string) => void }) {
  const { data, error, loading, reload } = useAdmin<AdviceTrail>(sel, { op: "advice_trail" });
  const [open, setOpen] = useOption("open");
  const owners = useOwners(sel.org);
  const notes = [...(data?.notes ?? [])].sort(
    (a, b) => b.served_in.length - a.served_in.length || a.key.localeCompare(b.key),
  );
  const unproven = notes.filter((n) => n.served_in.length > 0 && n.validated_in.length === 0);
  // Grouped by impact from the server's evidence; an engine that predates it gets the plain list.
  const hasEvidence = notes.some((n) => n.withheld_in !== undefined);
  const graded = notes.map((n) => ({ n, ...impact(n) }));
  const count = (i: Impact) => graded.filter((g) => g.is === i).length;
  const showImpact = hasEvidence;
  // A header count jumps to its group.
  const jump = (i: Impact) => {
    const c = count(i);
    return c ? (
      <button
        className="st-jump"
        onClick={() =>
          document.getElementById(`advice-${i}`)?.scrollIntoView({ behavior: "smooth" })
        }
      >
        {c}
      </button>
    ) : (
      c
    );
  };
  const detail = (n: Note, cols: number) => (
    <tr className="st-detail">
      <td colSpan={cols}>
        {n.note ? (
          <div className="st-detail-in st-note">
            <p className="st-note-src">
              The note AmpUp Memory serves for this situation, learned from{" "}
              <b>{n.written?.runs ?? n.learned_from.length} graded runs</b>
              {n.written && (
                <>
                  {" "}
                  ({n.written.runs - n.written.wrong} went right, {n.written.wrong} went wrong)
                </>
              )}
              : the steps of the runs that went right and the pitfalls of the rest.
            </p>
            <Items items={n.items ?? []} />
            <div className="st-note-body">
              {/* Without its items' ids (`<!--i:…-->`), as an agent is served it. */}
              <Markdown text={n.note.replace(/ ?<!--i:[0-9a-f]+-->/g, "")} />
            </div>
          </div>
        ) : (
          <p className="st-detail-in st-quiet">
            No note yet: a situation gets one once enough of its runs went wrong, from enough
            people.
          </p>
        )}
        <div className="st-detail-in st-trail">
          <Trail title="Served in" uses={n.served_in} onUser={onUser} />
          {n.withheld_in && (
            <Trail title="Withheld from (holdout)" uses={n.withheld_in} onUser={onUser} />
          )}
          <Trail title="Learned from" uses={n.learned_from} onUser={onUser} />
        </div>
      </td>
    </tr>
  );
  const name = (n: Note, isOpen: boolean) => (
    <>
      <button
        className="st-name-btn"
        aria-expanded={isOpen}
        onClick={() => setOpen(isOpen ? null : n.key)}
      >
        {n.situation}
      </button>
      <div className="st-quiet st-small">
        {n.category}
        {!n.note && !showImpact && " · no note yet"}
        {n.held && " · held back"}
        <OwnerHint who={owners.of("category", n.category)} />
      </div>
    </>
  );
  return (
    <Sheet
      title="Advice"
      facts={
        !data?.map
          ? undefined
          : showImpact
            ? IMPACTS.map(({ id, title }): [string, React.ReactNode] => [title, jump(id)])
            : [
                ["Notes", notes.length],
                ["Served", notes.reduce((n, x) => n + x.served_in.length, 0)],
                ["Never validated", unproven.length],
              ]
      }
    >
      {error ? (
        <Failed error={error} onRetry={reload} />
      ) : loading ? (
        <p className="st-quiet st-pad">Reading the advice map…</p>
      ) : !data?.map ? (
        <p className="st-quiet st-pad">No advice learned here yet, or none you may read.</p>
      ) : notes.length === 0 ? (
        <p className="st-quiet st-pad">The advice map has no notes yet.</p>
      ) : showImpact ? (
        <div className="st-pad-x">
          <p className="st-quiet st-small st-impact-note">
            Each person counts once, by their latest judged session with the note. Sessions run by
            programs (a <code>claude -p</code> grader, a probe) are left out. “Went well” is what
            the person said (their rating, or their next message accepting the answer), not the
            judge. A note is proven once that covers {data.proof_sessions ?? 5} sessions on each
            side, served and held out.
          </p>
          {IMPACTS.map(({ id, title, about }) => {
            const rows = graded.filter((g) => g.is === id);
            if (!rows.length) return null;
            return (
              <section key={id} id={`advice-${id}`} className={`st-impact st-impact-${id}`}>
                <h2>
                  {title} <span className="st-quiet st-fig">{rows.length}</span>
                </h2>
                <p className="st-quiet st-small">{about}</p>
                <div className="st-table-wrap">
                  {id === "none" ? (
                    // Nothing served, nothing to measure: the situation and the runs seen so far.
                    <table className="st-table">
                      <thead>
                        <tr>
                          <th scope="col">Situation</th>
                          <th scope="col" className="st-num">
                            Runs seen
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {rows.map(({ n }) => {
                          const isOpen = open === n.key;
                          return (
                            <Fragment key={n.key}>
                              <tr>
                                <td>{name(n, isOpen)}</td>
                                <td className="st-num st-fig">{n.learned_from.length}</td>
                              </tr>
                              {isOpen && detail(n, 2)}
                            </Fragment>
                          );
                        })}
                      </tbody>
                    </table>
                  ) : (
                    <table className="st-table">
                      <thead>
                        <tr>
                          <th scope="col">Note</th>
                          <th scope="col">Why</th>
                          <th
                            scope="col"
                            className="st-num"
                            title="People served it, and how many applied it"
                          >
                            Applied
                          </th>
                          <th
                            scope="col"
                            className="st-num"
                            title="The judge: helped / didn't help"
                          >
                            Judge
                          </th>
                          <th
                            scope="col"
                            className="st-num"
                            title="Sessions it was served in that went well, by what the person said"
                          >
                            Went well, served
                          </th>
                          <th
                            scope="col"
                            className="st-num"
                            title="Sessions it was withheld from (the holdout) that went well, by what the person said"
                          >
                            Went well, held out
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {rows.map(({ n, why }) => {
                          const isOpen = open === n.key;
                          const e = n.evidence;
                          return (
                            <Fragment key={n.key}>
                              <tr>
                                <td>{name(n, isOpen)}</td>
                                <td className={id === "hurting" ? "st-out" : "st-quiet"}>{why}</td>
                                <td className="st-num st-fig">
                                  {e?.served ? `${e.applied} of ${e.served}` : "—"}
                                </td>
                                <td className="st-num st-fig">
                                  {e?.served ? `${e.helped} / ${e.not_helped}` : "—"}
                                </td>
                                <td className="st-num st-fig">
                                  {e
                                    ? pct(e.checked_served_succeeded, e.checked_served_failed)
                                    : "—"}
                                </td>
                                <td className="st-num st-fig">
                                  {e
                                    ? pct(e.checked_withheld_succeeded, e.checked_withheld_failed)
                                    : "—"}
                                </td>
                              </tr>
                              {isOpen && detail(n, 6)}
                            </Fragment>
                          );
                        })}
                      </tbody>
                    </table>
                  )}
                </div>
              </section>
            );
          })}
        </div>
      ) : (
        <div className="st-table-wrap st-pad-x">
          <table className="st-table">
            <thead>
              <tr>
                <th scope="col">Note</th>
                <th scope="col" className="st-num">
                  Learned from
                </th>
                <th scope="col" className="st-num">
                  Served
                </th>
                <th scope="col" className="st-num">
                  Validated
                </th>
                <th scope="col" className="st-num">
                  People
                </th>
                <th scope="col">Last served</th>
              </tr>
            </thead>
            <tbody>
              {notes.map((n) => {
                const isOpen = open === n.key;
                return (
                  <Fragment key={n.key}>
                    <tr>
                      <td>{name(n, isOpen)}</td>
                      <td className="st-num st-fig">{n.learned_from.length}</td>
                      <td className="st-num st-fig">{n.served_in.length}</td>
                      <td
                        className={`st-num st-fig ${
                          n.served_in.length && !n.validated_in.length ? "st-watch" : ""
                        }`}
                      >
                        {n.validated_in.length}
                      </td>
                      <td className="st-num st-fig">{n.people}</td>
                      <td className="st-quiet">{ago(n.served_in[0]?.at)}</td>
                    </tr>
                    {isOpen && detail(n, 6)}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Sheet>
  );
}

/** A note's items that judged sessions say helped or harmed, those that harmed first. */
function Items({ items }: { items: Item[] }) {
  const told = items
    .filter((i) => i.helped || i.harmed)
    .sort((a, b) => b.harmed - a.harmed || b.helped - a.helped);
  if (!told.length) return null;
  return (
    <ul className="st-items">
      {told.map((i) => (
        <li key={i.id}>
          <span className={i.harmed > i.helped ? "st-out" : "st-within"}>
            {i.harmed > i.helped ? "harmed" : "helped"}
          </span>{" "}
          <span className="st-quiet st-fig">
            {i.helped}↑ {i.harmed}↓ of {i.served}
          </span>{" "}
          {i.text}
        </li>
      ))}
    </ul>
  );
}

function Trail({
  title,
  uses,
  onUser,
}: {
  title: string;
  uses: Use[];
  onUser: (u: string) => void;
}) {
  return (
    <section>
      <h3>
        {title} <span className="st-quiet st-fig">{uses.length}</span>
      </h3>
      {uses.length === 0 ? (
        <p className="st-quiet">None yet.</p>
      ) : (
        <ul className="st-uses">
          {uses.slice(0, 20).map((u) => (
            <li key={`${title}-${u.id}`}>
              <span className="st-quiet st-nowrap">{ago(u.at)}</span>{" "}
              {u.user && (
                <button className="st-link" onClick={() => onUser(u.user!)}>
                  {u.user}
                </button>
              )}{" "}
              {u.reward != null && (
                <span className={u.reward > 0 ? "st-within" : "st-out"}>
                  {u.reward > 0 ? "went right " : "went wrong "}
                </span>
              )}
              {u.question ? (
                <span className="st-clamp">— {u.question}</span>
              ) : (
                <span className="st-quiet">— withheld</span>
              )}
              {u.helped === false && <span className="st-out"> · didn't help</span>}
              {u.checked === true && <span className="st-within"> · the person was satisfied</span>}
              {u.checked === false && <span className="st-out"> · the person corrected it</span>}
              {u.outcome && <span className="st-quiet"> · {u.outcome}</span>}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/* ---------- Branches ---------- */

interface Branch {
  name: string;
  skills: number;
}
interface Diff {
  main_changed: number;
  branch_changed: number;
  /** Commits on the branch main doesn't have yet: none once it's merged (absent from older engines). */
  branch_commits?: number;
  conflicts: string[];
  /** Each conflict's values on both sides since the branch's base, those the admin may read. */
  conflict_values?: ConflictValues[];
}
interface ConflictValues {
  key: string;
  main: string[];
  branch: string[];
  main_hidden: number;
  branch_hidden: number;
}
type Side = "main" | "branch";

export function Branches({
  sel,
  branches,
}: {
  sel: Selection;
  branches: Branch[] | null | undefined;
}) {
  const others = (branches ?? []).filter((b) => b.name !== "main");
  const owners = useOwners(sel.org);
  return (
    <Sheet title="Branches" facts={[["Branches off main", others.length]]}>
      <p className="st-lede st-quiet">
        Long-running agents and memory builds write on a branch of their own. Where main and the
        branch changed a key differently, the key&apos;s owner picks the value that stands; the
        other is kept as history, true until the merge.
      </p>
      {branches === undefined ? (
        <p className="st-quiet st-pad">Reading branches…</p>
      ) : others.length === 0 ? (
        <p className="st-quiet st-pad">Only main: nothing has diverged.</p>
      ) : (
        <>
          <div className="st-table-wrap st-pad-x">
            <table className="st-table">
              <thead>
                <tr>
                  <th scope="col">Branch</th>
                  <th scope="col" className="st-num">
                    Changed on branch
                  </th>
                  <th scope="col" className="st-num">
                    Changed on main
                  </th>
                  <th scope="col">Conflicts</th>
                  <th scope="col" className="st-act">
                    <span className="st-sr">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {others.map((b) => (
                  <BranchRow key={b.name} sel={sel} name={b.name} ownerOf={owners.of} />
                ))}
              </tbody>
            </table>
          </div>
          <p className="st-quiet st-small st-pad-x">
            Branches already merged into main are left out. A session&apos;s branch (where the org
            gives each session one) merges on its own once the session ends, unless a key conflicts.
          </p>
        </>
      )}
    </Sheet>
  );
}

/** A side's values for a conflicting key, and how many come from what the admin can't read. */
function Values({ values, hidden }: { values: string[]; hidden: number }) {
  return (
    <>
      {values.map((x) => (
        <div key={x}>{x}</div>
      ))}
      {hidden > 0 && <div className="st-quiet st-small">{hidden} you can&apos;t read</div>}
    </>
  );
}

function BranchRow({
  sel,
  name,
  ownerOf,
}: {
  sel: Selection;
  name: string;
  ownerOf: (kind: OwnerKind, area: string) => string | undefined;
}) {
  const { data, error, loading, reload } = useAdmin<Diff>(sel, { op: "branch_diff", branch: name });
  const [merging, setMerging] = useState(false);
  const [said, setSaid] = useState<string | null>(null);
  // The branch opened is kept in the link (`?branch=`), one at a time.
  const [opened, setOpened] = useOption("branch");
  const open = opened === name;
  const setOpen = (o: boolean) => setOpened(o ? name : null);
  const [keep, setKeep] = useState<Record<string, Side>>({});
  const conflicts = data?.conflicts ?? [];
  const decided = conflicts.filter((k) => keep[k]).length;
  const merge = async () => {
    setMerging(true);
    setSaid(null);
    try {
      const chosen = Object.fromEntries(conflicts.map((k) => [k, keep[k]]));
      await admin(sel, {
        op: "merge_branch",
        branch: name,
        ...(conflicts.length ? { keep: chosen } : {}),
      });
      setSaid(
        conflicts.length
          ? `Merged into main, ${conflicts.length} ${conflicts.length === 1 ? "conflict" : "conflicts"} resolved.`
          : "Merged into main.",
      );
      setKeep({});
      setOpen(false);
      reload();
    } catch (e) {
      setSaid(errorText(e));
    } finally {
      setMerging(false);
    }
  };
  const n = data?.conflicts.length ?? 0;
  // Merged, and nothing written on it since: nothing to decide here.
  if (data?.branch_commits === 0 && !said) return null;
  const nothing = data ? (data.branch_commits ?? data.branch_changed) === 0 : false;
  return (
    <>
      <tr>
        <td>
          <span className="mono">{name}</span>
          {name.startsWith("s-") && <span className="st-quiet st-small"> · a session&apos;s</span>}
        </td>
        <td className="st-num st-fig">{data ? data.branch_changed : "…"}</td>
        <td className="st-num st-fig">{data ? data.main_changed : "…"}</td>
        <td>
          {error ? (
            <span className="st-out">{errorText(error)}</span>
          ) : loading ? (
            "…"
          ) : n === 0 ? (
            <span className="st-within">None</span>
          ) : (
            <button
              className="st-name-btn st-out"
              aria-expanded={open}
              onClick={() => setOpen(!open)}
            >
              {n} {n === 1 ? "key" : "keys"}
            </button>
          )}
        </td>
        <td className="st-act">
          <button
            className="st-btn"
            disabled={merging || !data || decided < n || nothing}
            title={
              decided < n
                ? `Pick the value that stands for ${n - decided} more ${n - decided === 1 ? "key" : "keys"}`
                : nothing
                  ? "Nothing on the branch to merge"
                  : "Merge this branch into main"
            }
            onClick={merge}
          >
            {merging ? "Merging…" : "Merge"}
          </button>
        </td>
      </tr>
      {(open || said) && (
        <tr className="st-detail">
          <td colSpan={5}>
            <div className="st-detail-in">
              {said && <p role="status">{said}</p>}
              {open && (
                <table className="st-table st-conflicts">
                  <thead>
                    <tr>
                      <th scope="col">Key</th>
                      <th scope="col">On main</th>
                      <th scope="col">On {name}</th>
                      <th scope="col">Keep</th>
                    </tr>
                  </thead>
                  <tbody>
                    {conflicts.map((k) => {
                      const v = data?.conflict_values?.find((c) => c.key === k);
                      // A key is `<type>:<id>.<predicate>`: its entity type's owner decides it.
                      return (
                        <tr key={k}>
                          <td>
                            <span className="mono">{k}</span>
                            <OwnerHint who={ownerOf("entity_type", k.split(":")[0] ?? "")} />
                          </td>
                          <td>
                            <Values values={v?.main ?? []} hidden={v?.main_hidden ?? 0} />
                          </td>
                          <td>
                            <Values values={v?.branch ?? []} hidden={v?.branch_hidden ?? 0} />
                          </td>
                          <td>
                            <div className="st-seg" role="group" aria-label={`Keep for ${k}`}>
                              {(["main", "branch"] as Side[]).map((side) => (
                                <button
                                  key={side}
                                  className={`st-seg-b ${keep[k] === side ? "on" : ""}`}
                                  aria-pressed={keep[k] === side}
                                  onClick={() => setKeep({ ...keep, [k]: side })}
                                >
                                  {side === "main" ? "Main" : "Branch"}
                                </button>
                              ))}
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              )}
            </div>
          </td>
        </tr>
      )}
    </>
  );
}
