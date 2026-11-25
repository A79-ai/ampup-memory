import "./home.css";
import { useEffect, useMemo, useState } from "react";
import { ApiError, browse, call } from "./api";
import { ago, when } from "./format";
import { appliedSaid, DreamSaid, dreamState } from "./review";
import { needsMe, review, type DreamList } from "./review-api";
import type { Branch, Child, Focus, Listing, Selection } from "./types";
import { EmptyState, ErrorState, Icon } from "./ui";
import { errorText, transport, type Resolved } from "../transport";

// Studio's Home (`#/home`): the memory being read (the org's, or the team picked) as its builders see it. What
// changed this week, what was answered from it, and what waits on the reader, each number a link into the
// section that holds it. Every number is one another section's read would give; one with no cheap read is left out,
// and one the reader may not make (an admin's, a review's) is too. They come in one read (`home`), counts and short
// lists only; from a server before it, a read each. A server that fails the one read is not asked for each part
// instead: Home shows its last answer, marked as not refreshed, or says it failed, with a way to try again.

const WEEK_DAYS = 7;
const WEEK_US = WEEK_DAYS * 86_400_000_000;
/** The newest facts read to say where this week's came from (`count` is the whole week's, whatever this is). */
const FACT_SAMPLE = 200;

/** A link into a section: the focus, and its options (`#/review?tab=conflicts`). */
export type HrefOf = (f: Focus, options?: Record<string, string>) => string;

export interface HomeProps {
  sel: Selection;
  /** What the memory read is called: the org's name, or its team's. */
  scope: string;
  /** The reader, as the memory names them. */
  me?: string;
  admin: boolean;
  can: Resolved;
  href: HrefOf;
  /**
   * Mount point: the "Add to memory" control (a file, a note or a link, added as the reader). Shown first among
   * the actions and in the new memory's guide. Nothing when absent.
   */
  addToMemory?: React.ReactNode;
  /**
   * Where the host connects a source and an agent (the console's Sources and setup pages). Without them, Home
   * says how instead.
   */
  connect?: { sources?: string; agents?: string };
  /**
   * What the one read (`home`) answered so far, for the page around Home: the branches and how many types the
   * schema declares, so it reads neither while Home is open. null: no such read (a server before it, or it failed).
   */
  onSummary?: (h: HomeSummary | null) => void;
}

/**
 * `/browse {op: "home"}`: Home's numbers. A part is null where the reader may not read it (or it failed), and
 * absent while it is still worked out: `pending` names those, for an ask again with `parts`.
 */
export interface HomeSummary {
  /** The week's start (µs): the hour it is now, seven days back. */
  since: number;
  pending?: string[];
  /** The root folders' counts. */
  counts?: Record<string, number | null> | null;
  /** The newest source folders, the newest first (with `week`). */
  sources?: Child[] | null;
  /** The week's facts: how many, how many of the newest were read, and of those the entities and sources. */
  week?: { count: number; sample: number; entities: number; by_source: [string, number][] } | null;
  lessons?: number | null;
  /** The team's notes served this week (with `week`). */
  advice?: { times: number; notes: number; waiting: number } | null;
  activity?: {
    people: number;
    sessions: number;
    served?: number;
    of?: number;
    /** Sessions, runs and answers begun on each of the last 30 days (UTC day start, µs), oldest first. */
    by_day?: [number, number][];
  } | null;
  /** The facts that took effect on each of the last 30 days (UTC day start, µs), oldest first (with `week`). */
  month?: { facts: [number, number][] } | null;
  proposals?: { mine: number; pending: number } | null;
  conflicts?: number | null;
  dream?: DreamList | null;
  branches?: Branch[] | null;
  schema?: { types: number };
}

/** How many times Home asks again for the parts still worked out (an ask that failed included), before it leaves
 * them out. */
const ASK_AGAIN = 5;
/** How long Home waits before asking again after an ask failed, doubled with each failure in a row. */
const AFTER_FAILED_MS = 1000;

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Whether the server doesn't know the one read (`home`): a server from before it, which refuses an op it doesn't
 * know with a 400 "unknown op". Only that is answered with a read each; a server that fails it otherwise (5xx, a
 * timeout, a dropped connection) is struggling, and a read each would only add to its load.
 */
