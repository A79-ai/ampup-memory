import "./home.css";
import { useEffect, useMemo, useState } from "react";
import { browse, call } from "./api";
import { when } from "./format";
import { appliedSaid, DreamSaid, dreamState } from "./review";
import { needsMe, review, type DreamList } from "./review-api";
import type { Branch, Child, Focus, Listing, Selection } from "./types";
import { EmptyState, Icon } from "./ui";
import { transport, type Resolved } from "../transport";

// Studio's Home (`#/home`): the memory being read (the org's, or the team picked) as its builders see it. What
// changed this week, what was answered from it, and what waits on the reader, each number a link into the
// section that holds it. Every number is one another section's read would give; one with no cheap read is left out,
// and one the reader may not make (an admin's, a review's) is too. They come in one read (`home`), counts and short
// lists only; from a server before it, a read each.

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
  activity?: { people: number; sessions: number; served?: number; of?: number } | null;
  proposals?: { mine: number; pending: number } | null;
  conflicts?: number | null;
  dream?: DreamList | null;
  branches?: Branch[] | null;
  schema?: { types: number };
}

/** How many times Home asks again for the parts still worked out (an ask that failed included), before it leaves
 * them out. */
const ASK_AGAIN = 5;

/** The fields a pending part stands for: `week` brings the sources and the advice served too. */
const fieldsOf = (part: string) => (part === "week" ? ["week", "sources", "advice"] : [part]);

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
  advice: { used: number; people: number; ratified: boolean; held?: boolean }[];
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
  // server with no such read (or the read failed), and Home makes a read each, as before it.
  const [said, setSaid] = useState<{ key: string; h: HomeSummary | null } | null>(null);
  // The answer the host kept from the last open, shown at once while the read is out (and, part by part, while the
  // server works out the rest), with a quiet "updating" beside the title. The fresh answer replaces it.
  const kept = useMemo(() => keptHome(sel), [key]);
  useEffect(() => {
    let live = true;
    void (async () => {
      // The proposals wait for their branches' states to be built, which a first ask doesn't wait for: asked for
      // beside it, so that the wait starts with the page rather than once the first ask has answered.
      const beside = browse<HomeSummary>({ op: "home", parts: ["proposals"] }, sel).catch(() => null);
      let h: HomeSummary;
      try {
        h = await browse<HomeSummary>({ op: "home" }, sel);
      } catch {
        if (live) setSaid({ key, h: null });
        return;
      }
      if (live && h.pending?.includes("proposals")) {
        if (h.pending.length > 1) setSaid({ key, h });
        const p = await beside;
        if (p && !p.pending?.includes("proposals")) {
          h = { ...h, proposals: p.proposals, pending: h.pending.filter((x) => x !== "proposals") };
        }
      }
      for (let again = 0; live; again++) {
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
        } catch {
          // Asked again on the next turn: what it waited for goes on being worked out, and is kept for it.
        }
      }
    })();
    return () => {
      live = false;
    };
  }, [key]);
  // undefined until it answers; null: a read each.
  const fresh = said?.key === key ? said.h : undefined;
  // Until the fresh answer is whole, the kept one fills in for what it hasn't answered yet. A fresh read that
  // failed (null) drops it: Home then reads each part, as with a server before the one read. Made once per answer:
  // a new object each render would tell the page around Home (`onSummary`) again on every render, which renders
  // Home again.
  const one = useMemo(
    () => (fresh === undefined ? kept : fresh && kept ? overKept(fresh, kept) : fresh),
    [fresh, kept],
  );
  const updating = !!kept && (fresh === undefined || !!fresh?.pending?.length);
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
      { op: "facts", valid_from: { gte: since }, limit: FACT_SAMPLE, order: "newest" },
      sel,
    ),
  );
  const facts = factsGot.data;
  const lessonsGot = useGot(key, each && can.lessons, () =>
    call<{ count: number }>(sel, "lessons", { since, count: true }),
  );
  const lessons = !can.lessons
    ? undefined
    : one
      ? one.lessons == null
        ? undefined
        : { count: one.lessons }
      : lessonsGot.data;
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
      waiting: advice.advice.filter((a) => a.used > 0 && !a.ratified && !a.held).length,
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
  const pending = waits ? waits.pending : proposals?.filter((p) => p.status === "pending_review").length;
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

  return (
    <div className="hm">
      <header className="hm-head">
        <div>
          <div className="eyebrow">Home</div>
          <h1 className="title">{scope}</h1>
          <p className="subtitle">
            What changed this week, what it answered, and what needs you
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
          </p>
        </div>
        {!empty && actions}
      </header>

      {dream && <DreamSummary list={dream} href={href} />}

      {empty ? (
        <NewMemory addToMemory={addToMemory} connect={connect} />
      ) : (
        <>
          <NeedsYou
            // Settled once every read behind a row has answered (or failed, or isn't the reader's to make).
            settled={
              done("proposals", proposalsGot) &&
              done("conflicts", conflictsGot) &&
              done("dream", dreamGot) &&
              done("week", adviceGot)
            }
            needs={[
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
                served && {
                  key: "advice",
                  href: href({ kind: "advice" }),
                  n: served.waiting,
                  one: "team note served this week waits for ratifying",
                  many: "team notes served this week wait for ratifying",
                  where: "on Advice, not counted in Review",
                },
              !admin &&
                pending !== undefined &&
                mine === 0 && {
                  key: "others",
                  href: href({ kind: "review" }, { tab: "proposals", filter: "others" }),
                  n: pending,
                  one: "proposal waits on other owners",
                  many: "proposals wait on other owners",
                  quiet: true,
                },
            ]}
          />

          <section className="hm-sec" aria-labelledby="hm-learned">
            <h2 className="h-section" id="hm-learned">
              <Icon name="history" size={14} /> Learned this week
            </h2>
            <div className="hm-tiles">
              {weekFacts && (
                <Tile
                  href={href({ kind: "folder", path: ["entities"] })}
                  n={weekFacts.count}
                  label="new facts"
                  title="Facts whose time starts in the last 7 days"
                />
              )}
              {weekFacts && (
                <Tile
                  href={href({ kind: "folder", path: ["entities"] })}
                  n={weekFacts.entities}
                  plus={weekFacts.sampled}
                  label={
                    weekFacts.entities === 1 ? "entity they are about" : "entities they are about"
                  }
                />
              )}
              {lessons && (
                <Tile
                  href={href({ kind: "learned" })}
                  n={lessons.count}
                  label="lessons from runs"
                />
              )}
              {/* Each tile's place holds while its read is out, so the page doesn't jump as they land. */}
              {!done("week", factsGot) && <TileSkeleton count={2} />}
              {!done("lessons", lessonsGot) && <TileSkeleton />}
            </div>
            {weekFacts && weekFacts.bySource.length > 0 && (
              <p className="hm-by">
                {weekFacts.sampled ? `Of the newest ${FACT_SAMPLE}: ` : "From "}
                {weekFacts.bySource.map(([p, n], i) => (
                  <span key={p}>
                    {i > 0 && ", "}
                    <a className="link" href={href({ kind: "folder", path: ["sources", p] })}>
                      {p}
                    </a>{" "}
                    <span className="hm-fig">{n.toLocaleString()}</span>
                  </span>
                ))}
              </p>
            )}
          </section>

          <section className="hm-sec" aria-labelledby="hm-answered">
            <h2 className="h-section" id="hm-answered">
              <Icon name="chat" size={14} /> Answered this week
            </h2>
            <div className="hm-tiles">
              {asked && (
                <Tile
                  href={href({ kind: "people" }, { days: String(WEEK_DAYS) })}
                  n={asked.sessions}
                  label="sessions and runs"
                  note={`by ${asked.people} ${asked.people === 1 ? "person" : "people"}`}
                />
              )}
              {asked?.served !== undefined && asked.of !== undefined && (
                // Advice's details count the same (Serving's "Sessions served a note"), at 7 days.
                <Tile
                  href={href({ kind: "serving" }, { days: String(WEEK_DAYS) })}
                  n={asked.served}
                  label={asked.served === 1 ? "session served a note" : "sessions served a note"}
                  note={`of ${asked.of.toLocaleString()} sessions`}
                />
              )}
              {served && (
                <Tile
                  href={href({ kind: "advice" })}
                  n={served.times}
                  label="times advice was served"
                  note={`${served.notes} ${served.notes === 1 ? "note" : "notes"}`}
                />
              )}
              {counts && count("sessions") > 0 && (
                // Not the activity count above: the reader's own records in `sessions/`, kept since the memory began.
                <Tile
                  href={href({ kind: "folder", path: ["sessions"] })}
                  n={count("sessions")}
                  label="of your session records kept"
                  note="yours, all time"
                  title="Your own records in the sessions/ folder, since the memory began: an ended session's record and a coding session's hook notes each count, so one coding session can count twice"
                />
              )}
              {!done("activity", activityGot) && <TileSkeleton count={2} />}
              {!done("week", adviceGot) && <TileSkeleton />}
            </div>
            {admin && (
              <p className="hm-by">
                What went unanswered:{" "}
                <a className="link" href={href({ kind: "gaps" }, { days: String(WEEK_DAYS) })}>
                  Not answered
                </a>
              </p>
            )}
          </section>

          {
            <section className="hm-sec" aria-labelledby="hm-held">
              <h2 className="h-section" id="hm-held">
                <Icon name="database" size={14} /> In memory
              </h2>
              {/* The counts read every fact the reader may: the sources below don't wait for them. */}
              {!done("counts", rootGot) && (
                <div className="hm-tiles hm-tiles-sm">
                  <TileSkeleton count={4} />
                </div>
              )}
              {counts && (
                <div className="hm-tiles hm-tiles-sm">
                  {(
                    [
                      ["sources", "episodes"],
                      ["entities", "entities"],
                      ["notes", "notes"],
                      ["advice", "advice"],
                      ["skills", "skills"],
                    ] as const
                  ).map(([name, label]) =>
                    (name === "sources" || name === "entities") && counts[name] == null ? (
                      <TileSkeleton key={name} />
                    ) : name === "sources" || name === "entities" || count(name) > 0 ? (
                      <Tile
                        key={name}
                        href={href({ kind: "folder", path: [name] })}
                        n={count(name)}
                        label={label}
                      />
                    ) : null,
                  )}
                </div>
              )}
              {recent.length > 0 && <p className="hm-by">Sources, by their newest episode:</p>}
              {recent.length > 0 && (
                <ul className="hm-sources" aria-label="Sources">
                  {recent.slice(0, 8).map(({ c, at }) => (
                    <SourceRow key={c.name} c={c} at={at} since={since} href={href} />
                  ))}
                </ul>
              )}
            </section>
          }
        </>
      )}
    </div>
  );
}

