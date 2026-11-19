import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { optionsOf, setOptions, subscribe } from "./hashOptions";
import { transport } from "../transport";
import { ApiError, browse, call, keptBrowse, keptSearch, search } from "./api";
import { ago, day, entityName, humanize, named, short, sourceKind, sourceTitle, when } from "./format";
import { EntityLink, Layout } from "./layouts";
import { Markdown } from "./markdown";
import type { OrgType } from "./schema";
import { shapeOf } from "./shape";
import { mcp } from "./tools";
import { label } from "./tree";
import type {
  Aip,
  Child,
  EntityView,
  EpisodeView,
  Fact,
  Focus,
  Listing,
  Selection,
  SkillView,
  AdviceView,
} from "./types";
import { CopyId, EmptyState, ErrorState, Icon, Skeleton } from "./ui";
import { EntityOwnership } from "./review";
import { keepNarrow, LearnedBar, narrowReq, useNarrow } from "./learned";
import { AddToMemoryButton } from "./addToMemory";
import { review, type Conflict } from "./review-api";
import { Connections, Disputes, DreamDecisions, EntityFacts, type EntityPage } from "./entity";

type State<T> =
  | { status: "loading" }
  | { status: "error"; message: string; code?: number }
  | { status: "ok"; data: T };

/** Loads once per mount (each pane is keyed by what it shows), and again on `retry`, or on `reload` keeping what
 * shows until the new answer comes. What the same read last answered on this page (`kept`) shows meanwhile, in
 * place of a skeleton. */
function useLoad<T>(
  load: () => Promise<T>,
  deps: unknown[],
  kept?: () => T | undefined,
): [State<T>, () => void, () => void] {
  const [s, setS] = useState<State<T>>(() => {
    const data = kept?.();
    return data === undefined ? { status: "loading" } : { status: "ok", data };
  });
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    load().then(
      (data) => live && setS({ status: "ok", data }),
      (e) =>
        live &&
        setS({
          status: "error",
          code: e instanceof ApiError ? e.status : undefined,
          message:
            e instanceof ApiError && e.status === 404
              ? "It isn't here, or you can't read it."
              : e.message,
        }),
    );
    return () => {
      live = false;
    };
  }, [...deps, n]);
  const retry = () => {
    setS({ status: "loading" });
    setN((x) => x + 1);
  };
  const reload = () => setN((x) => x + 1);
  return [s, retry, reload];
}

function Loading<T>({
  s,
  retry,
  rows,
  children,
}: {
  s: State<T>;
  retry: () => void;
  rows?: number;
  children: (d: T) => React.ReactNode;
}) {
  if (s.status === "loading") return <Skeleton rows={rows} />;
  if (s.status === "error")
    return s.code === 404 ? (
      <EmptyState icon="lock" title="Nothing to show">
        {s.message}
      </EmptyState>
    ) : (
      <ErrorState title="Couldn't load this" message={s.message} onRetry={retry} />
    );
  return <>{children(s.data)}</>;
}

/** The right-hand pane: whatever is in focus, read in full. */
export function Inspector({
  focus,
  sel,
  onFocus,
  schema,
  admin = false,
  me,
}: {
  focus: Focus | null;
  sel: Selection;
  onFocus: (f: Focus) => void;
  /** The org's schema (base and active goals' slices), for each entity's typed fields; absent until read. */
  schema?: Map<string, OrgType>;
  /** The org's admin: search narrows to what one person wrote or said, too; an entity's owner is theirs to assign. */
  admin?: boolean;
  /** The signed-in person as memstore names them, for an entity's owner panel. */
  me?: string;
}) {
  if (!focus || (focus.kind === "folder" && focus.path.length === 0))
    return <Overview key={JSON.stringify(sel)} sel={sel} onFocus={onFocus} />;
  switch (focus.kind) {
    case "folder":
      return (
        <FolderView
          key={JSON.stringify(focus.path)}
          path={focus.path}
          sel={sel}
          onFocus={onFocus}
          admin={admin}
        />
      );
    case "entity":
      return (
        <EntityPane
          key={JSON.stringify(focus.path)}
          path={focus.path}
          sel={sel}
          onFocus={onFocus}
          schema={schema}
          admin={admin}
          me={me}
        />
      );
    case "skill":
      // A skill is `skills/<goal>/<slug>`: a shorter path (`#/skill/skills`, `#/skill/skills/<goal>`) is a folder, opened as
      // one, not read as a skill.
      if (focus.path[0] === "skills" && focus.path.length < 3)
        return (
          <FolderView
            key={JSON.stringify(focus.path)}
            path={focus.path}
            sel={sel}
            onFocus={onFocus}
            admin={admin}
          />
        );
      // A piece of advice opens as a skill does (`#/skill/advice/…`): a note to read.
      return focus.path[0] === "advice" ? (
        <AdvicePane
          key={JSON.stringify(focus.path)}
          path={focus.path}
          sel={sel}
          onFocus={onFocus}
        />
      ) : (
        <SkillPane key={JSON.stringify(focus.path)} path={focus.path} sel={sel} onFocus={onFocus} />
      );
    case "episode":
      return <EpisodePane key={focus.id} id={focus.id} sel={sel} onFocus={onFocus} />;
    case "search":
      return (
        <SearchPane
          key={focus.query}
          query={focus.query}
          sel={sel}
          onFocus={onFocus}
          types={schema ? [...schema.keys()].sort() : []}
          admin={admin}
        />
      );
    case "tools":
    case "schema":
    case "goals":
      return null;
  }
}

const ROOTS: Record<
  string,
  { icon: string; kind: string; title: string; note: string; unit: string }
