import { entityName, humanize, personName } from "./format";
import { isEnded, scoreLevel, type Pred, type Shape } from "./shape";
import type { Fact, Focus } from "./types";
import { Icon } from "./ui";

/** Another entity, as a chip that opens it. */
export function EntityLink({ entity, onFocus }: { entity: string; onFocus: (f: Focus) => void }) {
  const i = entity.indexOf(":");
  const path =
    i < 0 ? ["entities", "_", entity] : ["entities", entity.slice(0, i), entity.slice(i + 1)];
  const n = entityName(entity);
  return (
    <button className="chip" title={entity} onClick={() => onFocus({ kind: "entity", path })}>
      <Icon name="entity" size={12} />
      {n.type && <span className="chip-type">{n.type}</span>}
      {n.name}
    </button>
  );
}

/** Enum values that read as good, middling or bad, so their badge says so. */
const TONE: Record<string, string> = {
  won: "badge-ok",
  done: "badge-ok",
  completed: "badge-ok",
  resolved: "badge-ok",
  confirmed: "badge-ok",
  approved: "badge-ok",
  supportive: "badge-ok",
  lost: "badge-error",
  dropped: "badge-error",
  rejected: "badge-error",
  opposed: "badge-error",
  cancelled: "badge-error",
  no_show: "badge-error",
};

/** One value, as its predicate's type draws it. */
export function Value({
  type,
  value,
  onFocus,
}: {
  type: string;
  value: string;
  onFocus: (f: Focus) => void;
}) {
  switch (type) {
    case "ref":
      return <EntityLink entity={value} onFocus={onFocus} />;
    case "money": {
      const n = Number(value.replace(/[^0-9.-]/g, ""));
      return (
        <span className="field-money">
          {Number.isFinite(n) && value.trim() ? `$${n.toLocaleString()}` : value}
        </span>
      );
    }
    case "enum":
      return <span className={`badge ${TONE[value] ?? ""}`}>{value.replace(/_/g, " ")}</span>;
    case "date":
    case "timestamp":
      return <span className="mono">{value}</span>;
    default:
      return <span>{value}</span>;
  }
}

const newestFirst = (a: Fact, b: Fact) => b.valid_from - a.valid_from;

/**
 * An entity drawn with its type's layout (`shape.ts`): a pipeline item's amount, stage and scorecard, a person's
 * card, a task's checkbox and due date, an event's time and people, an issue's quote, a topic's notes, or a grid.
 * Each predicate the schema declares shows its current value, or "not known yet". `facts` are the current ones.
 */
