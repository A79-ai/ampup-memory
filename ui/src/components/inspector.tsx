"use client";

import { useEffect, useState } from "react";
import { ApiError, browse, search, when } from "./api";
import { Icon, label } from "./tree";
import type { Aip, EntityView, EpisodeView, Fact, Focus, Listing, Selection, SkillView } from "./types";

type State<T> = { status: "loading" } | { status: "error"; message: string } | { status: "ok"; data: T };

/** Loads once per mount (each pane is keyed by what it shows). */
function useLoad<T>(load: () => Promise<T>, deps: unknown[]): State<T> {
  const [s, setS] = useState<State<T>>({ status: "loading" });
  useEffect(() => {
    let live = true;
    load().then(
      (data) => live && setS({ status: "ok", data }),
      (e) =>
        live &&
        setS({
          status: "error",
          message: e instanceof ApiError && e.status === 404 ? "Nothing here, or you can't read it." : e.message,
        }),
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return s;
}

/** The right-hand pane: whatever is in focus, read in full. */
export function Inspector({
  focus,
  sel,
  onFocus,
}: {
  focus: Focus | null;
  sel: Selection;
  onFocus: (f: Focus) => void;
}) {
  if (!focus)
    return (
      <div className="empty">
        <p>Pick something in the tree.</p>
        <p className="muted">
          <b>sources/</b> holds episodes by where they came from; <b>entities/</b> holds facts by what they
          are about; <b>skills/</b> holds the skills compiled from the scope&apos;s documents (a build may
          compile them on a branch of its own: pick it to see them).
        </p>
      </div>
    );
  switch (focus.kind) {
    case "folder":
      return <FolderView key={JSON.stringify(focus.path)} path={focus.path} sel={sel} onFocus={onFocus} />;
    case "entity":
      return <EntityPane key={JSON.stringify(focus.path)} path={focus.path} sel={sel} onFocus={onFocus} />;
    case "skill":
      return <SkillPane key={JSON.stringify(focus.path)} path={focus.path} sel={sel} onFocus={onFocus} />;
    case "episode":
      return <EpisodePane key={focus.id} id={focus.id} sel={sel} onFocus={onFocus} />;
    case "search":
      return <SearchPane key={focus.query} query={focus.query} sel={sel} onFocus={onFocus} />;
    case "tools":
      return null;
  }
}

function Loading<T>({ s, children }: { s: State<T>; children: (d: T) => React.ReactNode }) {
  if (s.status === "loading") return <div className="muted pad">Loading…</div>;
  if (s.status === "error") return <div className="error pad">{s.message}</div>;
  return <>{children(s.data)}</>;
}

function FolderView({ path, sel, onFocus }: { path: string[]; sel: Selection; onFocus: (f: Focus) => void }) {
  const s = useLoad(() => browse<Listing>({ op: "ls", path, limit: 500 }, sel), [path, sel]);
  const open = (c: Listing["children"][number]): Focus =>
    c.kind === "episode"
      ? { kind: "episode", id: c.episode_id! }
      : c.kind === "skill" || c.kind === "skill_index"
        ? { kind: "skill", path: c.path }
        : { kind: c.kind, path: c.path };
  return (
    <Loading s={s}>
      {(l) => (
        <section>
          <h2 className="title">
            <Icon kind="folder" /> {path.at(-1) ?? "memory"}
          </h2>
          <p className="muted">
            {l.children.length}
            {l.next ? "+" : ""} {l.children.length === 1 ? "item" : "items"}
          </p>
          {path[0] === "skills" && path.length === 2 && (
            <p>
              <button className="link" onClick={() => onFocus({ kind: "skill", path: [...path, "index"] })}>
                <Icon kind="skill_index" /> The routing index
              </button>{" "}
              <span className="muted">(one line per skill: when to use it)</span>
            </p>
          )}
          {l.children.some((c) => c.kind === "skill") ? (
            <table className="tbl skills">
              <thead>
                <tr>
                  <th>Skill</th>
                  <th>When</th>
                </tr>
              </thead>
              <tbody>
                {l.children.map((c) => (
                  <tr key={JSON.stringify(c.path)} className="clickable" onClick={() => onFocus(open(c))}>
                    <td className="skill-name">
                      <Icon kind={c.kind} /> <span className="mono">{c.name}</span>
                    </td>
                    <td>{c.kind === "skill_index" ? <span className="muted">the routing index: {c.count} skills</span> : c.when}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th className="num">Count</th>
                <th>Time</th>
              </tr>
            </thead>
            <tbody>
              {l.children.map((c) => (
                <tr
                  key={JSON.stringify(c.path) + (c.episode_id ?? "")}
                  className="clickable"
                  onClick={() => onFocus(open(c))}
                >
                  <td>
                    <Icon kind={c.kind} /> <span className="mono">{c.name === c.episode_id ? `${c.name.slice(0, 16)}…` : label(c)}</span>
                  </td>
                  <td className="muted">{c.kind}</td>
                  <td className="num">{c.count ?? ""}</td>
                  <td className="muted">{when(c.occurred_at ?? c.latest)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          )}
          {l.next && <p className="muted">More in the tree (showing the first {l.children.length}).</p>}
        </section>
      )}
    </Loading>
  );
}

function EntityPane({ path, sel, onFocus }: { path: string[]; sel: Selection; onFocus: (f: Focus) => void }) {
  const s = useLoad(() => browse<EntityView>({ op: "ls", path }, sel), [path, sel]);
  return (
    <Loading s={s}>
      {(e) => (
        <section>
          <h2 className="title">
            <Icon kind="entity" /> <span className="mono">{e.entity}</span>
          </h2>
          <p className="muted">
            {e.facts.length} {e.facts.length === 1 ? "fact" : "facts"}, current and past
          </p>
          <Facts facts={e.facts} onFocus={onFocus} />
        </section>
      )}
    </Loading>
  );
}

function EpisodePane({ id, sel, onFocus }: { id: string; sel: Selection; onFocus: (f: Focus) => void }) {
  const s = useLoad(() => browse<EpisodeView>({ op: "episode", id }, sel), [id, sel]);
  return (
    <Loading s={s}>
      {(e) => (
        <section>
          <h2 className="title">
            <Icon kind="episode" /> <span className="mono">{e.source}</span>
          </h2>
          <dl className="meta">
            <dt>Occurred</dt>
            <dd>{when(e.occurred_at)}</dd>
            <dt>Episode</dt>
            <dd className="mono">{e.episode_id}</dd>
            <dt>In</dt>
            <dd>
              {e.path.map((seg, i) => (
                <span key={i}>
                  {i > 0 && <span className="muted"> / </span>}
                  {i < e.path.length - 1 ? (
                    <button className="link mono" onClick={() => onFocus({ kind: "folder", path: e.path.slice(0, i + 1) })}>
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
          <h3>Text{e.truncated ? " (first 256 KiB)" : ""}</h3>
          <pre className="body">{e.body}</pre>
          <h3>Facts drawn from it</h3>
          {e.facts.length ? <Facts facts={e.facts} onFocus={onFocus} hideEvidence={id} /> : <p className="muted">None.</p>}
        </section>
      )}
    </Loading>
  );
}

/** A compiled skill, its markdown then its AIP step graph; or a routing index. */
function SkillPane({ path, sel, onFocus }: { path: string[]; sel: Selection; onFocus: (f: Focus) => void }) {
  const s = useLoad(() => browse<SkillView>({ op: "ls", path }, sel), [path, sel]);
  return (
    <Loading s={s}>
      {(k) => (
        <section>
          <h2 className="title">
            <Icon kind={k.kind} /> <span className="mono">{k.name}</span>
          </h2>
          <dl className="meta">
            {k.goal && (
              <>
                <dt>Goal</dt>
                <dd>
                  <button className="link mono" onClick={() => onFocus({ kind: "folder", path: ["skills", k.goal!] })}>
                    {k.goal}
                  </button>
                </dd>
              </>
            )}
            {k.occurred_at != null && (
              <>
                <dt>Compiled</dt>
                <dd>{when(k.occurred_at)}</dd>
              </>
            )}
            {k.episode_id && (
              <>
                <dt>Version</dt>
                <dd>
                  <button className="chip mono" title={k.episode_id} onClick={() => onFocus({ kind: "episode", id: k.episode_id! })}>
                    {k.episode_id.slice(0, 10)}
                  </button>
                </dd>
              </>
            )}
          </dl>
          <Markdown text={k.markdown} goal={k.goal} onFocus={onFocus} />
          {k.kind === "skill" && (
            <>
              <h3>AIP step graph</h3>
              {k.aip == null ? (
                <p className="muted">No AIP graph yet.</p>
              ) : typeof k.aip === "string" ? (
                <pre className="body">{k.aip}</pre>
              ) : (
                <AipGraph aip={k.aip} />
              )}
            </>
          )}
        </section>
      )}
    </Loading>
  );
}

/** An AIP graph's purpose, triggers and steps with what each takes and gives, then the JSON itself. */
function AipGraph({ aip }: { aip: Aip }) {
  const names = (xs?: { name: string }[]) => (xs ?? []).map((x) => x.name);
  return (
    <div className="aip">
      {aip.purpose && <p>{aip.purpose}</p>}
      {(aip.trigger_when?.length ?? 0) > 0 && (
        <>
          <h4>Use when</h4>
          <ul>{aip.trigger_when!.map((t, i) => <li key={i}>{t}</li>)}</ul>
        </>
      )}
      {(aip.do_not_use_when?.length ?? 0) > 0 && (
        <>
          <h4>Don&apos;t use when</h4>
          <ul>{aip.do_not_use_when!.map((t, i) => <li key={i}>{t}</li>)}</ul>
        </>
      )}
      <ol className="aip-steps">
        {(aip.steps ?? []).map((st, i) => (
          <li key={i}>
            <div className="mono aip-step">{st.name}</div>
            {st.description && <div>{st.description}</div>}
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
          </li>
        ))}
      </ol>
      <details>
        <summary className="muted">JSON (AIP {aip.aip ?? ""})</summary>
        <pre className="body">{JSON.stringify(aip, null, 2)}</pre>
      </details>
    </div>
  );
}

/** The markdown the compiler writes (headings, **bold**, `code`, numbered and bulleted lists), as elements; a
 * skill named in `code` in an index links to it. */
function Markdown({ text, goal, onFocus }: { text: string; goal?: string; onFocus: (f: Focus) => void }) {
  // Where the skills an index names are: `skills/`, or, from a memstore before skills were per scope, the goal's.
  const base = goal ? ["skills", goal] : ["skills"];
  const inline = (s: string, at: number): React.ReactNode[] =>
    s.split(/(\*\*[^*]+\*\*|`[^`]+`)/).map((part, i) => {
      const k = `${at}-${i}`;
      if (part.startsWith("**") && part.endsWith("**") && part.length > 4) return <b key={k}>{part.slice(2, -2)}</b>;
      if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
        const code = part.slice(1, -1);
        const skill = /^[a-z0-9][a-z0-9~-]*$/.test(code) && code !== goal;
        return skill ? (
          <button key={k} className="link mono" onClick={() => onFocus({ kind: "skill", path: [...base, code] })}>
            {code}
          </button>
        ) : (
          <code key={k}>{code}</code>
        );
      }
      return part;
    });
  const out: React.ReactNode[] = [];
  let list: { ordered: boolean; items: React.ReactNode[] } | null = null;
  const flush = () => {
    if (!list) return;
    const items = list.items.map((it, i) => <li key={i}>{it}</li>);
    out.push(list.ordered ? <ol key={out.length}>{items}</ol> : <ul key={out.length}>{items}</ul>);
    list = null;
  };
  text.split("\n").forEach((line, n) => {
    const h = /^(#{1,4}) (.*)$/.exec(line);
    const li = /^(\d+\.|[-*]) (.*)$/.exec(line);
    if (li) {
      const ordered = /\d/.test(li[1]);
      if (list && list.ordered !== ordered) flush();
      list ??= { ordered, items: [] };
      list.items.push(inline(li[2], n));
      return;
    }
    flush();
    if (h) {
      const Tag = (["h3", "h3", "h4", "h4"] as const)[h[1].length - 1];
      out.push(<Tag key={out.length}>{inline(h[2], n)}</Tag>);
    } else if (line.trim()) {
      out.push(<p key={out.length}>{inline(line, n)}</p>);
    }
  });
  flush();
  return <div className="md">{out}</div>;
}

function SearchPane({ query, sel, onFocus }: { query: string; sel: Selection; onFocus: (f: Focus) => void }) {
  const s = useLoad(() => search<{ facts: Fact[] }>(query, sel), [query, sel]);
  return (
    <Loading s={s}>
      {(r) => (
        <section>
          <h2 className="title">Search: “{query}”</h2>
          <p className="muted">{r.facts.length} best-matching facts</p>
          {r.facts.length ? <Facts facts={r.facts} onFocus={onFocus} showEntity /> : <p className="muted">Nothing found.</p>}
        </section>
      )}
    </Loading>
  );
}

function EntityLink({ entity, onFocus }: { entity: string; onFocus: (f: Focus) => void }) {
  const i = entity.indexOf(":");
  const path = i < 0 ? ["entities", "_", entity] : ["entities", entity.slice(0, i), entity.slice(i + 1)];
  return (
    <button className="chip mono" onClick={() => onFocus({ kind: "entity", path })}>
      {entity}
    </button>
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
  return (
    <table className="tbl facts">
      <thead>
        <tr>
          {showAll && <th>Entity</th>}
          <th>Predicate</th>
          <th>Value</th>
          <th>Valid</th>
          <th>Evidence</th>
        </tr>
      </thead>
      <tbody>
        {facts.map((f) => {
          const ended = f.valid_to != null;
          return (
            <tr key={f.fact_id} className={ended ? "ended" : ""}>
              {showAll && (
                <td>
                  <EntityLink entity={f.entity} onFocus={onFocus} />
                </td>
              )}
              <td className="mono">{f.predicate}</td>
              <td>
                <div>{f.value}</div>
                {f.quote && <div className="quote">“{f.quote}”</div>}
              </td>
              <td className="muted nowrap">
                {when(f.valid_from)}
                <br />
                {ended ? `→ ${when(f.valid_to)}` : <span className="current">current</span>}
              </td>
              <td>
                {f.source_episodes
                  .filter((e) => e !== hideEvidence)
                  .map((e) => (
                    <button key={e} className="chip mono" title={e} onClick={() => onFocus({ kind: "episode", id: e })}>
                      {e.slice(0, 10)}
                    </button>
                  ))}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