> = {
  sources: {
    icon: "episode",
    kind: "k-doc",
    title: "Sources",
    note: "Everything remembered, by where it came from",
    unit: "episodes",
  },
  entities: {
    icon: "entity",
    kind: "k-entity",
    title: "Entities",
    note: "Facts, by what they are about, kept over time",
    unit: "entities",
  },
  skills: {
    icon: "skill",
    kind: "k-skill",
    title: "Skills",
    note: "Procedures compiled for each goal",
    unit: "skills",
  },
  notes: {
    icon: "episode",
    kind: "k-doc",
    title: "Notes",
    note: "What you corrected or settled, by area and topic; what you asked to remember; what the team shares",
    unit: "notes",
  },
  advice: {
    icon: "skill",
    kind: "k-skill",
    title: "Advice",
    note: "Learned from sessions: the team's and yours, by area and situation",
    unit: "pieces",
  },
  sessions: {
    icon: "episode",
    kind: "k-doc",
    title: "Sessions",
    note: "Your sessions: ended records by day, and what each tool's hooks kept by repo",
    unit: "sessions",
  },
};

/** The memory at a glance: its top-level trees, and where to start. */
function Overview({ sel, onFocus }: { sel: Selection; onFocus: (f: Focus) => void }) {
  const [s, retry] = useLoad(
    () => browse<Listing>({ op: "ls", path: [] }, sel),
    [sel],
    () => keptBrowse<Listing>({ op: "ls", path: [] }, sel),
  );
  return (
    <section>
      <div className="head">
        <span className="kind k-doc">
          <Icon name="database" size={22} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Memory</div>
          <h1 className="title">{sel.team ? `${sel.org} / ${sel.team}` : sel.org}</h1>
          <div className="subtitle">
            What this memory holds, with the evidence behind every fact.
          </div>
        </div>
      </div>
      <Loading s={s} retry={retry} rows={2}>
        {(l) =>
          l.children.length === 0 ? (
            <EmptyState icon="database" title="Nothing remembered yet">
              Once sources are connected and synced, their episodes appear under Sources and the
              facts drawn from them under Entities.
            </EmptyState>
          ) : (
            <div className="grid">
              {l.children.map((c) => {
                const r = ROOTS[c.name] ?? {
                  icon: "folder",
                  kind: "k-folder",
                  title: humanize(c.name),
                  note: "",
                  unit: "items",
                };
                return (
                  <button
                    key={c.name}
                    className="tile"
                    onClick={() => onFocus({ kind: "folder", path: c.path })}
                  >
                    <div className="tile-top">
                      <span className={`kind ${r.kind}`}>
                        <Icon name={r.icon} />
                      </span>
                      <Icon name="arrow" className="muted" />
                    </div>
                    <div className="tile-name">{r.title}</div>
                    {c.count != null && (
                      <div className="tile-stat">
                        {c.count.toLocaleString()}{" "}
                        <span className="tile-note">
                          {c.count === 1 ? r.unit.replace(/s$/, "") : r.unit}
                        </span>
                      </div>
                    )}
                    <div className="tile-note">{r.note}</div>
                  </button>
                );
              })}
            </div>
          )
        }
      </Loading>
      <div className="h-section" style={{ marginTop: "var(--s8)" }}>
        Start here
      </div>
      <div className="list">
        <StartRow
          icon="entity"
          cls="k-entity"
          title="Look up what is known about something"
          sub="Open an entity to see its current facts, how they changed, and the episodes behind each"
          onClick={() => onFocus({ kind: "folder", path: ["entities"] })}
        />
        <StartRow
          icon="chat"
          cls="k-chat"
          title="Read what was said"
          sub="Calls, threads, notes and records, by the source they came from"
          onClick={() => onFocus({ kind: "folder", path: ["sources"] })}
        />
        <StartRow
          icon="tools"
          cls="k-code"
          title="Ask it as an agent would"
          sub="Call the memory's tools and see exactly what an agent gets back"
          onClick={() => onFocus({ kind: "tools" })}
        />
      </div>
    </section>
  );
}

function StartRow(p: {
  icon: string;
  cls: string;
  title: string;
  sub: string;
  onClick: () => void;
}) {
  return (
    <button className="list-row" onClick={p.onClick}>
      <span className={`kind kind-sm ${p.cls}`}>
        <Icon name={p.icon} size={13} />
      </span>
      <div className="list-main">
        <div className="list-title">{p.title}</div>
        <div className="list-sub">{p.sub}</div>
      </div>
      <Icon name="chevron" />
    </button>
  );
}

const open = (c: Child): Focus =>
  c.kind === "episode"
    ? { kind: "episode", id: c.episode_id! }
    : c.kind === "skill" || c.kind === "skill_index" || c.kind === "advice"
      ? { kind: "skill", path: c.path }
      : { kind: c.kind, path: c.path };