const unknownOp = (e: unknown) =>
  e instanceof ApiError && e.status === 400 && /unknown op/i.test(e.message);

/** The fields a pending part stands for: `week` brings the sources and the advice served too. */
const fieldsOf = (part: string) =>
  part === "week" ? ["week", "sources", "advice", "month"] : [part];

/** `h` with what is still pending left out: null, as a part that failed. */
function settled(h: HomeSummary): HomeSummary {
  const gone = (h.pending ?? []).flatMap(fieldsOf);
  return { ...h, ...Object.fromEntries(gone.map((p) => [p, null])), pending: [] };
}

/**
 * Where the host keeps Home's last complete answer (`Transport.kept`), with its shape's version: a change to
 * `HomeSummary` that an older kept answer wouldn't fit takes the next number.
 */
const KEPT = "home/1";

/**
 * Whether `h` is worth keeping for the next open: the server worked out every part (none pending, none given up)
 * and counted every root folder. An empty memory isn't kept either: one filled since would open on the new memory's
 * guide.
 */
function keepable(h: HomeSummary): boolean {
  const counts = h.counts;
  return (
    !h.pending?.length &&
    !!counts &&
    Object.values(counts).every((n) => typeof n === "number") &&
    !(counts.sources === 0 && counts.entities === 0)
  );
}

/** What the host kept for this selection, if it is a Home answer at all. */
function keptHome(sel: Selection): HomeSummary | undefined {
  try {
    const h = transport().kept?.get(sel, KEPT) as HomeSummary | undefined;
    return h && typeof h === "object" && typeof h.since === "number" && !h.pending?.length
      ? h
      : undefined;
  } catch {
    return undefined;
  }
}

/**
 * The fresh answer so far, its pending parts filled from the kept one: a part the server is still working out shows
 * what it was last time instead of a skeleton, until its own answer lands.
 */
function overKept(h: HomeSummary, kept: HomeSummary): HomeSummary {
  const filled = (h.pending ?? []).filter((p) => fieldsOf(p).every((f) => f in kept));
  if (!filled.length) return h;
  const from = filled.flatMap(fieldsOf).map((f) => [f, kept[f as keyof HomeSummary]]);
  return { ...h, ...Object.fromEntries(from), pending: h.pending!.filter((p) => !filled.includes(p)) };
}

type Got<T> = { key: string; data?: T; failed?: boolean };

/**
 * One read for Home, again when `key` changes; none when `on` is false. A failure leaves its numbers out. `done`:
 * it answered, failed, or isn't made.
 */
function useGot<T>(key: string, on: boolean, load: () => Promise<T>): { data?: T; done: boolean } {
  const [got, setGot] = useState<Got<T> | null>(null);
  useEffect(() => {
    if (!on) return;
    let live = true;
    load().then(
      (data) => live && setGot({ key, data }),
      () => live && setGot({ key, failed: true }),
    );
    return () => {
      live = false;
    };
  }, [key, on]);
  const mine = got?.key === key ? got : null;
  return { data: mine?.data, done: !on || mine !== null };
}

/** How often, and how many times, Home asks again for the root's counts while the server is still counting them. */
const RECOUNT_MS = 2000;
const RECOUNT_TIMES = 30;

/**
 * The Explorer's root, with each folder's count: asked again while some folder has no count yet (the server counts
 * a large memory in the background and answers the folders without counts meanwhile).
 */
