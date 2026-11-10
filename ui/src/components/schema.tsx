"use client";

import { useEffect, useMemo, useState } from "react";
import { browse } from "./api";
import type { ExtractSection, PredicateDef, SchemaDoc, SchemaView, Selection } from "./types";

const prefixesOf = (s: SchemaDoc) => s.extract?.sources?.prefixes ?? [];

/** What `outcomes` asks for, in words. */
function outcomesText(o: ExtractSection["outcomes"]): string {
  if (Array.isArray(o)) return o.length ? `only from ${o.join(", ")}` : "off";
  return o ? "from every source" : "off";
}

/** The prefixes version `i` of the history names that the one before it didn't, and the ones it dropped. */
function prefixDiff(history: SchemaDoc[], i: number): { added: string[]; removed: string[] } {
  const now = prefixesOf(history[i]);
  const was = i > 0 ? prefixesOf(history[i - 1]) : [];
  return { added: now.filter((p) => !was.includes(p)), removed: was.filter((p) => !now.includes(p)) };
}

/** What bounds a predicate's value: its enum, the types a ref may name, its unit. */
function predicateNote(p: PredicateDef): string {
  const bits: string[] = [];
  if (p.values?.length) bits.push(p.values.join(" | "));
  if (p.to?.length) bits.push(`→ ${p.to.join(", ")}`);
  if (p.unit) bits.push(p.unit);
  return bits.join(" · ");
}

function Copy({ text }: { text: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className="link"
      onClick={() => {
        navigator.clipboard?.writeText(text).then(
          () => {
            setDone(true);
            setTimeout(() => setDone(false), 1500);
          },
          () => {},
        );
      }}
    >
      {done ? "copied" : "copy JSON"}
    </button>
  );
}