function FolderView({
  path,
  sel,
  onFocus: focus,
  admin = false,
}: {
  path: string[];
  sel: Selection;
  onFocus: (f: Focus) => void;
  admin?: boolean;
}) {
  // The entities and their types narrow to what was learned from a source or a person (`?from=`, `?by=`).
  const learned = path[0] === "entities" && path.length <= 2;
  const link = useNarrow();
  const narrow = learned ? link : { from: "", by: "" };
  const req = { op: "ls", path, limit: 500, ...narrowReq(narrow) };
  const onFocus = (f: Focus) => keepNarrow(narrow, focus, f);
  const [s, retry] = useLoad(
    () => browse<Listing>(req, sel),
    [path, sel, narrow.from, narrow.by],
    () => keptBrowse<Listing>(req, sel),
  );
  const root = path.length === 1 ? ROOTS[path[0] ?? ""] : undefined;
  const name = path.at(-1) ?? "";
  return (
    <section>
      <div className="head">
        <span className={`kind ${ROOTS[path[0] ?? ""]?.kind ?? "k-folder"}`}>
          <Icon name={root ? root.icon : "folder"} size={20} />
        </span>
        <div className="head-text">
          <div className="eyebrow">
            {root ? "Folder" : path.slice(0, -1).map(humanize).join(" / ")}
          </div>
          <h1 className="title">{root ? root.title : humanize(name || "∅")}</h1>
          <div className="subtitle">
            {s.status === "ok" && (
              <span>
                {s.data.children.length}
                {s.data.next ? "+" : ""} {s.data.children.length === 1 ? "item" : "items"}
              </span>
            )}
            {root && <span>{root.note}</span>}
          </div>
        </div>
      </div>
      {learned && (
        <LearnedBar sel={sel} admin={admin} narrow={narrow} type={path[1]} onFocus={focus} />
      )}
      {path.length === 1 && path[0] === "sources" && (
        <AddToMemoryButton sel={sel} onOpen={onFocus} onAdded={retry} />
      )}
      <Loading s={s} retry={retry}>
        {(l) => (
          <>
            {path[0] === "skills" && path.length === 2 && (
              <div className="list" style={{ marginBottom: "var(--s4)" }}>
                <StartRow
                  icon="skill_index"
                  cls="k-skill"
                  title="Routing index"
                  sub="One line per skill: when an agent should use it"
                  onClick={() => onFocus({ kind: "skill", path: [...path, "index"] })}
                />
              </div>
            )}
            {l.children.length === 0 ? (
              <EmptyState title="This folder is empty">
                {narrow.from || narrow.by
                  ? "Nothing here was learned from that, or nothing of it that you can read."
                  : "There is nothing here that you can read."}
              </EmptyState>
            ) : l.children.every((c) => c.kind === "folder") ? (
              <div className="grid">
                {l.children.map((c) => (
                  <button
                    key={JSON.stringify(c.path)}
                    className="tile"
                    onClick={() => onFocus(open(c))}
                  >
                    <div className="tile-top">
                      <span className="tile-name">{humanize(c.name || "∅")}</span>
                      <Icon name="chevron" className="muted" />
                    </div>
                    {c.count != null && <div className="tile-stat">{c.count.toLocaleString()}</div>}
                    <div className="tile-note">
                      <span className="mono">{c.name}</span>
                      {c.latest ? ` · updated ${ago(c.latest)}` : ""}
                    </div>
                  </button>
                ))}
              </div>
            ) : (
              <div className="list">
                {l.children.map((c) => (
                  <Row
                    key={JSON.stringify(c.path) + (c.episode_id ?? "")}
                    c={c}
                    onFocus={onFocus}
                  />
                ))}
              </div>
            )}
            {l.next && (
              <p className="fine">
                Showing the first {l.children.length}; the tree pages through the rest.
              </p>
            )}
          </>
        )}
      </Loading>
    </section>
  );
}

/** One child in a folder's list: what it is, named to be read, with when. */
function Row({ c, onFocus }: { c: Child; onFocus: (f: Focus) => void }) {
  const t = c.occurred_at ?? c.latest;
  let icon: string = c.kind;
  let cls = "k-folder";
  let title = humanize(c.name || "∅");
  let sub: React.ReactNode = <span className="mono">{c.name}</span>;
  if (c.kind === "entity") {
    const e = entityName(c.entity ?? c.path.slice(1).join(":"));
    cls = "k-entity";
    title = e.name;
    sub = (
      <>
        <span className="mono">{c.entity ?? c.name}</span>
        {c.count != null && ` · ${c.count} ${c.count === 1 ? "fact" : "facts"}`}
      </>
    );
  } else if (c.kind === "episode") {
    const k = sourceKind(c.path.slice(1).join(":"));
    icon = k.kind;
    cls = `k-${k.kind}`;
    title =
      c.name === c.episode_id
        ? c.occurred_at
          ? `${k.label} · ${day(c.occurred_at)}`
          : label(c)
        : sourceTitle(c.path.slice(1).join(":"));
    sub = <span className="mono">{c.episode_id ? short(c.episode_id, 12) : c.name}</span>;
  } else if (c.kind === "advice") {
    cls = "k-skill";
    icon = "skill";
    sub = [c.status, c.runs != null ? `${c.runs} ${c.runs === 1 ? "run" : "runs"}` : null]
      .filter(Boolean)
      .join(" · ");
  } else if (c.kind === "skill" || c.kind === "skill_index") {
    cls = "k-skill";
    title = c.kind === "skill_index" ? "Routing index" : humanize(c.name);
    sub =
      c.kind === "skill_index"
        ? `${c.count ?? 0} skills`
        : (c.when ?? <span className="mono">{c.name}</span>);
  }
  if (c.title) {
    title = c.title;
    if (c.kind === "episode")
      sub = <span className="mono">{c.occurred_at ? day(c.occurred_at) : c.name}</span>;
  }
  const t2 = c.kind === "advice" ? (c.learned_at ?? undefined) : t;
  return (
    <button className="list-row" onClick={() => onFocus(open(c))}>
      <span className={`kind kind-sm ${cls}`}>
        <Icon name={icon} size={13} />
      </span>
      <div className="list-main">
        <div className="list-title">{title}</div>
        <div className="list-sub">{sub}</div>
      </div>
      {t2 ? (
        <div className="list-meta" title={when(t2)}>
          {ago(t2)}
        </div>
      ) : c.count != null && c.kind === "folder" ? (
        <div className="list-meta">{c.count}</div>
      ) : null}
      <Icon name="chevron" />
    </button>
  );
}

const newestFirst = (a: Fact, b: Fact) => b.valid_from - a.valid_from;

/** An entity's page (`/browse {op: "entity"}`), or its facts alone from a server that doesn't answer it yet. */
async function entityPage(path: string[], sel: Selection): Promise<EntityPage> {
  try {
    return await browse<EntityPage>({ op: "entity", path }, sel);
  } catch (e) {
    if (e instanceof ApiError && e.status === 400 && /unknown op/i.test(e.message))
      return browse<EntityView>({ op: "ls", path }, sel);
    throw e;
  }
}

