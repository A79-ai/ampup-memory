// An entity's page, past its header: each fact with where it came from and who may read it, what the entity is
// connected to, what is in dispute on it, and a way to correct a fact. Everything comes from memstore's
// `/browse {op: "entity"}` (src/entity_page.rs) and the conflict ops of `/review` (src/server/conflict_ops.rs),
// read as the signed-in person: a fact, an episode, a link or a conflict they can't read isn't here, nor counted.
import { useEffect, useMemo, useState } from "react";
import { ApiError } from "./api";
import { addedBy, day, entityName, humanize, personName, sourceKind, sourceTitle, when } from "./format";
import { fitView, seed, step, type Sim } from "./graphLayout";
import type { GraphEdge, GraphNode } from "./graphTypes";
import { EntityLink, FactText } from "./layouts";
import { DreamCard } from "./review";
import {
  entityFocus,
  review,
  typeOf,
  type Conflict,
  type ConflictSide,
  type DreamDecision,
} from "./review-api";
import { closedValues, ValueField } from "./schema";
import type { Fact, Focus, OrgType, Selection } from "./types";
import { Icon } from "./ui";
import "./entity.css";

/** One episode a fact cites, as the entity page reads it. */
export type EntityEpisode = {
  source: string;
  occurred_at: number;
  type?: string | null;
  /** What its type says it is called (a call's recorder title), when it says. */
  title?: string | null;
  origin?: "agent" | "stated" | "observed" | null;
  /** Its ACL: `*`, `user:…`, `group:…`, `role:…`. */
  readers: string[];
};

export type EntityFact = Fact & {
  origin?: "agent" | "stated" | "observed";
  /** Independent sources the reader may read that say it. */
  corroboration?: number;
  said_by?: string;
  value_ref?: string;
  /** Open facts the reader may read that it is recorded in conflict with. */
  in_conflict_with?: string[];
  cite?: Record<string, unknown>;
};

export type Link = { predicate: string; entity: string };

export type EntityPage = {
  path: string[];
  entity: string;
  facts: EntityFact[];
  /** Absent from a server older than the entity page: it answers `ls`, without provenance. */
  episodes?: Record<string, EntityEpisode>;
  links?: { out: Link[]; in: Link[]; more?: boolean };
};

const norm = (s: string) => s.trim().toLowerCase();

/** Who may read an episode, in plain words. Groups read as who is in them: the org's own (`<org>.all`, which
 * everyone the org admits holds) as everyone in it, a private Slack channel's (`<org>.slack.<id>`) as its members;
 * another group by its name. */
function audience(
  acl: string[],
  me: string | undefined,
  sel: Selection,
): { label: string; open: boolean } {
  if (acl.includes("*"))
    return { label: sel.team ? `Everyone in ${sel.team}` : "Everyone in the org", open: true };
  const users = acl.filter((a) => a.startsWith("user:")).map((a) => a.slice(5));
  const groups = acl
    .filter((a) => a.startsWith("group:") || a.startsWith("role:"))
    .map((a) => a.slice(a.indexOf(":") + 1));
  const org = norm(sel.org);
  if (groups.some((g) => norm(g) === `${org}.all`))
    return { label: "Everyone in the org", open: true };
  if (!groups.length && users.length) {
    if (users.length === 1 && me && norm(users[0]!) === norm(me))
      return { label: "Only you", open: false };
    return { label: `Only ${users.map((u) => personName(`_:${u}`)).join(", ")}`, open: false };
  }
  if (groups.length) {
    const channel = (g: string) => norm(g).startsWith(`${org}.slack.`);
    const channels = groups.filter(channel).length;
    const named = [
      ...groups.filter((g) => !channel(g)),
      ...(channels ? [channels === 1 ? "a private Slack channel" : `${channels} private Slack channels`] : []),
    ];
    return {
      label: `${channels === groups.length ? "Members of" : "People in"} ${named.join(", ")}${
        users.length ? ` and ${users.length} more` : ""
      }`,
      open: false,
    };
  }
  return { label: "Restricted", open: false };
}