/** The org's schema: what it types memory with, what its `extract` section reads, and how it changed. */
export function Schema({ sel }: { sel: Selection }) {
  const [view, setView] = useState<{ key: string; v?: SchemaView; error?: string } | null>(null);
  const [raw, setRaw] = useState(false);
  const key = JSON.stringify(sel);

  useEffect(() => {
    let live = true;
    browse<SchemaView>({ op: "schema" }, sel).then(
      (v) => live && setView({ key, v }),
      (e) => live && setView({ key, error: e.message }),
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const current = view?.key === key ? view : null;
  const schema = current?.v?.schema ?? null;
  const history = useMemo(() => current?.v?.history ?? [], [current]);
  const text = useMemo(() => JSON.stringify(schema, null, 2), [schema]);

  if (!current) return <main className="schema-main muted pad">Loading the schema…</main>;
  if (current.error) {
    const old = /unknown op/i.test(current.error);
    return (
      <main className="schema-main pad error">
        {old ? "This memstore predates schema browsing: redeploy it from main." : current.error}
      </main>
    );
  }
  if (!schema) {
    return (
      <main className="schema-main">
        <div className="schema-wrap">
          <h2 className="schema-name">No schema</h2>
          <p className="muted">
            This memory is untyped: nothing is read with a schema, and facts take whatever predicates they come with.
            An admin sets one with <span className="mono">memstore schema set</span>.
          </p>
        </div>
      </main>
    );
  }

  const x = schema.extract;
  const types = Object.entries(schema.types);
  const predicates = types.reduce((n, [, t]) => n + Object.keys(t.predicates).length, 0);

  return (
    <main className="schema-main">
      <div className="schema-wrap">
        <header className="schema-head">
          <div>
            <div className="schema-kicker mono">schema · {sel.branch ?? "main"}</div>
            <h2 className="schema-name">
              {schema.name}
              <span className="badge badge-on mono">v{history.length || schema.version}</span>
              <span className="badge mono" title={schema.mode === "open" ? "Undeclared predicates are stored as they come" : "A fact's predicate must be declared for its type"}>
                {schema.mode ?? "strict"}
              </span>
            </h2>
            {schema.description && <p className="schema-desc">{schema.description}</p>}
          </div>
          <div className="schema-stats">
            <div>
              <b>{types.length}</b>
              <span>types</span>
            </div>
            <div>
              <b>{predicates}</b>
              <span>predicates</span>
            </div>
            <div>
              <b>{prefixesOf(schema).length || (x ? "all" : 0)}</b>
              <span>sources read</span>
            </div>
          </div>
        </header>

        <section className="card">
          <h3 className="card-title">
            Reads <span className="muted">what extraction takes from episodes</span>
          </h3>
          {!x ? (
            <p className="muted">
              No <span className="mono">extract</span> section: this schema reads nothing. Episodes are extracted
              goal-blind where that is on for the org.
            </p>
          ) : (
            <>
              <div className="kv">
                <div className="k">Source prefixes</div>
                <div className="v">
                  {prefixesOf(schema).length ? (
                    prefixesOf(schema).map((p) => (
                      <span key={p} className="tag mono">
                        {p}
                      </span>
                    ))
                  ) : (
                    <span className="muted">every source</span>
                  )}
                </div>
                {x.sources?.about?.length ? (
                  <>
                    <div className="k">About</div>
                    <div className="v">
                      {x.sources.about.map((a) => (
                        <span key={a} className="tag mono">
                          {a}
                        </span>
                      ))}
                    </div>
                  </>
                ) : null}
                {x.sources?.from || x.sources?.to ? (
                  <>
                    <div className="k">Occurred</div>
                    <div className="v mono">
                      {x.sources.from ?? "…"} → {x.sources.to ?? "…"}
                    </div>
                  </>
                ) : null}
                <div className="k">Facts per episode</div>
                <div className="v">{x.facts_per_episode ?? <span className="muted">no cap</span>}</div>
                <div className="k">Outcomes</div>
                <div className="v">{outcomesText(x.outcomes)}</div>
                {x.style && (
                  <>
                    <div className="k">Style</div>
                    <div className="v mono">{x.style}</div>
                  </>
                )}
                {x.writer && (
                  <>
                    <div className="k">Writer</div>
                    <div className="v">
                      <span className="mono">{x.writer.id}</span>
                      {x.writer.names?.length ? <span className="muted"> · {x.writer.names.join(", ")}</span> : null}
                    </div>
                  </>
                )}
                {x.aggregates && (
                  <>
                    <div className="k">Summarised</div>
                    <div className="v">
                      per <span className="mono">{x.aggregates.subject}</span>
                      {x.aggregates.elements && (
                        <span className="muted"> · {Object.keys(x.aggregates.elements).join(", ")}</span>
                      )}
                    </div>
                  </>
                )}
              </div>
              <p className="note">
                Each episode is read once, whichever version of the schema reads it. Naming a new prefix reads the
                episodes it newly names; the ones already read stay read.
              </p>
            </>
          )}
        </section>

        {history.length > 0 && (
          <section className="card">
            <h3 className="card-title">
              Versions <span className="muted">{history.length} committed on this branch</span>
            </h3>
            <ol className="timeline">
              {history
                .map((h, i) => ({ h, i }))
                .reverse()
                .map(({ h, i }) => {
                  const { added, removed } = prefixDiff(history, i);
                  const same = JSON.stringify(h) === JSON.stringify(history[i - 1]);
                  return (
                    <li key={i} className={i === history.length - 1 ? "tl tl-now" : "tl"}>
                      <span className="tl-dot" />
                      <div className="tl-body">
                        <div className="tl-head">
                          <b className="mono">v{i + 1}</b>
                          <span className="muted">{h.name}</span>
                          {i === history.length - 1 && <span className="badge badge-on mono">current</span>}
                          {i === 0 && <span className="badge mono">first</span>}
                        </div>
                        <div className="tl-diff">
                          {added.map((p) => (
                            <span key={p} className="tag tag-add mono">
                              + {p}
                            </span>
                          ))}
                          {removed.map((p) => (
                            <span key={p} className="tag tag-del mono">
                              − {p}
                            </span>
                          ))}
                          {i > 0 && same && <span className="muted">same as v{i}</span>}
                          {i > 0 && !same && !added.length && !removed.length && (
                            <span className="muted">no change to the sources read</span>
                          )}
                          {i === 0 && !added.length && <span className="muted">reads every source</span>}
                        </div>
                      </div>
                    </li>
                  );
                })}
            </ol>
          </section>
        )}

        <section className="card">
          <h3 className="card-title">
            Types <span className="muted">what facts may be about</span>
          </h3>
          <div className="types">
            {types.map(([name, t]) => {
              const ps = Object.entries(t.predicates);
              return (
                <details key={name} className="type" open={types.length <= 4}>
                  <summary>
                    <span className="mono type-name">{name}</span>
                    <span className="muted type-desc">{t.description}</span>
                    <span className="badge mono">{ps.length}</span>
                  </summary>
                  {ps.length > 0 && (
                    <table className="tbl">
                      <thead>
                        <tr>
                          <th>Predicate</th>
                          <th>Value</th>
                          <th>Holds</th>
                          <th>Meaning</th>
                        </tr>
                      </thead>
                      <tbody>
                        {ps.map(([p, d]) => (
                          <tr key={p}>
                            <td className="mono nowrap">{p}</td>
                            <td>
                              <span className="vt mono">{d.type}</span>
                              {d.part_of && <span className="vt vt-part mono">part of</span>}
                            </td>
                            <td className="muted">
                              {d.card === "many" ? "many" : "one"}
                              {predicateNote(d) && <div className="mono pnote">{predicateNote(d)}</div>}
                            </td>
                            <td className="muted">{d.description}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </details>
              );
            })}
          </div>
        </section>

        {schema.guidance?.length ? (
          <section className="card">
            <h3 className="card-title">
              Guidance <span className="muted">told to the model and to agents querying</span>
            </h3>
            <ul className="guidance">
              {schema.guidance.map((g, i) => (
                <li key={i}>{g}</li>
              ))}
            </ul>
          </section>
        ) : null}

        <section className="card">
          <h3 className="card-title">
            Raw
            <span className="card-actions">
              <button className="link" onClick={() => setRaw((r) => !r)}>
                {raw ? "hide" : "show"}
              </button>
              <Copy text={text} />
            </span>
          </h3>
          {raw && <pre className="raw mono">{text}</pre>}
        </section>
      </div>
    </main>
  );
}