function EntityPane({
  path,
  sel,
  onFocus,
  schema,
  admin = false,
  me,
}: {
  path: string[];
  sel: Selection;
  onFocus: (f: Focus) => void;
  schema?: Map<string, OrgType>;
  admin?: boolean;
  me?: string;
}) {
  const [s, retry, reload] = useLoad(
    () => entityPage(path, sel),
    [path, sel],
    () => keptBrowse<EntityPage>({ op: "entity", path }, sel),
  );
  // Its conflicts, read as the caller (`list_conflicts`); none from a server without them.
  const entity = s.status === "ok" ? s.data.entity : null;
  const [conflicts, setConflicts] = useState<Conflict[]>([]);
  const [rev, setRev] = useState(0);
  useEffect(() => {
    if (!entity) return;
    let live = true;
    // One entity's conflicts: a page of up to 200 (the most a page holds) covers all but a pathological one.
    review.conflicts(sel, { entity, limit: 200 }).then(
      (p) => live && setConflicts(p.conflicts),
      () => live && setConflicts([]),
    );
    return () => {
      live = false;
    };
  }, [entity, sel, rev]);
  // Two facts a correction just put in dispute, until the page shows them so: a read may be served the memory
  // from just before the check while the next state is built, so it reads again, a while at most.
  const [expect, setExpect] = useState<{ facts: [string, string]; until: number } | null>(null);
  const page = s.status === "ok" ? s.data : null;
  useEffect(() => {
    if (!expect || !page) return;
    const [a, b] = expect.facts;
    const has = (c: Conflict, id: string) => c.facts.some((x) => x.fact_id === id);
    const listed = conflicts.some((c) => has(c, a) && has(c, b));
    const fact = (id: string) => page.facts.find((x) => x.fact_id === id);
    const marked = [a, b].every((id) => fact(id)?.in_conflict_with?.includes(id === a ? b : a));
    // A side ended meanwhile (one kept, say) settles it too.
    const ended = [a, b].some((id) => fact(id)?.valid_to != null);
    if ((listed && marked) || ended || Date.now() > expect.until) {
      setExpect(null);
      return;
    }
    // Each answer restarts the wait: a slow read holds the next one back.
    const t = setTimeout(() => {
      reload();
      setRev((x) => x + 1);
    }, 1500);
    return () => clearTimeout(t);
  }, [expect, page, conflicts]);
  /** Read the page and its conflicts again, keeping what shows until they come; `disputed`: the two facts a
   * correction put in dispute, read again until they show it. */
  const changed = (disputed?: [string, string]) => {
    reload();
    setRev((x) => x + 1);
    if (disputed) setExpect({ facts: disputed, until: Date.now() + 45_000 });
  };
  return (
    <Loading s={s} retry={retry}>
      {(e) => {
        const n = entityName(e.entity);
        const current = e.facts
          .filter((f) => f.valid_to == null)
          .sort((a, b) => a.predicate.localeCompare(b.predicate));
        const past = e.facts.filter((f) => f.valid_to != null).sort(newestFirst);
        const sources = new Set(e.facts.flatMap((f) => f.source_episodes)).size;
        const latest = Math.max(0, ...e.facts.map((f) => f.valid_from));
        const links = e.links;
        const linked = links ? new Set([...links.out, ...links.in].map((l) => l.entity)).size : 0;
        // Its layout, picked from its type's shape in the org's schema (shape.ts); none for a type it doesn't declare.
        const shape = n.type && schema ? shapeOf(n.type, schema) : null;
        return (
          <section>
            <div className="head">
              <span className="kind k-entity">
                <Icon name="entity" size={22} />
              </span>
              <div className="head-text">
                <div className="eyebrow">{n.type ? humanize(n.type) : "Entity"}</div>
                <h1 className="title">{n.name}</h1>
                <div className="subtitle">
                  <span className="mono">{e.entity}</span>
                  <span className="badge badge-accent">{current.length} current</span>
                  {past.length > 0 && <span className="badge">{past.length} past</span>}
                  {conflicts.length > 0 && (
                    <span className="badge badge-error">{conflicts.length} in dispute</span>
                  )}
                  <span>
                    from {sources} {sources === 1 ? "episode" : "episodes"}
                  </span>
                  {latest > 0 && <span title={when(latest)}>updated {ago(latest)}</span>}
                </div>
              </div>
            </div>
            <EntityOwnership entity={e.entity} sel={sel} me={me} admin={admin} onFocus={onFocus} />
            {e.facts.length === 0 ? (
              <EmptyState icon="entity" title="No facts yet">
                Nothing has been learned about {e.entity} that you can read.
              </EmptyState>
            ) : (
              <>
                {shape && shape.preds.length > 0 && (
                  <Layout
                    entity={e.entity}
                    type={n.type}
                    shape={shape}
                    facts={current}
                    onFocus={onFocus}
                  />
                )}
                <div className="h-section">
                  What is true now <span className="count">{current.length}</span>
                </div>
                {current.length ? (
                  <EntityFacts
                    page={e}
                    facts={current}
                    sel={sel}
                    me={me}
                    admin={admin}
                    schema={schema}
                    onFocus={onFocus}
                    onChanged={changed}
                  />
                ) : (
                  <div className="card card-pad muted">
                    Every fact about it has ended; its history is below.
                  </div>
                )}
                <Disputes
                  conflicts={conflicts}
                  page={e}
                  sel={sel}
                  onFocus={onFocus}
                  onChanged={changed}
                />
                <DreamDecisions
                  entity={e.entity}
                  sel={sel}
                  admin={admin}
                  me={me}
                  onFocus={onFocus}
                  onChanged={changed}
                />
                {links && linked > 0 && (
                  <details className="history en-more" open={linked <= 6}>
                    <summary className="h-section">
                      <Icon name="chevron" size={14} /> Connections{" "}
                      <span className="count">
                        {linked} {linked === 1 ? "entity" : "entities"}
                      </span>
                    </summary>
                    <Connections entity={e.entity} links={links} onFocus={onFocus} />
                  </details>
                )}
                {past.length > 0 && (
                  <History count={past.length} open={current.length === 0}>
                    {() => (
                      <EntityFacts
                        page={e}
                        facts={past}
                        sel={sel}
                        me={me}
                        admin={admin}
                        onFocus={onFocus}
                        onChanged={changed}
                      />
                    )}
                  </History>
                )}
              </>
            )}
          </section>
        );
      }}
    </Loading>
  );
}