function useRoot(
  key: string,
  on: boolean,
  sel: HomeProps["sel"],
): { data?: Listing; done: boolean } {
  const [got, setGot] = useState<Got<Listing> | null>(null);
  useEffect(() => {
    if (!on) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const ask = (left: number) => {
      browse<Listing>({ op: "ls", path: [] }, sel).then(
        (data) => {
          if (!live) return;
          setGot({ key, data });
          if (left > 0 && data.children.some((c) => c.count == null)) {
            timer = setTimeout(() => ask(left - 1), RECOUNT_MS);
          }
        },
        () => live && setGot((g) => (g?.key === key && g.data ? g : { key, failed: true })),
      );
    };
    ask(RECOUNT_TIMES);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [key, on]);
  const mine = got?.key === key ? got : null;
  return { data: mine?.data, done: !on || mine !== null };
}

function useRead<T>(key: string, on: boolean, load: () => Promise<T>): T | undefined {
  return useGot(key, on, load).data;
}

interface FactsPage {
  count: number;
  facts: { entity: string; sources?: { episode_id: string; source?: string | null }[] }[];
}
interface AdviceUsage {
  advice: { used: number; people: number; ratified: boolean; held?: boolean; mine?: boolean }[];
}
interface Activity {
  people: { user: string; sessions: number; runs: number; answers: number }[];
  serving?: { sessions: number; sessions_served: number };
}

/** A source's first segment, as the Explorer files it under `sources/`. */
const prefixOf = (source: string | null | undefined) => (source ?? "").split(":")[0] || "unknown";

export function Home({
  sel,
  scope,
  me,
  admin,
  can,
  href,
  addToMemory,
  connect,
  onSummary,
}: HomeProps) {
  const key = JSON.stringify([sel.org, sel.team, sel.branch]);

  // Every number in one read, shown as it answers: the parts not worked out in time are asked for again. null: a
  // server with no such read (it refused the op as unknown), and Home makes a read each, as before it.
  const [said, setSaid] = useState<{ key: string; h: HomeSummary | null } | null>(null);
  // The one read failed (other than as unknown): why, shown over the kept answer or on its own, with "Try again".
  const [failed, setFailed] = useState<{ key: string; why: string } | null>(null);
  // Bumped by "Try again": the one read is made again.
  const [tries, setTries] = useState(0);
  // The answer the host kept from the last open, shown at once while the read is out (and, part by part, while the
  // server works out the rest), with a quiet "updating" beside the title. The fresh answer replaces it.
  const kept = useMemo(() => keptHome(sel), [key]);
  useEffect(() => {
    let live = true;
    setFailed(null);
    void (async () => {
      // The proposals wait for their branches' states to be built, which a first ask doesn't wait for: asked for
      // beside it, so that the wait starts with the page rather than once the first ask has answered.
      const beside = browse<HomeSummary>({ op: "home", parts: ["proposals"] }, sel).catch(() => null);
      let h: HomeSummary;
      try {
        h = await browse<HomeSummary>({ op: "home" }, sel);
      } catch (e) {
        if (!live) return;
        if (unknownOp(e)) setSaid({ key, h: null });
        else setFailed({ key, why: errorText(e) });
        return;
      }
      if (live && h.pending?.includes("proposals")) {
        if (h.pending.length > 1) setSaid({ key, h });
        const p = await beside;
        if (p && !p.pending?.includes("proposals")) {
          h = { ...h, proposals: p.proposals, pending: h.pending.filter((x) => x !== "proposals") };
        }
      }
      for (let again = 0, fails = 0; live; again++) {
        if (!h.pending?.length || again === ASK_AGAIN) {
          setSaid({ key, h: settled(h) });
          if (keepable(h)) {
            try {
              transport().kept?.set(sel, KEPT, h);
            } catch {
              // Not kept: the next open reads it again, as before.
            }
          }
          return;
        }
        setSaid({ key, h });
        try {
          h = { ...h, ...(await browse<HomeSummary>({ op: "home", parts: h.pending }, sel)) };
          fails = 0;
        } catch {
          // Asked again on the next turn, after a wait that grows with each failure in a row (none after the last
          // ask): what it waited for goes on being worked out, and is kept for it.
          if (again + 1 < ASK_AGAIN) await wait(AFTER_FAILED_MS * 2 ** fails++);
        }
      }
    })();
    return () => {
      live = false;
    };
  }, [key, tries]);
  // undefined until it answers; null: a read each.
  const fresh = said?.key === key ? said.h : undefined;
  // Until the fresh answer is whole, the kept one fills in for what it hasn't answered yet. A server without the
  // one read (null) drops it: Home then reads each part. One that failed it leaves the kept answer as it is. Made
  // once per answer: a new object each render would tell the page around Home (`onSummary`) again on every render,
  // which renders Home again.
  const one = useMemo(
    () => (fresh === undefined ? kept : fresh && kept ? overKept(fresh, kept) : fresh),
    [fresh, kept],
  );
  // Why the one read failed, for this selection: nothing fresh comes, and Home shows the kept answer as it was
  // (not as "updating"), or, with none kept, says so.
  const why = failed?.key === key ? failed.why : null;
  const retry = () => setTries((n) => n + 1);
  const updating = !why && !!kept && (fresh === undefined || !!fresh?.pending?.length);
  const each = one === null;
  useEffect(() => {
    if (one !== undefined) onSummary?.(one);
  }, [one]);
  // A part has answered (or isn't the reader's): from the one read, once it isn't pending; else once its own read has.
  const done = (part: string, got: { done: boolean }) =>
    each ? got.done : one !== undefined && !one.pending?.includes(part);

  // The week, fixed for the page: a re-render doesn't move it.
  const since = useMemo(() => Date.now() * 1000 - WEEK_US, [key]);

  // What the memory holds: the Explorer's root, with each folder's count.
  // Only when the one read is refused (an older server): the root's counts, asked again until counted.
  const rootGot = useRoot(key, each, sel);
  const counts: Record<string, number | null | undefined> | undefined = one
    ? (one.counts ?? undefined)
    : rootGot.data && Object.fromEntries(rootGot.data.children.map((c) => [c.name, c.count]));
  // Each source and when it last brought something.
  const sources = useRead(key, each, () =>
    browse<Listing>({ op: "ls", path: ["sources"], limit: 200 }, sel),
  );
  // The facts that took effect this week: how many, on how many entities, and from where.
  const factsGot = useGot(key, each, () =>
    browse<FactsPage>(
      // As `entities/` counts them: a note's own fact counts under notes.
      { op: "facts", valid_from: { gte: since }, limit: FACT_SAMPLE, order: "newest", notes: false },
      sel,
    ),
  );
  const facts = factsGot.data;
  const adviceGot = useGot(key, each, () =>
    browse<AdviceUsage>({ op: "advice_usage", days: WEEK_DAYS }, sel),
  );
  const advice = adviceGot.data;
  // The admins' read of who asked what (the team read, never the branch, as People reads it).
  const activityGot = useGot(key, each && admin, () =>
    call<Activity>({ org: sel.org, team: sel.team }, "admin", {
      op: "activity",
      sinceDays: WEEK_DAYS,
      limit: 1,
    }),
  );
  const activity = activityGot.data;
  const proposalsGot = useGot(key, each && can.review, () => review.proposals(sel));
  const proposals = proposalsGot.data;
  // Only the count: one conflict and the total.
  const conflictsGot = useGot(key, each && can.review, () =>
    review.conflicts(sel, { mine: true, limit: 1 }),
  );
  const conflicts = !can.review
    ? undefined
    : one
      ? one.conflicts == null
        ? undefined
        : { total: one.conflicts }
      : conflictsGot.data;
  // The dream's (an org admin's): its last pass, what waits, and what it applied this week, from one decision's read.
  const dreamGot = useGot(key, each && admin && can.review, () =>
    review.dream(sel, { status: "applied", since, limit: 1 }),
  );
  const dream = !(admin && can.review) ? undefined : one ? (one.dream ?? undefined) : dreamGot.data;

  const count = (name: string) => counts?.[name] ?? 0;
  // Empty only once counted: a folder the server is still counting has no count, and never reads as 0.
  const empty = counts?.sources === 0 && counts?.entities === 0;

  const weekFacts = useMemo(() => {
    if (one)
      return one.week
        ? {
            count: one.week.count,
            sampled: one.week.sample < one.week.count,
            entities: one.week.entities,
            bySource: one.week.by_source,
          }
        : null;
    if (!facts) return null;
    const entities = new Set(facts.facts.map((f) => f.entity));
    const bySource = new Map<string, number>();
    for (const f of facts.facts) {
      const p = prefixOf(f.sources?.[0]?.source);
      bySource.set(p, (bySource.get(p) ?? 0) + 1);
    }
    return {
      count: facts.count,
      sampled: facts.facts.length < facts.count,
      entities: entities.size,
      bySource: [...bySource].sort((a, b) => b[1] - a[1]),
    };
  }, [one, facts]);

  const served = useMemo(() => {
    if (one) return one.advice ?? null;
    if (!advice) return null;
    const used = advice.advice.filter((a) => a.used > 0);
    return {
      times: used.reduce((n, a) => n + a.used, 0),
      notes: used.length,
      waiting: advice.advice.filter((a) => !a.mine && a.used > 0 && !a.ratified && !a.held).length,
    };
  }, [one, advice]);

  const asked = useMemo(() => {
    if (one) return admin ? (one.activity ?? null) : null;
    if (!activity) return null;
    const people = activity.people.filter((p) => p.sessions + p.runs + p.answers > 0);
    return {
      people: people.length,
      sessions: people.reduce((n, p) => n + p.sessions + p.runs + p.answers, 0),
      served: activity.serving?.sessions_served,
      of: activity.serving?.sessions,
    };
  }, [one, admin, activity]);

  const waits = one && can.review ? (one.proposals ?? null) : null;
  const mine = waits ? waits.mine : proposals ? needsMe(proposals, me).length : undefined;
  const recent = ((one ? one.sources : sources?.children) ?? [])
    .map((c) => ({ c, at: c.latest ?? c.occurred_at ?? 0 }))
    .sort((a, b) => b.at - a.at);

  const ask = can.agent ? { kind: "agent" as const } : { kind: "ask" as const, question: "" };
  const actions = (
    <div className="hm-actions" aria-label="Quick actions">
      {addToMemory}
      <Out to={connect?.sources} icon="database" label="Connect a source" />
      <a className="btn" href={href(ask)}>
        <Icon name="chat" size={14} /> Ask
      </a>
      <a className="btn hm-search" href={href({ kind: "search", query: "" })}>
        <Icon name="search" size={14} /> Search
      </a>
    </div>
  );

  const needs: (NeedRow | false | undefined)[] = [
    mine !== undefined && {
      key: "proposals",
      href: href({ kind: "review" }, { tab: "proposals", filter: "mine" }),
      n: mine,
      one: "proposal needs your review",
      many: "proposals need your review",
    },
    dream !== undefined && {
      key: "dream",
      href: href({ kind: "review" }, { tab: "needs" }),
      n: dream.counts.waiting,
      one: "dream decision waits for you",
      many: "dream decisions wait for you",
    },
    conflicts !== undefined && {
      key: "conflicts",
      href: href({ kind: "review" }, { tab: "conflicts", mine: "1" }),
      n: conflicts.total,
      one: "conflict on what you own",
      many: "conflicts on what you own",
    },
    admin &&
      !!served && {
        key: "advice",
        href: href({ kind: "advice" }),
        n: served.waiting,
        one: "team note served this week waits for ratifying",
        many: "team notes served this week wait for ratifying",
      },
  ];
  const banners = needs.filter((r): r is NeedRow => !!r && r.n > 0);

  // The last 30 days, a bar each: the facts learned (everyone who reads the memory) and, for the org's admins, the
  // sessions, runs and answers begun. From a server before the month was counted, none.
  const days = useMemo(() => {
    const facts = one?.month?.facts;
    if (!facts?.length) return null;
    const asks = new Map((admin ? (one?.activity?.by_day ?? []) : []).map(([d, n]) => [d, n]));
    return facts.map(([day, n]) => ({ day, facts: n, asks: asks.get(day) ?? 0 }));
  }, [one, admin]);
  const withAsks = admin && !!one?.activity?.by_day;

  // The memory's totals, one figure each: the four folders a reader fills first.
  const totals = [
    { name: "sources", label: "Episodes" },
    { name: "entities", label: "Entities" },
    { name: "notes", label: "Notes" },
    { name: "advice", label: "Advice" },
  ] as const;

  return (
    <div className="hm">
      <header className="hm-head">
        <div>
          <h1 className="hm-title">{scope}</h1>
          <p className="subtitle">
            What your memory holds, and its last 30 days
            {sel.branch && sel.branch !== "main" ? (
              <span className="mono">· branch {sel.branch}</span>
            ) : null}
            {updating && (
              <span
                className="hm-updating"
                role="status"
                title="Shown from your last visit while the memory is read again"
              >
                · updating
              </span>
            )}
            {why && kept && (
              <span className="hm-updating" role="status" title={why}>
                · from your last visit, couldn't refresh.{" "}
                <button type="button" className="link" onClick={retry}>
                  Try again
                </button>
              </span>
            )}
          </p>
        </div>
        {!empty && actions}
      </header>

      {why && !kept && (
        <ErrorState title="Couldn't read this memory's numbers" message={why} onRetry={retry} />
      )}

      {/* Only what waits on the reader, one full-width banner each; nothing at all when nothing does. */}
      {banners.length > 0 && (
        <ul className="hm-banners" aria-label="Needs you">
          {banners.map((r) => (
            <Need key={r.key} row={r} />
          ))}
        </ul>
      )}
      {dream && <DreamSummary list={dream} href={href} />}

      {why && !kept ? null : empty ? (
        <NewMemory addToMemory={addToMemory} connect={connect} />
      ) : (
        <>
          <div className="hm-stats">
            {totals.map(({ name, label }) =>
              // Counting: no counts yet, or (episodes and entities, counted in the background) not this one yet. A
              // folder the memory doesn't have counts 0.
              counts && !((name === "sources" || name === "entities") && counts[name] == null) ? (
                <Stat
                  key={name}
                  href={href({ kind: "folder", path: [name] })}
                  label={label}
                  n={count(name)}
                  note="in memory"
                />
              ) : (
                <StatSkeleton key={name} />
              ),
            )}
          </div>

          <div className="hm-panels">
            <section className="hm-panel" aria-labelledby="hm-month">
              <h2 className="hm-panel-h" id="hm-month">
                Last 30 days
                {days && (
                  <span className="hm-legend">
                    <span className="hm-key hm-key-facts" /> New facts
                    {withAsks && (
                      <>
                        <span className="hm-key hm-key-asks" /> Sessions
                      </>
                    )}
                  </span>
                )}
              </h2>
              {days ? (
                <DailyChart days={days} asks={withAsks} />
              ) : (
                <p className="hm-quiet-line">{done("week", factsGot) ? "Nothing to chart yet" : ""}</p>
              )}
            </section>

            <section className="hm-panel" aria-labelledby="hm-sources">
              <h2 className="hm-panel-h" id="hm-sources">
                Sources
                <a className="hm-all" href={href({ kind: "folder", path: ["sources"] })}>
                  View all <Icon name="chevron" size={12} />
                </a>
              </h2>
              {recent.length > 0 ? (
                <ul className="hm-sources" aria-label="Sources">
                  {recent.slice(0, 6).map(({ c, at }) => (
                    <SourceRow key={c.name} c={c} at={at} since={since} href={href} />
                  ))}
                </ul>
              ) : (
                <p className="hm-quiet-line">No sources yet</p>
              )}
            </section>
          </div>
        </>
      )}
    </div>
  );
}

/** Where a figure will be, while its read is out: its shape, shimmering. */
function StatSkeleton() {
  return (
    <div className="hm-stat hm-stat-sk" aria-hidden="true">
      <span className="sk sk-label" />
      <span className="sk sk-n" />
    </div>
  );
}

/** A link out of Studio, to the host's page; nothing where the host gives none. */
function Out({ to, icon, label }: { to?: string; icon: string; label: string }) {
  if (!to) return null;
  return (
    <a className="btn" href={to}>
      <Icon name={icon} size={14} /> {label}
    </a>
  );
}

interface NeedRow {
  key: string;
  href: string;
  n: number;
  one: string;
  many: string;
}

/** One thing waiting on the reader, as a full-width banner into where it waits. */
function Need({ row: { href, n, one, many } }: { row: NeedRow }) {
  return (
    <li className="hm-banner">
      <a href={href}>
        <Icon name="alert" size={14} />
        <span>
          <span className="hm-fig">{n.toLocaleString()}</span> {n === 1 ? one : many}
        </span>
        <span className="hm-banner-go">
          Review <Icon name="chevron" size={12} />
        </span>
      </a>
    </li>
  );
}

/**
 * The dream, for its builders (an org admin): when it last ran and what it did, as Review's banner says it, and what
 * it did on its own, this week and in all, a link into Review's Done automatically. What waits on them is a row of
 * Needs you. Nothing for a memory it hasn't touched.
 */
function DreamSummary({ list, href }: { list: DreamList; href: HrefOf }) {
  const st = dreamState(list);
  if (st.ran === null && !list.counts.waiting && !list.counts.applied && !list.counts.history) return null;
  // A server before it counts the week answers the filtered read's total, which is the week's.
  const shown = st.week === undefined && list.total !== undefined ? { ...st, week: list.total } : st;
  return (
    <div className="hm-dream" role="status">
      <Icon name="moon" size={14} />
      <span className="hm-dream-text">
        <DreamSaid st={st} whenClass="hm-dream-when" />
      </span>
      {shown.applied > 0 && (
        <a className="link hm-dream-done" href={href({ kind: "review" }, { tab: "done" })}>
          Done automatically: {appliedSaid(shown)}
          <Icon name="chevron" size={12} />
        </a>
      )}
    </div>
  );
}

function Stat({ href, label, n, note }: { href: string; label: string; n: number; note?: string }) {
  return (
    <a className="hm-stat" href={href}>
      <span className="hm-stat-label">{label}</span>
      <span className="hm-n">{n.toLocaleString()}</span>
      {note && <span className="hm-note">{note}</span>}
    </a>
  );
}

/**
 * A bar a day, oldest on the left: the day's new facts stacked on its sessions (`asks`, the admins'), sized against
 * the busiest day. Hovering a bar (or focusing it) shows a card with its date and each count.
 */
function DailyChart({
  days,
  asks,
}: {
  days: { day: number; facts: number; asks: number }[];
  asks: boolean;
}) {
  const [on, setOn] = useState<number | null>(null);
  const max = Math.max(1, ...days.map((d) => d.facts + (asks ? d.asks : 0)));
  const date = (us: number) =>
    new Date(us / 1000).toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      timeZone: "UTC",
    });
  const height = (d: { facts: number; asks: number }) =>
    ((d.facts + (asks ? d.asks : 0)) / max) * 100;
  const picked = on === null ? null : days[on];
  return (
    <figure className="hm-chart">
      <div
        className="hm-cols"
        role="group"
        aria-label="New facts and sessions, a bar for each of the last 30 days"
        onMouseLeave={() => setOn(null)}
      >
        {days.map((d, i) => (
          <div
            key={d.day}
            className={`hm-col ${on === i ? "on" : ""}`}
            tabIndex={0}
            aria-label={`${date(d.day)}: ${d.facts} new facts${asks ? `, ${d.asks} sessions` : ""}`}
            onMouseEnter={() => setOn(i)}
            onFocus={() => setOn(i)}
            onBlur={() => setOn(null)}
          >
            <div className="hm-stack" style={{ height: `${height(d)}%` }}>
              {d.facts > 0 && <span className="hm-seg hm-seg-facts" style={{ flexGrow: d.facts }} />}
              {asks && d.asks > 0 && <span className="hm-seg hm-seg-asks" style={{ flexGrow: d.asks }} />}
            </div>
          </div>
        ))}
        {picked && on !== null && (
          // Above a short bar, kept inside the chart at either end; beside a tall one, at the chart's top (on its
          // right, or its left in the chart's right half), so it never leaves the panel or covers the bar.
          <div
            className="hm-tip"
            role="tooltip"
            style={
              height(picked) > 45
                ? on < days.length / 2
                  ? { top: 0, left: `calc(${((on + 1) / days.length) * 100}% + 8px)` }
                  : { top: 0, right: `calc(${((days.length - on) / days.length) * 100}% + 8px)` }
                : {
                    left: `${((on + 0.5) / days.length) * 100}%`,
                    bottom: `calc(${height(picked)}% + 14px)`,
                    transform: `translateX(${on < 3 ? "-12%" : on > days.length - 4 ? "-88%" : "-50%"})`,
                  }
            }
          >
            <div className="hm-tip-date">{date(picked.day)}</div>
            {asks && (
              <div className="hm-tip-row">
                <span className="hm-key hm-key-asks" /> Sessions
                <b>{picked.asks.toLocaleString()}</b>
              </div>
            )}
            <div className="hm-tip-row">
              <span className="hm-key hm-key-facts" /> New facts
              <b>{picked.facts.toLocaleString()}</b>
            </div>
          </div>
        )}
      </div>
      <figcaption className="hm-axis">
        <span>30 days ago</span>
        <span>Today</span>
      </figcaption>
    </figure>
  );
}