/** Who may read a fact: the narrowest of its episodes (a reader must read every one). */
function factAudience(
  f: EntityFact,
  eps: Record<string, EntityEpisode>,
  me: string | undefined,
  sel: Selection,
) {
  const each = f.source_episodes
    .map((e) => eps[e])
    .filter((e): e is EntityEpisode => !!e)
    .map((e) => audience(e.readers, me, sel));
  if (!each.length) return null;
  const closed = [...new Set(each.filter((a) => !a.open).map((a) => a.label))];
  return closed.length ? { label: closed.join(" and "), open: false } : each[0]!;
}

/** A source as a title; a person's own note, or a fact an admin stated, by who wrote it; what a person added in
 * Studio as they added it. */
function titleOf(source: string): string {
  const note = "mcp:remember_memory:";
  if (!source.startsWith(note) || addedBy(source)) return sourceTitle(source);
  const rest = source.slice(note.length);
  const author = rest.split(":")[0] || "someone";
  return rest.slice(author.length + 1).startsWith("fact:")
    ? `Stated by ${author}`
    : `Note by ${author}`;
}

const ORIGIN: Record<string, string> = {
  observed: "Recorded by a system",
  stated: "Said by a person",
  agent: "An agent concluded it",
};

/** Where a fact came from, who said it, when, who may read it and how many sources back it. */
function Provenance({
  f,
  eps,
  me,
  sel,
  onFocus,
}: {
  f: EntityFact;
  eps: Record<string, EntityEpisode>;
  me?: string;
  sel: Selection;
  onFocus: (f: Focus) => void;
}) {
  const aud = factAudience(f, eps, me, sel);
  const n = f.corroboration ?? 0;
  return (
    <div className="en-prov">
      {f.source_episodes.map((id) => {
        const e = eps[id];
        const k = e ? sourceKind(e.source) : { kind: "episode", label: "Episode" };
        return (
          <button
            key={id}
            className="chip"
            title={e ? `${e.source}, ${when(e.occurred_at)}` : `episode ${id}`}
            onClick={() => onFocus({ kind: "episode", id })}
          >
            <Icon name={k.kind} size={12} />
            {e ? e.title || titleOf(e.source) : id.slice(0, 8)}
          </button>
        );
      })}
      {f.said_by && <span title="Who said it">{personName(`_:${f.said_by}`)}</span>}
      {f.source_episodes[0] && eps[f.source_episodes[0]] && (
        <span title={when(eps[f.source_episodes[0]]!.occurred_at)}>
          {day(eps[f.source_episodes[0]]!.occurred_at)}
        </span>
      )}
      {aud && (
        <span
          className={`en-aud ${aud.open ? "" : "en-aud-closed"}`}
          title="Who can read this fact"
        >
          <Icon name={aud.open ? "user" : "lock"} size={12} /> {aud.label}
        </span>
      )}
      {f.origin === "agent" ? (
        <span className="badge" title={ORIGIN.agent}>
          an agent concluded
        </span>
      ) : (
        f.corroboration != null && (
          <span
            title={`${f.origin ? `${ORIGIN[f.origin]}. ` : ""}Independent sources you can read that say it`}
          >
            {n <= 1 ? "1 source" : `backed by ${n} sources`}
          </span>
        )
      )}
    </div>
  );
}

/** The most facts a list draws before "Show all". */
const FEW_FACTS = 200;