function EpisodePane({
  id,
  sel,
  onFocus,
}: {
  id: string;
  sel: Selection;
  onFocus: (f: Focus) => void;
}) {
  const [s, retry] = useLoad(
    () => browse<EpisodeView>({ op: "episode", id }, sel),
    [id, sel],
    () => keptBrowse<EpisodeView>({ op: "episode", id }, sel),
  );
  return (
    <Loading s={s} retry={retry} rows={6}>
      {(e) => {
        const k = sourceKind(e.source);
        return (
          <section>
            <div className="head">
              <span className={`kind k-${k.kind}`}>
                <Icon name={k.kind} size={20} />
              </span>
              <div className="head-text">
                <div className="eyebrow">{k.label}</div>
                <h1 className="title">{e.title || sourceTitle(e.source)}</h1>
                <div className="subtitle">
                  <span>
                    <Icon name="clock" size={13} /> {when(e.occurred_at)}
                  </span>
                  <span>{ago(e.occurred_at)}</span>
                  <span>
                    {e.facts.length} {e.facts.length === 1 ? "fact" : "facts"} drawn
                  </span>
                </div>
              </div>
            </div>
            <div className="card card-pad">
              <dl className="meta">
                <dt>Source</dt>
                <dd className="mono">{e.source}</dd>
                <dt>Episode</dt>
                <dd>
                  <CopyId id={e.episode_id} n={16} />
                </dd>
                <dt>Location</dt>
                <dd>
                  {e.path.map((seg, i) => (
                    <span key={i} className="nowrap">
                      {i > 0 && <span className="muted"> / </span>}
                      {i < e.path.length - 1 ? (
                        <button
                          className="link"
                          onClick={() => onFocus({ kind: "folder", path: e.path.slice(0, i + 1) })}
                        >
                          {seg || "∅"}
                        </button>
                      ) : (
                        <span className="mono">{seg}</span>
                      )}
                    </span>
                  ))}
                </dd>
                {e.about.length > 0 && (
                  <>
                    <dt>About</dt>
                    <dd>
                      {e.about.map((a) => (
                        <EntityLink key={a} entity={a} onFocus={onFocus} />
                      ))}
                    </dd>
                  </>
                )}
              </dl>
            </div>
            <div className="h-section">
              Content {e.truncated && <span className="count">first 256 KiB</span>}
            </div>
            <div className="card">
              <Body text={e.body} />
            </div>
            <div className="h-section">
              Facts drawn from it <span className="count">{e.facts.length}</span>
            </div>
            {e.facts.length ? (
              <Facts facts={e.facts} onFocus={onFocus} hideEvidence={id} />
            ) : (
              <div className="card card-pad muted">No facts were drawn from this episode.</div>
            )}
          </section>
        );
      }}
    </Loading>
  );
}

/** `Dana Whitfield (Globex, VP Operations): …`: a turn of a conversation, with an optional role. */
const TURN = /^([A-Z][\p{L}.'’-]*(?: [A-Z][\p{L}.'’-]*){0,3})(?: \(([^)]{1,80})\))?: (.+)$/u;

/** A body that is one JSON object of at most 60 fields (a CRM record), as its fields; else null. */
function record(text: string): [string, unknown][] | null {
  if (!text.startsWith("{")) return null;
  try {
    const v = JSON.parse(text);
    const entries =
      v && typeof v === "object" && !Array.isArray(v)
        ? Object.entries(v as Record<string, unknown>)
        : [];
    return entries.length && entries.length <= 60 ? entries : null;
  } catch {
    return null;
  }
}