/** Where a tile will be, while its read is out: its shape, shimmering. */
function TileSkeleton({ count = 1 }: { count?: number }) {
  return (
    <>
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="hm-tile hm-tile-sk" aria-hidden="true">
          <span className="sk sk-n" />
          <span className="sk sk-label" />
        </div>
      ))}
    </>
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
  /** Not the reader's to act on: shown, never as a call to act. */
  quiet?: boolean;
  /** Where it waits, when that isn't Review (whose "Needs you" counts the rest). */
  where?: string;
}

/**
 * What waits on the reader: a row per kind of thing with any waiting, none for what has nothing. With nothing, one
 * calm line, once every read has answered (before then, nothing: a row may still come).
 */
function NeedsYou({ needs, settled }: { needs: (NeedRow | false | null | undefined)[]; settled: boolean }) {
  const rows = needs.filter((r): r is NeedRow => !!r && r.n > 0);
  const calls = rows.filter((r) => !r.quiet);
  return (
    <section className="hm-sec" aria-labelledby="hm-needs" aria-busy={!settled}>
      <h2 className="h-section" id="hm-needs">
        <Icon name="alert" size={14} /> Needs you
      </h2>
      {!calls.length && !settled && (
        // Before every read has answered, a row may still come: the rows' places, shimmering.
        <div className="hm-needs" aria-hidden="true">
          {[0, 1, 2].map((i) => (
            <div key={i} className="sk hm-need-sk" />
          ))}
        </div>
      )}
      {!calls.length && settled && (
        <p className="hm-calm">
          <Icon name="check" size={14} /> Nothing needs you
        </p>
      )}
      {rows.length > 0 && (
        <ul className="hm-needs">
          {rows.map((r) => (
            <Need key={r.key} row={r} />
          ))}
        </ul>
      )}
    </section>
  );
}

function Need({ row: { href, n, one, many, quiet, where } }: { row: NeedRow }) {
  return (
    <li className={`hm-need ${quiet ? "" : "hm-need-on"}`}>
      <a href={href}>
        <span className="hm-fig">{n.toLocaleString()}</span> {n === 1 ? one : many}
        {where && <span className="muted"> · {where}</span>}
        <Icon name="chevron" size={12} />
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

function Tile({
  href,
  n,
  label,
  note,
  plus,
  title,
}: {
  href: string;
  n: number;
  label: string;
  note?: string;
  plus?: boolean;
  title?: string;
}) {
  return (
    <a className="hm-tile" href={href} title={title}>
      <span className="hm-n">
        {n.toLocaleString()}
        {plus ? "+" : ""}
      </span>
      <span className="hm-label">{label}</span>
      {note && <span className="hm-note">{note}</span>}
    </a>
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
      <a className="hm-source" href={href(to)}>
        <Icon name="folder" size={14} />
        <span className="hm-source-name mono">{c.name}</span>
        <span className="hm-fig">{(c.count ?? 1).toLocaleString()}</span>
        <span className={fresh ? "hm-fresh" : "hm-quiet"}>{at ? when(at) : ""}</span>
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
