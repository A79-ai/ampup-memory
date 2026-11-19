import { useEffect, useState } from "react";
import { browse, keptBrowse } from "./api";
import { humanize } from "./format";
import { shapeOf } from "./shape";
import type { Focus, Listing, OrgType, Predicate, Schema, Selection } from "./types";
import { EmptyState, ErrorState, Icon, Skeleton } from "./ui";

export type { OrgPredicate, OrgType } from "./types";

/** A schema's types, as the layouts and the Schema section read them. */
export function schemaTypes(schema: Schema | null | undefined): Map<string, OrgType> {
  const types = new Map<string, OrgType>();
  for (const [name, def] of Object.entries(schema?.types ?? {}))
    types.set(name, {
      description: def.description,
      from: "base",
      predicates: new Map(
        Object.entries(def.predicates ?? {}).map(([p, d]) => [p, { ...d, from: "base" }]),
      ),
    });
  return types;
}

/** The values `entity`'s `predicate` may take when the schema closes it (an enum with `values`); else undefined. */
export function closedValues(
  types: Map<string, OrgType> | undefined,
  entity: string,
  predicate: string,
): string[] | undefined {
  const d = types?.get(entity.slice(0, Math.max(0, entity.indexOf(":"))))?.predicates.get(predicate);
  return d?.type === "enum" && d.values?.length ? d.values : undefined;
}

/**
 * A value to write for an attribute: a pick of its allowed values when the schema closes it (memstore refuses any
 * other), else free text.
 */
export function ValueField({
  values,
  value,
  onChange,
  placeholder,
  label,
  autoFocus,
}: {
  values?: string[];
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  label?: string;
  autoFocus?: boolean;
}) {
  if (values)
    return (
      <select
        className="field"
        value={values.includes(value) ? value : ""}
        autoFocus={autoFocus}
        aria-label={label}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="" disabled>
          {placeholder ?? "Choose a value"}
        </option>
        {values.map((v) => (
          <option key={v} value={v}>
            {v}
          </option>
        ))}
      </select>
    );
  return (
    <input
      className="field"
      value={value}
      autoFocus={autoFocus}
      aria-label={label}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
      maxLength={2000}
    />
  );
}

export type SchemaState = {
  schema?: Schema | null;
  /** How many versions of it were committed on the branch read. */
  versions?: number;
  types?: Map<string, OrgType>;
  error?: string;
};

/**
 * The schema the memory is typed with on the branch read, read once for the page through memstore's
 * `/browse {op: schema}` as the signed-in person: the Schema section and each entity's layout come from it.
 * Nothing is read for someone who can't read the memory (the platform admin).
 */
export function useOrgSchema(sel: Selection, enabled: boolean): [SchemaState | null, () => void] {
  const [s, setS] = useState<SchemaState | null>(null);
  const [n, setN] = useState(0);
  const key = JSON.stringify([sel.org, sel.team, sel.branch]);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    type Read = { schema: Schema | null; history?: Schema[] };
    const show = (r: Read) =>
      setS({ schema: r.schema, versions: r.history?.length, types: schemaTypes(r.schema) });
    // What it last answered on this page shows while it is read again.
    const kept = keptBrowse<Read>({ op: "schema" }, sel);
    if (kept) show(kept);
    browse<Read>({ op: "schema" }, sel).then(
      (r) => live && show(r),
      (e: Error) => live && setS({ error: e.message }),
    );
    return () => {
      live = false;
    };
    // `sel` is read through `key`.
  }, [key, enabled, n]);
  return [
    s,
    () => {
      setS(null);
      setN((x) => x + 1);
    },
  ];
}

const valuesOf = (d: Predicate) =>
  d.values?.length
    ? d.values.join(", ")
    : d.to?.length
      ? `→ ${d.to.join(", ")}`
      : (d.description ?? "");