/** An episode's text, laid out by what it is: a record's fields, a conversation's turns, or prose. */
function Body({ text }: { text: string }) {
  const trimmed = text.trim();
  const entries = record(trimmed);
  if (entries)
    return (
      <div className="kv">
        {entries.flatMap(([k, x]) => [
          <div key={`k-${k}`} className="k">
            {k}
          </div>,
          <div key={`v-${k}`} className="v">
            {typeof x === "string" || typeof x === "number" || typeof x === "boolean" ? (
              String(x)
            ) : (
              <code>{JSON.stringify(x)}</code>
            )}
          </div>,
        ])}
      </div>
    );
  const lines = trimmed.split("\n");
  const turns = lines.map((l) => TURN.exec(l.trim()));
  const first = turns.findIndex(Boolean);
  if (turns.filter(Boolean).length >= 2) {
    const lead = lines.slice(0, first).join("\n").trim();
    const speakers: string[] = [];
    return (
      <div className="card-pad">
        {lead && <div className="body-lead">{lead}</div>}
        <div className="turns">
          {lines.map((l, i) => {
            const m = turns[i];
            if (!m)
              return i > first && l.trim() ? (
                <div key={i} className="turn-text">
                  {l}
                </div>
              ) : null;
            const who = m[1] ?? "";
            if (!speakers.includes(who)) speakers.push(who);
            const initials = who
              .split(" ")
              .map((w) => w[0])
              .slice(0, 2)
              .join("");
            return (
              <div key={i} className={`turn ${speakers.indexOf(who) % 2 ? "turn-alt" : ""}`}>
                <span className="avatar">{initials}</span>
                <div>
                  <div className="turn-who">
                    {m[1]} {m[2] && <span className="turn-role">· {m[2]}</span>}
                  </div>
                  <div className="turn-text">{m[3]}</div>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    );
  }
  return <div className="card-pad body-text">{trimmed}</div>;
}

/** A piece of advice learned from sessions: its note, with how it stands. */
function AdvicePane({
  path,
  sel,
  onFocus,
}: {
  path: string[];
  sel: Selection;
  onFocus: (f: Focus) => void;
}) {
  const [s, retry] = useLoad(
    () => browse<AdviceView>({ op: "ls", path }, sel),
    [path, sel],
    () => keptBrowse<AdviceView>({ op: "ls", path }, sel),
  );
  return (
    <Loading s={s} retry={retry} rows={6}>
      {({ advice: a }) =>
        // A folder of advice (`#/skill/advice`), or anything else that isn't one piece: say so rather than read it.
        !a ? (
          <EmptyState icon="skill" title="No advice here">
            <span className="mono">{path.join("/")}</span> isn't a piece of advice.{" "}
            <button className="link" onClick={() => onFocus({ kind: "folder", path: ["advice"] })}>
              See all advice
            </button>
          </EmptyState>
        ) : (
        <section>
          <div className="head">
            <span className="kind k-skill">
              <Icon name="skill" size={20} />
            </span>
            <div className="head-text">
              <div className="eyebrow">
                {a.space === "mine" ? "Your advice" : "Team advice"} ·{" "}
                <button
                  className="link"
                  onClick={() => onFocus({ kind: "folder", path: path.slice(0, 3) })}
                >
                  {a.category}
                </button>
              </div>
              <h1 className="title">{a.situation}</h1>
              <div className="subtitle">
                {a.runs != null && (
                  <span>
                    {a.runs} runs{a.wrong ? `, ${a.wrong} went wrong` : ""}
                  </span>
                )}
                {a.learned_at != null && (
                  <span title={when(a.learned_at)}>learned {ago(a.learned_at)}</span>
                )}
                {a.needs_backing && <span className="chip">needs backing</span>}
                {a.merged_into && <span className="chip">merged into {a.merged_into}</span>}
                {a.note_id && (
                  <button
                    className="chip mono"
                    title={a.note_id}
                    onClick={() => onFocus({ kind: "episode", id: a.note_id! })}
                  >
                    note {short(a.note_id, 10)}
                  </button>
                )}
              </div>
            </div>
          </div>
          <div className="card card-pad">
            {a.text ? (
              <Markdown text={a.text} />
            ) : (
              <EmptyState title="No note yet">
                Advice for this situation has not been written yet.
              </EmptyState>
            )}
          </div>
        </section>
        )
      }
    </Loading>
  );
}

/** A compiled skill, its markdown then its AIP step graph; or a goal's routing index. */
function SkillPane({
  path,
  sel,
  onFocus,
}: {
  path: string[];
  sel: Selection;
  onFocus: (f: Focus) => void;
}) {
  const [s, retry] = useLoad(
    () => browse<SkillView>({ op: "ls", path }, sel),
    [path, sel],
    () => keptBrowse<SkillView>({ op: "ls", path }, sel),
  );
  return (
    <Loading s={s} retry={retry} rows={6}>
      {(k) =>
        // What the path names isn't a skill (a folder, or something else): say so rather than read it as one.
        (k.kind !== "skill" && k.kind !== "skill_index") || typeof k.markdown !== "string" ? (
          <EmptyState icon="skill" title="No skill here">
            <span className="mono">{path.join("/") || "/"}</span> isn't a skill.{" "}
            <button className="link" onClick={() => onFocus({ kind: "folder", path: ["skills"] })}>
              See every skill
            </button>
          </EmptyState>
        ) : (
        <section>
          <div className="head">
            <span className="kind k-skill">
              <Icon name={k.kind} size={20} />
            </span>
            <div className="head-text">
              <div className="eyebrow">{k.kind === "skill_index" ? "Routing index" : "Skill"}</div>
              <h1 className="title">
                {k.kind === "skill_index" ? `Skills for ${humanize(k.goal)}` : humanize(k.name)}
              </h1>
              <div className="subtitle">
                <span>
                  Goal{" "}
                  <button
                    className="link"
                    onClick={() => onFocus({ kind: "folder", path: ["skills", k.goal] })}
                  >
                    {k.goal}
                  </button>
                </span>
                {k.occurred_at != null && (
                  <span title={when(k.occurred_at)}>compiled {ago(k.occurred_at)}</span>
                )}
                {k.episode_id && (
                  <button
                    className="chip mono"
                    title={k.episode_id}
                    onClick={() => onFocus({ kind: "episode", id: k.episode_id! })}
                  >
                    version {short(k.episode_id, 10)}
                  </button>
                )}
              </div>
            </div>
          </div>
          <div className="card card-pad">
            <Markdown
              text={k.markdown}
              // A skill named in `code` in its goal's index links to it.
              code={(c) =>
                /^[a-z0-9][a-z0-9~-]*$/.test(c) && c !== k.goal ? (
                  <button
                    className="link mono"
                    onClick={() => onFocus({ kind: "skill", path: ["skills", k.goal, c] })}
                  >
                    {c}
                  </button>
                ) : null
              }
            />
          </div>
          {k.kind === "skill" && (k.sources !== undefined || k.sources_hidden) && (
            <>
              <div className="h-section">Compiled from</div>
              <div className="card card-pad">
                {k.sources?.length ? (
                  <div className="chips">
                    {k.sources.map((e) => (
                      <button
                        key={e.episode_id}
                        className="chip"
                        title={e.episode_id}
                        onClick={() => onFocus({ kind: "episode", id: e.episode_id })}
                      >
                        <Icon name="episode" size={13} />
                        {e.name || short(e.episode_id, 10)}
                      </button>
                    ))}
                  </div>
                ) : (
                  <span className="muted">No source episode you may read.</span>
                )}
                {(k.sources_hidden ?? 0) > 0 && (
                  <p className="muted">
                    And {k.sources_hidden} {k.sources_hidden === 1 ? "episode" : "episodes"} you
                    can't read.
                  </p>
                )}
              </div>
            </>
          )}
          {k.kind === "skill" && (
            <>
              <div className="h-section">AIP step graph</div>
              {k.aip == null ? (
                <div className="card card-pad muted">
                  No AIP graph has been compiled for this skill yet.
                </div>
              ) : typeof k.aip === "string" ? (
                <pre className="code">{k.aip}</pre>
              ) : (
                <AipGraph aip={k.aip} />
              )}
            </>
          )}
        </section>
        )
      }
    </Loading>
  );
}

/** An AIP graph's purpose, triggers and steps with what each takes and gives, then the JSON itself. */
function AipGraph({ aip }: { aip: Aip }) {
  const names = (xs?: { name: string }[]) => (xs ?? []).map((x) => x.name);
  return (
    <div className="aip">
      {(aip.purpose ||
        (aip.trigger_when?.length ?? 0) > 0 ||
        (aip.do_not_use_when?.length ?? 0) > 0) && (
        <div className="card card-pad md">
          {aip.purpose && <p>{aip.purpose}</p>}
          {(aip.trigger_when?.length ?? 0) > 0 && (
            <>
              <h4>Use when</h4>
              <ul>
                {aip.trigger_when!.map((t, i) => (
                  <li key={i}>{t}</li>
                ))}
              </ul>
            </>
          )}
          {(aip.do_not_use_when?.length ?? 0) > 0 && (
            <>
              <h4>Don&apos;t use when</h4>
              <ul>
                {aip.do_not_use_when!.map((t, i) => (
                  <li key={i}>{t}</li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}
      <ol className="aip-steps">
        {(aip.steps ?? []).map((st, i) => (
          <li key={i}>
            <div>
              <div className="aip-step">{humanize(st.name)}</div>
              {st.description && <div className="muted">{st.description}</div>}
              <div className="aip-io">
                {names(st.inputs).map((n) => (
                  <span key={`in-${n}`} className="chip mono" title="input">
                    ← {n}
                  </span>
                ))}
                {names(st.outputs).map((n) => (
                  <span key={`out-${n}`} className="chip mono chip-out" title="output">
                    → {n}
                  </span>
                ))}
              </div>
            </div>
          </li>
        ))}
      </ol>
      <details>
        <summary className="muted" style={{ cursor: "pointer", fontSize: "var(--t-sm)" }}>
          The graph as JSON (AIP {aip.aip ?? ""})
        </summary>
        <pre className="code" style={{ marginTop: "var(--s2)" }}>
          {JSON.stringify(aip, null, 2)}
        </pre>
      </details>
    </div>
  );
}

/** The episode types memstore knows (`crates/memstore-core/src/episode_types`), as a search narrows to them. */
export const EPISODE_TYPES = [
  ["sales_call", "Calls"],
  ["slack_thread", "Slack threads"],
  ["email", "Email"],
  ["crm_update", "CRM updates"],
  ["doc", "Documents and notes"],
] as const;

/**
 * What a search is narrowed to: when its facts were stated, the episodes they cite, their entity's type, and
 * (for the org's admins) the person whose notes or conversations they come from.
 */
type Facets = {
  from: string;
  to: string;
  kinds: string[];
  type: string;
  newest: boolean;
  person: string;
};
const NO_FACETS: Facets = { from: "", to: "", kinds: [], type: "", newest: false, person: "" };
const narrowed = (f: Facets) =>
  Boolean(f.from || f.to || f.kinds.length || f.type || f.newest || f.person);

/**
 * The facts from what one person wrote or said (memstore's `/admin {op: "user_facts"}`): those citing their
 * notes or their sessions' and runs' transcripts, naming the query's words, then narrowed here by date and
 * entity type. The org's admins only; memstore leaves out what the admin can't read.
 */
async function personSearch(query: string, f: Facets, sel: Selection): Promise<{ facts: Fact[] }> {
  const r = await call<{ facts: Fact[] }>({ org: sel.org, team: sel.team }, "admin", {
    op: "user_facts",
    user: f.person,
    query,
    limit: 500,
  });
  const from = f.from ? Date.parse(f.from) * 1000 : -Infinity;
  const to = f.to ? (Date.parse(f.to) + 86_400_000) * 1000 : Infinity;
  const facts = r.facts.filter(
    (x) =>
      x.valid_from >= from && x.valid_from < to && (!f.type || x.entity.startsWith(`${f.type}:`)),
  );
  if (f.newest) facts.sort((a, b) => b.valid_from - a.valid_from);
  return { facts: facts.slice(0, 100) };
}

/**
 * The org's members and whoever was active in its memory this year, for the admin's "by person" facet; none
 * where the org's activity setting is off (memstore refuses both reads then).
 */
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
      }),
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

/** A hit of `search_memory` as JSON: a fact, and the entity's other facts it was grouped with. */
type Hit = Fact & { facts?: Fact[] };

/**
 * A search narrowed by its facets: memstore's `search_memory` (as an agent calls it) with the dates, the
 * episode types and the entity type, as JSON. Unnarrowed, the page's own `/search` answers (and is kept).
 */
async function facetSearch(query: string, f: Facets, sel: Selection): Promise<{ facts: Fact[] }> {
  const r = await mcp<{ content?: { text?: string }[]; isError?: boolean }>(sel, "tools/call", {
    name: "search_memory",
    arguments: {
      query,
      k: 30,
      format: "json",
      ...(f.from ? { start_date: f.from } : {}),
      ...(f.to ? { end_date: f.to } : {}),
      ...(f.kinds.length ? { episode_type: f.kinds } : {}),
      ...(f.type ? { type: f.type } : {}),
      ...(f.newest ? { order: "newest" } : {}),
    },
  });
  const text = (r.content ?? []).map((c) => c.text ?? "").join("\n");
  if (r.isError) throw new Error(text || "search_memory failed");
  const hits = JSON.parse(text || "[]") as Hit[];
  const seen = new Set<string>();
  const facts: Fact[] = [];
  for (const h of hits)
    for (const x of [h, ...(h.facts ?? [])])
      if (!seen.has(x.fact_id)) {
        seen.add(x.fact_id);
        facts.push(x);
      }
  return { facts };
}

function SearchPane({
  query,
  sel,
  onFocus,
  types,
  admin,
}: {
  query: string;
  sel: Selection;
  onFocus: (f: Focus) => void;
  types: string[];
  admin: boolean;
}) {
  // The narrowing is kept in the link (`?from=…&to=…&kinds=…&type=…&newest=1&person=…`).
  const hash = useSyncExternalStore(subscribe, () => location.hash);
  const facets = useMemo<Facets>(() => {
    const o = optionsOf(hash);
    const kinds = o.get("kinds");
    return {
      from: o.get("from") ?? "",
      to: o.get("to") ?? "",
      kinds: kinds ? kinds.split(",") : [],
      type: o.get("type") ?? "",
      newest: o.get("newest") === "1",
      person: o.get("person") ?? "",
    };
  }, [hash]);
  const setFacets = (f: Facets) =>
    setOptions({
      from: f.from || null,
      to: f.to || null,
      kinds: f.kinds.length ? f.kinds.join(",") : null,
      type: f.type || null,
      newest: f.newest ? "1" : null,
      person: f.person || null,
    });
  const people = usePeople(sel, admin);
  const on = narrowed(facets);
  const [s, retry] = useLoad(
    () =>
      facets.person
        ? personSearch(query, facets, sel)
        : on
          ? facetSearch(query, facets, sel)
          : search<{ facts: Fact[] }>(query, sel),
    [query, sel, JSON.stringify(facets)],
    on ? undefined : () => keptSearch<{ facts: Fact[] }>(query, sel),
  );
  const set = (p: Partial<Facets>) => setFacets({ ...facets, ...p });
  return (
    <section>
      <div className="head">
        <span className="kind k-entity">
          <Icon name="search" size={20} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Search</div>
          <h1 className="title">“{query}”</h1>
          <div className="subtitle">
            {s.status === "ok"
              ? `${s.data.facts.length} best-matching facts, current and past${on ? ", narrowed" : ""}`
              : "Searching facts…"}
          </div>
        </div>
      </div>
      <div className="facets" role="group" aria-label="Narrow the search">
        <label className="facet">
          <span>Stated from</span>
          <input
            type="date"
            className="field"
            value={facets.from}
            max={facets.to || undefined}
            onChange={(e) => set({ from: e.target.value })}
          />
        </label>
        <label className="facet">
          <span>to</span>
          <input
            type="date"
            className="field"
            value={facets.to}
            min={facets.from || undefined}
            onChange={(e) => set({ to: e.target.value })}
          />
        </label>
        {types.length > 0 && (
          <label className="facet">
            <span>About a</span>
            <select
              className="field"
              value={facets.type}
              onChange={(e) => set({ type: e.target.value })}
            >
              <option value="">any type</option>
              {types.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </label>
        )}
        <label className="facet">
          <span>Order</span>
          <select
            className="field"
            value={facets.newest ? "newest" : "relevance"}
            onChange={(e) => set({ newest: e.target.value === "newest" })}
          >
            <option value="relevance">best match</option>
            <option value="newest">newest first</option>
          </select>
        </label>
        {people.length > 0 && (
          <label className="facet">
            <span>From person</span>
            <select
              className="field"
              value={facets.person}
              onChange={(e) => set({ person: e.target.value, kinds: [] })}
            >
              <option value="">anyone</option>
              {people.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          </label>
        )}
        {!facets.person && (
          <div className="facet facet-kinds">
            <span>From</span>
            {EPISODE_TYPES.map(([k, name]) => (
              <button
                key={k}
                className={`chip ${facets.kinds.includes(k) ? "chip-on" : ""}`}
                aria-pressed={facets.kinds.includes(k)}
                onClick={() =>
                  set({
                    kinds: facets.kinds.includes(k)
                      ? facets.kinds.filter((x) => x !== k)
                      : [...facets.kinds, k],
                  })
                }
              >
                {name}
              </button>
            ))}
          </div>
        )}
        {on && (
          <button className="link" onClick={() => setFacets(NO_FACETS)}>
            Clear
          </button>
        )}
      </div>
      <Loading s={s} retry={retry}>
        {(r) =>
          r.facts.length ? (
            <Facts facts={r.facts} onFocus={onFocus} showEntity />
          ) : (
            <EmptyState icon="search" title="No matching facts">
              {facets.person
                ? `Nothing you can read from ${facets.person}'s notes or conversations names “${query}”${
                    facets.from || facets.to || facets.type ? " with these filters" : ""
                  }.`
                : `Nothing you can read matches “${query}”${on ? " with these filters" : ""}. Try fewer or different words${
                    on ? ", or clear the filters" : ""
                  }.`}
            </EmptyState>
          )
        }
      </Loading>
    </section>
  );
}

/** An entity's ended facts, drawn only once the reader opens them. */
function History({
  count,
  open,
  children,
}: {
  count: number;
  open: boolean;
  children: () => React.ReactNode;
}) {
  const [opened, setOpened] = useState(open);
  return (
    <details
      className="history en-more"
      open={open}
      onToggle={(ev) => ev.currentTarget.open && setOpened(true)}
    >
      <summary className="h-section">
        <Icon name="chevron" size={14} /> History{" "}
        <span className="count">{count} superseded or ended</span>
      </summary>
      {opened && children()}
    </details>
  );
}

function Facts({
  facts,
  onFocus,
  showEntity,
  hideEvidence,
}: {
  facts: Fact[];
  onFocus: (f: Focus) => void;
  showEntity?: boolean;
  hideEvidence?: string;
}) {
  const showAll = showEntity || facts.some((f) => f.entity !== facts[0]?.entity);
  const top = Math.max(0, ...facts.map((f) => f.score ?? 0));
  return (
    <div className="card facts">
      {facts.map((f) => {
        const ended = f.valid_to != null;
        const evidence = f.source_episodes.filter((e) => e !== hideEvidence);
        return (
          <div key={f.fact_id} className={`fact ${ended ? "fact-past" : ""}`}>
            <div className="fact-key" title={f.predicate}>
              {humanize(f.predicate)}
            </div>
            <div>
              {showAll && (
                <div className="fact-entity">
                  <EntityLink entity={f.entity} onFocus={onFocus} />
                </div>
              )}
              <div className="fact-value">
                {named(f.value) ? (
                  <EntityLink entity={f.value} onFocus={onFocus} />
                ) : (
                  f.value
                )}
              </div>
              {f.quote && <div className="quote">“{f.quote}”</div>}
            </div>
            <div className="fact-side">
              <div className="fact-when">
                {ended ? (
                  <span
                    className="badge badge-past"
                    title={`${when(f.valid_from)} → ${when(f.valid_to)}`}
                  >
                    ended
                  </span>
                ) : (
                  <span className="badge badge-ok" title={`since ${when(f.valid_from)}`}>
                    current
                  </span>
                )}
                <span
                  title={ended ? `${when(f.valid_from)} → ${when(f.valid_to)}` : when(f.valid_from)}
                >
                  {ended
                    ? `${day(f.valid_from)} – ${day(f.valid_to)}`
                    : `since ${day(f.valid_from)}`}
                </span>
              </div>
              {f.score != null && top > 0 && (
                <span className="score" title={`relevance ${f.score.toFixed(3)}`}>
                  <span style={{ width: `${Math.max(8, (f.score / top) * 100)}%` }} />
                </span>
              )}
              {evidence.length > 0 && (
                <div className="evidence">
                  {evidence.map((e) => (
                    <button
                      key={e}
                      className="chip"
                      title={`Evidence: episode ${e}`}
                      onClick={() => onFocus({ kind: "episode", id: e })}
                    >
                      <Icon name="episode" size={12} />
                      <span className="mono">{short(e)}</span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
