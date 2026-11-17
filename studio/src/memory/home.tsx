import "./home.css";
import { useEffect, useMemo, useState } from "react";
import { browse, call } from "./api";
import { when } from "./format";
import { appliedSaid, DreamSaid, dreamState } from "./review";
import { needsMe, review, type DreamList } from "./review-api";
import type { Child, Focus, Listing, Selection } from "./types";
import { EmptyState, Icon } from "./ui";
import type { Resolved } from "../transport";

// Studio's Home (`#/w/`): the memory being read (the org's, or the team picked) as its builders see it. What
// changed this week, what was answered from it, and what waits on the reader, each number a link into the
// section that holds it. Every number comes from a read another section already makes; one with no cheap read is
// left out, and one the reader may not make (an admin's, a review's) is too.

const WEEK_DAYS = 7;
const WEEK_US = WEEK_DAYS * 86_400_000_000;
/** The newest facts read to say where this week's came from (`count` is the whole week's, whatever this is). */
const FACT_SAMPLE = 200;

/** A link into a section: the focus, and its options (`#/r/?tab=conflicts`). */
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
}: HomeProps) {
  const key = JSON.stringify([sel.org, sel.team, sel.branch]);
  // The week, fixed for the page: a re-render doesn't move it.
  const since = useMemo(() => Date.now() * 1000 - WEEK_US, [key]);

  // What the memory holds: the Explorer's root, with each folder's count.
  const root = useRead(key, true, () => browse<Listing>({ op: "ls", path: [] }, sel));
  // Each source and when it last brought something.
  const sources = useRead(key, true, () =>
    browse<Listing>({ op: "ls", path: ["sources"], limit: 200 }, sel),
  );
  // The facts that took effect this week: how many, on how many entities, and from where.
  const facts = useRead(key, true, () =>
    browse<FactsPage>(
      { op: "facts", valid_from: { gte: since }, limit: FACT_SAMPLE, order: "newest" },
      sel,
    ),
  );
  const lessons = useRead(key, can.lessons, () =>
    call<{ count: number }>(sel, "lessons", { since, count: true }),
  );
  const adviceGot = useGot(key, true, () =>
    browse<AdviceUsage>({ op: "advice_usage", days: WEEK_DAYS }, sel),
  );
  const advice = adviceGot.data;
  // The admins' read of who asked what (the team read, never the branch, as People reads it).
  const activity = useRead(key, admin, () =>
    call<Activity>({ org: sel.org, team: sel.team }, "admin", {
      op: "activity",
      sinceDays: WEEK_DAYS,
      limit: 1,
    }),
  );
  const proposalsGot = useGot(key, can.review, () => review.proposals(sel));
  const proposals = proposalsGot.data;
  // Only the count: one conflict and the total.
  const conflictsGot = useGot(key, can.review, () =>
    review.conflicts(sel, { mine: true, limit: 1 }),
  );
  const conflicts = conflictsGot.data;
  // The dream's (an org admin's): its last pass, what waits, and what it applied this week, from one decision's read.
  const dreamGot = useGot(key, admin && can.review, () =>
    review.dream(sel, { status: "applied", since, limit: 1 }),
  );
  const dream = dreamGot.data;

  const count = (name: string) => root?.children.find((c) => c.name === name)?.count ?? 0;
  const empty = root !== undefined && count("sources") === 0 && count("entities") === 0;

  const weekFacts = useMemo(() => {
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
  }, [facts]);

  const served = useMemo(() => {
    if (!advice) return null;
    const used = advice.advice.filter((a) => a.used > 0);
    return {
      times: used.reduce((n, a) => n + a.used, 0),
      notes: used.length,
      waiting: advice.advice.filter((a) => a.used > 0 && !a.ratified && !a.held).length,
    };
  }, [advice]);

  const asked = useMemo(() => {
    if (!activity) return null;
    const people = activity.people.filter((p) => p.sessions + p.runs + p.answers > 0);
    return {
      people: people.length,
      sessions: people.reduce((n, p) => n + p.sessions + p.runs + p.answers, 0),
      served: activity.serving?.sessions_served,
      of: activity.serving?.sessions,
    };
  }, [activity]);

  const mine = proposals ? needsMe(proposals, me).length : undefined;
  const pending = proposals?.filter((p) => p.status === "pending_review").length;
  const recent = (sources?.children ?? [])
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
      <a className="btn" href={href({ kind: "search", query: "" })}>
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
            settled={proposalsGot.done && conflictsGot.done && dreamGot.done && adviceGot.done}
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
                <Tile
                  href={href({ kind: "serving" }, { days: String(WEEK_DAYS) })}
                  n={asked.served}
                  label="sessions given advice"
                  note={`of ${asked.of.toLocaleString()}`}
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
              {root && count("sessions") > 0 && (
                <Tile
                  href={href({ kind: "folder", path: ["sessions"] })}
                  n={count("sessions")}
                  label="of your sessions kept"
                  note="all time"
                />
              )}
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

          {(root || sources) && (
            <section className="hm-sec" aria-labelledby="hm-held">
              <h2 className="h-section" id="hm-held">
                <Icon name="database" size={14} /> In memory
              </h2>
              {/* The counts read every fact the reader may: the sources below don't wait for them. */}
              {root && (
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
                    name === "sources" || name === "entities" || count(name) > 0 ? (
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
          )}
        </>
      )}
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
  /** Not the reader's to act on: shown, never as a call to act. */
  quiet?: boolean;
}

/**
 * What waits on the reader: a row per kind of thing with any waiting, none for what has nothing. With nothing, one
 * calm line, once every read has answered (before then, nothing: a row may still come).
 */
function NeedsYou({ needs, settled }: { needs: (NeedRow | false | null | undefined)[]; settled: boolean }) {
  const rows = needs.filter((r): r is NeedRow => !!r && r.n > 0);
  const calls = rows.filter((r) => !r.quiet);
  if (!calls.length && !settled) return null;
  return (
    <section className="hm-sec" aria-labelledby="hm-needs">
      <h2 className="h-section" id="hm-needs">
        <Icon name="alert" size={14} /> Needs you
      </h2>
      {!calls.length && (
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

function Need({ row: { href, n, one, many, quiet } }: { row: NeedRow }) {
  return (
    <li className={`hm-need ${quiet ? "" : "hm-need-on"}`}>
      <a href={href}>
        <span className="hm-fig">{n.toLocaleString()}</span> {n === 1 ? one : many}
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