/** Facts with their provenance; a current one can be corrected by an org admin. */
export function EntityFacts({
  page,
  facts,
  sel,
  me,
  admin,
  schema,
  onFocus,
  onChanged,
}: {
  page: EntityPage;
  facts: EntityFact[];
  sel: Selection;
  me?: string;
  admin: boolean;
  /** The org's schema: a correction of an attribute it closes picks one of its values. */
  schema?: Map<string, OrgType>;
  onFocus: (f: Focus) => void;
  /** Read the page again; `disputed`: two facts a correction put in dispute, read again until they show it. */
  onChanged: (disputed?: [string, string]) => void;
}) {
  const eps = page.episodes ?? {};
  // A value naming an entity the reader may read opens it.
  const named = new Set((page.links?.out ?? []).map((l) => l.entity));
  const [fixing, setFixing] = useState<string | null>(null);
  // An entity can hold hundreds: the first FEW_FACTS until the reader asks for the rest.
  const [all, setAll] = useState(false);
  const shown = all ? facts : facts.slice(0, FEW_FACTS);
  return (
    <>
      <div className="card facts en-facts">
        {shown.map((f) => {
          const ended = f.valid_to != null;
          const disputed = !ended && (f.in_conflict_with?.length ?? 0) > 0;
          return (
            <div key={f.fact_id} className={`fact ${ended ? "fact-past" : ""}`}>
              <div className="fact-key" title={f.predicate}>
                {humanize(f.predicate)}
              </div>
              <div>
                <div className="fact-value">
                  {f.value_ref || named.has(f.value) ? (
                    <EntityLink entity={f.value_ref ?? f.value} onFocus={onFocus} />
                  ) : (
                    <FactText value={f.value} />
                  )}
                </div>
                {f.quote && f.quote !== f.value && <div className="quote">“{f.quote}”</div>}
                {page.episodes && (
                  <Provenance f={f} eps={eps} me={me} sel={sel} onFocus={onFocus} />
                )}
                {fixing === f.fact_id && (
                  <Correct
                    f={f}
                    values={closedValues(schema, f.entity, f.predicate)}
                    open={factAudience(f, eps, me, sel)?.open ?? true}
                    sel={sel}
                    onDone={(disputed) => {
                      setFixing(null);
                      onChanged(disputed);
                    }}
                    onAdded={() => onChanged()}
                    onCancel={() => setFixing(null)}
                  />
                )}
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
                  ) : disputed ? (
                    <span className="badge badge-error" title="Another source says otherwise">
                      in dispute
                    </span>
                  ) : (
                    <span className="badge badge-ok" title={`since ${when(f.valid_from)}`}>
                      current
                    </span>
                  )}
                  <span
                    title={
                      ended ? `${when(f.valid_from)} → ${when(f.valid_to)}` : when(f.valid_from)
                    }
                  >
                    {ended
                      ? `${day(f.valid_from)} – ${day(f.valid_to)}`
                      : `since ${day(f.valid_from)}`}
                  </span>
                </div>
                {admin && !ended && fixing !== f.fact_id && (
                  <button className="link en-fix" onClick={() => setFixing(f.fact_id)}>
                    Correct this
                  </button>
                )}
              </div>
            </div>
          );
        })}
      </div>
      {shown.length < facts.length && (
        <button className="btn" onClick={() => setAll(true)}>
          Show all {facts.length.toLocaleString()}
        </button>
      )}
    </>
  );
}

const errorText = (e: unknown) =>
  e instanceof ApiError && e.status === 404
    ? "It isn't here, or you can't read it."
    : e instanceof Error
      ? e.message
      : String(e);

/**
 * "Correct this": the admin states the right value through Review's "Add a fact" (`add_fact`), prefilled with the
 * fact's entity and attribute, then the conflict check runs so the two values show in dispute, to keep one. An
 * attribute the schema closes is corrected by picking one of its values.
 */
