import { useEffect, useState } from "react";
import { browse, call } from "./api";
import { ago, day, humanize, when } from "./format";
import { setOptions, useOption } from "./hashOptions";
import { EntityLink } from "./layouts";
import { transport } from "../transport";
import type { Fact, Focus, Selection } from "./types";
import { Icon } from "./ui";

/**
 * What the Explorer's entities are narrowed to, from the link: `from` (a source prefix: what was learned from it) and
 * `by` (a person, for the org's admins: what was learned from what they wrote or said). `learnedLink` in
 * hashOptions.ts spells the link.
 */
export type Narrow = { from: string; by: string };

export function useNarrow(): Narrow {
  const [from] = useOption("from");
  const [by] = useOption("by");
  return { from, by };
}

/** The narrowing as `/browse`'s `ls` takes it. */
export const narrowReq = (n: Narrow): Record<string, string> => ({
  ...(n.from ? { from: n.from } : {}),
  ...(n.by ? { by: n.by } : {}),
});

/** Opens `f` keeping the narrowing: a folder of entities stays narrowed as it is walked. */
export function keepNarrow(n: Narrow, onFocus: (f: Focus) => void, f: Focus) {
  onFocus(f);
  if (f.kind === "folder" && (n.from || n.by)) setOptions(narrowReq(n));
}

/** One source, as `/browse {op: "sources"}` answers it: what it taught the memory the caller may read. */
export type LearnedSource = {
  source: string;
  type: string | null;
  episodes: number;
  last_written: number | null;
  last_occurred: number | null;
  facts: number;
  current: number;
  last_fact: number | null;
  entities: number;
  top_entities: { entity: string; facts: number }[];
};

/** The org's people for the admin's "from person": its members and whoever was active this year. */
function usePeople(sel: Selection, admin: boolean): string[] {
  const [people, setPeople] = useState<string[]>([]);
  useEffect(() => {
    if (!admin) return;
    let live = true;
    const members = transport().members;
    Promise.all([
      call<{ people: { user: string }[] }>({ org: sel.org, team: sel.team }, "admin", {
        op: "activity",
        limit: 1,
        sinceDays: 365,
      }).catch(() => ({ people: [] })),
      members ? members(sel).catch((): string[] => []) : Promise.resolve<string[]>([]),
    ]).then(
      ([a, m]) => live && setPeople([...new Set([...a.people.map((p) => p.user), ...m])].sort()),
      () => live && setPeople([]),
    );
    return () => {
      live = false;
    };
  }, [sel.org, sel.team, admin]);
  return people;
}

/** Every source the caller may read, those that taught the most first (no top entities: a picker's list). */
function useSources(sel: Selection): LearnedSource[] | null {
  const [list, setList] = useState<LearnedSource[] | null>(null);
  useEffect(() => {
    let live = true;
    browse<{ sources: LearnedSource[] }>({ op: "sources", top: 0 }, sel).then(
      (r) => live && setList(r.sources),
      // A memstore without the op: no picker, a link's narrowing still applies.
      () => live && setList([]),
    );
    return () => {
      live = false;
    };
  }, [sel.org, sel.team, sel.branch]);
  return list;
}

/** A source's name to read: its connector and what it read (`slack:C0123` is "Slack · C0123"). */
export function sourceName(prefix: string): string {
  const i = prefix.indexOf(":");
  if (i < 0) return prefix;
  return `${humanize(prefix.slice(0, i))} · ${prefix.slice(i + 1)}`;
}

/**
 * The bar above the Explorer's entities: what they are narrowed to (a source, and for the org's admins a person),
 * a picker for each, and once narrowed, what was learned: the counts, and the newest facts with their entities.
 */