function SourceRow({ c, at, since, href }: { c: Child; at: number; since: number; href: HrefOf }) {
  const fresh = at >= since;
  const to: Focus =
    c.kind === "episode" && c.episode_id
      ? { kind: "episode", id: c.episode_id }
      : { kind: "folder", path: c.path };
  return (
    <li>
      <a className="hm-source" href={href(to)} title={at ? when(at) : undefined}>
        <span className={`hm-dot ${fresh ? "hm-dot-on" : ""}`} aria-hidden="true" />
        <span className="hm-source-name mono">{c.name}</span>
        <span className="hm-when">{at ? ago(at) : ""}</span>
        <span className="hm-fig">{(c.count ?? 1).toLocaleString()}</span>
      </a>
    </li>
  );
}

/** A memory with nothing in it yet: how to put something in, and what to connect. */
function NewMemory({
  addToMemory,
  connect,
}: {
  addToMemory?: React.ReactNode;
  connect?: { sources?: string; agents?: string };
}) {
  return (
    <div className="hm-new">
      <EmptyState icon="database" title="Nothing in this memory yet">
        It fills from what you add, the sources you connect, and the agents that use it. Each turns
        into episodes, and the facts, entities and advice learned from them show up here.
      </EmptyState>
      <ol className="hm-steps">
        <li className="card card-pad">
          <h3>Add to memory</h3>
          <p className="muted">
            A document, a note or a decision: the memory keeps it as an episode and learns its
            facts.
          </p>
          {addToMemory ?? (
            <p className="muted">
              Agents add with the <span className="mono">remember_memory</span> tool, scripts with{" "}
              <span className="mono">POST /remember</span>.
            </p>
          )}
        </li>
        <li className="card card-pad">
          <h3>Connect a source</h3>
          <p className="muted">
            Slack, a CRM, meetings or a repository, read on a schedule as new episodes.
          </p>
          {connect?.sources ? (
            <a className="btn" href={connect.sources}>
              <Icon name="database" size={14} /> Connect a source
            </a>
          ) : (
            <p className="muted">Sources are connected where the memory is administered.</p>
          )}
        </li>
        <li className="card card-pad">
          <h3>Connect an agent</h3>
          <p className="muted">
            An agent with the memory's MCP tools searches it as it works, is given advice, and
            leaves what it learned behind.
          </p>
          {connect?.agents ? (
            <a className="btn" href={connect.agents}>
              <Icon name="tools" size={14} /> Connect an agent
            </a>
          ) : (
            <p className="muted">
              Point it at this memory's <span className="mono">/mcp</span> endpoint, or install the
              Claude Code plugin.
            </p>
          )}
        </li>
      </ol>
    </div>
  );
}