function Correct({
  f,
  values,
  open,
  sel,
  onDone,
  onAdded,
  onCancel,
}: {
  f: EntityFact;
  /** The values a closed enum takes; free text when undefined. */
  values?: string[];
  /** Everyone may read the fact: a correction is read by everyone, so say so when it isn't. */
  open: boolean;
  sel: Selection;
  /** The correction is on main and in dispute with `f` (the two facts), or may be once a failed check lands. */
  onDone: (disputed: [string, string]) => void;
  /** On main beside `f`: the attribute holds more than one value. The form stays to say so. */
  onAdded: () => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(f.value);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<{ ok: boolean; text: string } | null>(null);
  const ready = value.trim() && value.trim() !== f.value;
  return (
    <form
      className="en-correct"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (!ready || busy) return;
        setBusy(true);
        review
          .addFact(sel, {
            entity: f.entity,
            predicate: f.predicate,
            value: value.trim(),
            note: note.trim() || undefined,
          })
          .then(async (r) => {
            const published = !r.review || r.review.status === "published";
            if (!published) {
              setSaid({ ok: true, text: "Added; it waits for the entity's owners to approve it." });
              setBusy(false);
              return;
            }
            const pair: [string, string] = [f.fact_id, r.fact_id];
            const check = await review.checkConflicts(sel).catch(() => null);
            // The check answers the first page of all it recorded across the memory: where many await it, this pair
            // can be past that page, so the thing's own conflicts say. A check that failed may still land.
            const disputed =
              !check ||
              check.conflicts.some((c) => pair.every((id) => c.facts.includes(id))) ||
              (await review.conflicts(sel, { entity: f.entity, limit: 200 }).then(
                (p) => p.conflicts.some((c) => pair.every((id) => c.facts.some((x) => x.fact_id === id))),
                () => true,
              ));
            setBusy(false);
            if (disputed) onDone(pair);
            else {
              setSaid({
                ok: true,
                text: "Added. This attribute holds more than one value, so both stay true.",
              });
              onAdded();
            }
          })
          .catch((e: unknown) => {
            setSaid({ ok: false, text: errorText(e) });
            setBusy(false);
          });
      }}
    >
      <label className="rv-label">
        <span>Correct value of {humanize(f.predicate).toLowerCase()}</span>
        <ValueField
          values={values}
          value={value}
          autoFocus
          onChange={(v) => {
            setValue(v);
            setSaid(null);
          }}
        />
      </label>
      <label className="rv-label">
        <span>
          Where it was said <em>optional</em>
        </span>
        <input
          className="field"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="e.g. confirmed on the renewal call"
          maxLength={2000}
        />
      </label>
      <div className="en-correct-row">
        <button className="btn btn-primary" disabled={busy || !ready}>
          {busy ? "Adding…" : "Add the correction"}
        </button>
        <button type="button" className="btn" onClick={onCancel}>
          Cancel
        </button>
      </div>
      <p className={`rv-small ${said && !said.ok ? "error-text" : "muted"}`}>
        {said?.text ??
          `Stated as you, beside the current value: both show under In dispute until one is kept.${
            open ? "" : " Your correction is readable by everyone in this memory, unlike this fact."
          }`}
      </p>
    </form>
  );
}

