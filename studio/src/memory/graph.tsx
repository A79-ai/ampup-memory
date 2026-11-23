// Graph: the org's entities and how they connect, with the open proposals drawn over them as pending
// changes. A node per entity, sized by how much is recorded; an edge wherever a fact names another entity;
// a ring on an owned entity; a dashed accent line for a link that waits for an owner. Plain force layout.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { displayName, humanize } from "./format";
import { entityFocus, review, typeOf, type Graph } from "./review-api";
import { capGraph, GRAPH_MAX } from "./graphCap";
import { replaceView, useFlag, useOption, useSetOption } from "./hashOptions";
import { fitView, labelIds, seed, step, type Sim } from "./graphLayout";
import type { Focus, Selection } from "./types";
import { CopyId, EmptyState, ErrorState, Icon, Skeleton } from "./ui";
import "./review.css";

const reducedMotion = () =>
  typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;

export function GraphSection({
  sel,
  me,
  focus,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  /** An entity to land on selected. */
  focus?: string;
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team]);
  const [data, setData] = useState<Graph | null>(null);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    // The server cuts it to what a page draws; one before it did sends everything, cut here.
    review.graph(sel, { limit: GRAPH_MAX, entity: focus }).then(
      (g) =>
        live &&
        (setTotal(g.total ?? g.nodes.length),
        setData(capGraph(g, GRAPH_MAX, focus)),
        setError(null)),
      (e: Error) => live && setError(e.message),
    );
    return () => {
      live = false;
    };
  }, [key, n]);

  // Which types are hidden, the pending links and the search are kept in the link (`?hide=…&pending=0&q=…`).
  const [hidden, setHidden] = useSetOption("hide");
  const [showPending, setShowPending] = useFlag("pending", true);
  const [q, setQ] = useOption("q");
  const [selected, setSelected] = useState<string | null>(focus ?? null);
  // The node picked is the link's (`#/graph/<entity>`): a pick replaces it, and a link moved under the graph
  // (back, or pasted) picks its node.
  useEffect(() => setSelected(focus ?? null), [focus]);
  useEffect(() => replaceView(`#/graph/${encodeURIComponent(selected ?? "")}`), [selected]);
  const [hover, setHover] = useState<string | null>(null);

  const box = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 900, h: 560 });
  const nodesRef = useRef<Sim[]>([]);
  const [, bump] = useState(0);
  const alpha = useRef(1);
  const autoFit = useRef(true);
  const raf = useRef(0);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const drag = useRef<{ id: string | null; sx: number; sy: number; ox: number; oy: number } | null>(
    null,
  );
  const moved = useRef(false);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(([en]) => {
      if (!en) return;
      const w = Math.max(1, en.contentRect.width);
      setSize({ w, h: Math.max(380, Math.min(640, w * 0.62)) });
    });
    ro.observe(el);
    return () => ro.disconnect();
    // The box exists only once there is data to draw.
  }, [data]);

  const types = useMemo(() => {
    const m = new Map<string, number>();
    for (const x of data?.nodes ?? []) m.set(x.type, (m.get(x.type) ?? 0) + 1);
    return [...m].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  }, [data]);
  const typeIndex = (t: string) =>
    Math.max(
      0,
      types.findIndex(([k]) => k === t),
    ) % 8;

  const edges = useMemo(
    () =>
      (data?.edges ?? []).filter(
        (e) =>
          (showPending || !e.pending) && !hidden.has(typeOf(e.from)) && !hidden.has(typeOf(e.to)),
      ),
    [data, showPending, hidden],
  );
  const shownNodes = useMemo(
    () => (data?.nodes ?? []).filter((x) => !hidden.has(x.type)),
    [data, hidden],
  );

  // Seed once per dataset; settle most of it before the first paint, unfold the rest on screen.
  useEffect(() => {
    if (!data) return;
    nodesRef.current = seed(data.nodes, size.w, size.h);
    const pre = reducedMotion() ? 300 : 120;
    for (let i = 0; i < pre; i++)
      step(nodesRef.current, data.edges, size.w, size.h, Math.max(0.03, 1 - i / 300));
    alpha.current = reducedMotion() ? 0 : 0.2;
    autoFit.current = true;
    setView(fitView(nodesRef.current, size.w, size.h));
    bump((x) => x + 1);
    // Positions are kept across filter changes; only a new dataset or a resize reseeds.
  }, [data, size.w, size.h]);

  const run = useCallback(() => {
    cancelAnimationFrame(raf.current);
    const tick = () => {
      step(
        nodesRef.current.filter((x) => !hidden.has(x.type)),
        edges,
        size.w,
        size.h,
        alpha.current,
      );
      alpha.current *= 0.975;
      bump((x) => x + 1);
      if (alpha.current > 0.004) raf.current = requestAnimationFrame(tick);
      else if (autoFit.current)
        setView(
          fitView(
            nodesRef.current.filter((x) => !hidden.has(x.type)),
            size.w,
            size.h,
          ),
        );
    };
    raf.current = requestAnimationFrame(tick);
  }, [edges, hidden, size.w, size.h]);
  useEffect(() => {
    if (!data || alpha.current <= 0.004) return;
    run();
    return () => cancelAnimationFrame(raf.current);
  }, [data, run]);
  const reheat = (a = 0.3) => {
    if (reducedMotion()) {
      // Interactions settle immediately too; reduced motion is not just a first-load preference.
      for (let i = 0; i < 40; i++)
        step(
          nodesRef.current.filter((x) => !hidden.has(x.type)),
          edges,
          size.w,
          size.h,
          a,
        );
      bump((x) => x + 1);
      return;
    }
    alpha.current = Math.max(alpha.current, a);
    run();
  };

  const lit = hover ?? selected;
  const showEdgeLabels = edges.filter((e) => e.from === lit || e.to === lit).length <= 6;
  const near = useMemo(() => {
    if (!lit) return null;
    const s = new Set<string>([lit]);
    for (const e of edges) {
      if (e.from === lit) s.add(e.to);
      if (e.to === lit) s.add(e.from);
    }
    return s;
  }, [lit, edges]);
  // Each entity's name to show: the server's, never a bare opaque id (format.ts `displayName`).
  const names = useMemo(
    () => new Map((data?.nodes ?? []).map((x) => [x.id, displayName(x.id, x)])),
    [data],
  );
  const nameOf = (id: string) => names.get(id) ?? displayName(id);
  const match = q.trim().toLowerCase();
  // A search finds an entity by its name or its id.
  const found = (id: string) =>
    id.toLowerCase().includes(match) || nameOf(id).toLowerCase().includes(match);
  const pos = new Map(nodesRef.current.map((x) => [x.id, x]));
  const dim = (id: string) => (near && !near.has(id)) || (match !== "" && !found(id));

  const labels = labelIds(
    nodesRef.current.filter((x) => !hidden.has(x.type) && !dim(x.id)),
    view,
    size.w,
    size.h,
    nameOf,
    (id) => (id === lit ? 2 : match && found(id) ? 1 : 0),
  );

  const toLocal = (cx: number, cy: number) => {
    const r = box.current!.getBoundingClientRect();
    return {
      x: (cx - r.left - view.x) / view.k,
      y: (cy - r.top - view.y) / view.k,
      left: r.left,
      top: r.top,
    };
  };
  const fit = () => {
    autoFit.current = true;
    setView(
      fitView(
        nodesRef.current.filter((x) => !hidden.has(x.type)),
        size.w,
        size.h,
      ),
    );
  };
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    autoFit.current = false;
    const p = toLocal(e.clientX, e.clientY);
    const k = Math.min(3, Math.max(0.1, view.k * (e.deltaY < 0 ? 1.1 : 0.9)));
    setView({ k, x: e.clientX - p.left - p.x * k, y: e.clientY - p.top - p.y * k });
  };
  const down = (e: React.PointerEvent, id: string | null) => {
    e.stopPropagation();
    autoFit.current = false;
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    moved.current = false;
    const node = id ? nodesRef.current.find((x) => x.id === id) : null;
    drag.current = {
      id,
      sx: e.clientX,
      sy: e.clientY,
      ox: node ? node.x : view.x,
      oy: node ? node.y : view.y,
    };
    if (node) node.pinned = true;
  };
  const move = (e: React.PointerEvent) => {
    const d = drag.current;
    if (!d) return;
    const dx = e.clientX - d.sx,
      dy = e.clientY - d.sy;
    if (Math.abs(dx) + Math.abs(dy) > 3) moved.current = true;
    if (d.id) {
      const node = nodesRef.current.find((x) => x.id === d.id);
      if (node) {
        node.x = d.ox + dx / view.k;
        node.y = d.oy + dy / view.k;
        reheat(0.12);
      }
    } else setView((v) => ({ ...v, x: d.ox + dx, y: d.oy + dy }));
  };
  const up = () => {
    const d = drag.current;
    if (d?.id) {
      const node = nodesRef.current.find((x) => x.id === d.id);
      if (node) node.pinned = false;
    }
    drag.current = null;
  };

  if (error) {
    const old = /unknown op|not supported|unexpected op/i.test(error);
    return old ? (
      <EmptyState icon="schema" title="The graph needs the next engine release">
        This server doesn't answer `entity_graph` yet.
      </EmptyState>
    ) : (
      <ErrorState message={error} onRetry={() => setN((x) => x + 1)} />
    );
  }

  const sel_ = selected ? (data?.nodes.find((x) => x.id === selected) ?? null) : null;
  const selEdges = selected ? edges.filter((e) => e.from === selected || e.to === selected) : [];

  return (
    <section className="gr">
      <div className="head">
        <span className="kind k-entity">
          <Icon name="schema" size={22} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Graph</div>
          <h1 className="title">What the memory holds, and how it connects</h1>
        </div>
        <div className="gr-tools">
          <label className="field gr-search">
            <Icon name="search" size={14} />
            <input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Find an entity"
              aria-label="Find an entity"
              spellCheck={false}
            />
          </label>
          <button className="btn" onClick={fit} title="Fit all visible entities">
            Fit
          </button>
        </div>
      </div>
      <p className="lede">
        Every entity, linked where one fact names another. Owned entities carry a ring; a dashed
        line is a change that waits for its owner and hasn't reached main.
        {data && total > data.nodes.length && (
          <>
            {" "}
            <b>
              Showing the {data.nodes.length.toLocaleString()} with the most facts, of{" "}
              {total.toLocaleString()} nodes
            </b>
            , including notes and linked records (owned entities and ones with changes waiting
            always shown). Explorer counts entities with any fact, ended ones included, and no
            notes, so its total differs. Open any other from Explorer or Search.
          </>
        )}
      </p>

      {!data ? (
        <Skeleton rows={8} title={false} />
      ) : data.nodes.length === 0 ? (
        <EmptyState icon="entity" title="Nothing to draw yet">
          The graph fills in as the memory learns about entities.
        </EmptyState>
      ) : (
        <div className="rv-layout">
          <div className="rv-main">
            <div className="gr-legend" role="group" aria-label="Entity types">
              {types.map(([t, count]) => (
                <button
                  key={t}
                  className={`chip gr-type ${hidden.has(t) ? "gr-off" : ""}`}
                  style={{ "--gt": `var(--gt-${typeIndex(t)})` } as React.CSSProperties}
                  aria-pressed={!hidden.has(t)}
                  onClick={() => {
                    const s = new Set(hidden);
                    if (s.has(t)) s.delete(t);
                    else s.add(t);
                    setHidden(s);
                    reheat(0.2);
                  }}
                >
                  <i /> {humanize(t)} <span className="muted">{count}</span>
                </button>
              ))}
              <label className={`chip gr-toggle ${showPending ? "" : "gr-off"}`}>
                <input
                  type="checkbox"
                  checked={showPending}
                  onChange={(e) => setShowPending(e.target.checked)}
                />
                <i className="gr-dash" /> pending changes
              </label>
            </div>
            <div
              className="gr-box card"
              ref={box}
              onWheel={onWheel}
              onPointerDown={(e) => down(e, null)}
              onPointerMove={move}
              onPointerUp={up}
              onPointerCancel={up}
              onDoubleClick={fit}
            >
              <svg
                width={size.w}
                height={size.h}
                viewBox={`0 0 ${size.w} ${size.h}`}
                role="img"
                aria-label={`${shownNodes.length} entities and ${edges.length} links`}
              >
                <g transform={`translate(${view.x} ${view.y}) scale(${view.k})`}>
                  <g>
                    {edges.map((e, i) => {
                      const a = pos.get(e.from),
                        b = pos.get(e.to);
                      if (!a || !b) return null;
                      const on = lit && (e.from === lit || e.to === lit);
                      return (
                        <g
                          key={i}
                          className={`gr-edge ${e.pending ? "gr-pending" : ""} ${on ? "gr-lit" : ""} ${near && !on ? "gr-dim" : ""}`}
                        >
                          <line x1={a.x} y1={a.y} x2={b.x} y2={b.y} />
                          {on && showEdgeLabels && (
                            <text
                              x={(a.x + b.x) / 2}
                              y={(a.y + b.y) / 2 - 5 / view.k}
                              textAnchor="middle"
                              className="gr-elabel"
                              style={{ fontSize: 10.5 / view.k }}
                            >
                              {e.predicate}
                            </text>
                          )}
                        </g>
                      );
                    })}
                  </g>
                  <g>
                    {[...shownNodes]
                      .sort((a, b) => Number(a.id === lit) - Number(b.id === lit))
                      .map((x) => {
                        const p = pos.get(x.id);
                        if (!p) return null;
                        const isSel = selected === x.id;
                        const name = nameOf(x.id);
                        return (
                          <g
                            key={x.id}
                            className={`gr-node ${isSel ? "gr-sel" : ""} ${dim(x.id) ? "gr-dim" : ""}`}
                            style={
                              { "--gt": `var(--gt-${typeIndex(x.type)})` } as React.CSSProperties
                            }
                            transform={`translate(${p.x} ${p.y})`}
                            tabIndex={0}
                            role="button"
                            aria-label={`${name === x.id ? name : `${name} (${x.id})`}${x.owner ? `, owned by ${x.owner}` : ""}${x.pending.length ? `, ${x.pending.length} change waiting` : ""}`}
                            aria-pressed={isSel}
                            onPointerDown={(e) => down(e, x.id)}
                            onClick={() =>
                              !moved.current && setSelected((s) => (s === x.id ? null : x.id))
                            }
                            onKeyDown={(e) => {
                              if (e.key === "Enter" || e.key === " ") {
                                e.preventDefault();
                                setSelected((s) => (s === x.id ? null : x.id));
                              }
                            }}
                            onFocus={() => setHover(x.id)}
                            onBlur={() => setHover((h) => (h === x.id ? null : h))}
                            onPointerEnter={() => setHover(x.id)}
                            onPointerLeave={() => setHover((h) => (h === x.id ? null : h))}
                          >
                            <title>{name === x.id ? name : `${name}\n${x.id}`}</title>
                            {x.pending.length > 0 && showPending && (
                              <circle className="gr-halo" r={p.r + 6} />
                            )}
                            {x.owner && <circle className="gr-ring" r={p.r + 3} />}
                            <circle className="gr-dot" r={p.r} />
                            {labels.has(x.id) && (
                              <text
                                className="gr-label"
                                y={p.r + 14 / view.k}
                                style={{ fontSize: 11.5 / view.k }}
                                textAnchor="middle"
                              >
                                {Array.from(name).length > 32
                                  ? Array.from(name).slice(0, 31).join("") + "…"
                                  : name}
                              </text>
                            )}
                          </g>
                        );
                      })}
                  </g>
                </g>
              </svg>
              <p className="gr-count muted" aria-hidden>
                {shownNodes.length} entities · {edges.length} links · drag to move, scroll to zoom
              </p>
            </div>
          </div>

          <aside className="rv-rail" aria-label="Selected entity">
            {sel_ ? (
              <div className="card card-pad gr-sel-card">
                <div className="gr-sel-head">
                  <span
                    className="chip gr-type"
                    style={{ "--gt": `var(--gt-${typeIndex(sel_.type)})` } as React.CSSProperties}
                  >
                    <i /> {humanize(sel_.type)}
                  </span>
                  <button
                    className="icon-btn"
                    aria-label="Clear selection"
                    onClick={() => setSelected(null)}
                  >
                    <Icon name="close" size={14} />
                  </button>
                </div>
                <h2 className="gr-sel-title">{nameOf(sel_.id)}</h2>
                <CopyId id={sel_.id} n={40} />
                <dl className="gr-kv">
                  <dt>Owner</dt>
                  <dd>
                    {sel_.owner ? (
                      sel_.owner === me ? (
                        <b>you</b>
                      ) : (
                        sel_.owner
                      )
                    ) : (
                      <span className="muted">none, publishes without review</span>
                    )}
                  </dd>
                  <dt>Facts</dt>
                  <dd>{sel_.facts}</dd>
                  <dt>Waiting</dt>
                  <dd>
                    {sel_.pending.length === 0 ? (
                      <span className="muted">nothing</span>
                    ) : (
                      sel_.pending.map((id) => (
                        <button
                          key={id}
                          className="link mono"
                          onClick={() => onFocus({ kind: "review", proposal: id })}
                        >
                          {id}
                        </button>
                      ))
                    )}
                  </dd>
                </dl>
                {selEdges.length > 0 && (
                  <>
                    <div className="h-section">Links</div>
                    <ul className="gr-links">
                      {selEdges.map((e, i) => {
                        const other = e.from === sel_.id ? e.to : e.from;
                        return (
                          <li key={i} className={e.pending ? "gr-pending" : ""}>
                            <span className="mono muted">
                              {e.from === sel_.id ? "→" : "←"} {e.predicate}
                            </span>
                            <button
                              className="link"
                              title={other}
                              onClick={() => setSelected(other)}
                            >
                              {nameOf(other)}
                            </button>
                            {e.pending && <span className="badge badge-accent">waiting</span>}
                          </li>
                        );
                      })}
                    </ul>
                  </>
                )}
                <button
                  className="btn btn-primary gr-open"
                  onClick={() => onFocus(entityFocus(sel_.id))}
                >
                  Open entity <Icon name="arrow" size={13} />
                </button>
              </div>
            ) : (
              <div className="card card-pad">
                <div className="h-section">Reading the graph</div>
                <ul className="gr-how">
                  <li>
                    <i className="gr-how-dot" /> A node is an entity; its size is how much is
                    recorded about it.
                  </li>
                  <li>
                    <i className="gr-how-ring" /> A ring means it has an owner: changes wait for
                    their approval.
                  </li>
                  <li>
                    <i className="gr-how-dash" /> A dashed line is a proposed link that hasn't
                    reached main.
                  </li>
                  <li>
                    Click a node to see what is recorded and what is waiting. Drag to rearrange.
                  </li>
                </ul>
                {data.nodes.some((x) => x.pending.length) && (
                  <p className="muted rv-small">
                    {data.nodes.filter((x) => x.pending.length).length} entities have a change
                    waiting.{" "}
                    <button className="link" onClick={() => onFocus({ kind: "review" })}>
                      See the inbox
                    </button>
                  </p>
                )}
              </div>
            )}
          </aside>
        </div>
      )}
    </section>
  );
}