/** `#/schema/<type>`: the types the org's memory holds, and what each records. */
export function SchemaView({
  schema,
  retry,
  sel,
  type,
  canBrowse,
  onFocus,
}: {
  schema: SchemaState | null;
  retry: () => void;
  sel: Selection;
  type?: string;
  canBrowse: boolean;
  onFocus: (f: Focus) => void;
}) {
  // How many entities of each type the memory holds, beside each type (members only: it reads the memory).
  const [counts, setCounts] = useState<Record<string, number>>({});
  useEffect(() => {
    if (!canBrowse) return;
    let live = true;
    browse<Listing>({ op: "ls", path: ["entities"] }, sel).then(
      (l) => live && setCounts(Object.fromEntries(l.children.map((c) => [c.name, c.count ?? 0]))),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [sel, canBrowse]);

  if (!schema) return <Skeleton rows={6} />;
  if (schema.error)
    return <ErrorState title="Couldn't read the schema" message={schema.error} onRetry={retry} />;
  const base = schema.schema;
  const types = schema.types ?? new Map<string, OrgType>();
  if (!types.size)
    return (
      <EmptyState icon="schema" title="No schema yet">
        This memory has no schema on this branch, so what it learns is stored untyped.
      </EmptyState>
    );
  const names = [...types.keys()].sort();
  const current = type && types.has(type) ? type : names[0]!;
  const t = types.get(current)!;
  const shape = shapeOf(current, types);
  return (
    <section>
      <div className="head">
        <span className="kind k-entity">
          <Icon name="schema" size={20} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Schema</div>
          <h1 className="title">
            {base?.name
              ? `${base.name}${base.version != null ? ` v${base.version}` : ""}`
              : "No schema"}
          </h1>
          <div className="subtitle">
            {base?.mode && <span className="badge">{base.mode}</span>}
            <span>{types.size} types</span>
            {schema.versions != null && schema.versions > 1 && (
              <span>{schema.versions} versions on this branch</span>
            )}
            {base?.extract?.sources?.prefixes?.length ? (
              <span>
                extracts from{" "}
                <span className="mono">{base.extract.sources.prefixes.join(" ")}</span>
              </span>
            ) : null}
          </div>
          {base?.description && <p className="schema-desc">{base.description}</p>}
        </div>
      </div>
      <div className="schema-grid">
        <nav className="card schema-types" aria-label="Types">
          {names.map((n) => (
            <button
              key={n}
              className={`schema-type ${n === current ? "on" : ""}`}
              aria-current={n === current}
              onClick={() => onFocus({ kind: "schema", type: n })}
            >
              <Icon name="entity" size={14} />
              <span className="mono">{n}</span>
              {counts[n] != null && <span className="count">{counts[n].toLocaleString()}</span>}
            </button>
          ))}
        </nav>
        <div className="card card-pad schema-type-card">
          <div className="schema-type-head">
            <div>
              <h2 className="mono">{current}</h2>
              {t.description && <p className="muted">{t.description}</p>}
              {shape && t.predicates.size > 0 && (
                <p className="muted">
                  Its entities are drawn as <b>{shape.label}</b>: it has {shape.why}.
                </p>
              )}
            </div>
            {canBrowse && (
              <button
                className="btn"
                onClick={() => onFocus({ kind: "folder", path: ["entities", current] })}
              >
                <Icon name="explorer" size={14} /> Open in Explorer
              </button>
            )}
          </div>
          {t.predicates.size === 0 ? (
            <p className="muted">It declares no predicates: its facts are stored untyped.</p>
          ) : (
            <div className="table-scroll">
              <table className="ptable">
                <thead>
                  <tr>
                    <th>Predicate</th>
                    <th>Type</th>
                    <th>Values</th>
                    <th>Holds</th>
                  </tr>
                </thead>
                <tbody>
                  {[...t.predicates].map(([p, d]) => (
                    <tr key={p}>
                      <td>
                        <div>{humanize(p)}</div>
                        <div className="mono muted">{p}</div>
                      </td>
                      <td>
                        <span className="badge mono">
                          {d.type}
                          {d.unit ? ` · ${d.unit}` : ""}
                        </span>
                      </td>
                      <td className="muted">{valuesOf(d)}</td>
                      <td className="muted">{d.card === "many" ? "many" : "one"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}