/** Conflicts on this entity the reader may see, each settled by keeping one side (`resolve_conflict`). */
export function Disputes({
  conflicts,
  page,
  sel,
  onFocus,
  onChanged,
}: {
  conflicts: Conflict[];
  page: EntityPage;
  sel: Selection;
  onFocus: (f: Focus) => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  if (!conflicts.length) return null;
  const eps = page.episodes ?? {};
  const side = (c: Conflict, s: ConflictSide) => {
    const e = s.episode_id ? eps[s.episode_id] : undefined;
    return (
      <div className="en-side" key={s.fact_id}>
        <div className="fact-value">{s.value}</div>
        {s.quote && s.quote !== s.value && <div className="quote">“{s.quote}”</div>}
        <div className="en-prov">
          {s.episode_id && (
            <button
              className="chip"
              title={e?.source ?? s.source ?? undefined}
              onClick={() => onFocus({ kind: "episode", id: s.episode_id! })}
            >
              <Icon name={e ? sourceKind(e.source).kind : "episode"} size={12} />
              {e ? e.title || titleOf(e.source) : s.source ? titleOf(s.source) : "Episode"}
            </button>
          )}
          <span>since {s.since_day}</span>
          {c.can_resolve && (
            <button
              className="btn en-keep"
              disabled={busy != null}
              onClick={() => {
                setBusy(s.fact_id);
                setErr(null);
                review
                  .resolve(sel, [c.facts[0].fact_id, c.facts[1].fact_id], { keep: s.fact_id })
                  .then(
                    () => {
                      setBusy(null);
                      onChanged();
                    },
                    (e: unknown) => {
                      setBusy(null);
                      setErr(errorText(e));
                    },
                  );
              }}
            >
              {busy === s.fact_id ? "Keeping…" : "Keep this"}
            </button>
          )}
        </div>
      </div>
    );
  };
  return (
    <>
      <div className="h-section">
        <Icon name="alert" size={14} /> In dispute <span className="count">{conflicts.length}</span>
        <a className="link en-head-link" href="#/review?tab=conflicts">
          Resolve in Review
        </a>
      </div>
      <div className="card en-disputes">
        {conflicts.map((c) => (
          <div key={`${c.facts[0].fact_id}/${c.facts[1].fact_id}`} className="en-dispute">
            <div className="fact-key" title={c.predicate}>
              {humanize(c.predicate)}
              {!c.can_resolve && (
                <span className="muted rv-small">
                  {c.owner ? `${c.owner} settles it` : "an org admin settles it"}
                </span>
              )}
            </div>
            <div className="en-sides">
              {side(c, c.facts[0])}
              {side(c, c.facts[1])}
            </div>
          </div>
        ))}
        {err && (
          <p className="error-text rv-small en-err" role="alert">
            {err}
          </p>
        )}
      </div>
    </>
  );
}

/** The newest decisions an entity's page shows; the rest are in Review. */
const DREAM_SHOWN = 20;

/**
 * What the dream decided about this entity (an ending, a link, a name), each with Undo, or Apply and Dismiss while
 * it waits: Review's own card. An org admin's, as the decisions are; nothing for anyone else, from a server without
 * them, or where none touches it.
 */
export function DreamDecisions({
  entity,
  sel,
  admin,
  me,
  onFocus,
  onChanged,
}: {
  entity: string;
  sel: Selection;
  admin: boolean;
  me?: string;
  onFocus: (f: Focus) => void;
  /** A card applied, undid or dismissed one: the entity's facts changed. */
  onChanged: () => void;
}) {
  const [list, setList] = useState<{ decisions: DreamDecision[]; total: number } | null>(null);
  useEffect(() => {
    if (!admin) return;
    let live = true;
    setList(null);
    review.dream(sel, { entity, limit: DREAM_SHOWN }).then(
      (r) => {
        // A server before the `entity` filter answers every decision: keep those touching this one.
        const decisions = r.decisions.filter((d) => d.facts.some((f) => f.entity === entity));
        if (live) setList({ decisions, total: r.total ?? decisions.length });
      },
      () => live && setList(null),
    );
    return () => {
      live = false;
    };
  }, [admin, entity, sel]);
  if (!admin || !list?.decisions.length) return null;
  return (
    <>
      <div className="h-section">
        <Icon name="moon" size={14} /> What the dream decided{" "}
        <span className="count">{list.total}</span>
        <a className="link en-head-link" href="#/review?tab=done">
          {list.total > list.decisions.length ? `All ${list.total} in Review` : "Open in Review"}
        </a>
      </div>
      <div className="rv-dreams en-dreams">
        {list.decisions.map((d) => (
          <DreamCard
            key={d.id}
            sel={sel}
            d={d}
            me={me}
            onChanged={onChanged}
            onFocus={onFocus}
          />
        ))}
      </div>
    </>
  );
}

const MAX_NEIGHBOURS = 24;

/** The entities this one names and those naming it: a small drawing (the Graph's layout), and the same as chips. */
export function Connections({
  entity,
  links,
  onFocus,
}: {
  entity: string;
  links: { out: Link[]; in: Link[]; more?: boolean };
  onFocus: (f: Focus) => void;
}) {
  const W = 640,
    H = 220;
  const { nodes, edges, view } = useMemo(() => {
    const ids = [...new Set([...links.out, ...links.in].map((l) => l.entity))].slice(
      0,
      MAX_NEIGHBOURS,
    );
    const shown = new Set(ids);
    const raw: GraphNode[] = [entity, ...ids].map((id) => ({
      id,
      type: typeOf(id),
      owner: null,
      facts: id === entity ? 12 : 2,
      pending: [],
    }));
    const edges: GraphEdge[] = [
      ...links.out
        .filter((l) => shown.has(l.entity))
        .map((l) => ({ from: entity, to: l.entity, predicate: l.predicate, pending: null })),
      ...links.in
        .filter((l) => shown.has(l.entity))
        .map((l) => ({ from: l.entity, to: entity, predicate: l.predicate, pending: null })),
    ];
    const nodes: Sim[] = seed(raw, W, H);
    const c = nodes[0]!;
    c.x = W / 2;
    c.y = H / 2;
    c.pinned = true;
    for (let i = 0; i < 260; i++) step(nodes, edges, W, H, Math.max(0.03, 1 - i / 260));
    return { nodes, edges, view: fitView(nodes, W, H + 24) };
  }, [entity, links]);
  const types = [...new Set(nodes.map((n) => n.type))];
  const pos = new Map(nodes.map((n) => [n.id, n]));
  const group = (ls: Link[]) => {
    const m = new Map<string, string[]>();
    for (const l of ls) m.set(l.predicate, [...(m.get(l.predicate) ?? []), l.entity]);
    return [...m];
  };
  const [lit, setLit] = useState<string | null>(null);
  useEffect(() => setLit(null), [entity]);
  return (
    <>
      <div className="card en-graph">
        <svg
          viewBox={`0 0 ${W} ${H}`}
          role="img"
          aria-label={`${nodes.length - 1} connected entities`}
        >
          <g transform={`translate(${view.x} ${view.y}) scale(${view.k})`}>
            {edges.map((e, i) => {
              const a = pos.get(e.from),
                b = pos.get(e.to);
              if (!a || !b) return null;
              const on = lit != null && (e.from === lit || e.to === lit);
              return (
                <g key={i} className={`gr-edge ${on ? "gr-lit" : ""}`}>
                  <line x1={a.x} y1={a.y} x2={b.x} y2={b.y} />
                  {on && (
                    <text
                      x={(a.x + b.x) / 2}
                      y={(a.y + b.y) / 2 - 5 / view.k}
                      textAnchor="middle"
                      className="gr-elabel"
                      style={{ fontSize: 10.5 / view.k }}
                    >
                      {humanize(e.predicate)}
                    </text>
                  )}
                </g>
              );
            })}
            {nodes.map((n) => {
              const centre = n.id === entity;
              const name = entityName(n.id).name;
              return (
                <g
                  key={n.id}
                  className={`gr-node ${centre ? "gr-sel" : ""}`}
                  style={
                    { "--gt": `var(--gt-${types.indexOf(n.type) % 8})` } as React.CSSProperties
                  }
                  transform={`translate(${n.x} ${n.y})`}
                  role={centre ? undefined : "button"}
                  tabIndex={centre ? undefined : 0}
                  aria-label={centre ? undefined : `Open ${n.id}`}
                  onClick={() => !centre && onFocus(entityFocus(n.id))}
                  onKeyDown={(e) => {
                    if (!centre && (e.key === "Enter" || e.key === " ")) {
                      e.preventDefault();
                      onFocus(entityFocus(n.id));
                    }
                  }}
                  onPointerEnter={() => setLit(n.id)}
                  onPointerLeave={() => setLit((x) => (x === n.id ? null : x))}
                >
                  <title>{n.id}</title>
                  <circle className="gr-dot" r={n.r} />
                  <text
                    className="gr-label"
                    y={n.r + 14 / view.k}
                    style={{ fontSize: 11.5 / view.k }}
                    textAnchor="middle"
                  >
                    {Array.from(name).length > 26
                      ? Array.from(name).slice(0, 25).join("") + "…"
                      : name}
                  </text>
                </g>
              );
            })}
          </g>
        </svg>
      </div>
      <dl className="meta en-links">
        {group(links.out).map(([p, es]) => (
          <div key={`o${p}`} className="en-link-row">
            <dt title="This entity names these">
              <span className="muted">→</span> {humanize(p)}
            </dt>
            <dd>
              {es.map((e) => (
                <EntityLink key={e} entity={e} onFocus={onFocus} />
              ))}
            </dd>
          </div>
        ))}
        {group(links.in).map(([p, es]) => (
          <div key={`i${p}`} className="en-link-row">
            <dt title="These entities name this one">
              <span className="muted">←</span> {humanize(p)}
            </dt>
            <dd>
              {es.map((e) => (
                <EntityLink key={e} entity={e} onFocus={onFocus} />
              ))}
            </dd>
          </div>
        ))}
      </dl>
      <p className="rv-small muted en-graph-note">
        {links.more || nodes.length - 1 >= MAX_NEIGHBOURS ? "Showing the first connections. " : ""}
        <button className="link" onClick={() => onFocus({ kind: "graph", entity })}>
          Open in the graph
        </button>
      </p>
    </>
  );
}