export function Layout({
  entity,
  type,
  shape,
  facts,
  onFocus,
}: {
  entity: string;
  type: string;
  shape: Shape;
  facts: Fact[];
  onFocus: (f: Focus) => void;
}) {
  const of = (p: Pred) => facts.filter((f) => f.predicate === p.key).sort(newestFirst);
  const latest = (p: Pred | undefined) => (p ? of(p)[0]?.value : undefined);
  // A one-valued predicate shows its latest value, a many-valued one all of them.
  const values = (p: Pred) => {
    const have = of(p);
    const shown = p.card === "many" ? have : have.slice(0, 1);
    return shown.length ? (
      shown.map((f) => <Value key={f.fact_id} type={p.type} value={f.value} onFocus={onFocus} />)
    ) : (
      <span className="unset">not known yet</span>
    );
  };
  const field = (p: Pred) => (
    <div
      key={p.key}
      className="field-cell"
      title={`${p.key}: ${p.type}${p.from === "base" ? "" : ` (goal ${p.from})`}`}
    >
      <div className="field-k">{humanize(p.key)}</div>
      <div className="field-v">{values(p)}</div>
    </div>
  );
  const grid = (ps: Pred[]) => ps.length > 0 && <div className="fields">{ps.map(field)}</div>;
  const without = (...shown: (Pred | undefined)[]) =>
    shape.preds.filter((p) => !shown.includes(p) && !shape.score.includes(p));
  const name = entityName(entity).name;
  const known = shape.preds.filter((p) => of(p).length > 0).length;

  let body: React.ReactNode;
  switch (shape.layout) {
    case "pipeline": {
      const date = shape.times[0];
      const notes = shape.text.filter((p) => of(p).length);
      body = (
        <>
          <div className="layout-hero">
            <div>
              <div className="field-k">{humanize(shape.money!.key)}</div>
              <div className="layout-big">{values(shape.money!)}</div>
              <div className="muted">{name}</div>
            </div>
            <div className="layout-row">
              {shape.flags.map(field)}
              {date && field(date)}
            </div>
          </div>
          <div className="layout-body">
            {grid(without(shape.money, ...shape.flags, date, ...shape.text))}
            {notes.map((p) => (
              <div key={p.key} className="callout">
                <b>{humanize(p.key)}</b> · {latest(p)}
              </div>
            ))}
          </div>
          <Scorecard shape={shape} latest={latest} />
        </>
      );
      break;
    }
    case "person": {
      const say = (k: string) => latest(shape.strings.find((p) => p.key === k));
      const sub = [say("title") ?? say("role"), say("company")].filter(Boolean).join(" · ");
      const who = personName(entity);
      const initials = who
        .split(" ")
        .slice(0, 2)
        .map((w) => w.charAt(0))
        .join("");
      body = (
        <>
          <div className="layout-hero">
            <div className="layout-who">
              <span className="person-mark" aria-hidden>
                {initials}
              </span>
              <div>
                <div className="layout-name">{who}</div>
                <div className="muted">{sub || <span className="mono">{entity}</span>}</div>
              </div>
            </div>
          </div>
          <div className="layout-body">
            {grid(shape.preds.filter((p) => !/^(title|company)$/.test(p.key)))}
          </div>
        </>
      );
      break;
    }
    case "task": {
      const head = shape.text[0]!;
      const due = shape.times[0]!;
      const done = isEnded(latest(shape.ends));
      const dueOn = latest(due);
      const overdue = !done && !!dueOn && dueOn < new Date().toISOString().slice(0, 10);
      body = (
        <>
          <div className="layout-hero">
            <div className="layout-who">
              <span
                className={`status-box ${done ? "status-done" : ""}`}
                aria-label={done ? "done" : "not done"}
              >
                {done && <Icon name="check" size={13} />}
              </span>
              <div>
                <div className="field-k">{humanize(head.key)}</div>
                <div className={`layout-name ${done ? "struck" : ""}`}>
                  {latest(head) ?? <span className="unset">not known yet</span>}
                </div>
              </div>
            </div>
            <div className="layout-row">
              {field(shape.ends!)}
              <div className="field-cell">
                <div className="field-k">{humanize(due.key)}</div>
                <div className="field-v">
                  {values(due)}
                  {overdue && <span className="badge badge-error">overdue</span>}
                </div>
              </div>
            </div>
          </div>
          <div className="layout-body">{grid(without(head, due, shape.ends))}</div>
        </>
      );
      break;
    }
    case "event": {
      const at = shape.times.find((p) => p.type === "timestamp")!;
      body = (
        <>
          <div className="layout-hero">
            <div>
              <div className="field-k">{humanize(at.key)}</div>
              <div className="layout-big layout-big-sm">{values(at)}</div>
              <div className="muted">{name}</div>
            </div>
            <div className="layout-row">{shape.flags.map(field)}</div>
          </div>
          <div className="layout-body">
            {shape.manyPeople.map((p) => (
              <div key={p.key} className="layout-people">
                <div className="field-k">
                  {humanize(p.key)} · {of(p).length}
                </div>
                <div className="field-v">{values(p)}</div>
              </div>
            ))}
            {grid(without(at, ...shape.flags, ...shape.manyPeople))}
          </div>
        </>
      );
      break;
    }
    case "issue": {
      const text = shape.text[0]!;
      body = (
        <>
          <div className="layout-hero">
            <div className="layout-row">{shape.flags.map(field)}</div>
          </div>
          <div className="layout-body">
            <blockquote className="quoteblock">
              {latest(text) ? `“${latest(text)}”` : <span className="unset">not known yet</span>}
            </blockquote>
            {grid(without(text, ...shape.flags))}
          </div>
        </>
      );
      break;
    }
    case "notes":
      body = (
        <div className="layout-body">
          {shape.preds.map((p) => (
            <div key={p.key} className="layout-note">
              <div className="field-k">{humanize(p.key)}</div>
              {of(p).length ? (
                <ul>
                  {(p.card === "many" ? of(p) : of(p).slice(0, 1)).map((f) => (
                    <li key={f.fact_id}>{f.value}</li>
                  ))}
                </ul>
              ) : (
                <span className="unset">not known yet</span>
              )}
            </div>
          ))}
        </div>
      );
      break;
    default:
      body = (
        <div className="layout-body">
          {grid(without())}
          <Scorecard shape={shape} latest={latest} />
        </div>
      );
  }

  return (
    <>
      <div className="h-section">
        As {humanize(type)}{" "}
        <span className="count">
          {known} of {shape.preds.length} known
        </span>
        <span className="layout-why" title={`Picked because ${type} has ${shape.why}`}>
          {shape.label}
        </span>
      </div>
      <div className={`card layout layout-${shape.layout}`}>{body}</div>
    </>
  );
}

/** Enums sharing one set of values, each a meter by its value's rank (the first value is the best). */
function Scorecard({
  shape,
  latest,
}: {
  shape: Shape;
  latest: (p: Pred | undefined) => string | undefined;
}) {
  if (!shape.score.length) return null;
  const rows = shape.score.map((p) => {
    const v = latest(p);
    return { p, v, at: v ? scoreLevel(p, v) : null };
  });
  const top = scoreLevel(shape.score[0]!, shape.score[0]!.values![0]!)!.top;
  const assessed = rows.filter((r) => r.v).length;
  const total = rows.reduce((n, r) => n + (r.at?.level ?? 0), 0);
  const strip = (k: string) => humanize(k.slice(shape.scorePrefix.length).replace(/^_+/, "") || k);
  return (
    <div className="scorecard">
      <div className="scorecard-head">
        <b>{shape.scorePrefix ? shape.scorePrefix.toUpperCase() : "Scorecard"}</b>
        <span className="muted">
          {assessed} of {rows.length} assessed · {Math.round((total / (rows.length * top)) * 100)}%
        </span>
      </div>
      <div className="scorecard-rows">
        {rows.map(({ p, v, at }) => (
          <div key={p.key} className="scorecard-row" title={p.key}>
            <span>{strip(p.key)}</span>
            <span className="meter" aria-hidden>
              {Array.from({ length: top }, (_, i) => (
                <i
                  key={i}
                  className={at && i < at.level ? (at.level === top ? "full" : "part") : ""}
                />
              ))}
            </span>
            <span className={v ? "muted" : "unset"}>{v ? v.replace(/_/g, " ") : "not known"}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