export function LearnedBar({
  sel,
  admin,
  narrow,
  type,
  onFocus,
}: {
  sel: Selection;
  admin: boolean;
  narrow: Narrow;
  /** The entity type folder it sits in (`entities/<type>`), if any: its newest facts are that type's. */
  type?: string;
  onFocus: (f: Focus) => void;
}) {
  const sources = useSources(sel);
  const people = usePeople(sel, admin);
  const on = Boolean(narrow.from || narrow.by);
  const known = sources?.some((s) => s.source === narrow.from);
  return (
    <div className="learned">
      <div className="facets" role="group" aria-label="Narrow to what was learned">
        {(Boolean(sources?.length) || Boolean(narrow.from)) && (
          <label className="facet">
            <span>Learned from</span>
            <select
              className="field"
              value={narrow.from}
              onChange={(e) => setOptions({ from: e.target.value || null })}
            >
              <option value="">any source</option>
              {narrow.from && !known && <option value={narrow.from}>{narrow.from}</option>}
              {(sources ?? []).map((s) => (
                <option key={s.source} value={s.source}>
                  {s.source} ({s.facts.toLocaleString()} {s.facts === 1 ? "fact" : "facts"})
                </option>
              ))}
            </select>
          </label>
        )}
        {admin && (people.length > 0 || narrow.by) && (
          <label className="facet">
            <span>From person</span>
            <select
              className="field"
              value={narrow.by}
              onChange={(e) => setOptions({ by: e.target.value || null })}
            >
              <option value="">anyone</option>
              {narrow.by && !people.includes(narrow.by) && (
                <option value={narrow.by}>{narrow.by}</option>
              )}
              {people.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          </label>
        )}
        {on && (
          <button className="link" onClick={() => setOptions({ from: null, by: null })}>
            Clear
          </button>
        )}
      </div>
      {on && <Learned sel={sel} narrow={narrow} type={type} onFocus={onFocus} />}
    </div>
  );
}

/** Once narrowed: what that source (or person) taught, and its newest facts. */
function Learned({
  sel,
  narrow,
  type,
  onFocus,
}: {
  sel: Selection;
  narrow: Narrow;
  type?: string;
  onFocus: (f: Focus) => void;
}) {
  const [tally, setTally] = useState<LearnedSource | null>(null);
  const [facts, setFacts] = useState<Fact[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const key = JSON.stringify([sel.org, sel.team, sel.branch, narrow.from, narrow.by, type]);
  useEffect(() => {
    let live = true;
    setTally(null);
    setFacts(null);
    setError(null);
    if (narrow.from)
      browse<{ sources: LearnedSource[] }>(
        { op: "sources", prefixes: [narrow.from], top: 0 },
        sel,
      ).then(
        (r) => live && setTally(r.sources[0] ?? null),
        () => {},
      );
    newest(sel, narrow, type).then(
      (f) => live && setFacts(f),
      (e: Error) => live && setError(e.message),
    );
    return () => {
      live = false;
    };
  }, [key]);
  return (
    <div className="learned-summary">
      <div className="banner banner-note">
        <Icon name="history" size={14} />
        <span>
          Showing only what was learned
          {narrow.from && (
            <>
              {" "}
              from <span className="mono">{narrow.from}</span>
            </>
          )}
          {narrow.by && (
            <>
              {" "}
              from what <span className="mono">{narrow.by}</span> wrote or said
            </>
          )}
          .
          {tally &&
            ` In all, ${tally.entities.toLocaleString()} ${tally.entities === 1 ? "entity" : "entities"} and ${tally.facts.toLocaleString()} ${tally.facts === 1 ? "fact" : "facts"} from ${tally.episodes.toLocaleString()} ${tally.episodes === 1 ? "episode" : "episodes"}`}
          {tally?.last_written ? `, the last written ${ago(tally.last_written)}` : ""}
          {tally ? "." : ""}
        </span>
      </div>
      {error ? (
        <p className="fine">Couldn't read its newest facts: {error}</p>
      ) : facts && facts.length > 0 ? (
        <>
          <div className="h-section">Newest facts</div>
          <div className="card facts">
            {facts.map((f) => (
              <div key={f.fact_id} className={`fact ${f.valid_to != null ? "fact-past" : ""}`}>
                <div className="fact-key" title={f.predicate}>
                  {humanize(f.predicate)}
                </div>
                <div>
                  <div className="fact-entity">
                    <EntityLink entity={f.entity} onFocus={onFocus} />
                  </div>
                  <div className="fact-value">{f.value}</div>
                </div>
                <div className="fact-side">
                  <div className="fact-when" title={when(f.valid_from)}>
                    {f.valid_to != null ? "ended" : `since ${day(f.valid_from)}`}
                  </div>
                </div>
              </div>
            ))}
          </div>
          <div className="h-section">{type ? `Every ${humanize(type)}` : "By entity type"}</div>
        </>
      ) : null}
    </div>
  );
}

/** The newest facts learned from the narrowing (at most 12): a source's through `/browse {op: "facts"}`, a person's
 * through the admins' `user_facts`, both kept only where they agree. */
async function newest(sel: Selection, n: Narrow, type?: string): Promise<Fact[]> {
  const N = 12;
  const fromSource = n.from
    ? browse<{ facts: Fact[] }>(
        {
          op: "facts",
          source: [n.from],
          limit: n.by ? 500 : N,
          ...(type ? { entity_type: type } : {}),
        },
        sel,
      ).then((r) => r.facts)
    : null;
  const fromPerson = n.by
    ? call<{ facts: Fact[] }>({ org: sel.org, team: sel.team }, "admin", {
        op: "user_facts",
        user: n.by,
        limit: 500,
      }).then((r) => r.facts)
    : null;
  const [a, b] = await Promise.all([fromSource, fromPerson]);
  let facts = a ?? b ?? [];
  if (a && b) {
    const ids = new Set(b.map((f) => f.fact_id));
    facts = a.filter((f) => ids.has(f.fact_id));
  }
  if (type) facts = facts.filter((f) => f.entity.split(":")[0] === type);
  return [...facts].sort((x, y) => y.valid_from - x.valid_from).slice(0, N);
}
