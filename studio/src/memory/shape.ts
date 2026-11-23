// Type-only and from types.ts alone, so the tests (NodeNext, no JSX) can import this file.
import type { OrgPredicate, OrgType } from "./types.js";

/**
 * The layout an entity is drawn with, picked from the shape of its type's predicates in the org's schema, never
 * from the type's name: an org that calls its deals "opportunity" gets the same layout, as long as it has an amount
 * and a stage. The first rule a type's shape meets picks it:
 *
 *   pipeline  a money predicate and enums (an amount and its stage, status)
 *   person    an `email` or `title` string
 *   task      text, a reference to one person, a date, and a status that ends (done, resolved)
 *   event     a timestamp and references to many people, with no single owner
 *   issue     text and a status that ends
 *   notes     nothing but text (a topic: its summary, issues, next steps)
 *   grid      anything else: each predicate in a grid
 *
 * People are references to a type that itself has a person's shape. Three or more enums sharing one set of values
 * (MEDDPICC's confirmed, partial, unknown, none) are a scorecard, drawn apart from the other enums.
 */
export type LayoutId = "pipeline" | "person" | "task" | "event" | "issue" | "notes" | "grid";

export type Pred = OrgPredicate & { key: string };

export type Shape = {
  layout: LayoutId;
  /** What the layout is called, for the page and the Schema section. */
  label: string;
  /** Which predicates picked it, in a phrase: "a money amount (amount) and enums (stage, status)". */
  why: string;
  preds: Pred[];
  money?: Pred;
  /** The enums that aren't part of the scorecard. */
  flags: Pred[];
  /** A scorecard's enums, in schema order; empty when the type has none. */
  score: Pred[];
  /** What the scorecard's predicates start with (`meddpicc`), to title it. */
  scorePrefix: string;
  onePerson: Pred[];
  manyPeople: Pred[];
  otherRefs: Pred[];
  times: Pred[];
  text: Pred[];
  strings: Pred[];
  /** An enum one of whose values ends the thing (done, completed, resolved, addressed). */
  ends?: Pred;
};

export const LABELS: Record<LayoutId, string> = {
  pipeline: "Pipeline item",
  person: "Person",
  task: "Task",
  event: "Event",
  issue: "Issue",
  notes: "Notes",
  grid: "Fields",
};

const CONTACT = /^(email|title)$/;
const ENDED = /^(done|completed|resolved|addressed)$/;

/** Whether a status value means the thing is over: a task done, an issue resolved. */
export const isEnded = (v: string | undefined) => !!v && ENDED.test(v);

/** The types whose shape is a person's: references to them are people (an owner, a champion, a participant). */
export function personTypes(types: Map<string, OrgType>): Set<string> {
  const out = new Set<string>();
  for (const [name, t] of types)
    if ([...t.predicates].some(([k, p]) => p.type === "string" && CONTACT.test(k))) out.add(name);
  return out;
}

const names = (ps: Pred[]) => ps.map((p) => p.key).join(", ");

function commonPrefix(keys: string[]): string {
  let pre = keys[0] ?? "";
  for (const k of keys) while (!k.startsWith(pre)) pre = pre.slice(0, -1);
  return pre.replace(/_+$/, "");
}

/** The shape of a type the schema declares, and the layout it picks; null for a type it doesn't. */
export function shapeOf(type: string, types: Map<string, OrgType>): Shape | null {
  const t = types.get(type);
  if (!t) return null;
  const people = personTypes(types);
  const preds: Pred[] = [...t.predicates].map(([key, p]) => ({ key, ...p }));
  const enums = preds.filter((p) => p.type === "enum");

  // A scorecard: three or more enums with the same three or more values.
  const bySet = new Map<string, Pred[]>();
  for (const e of enums)
    if ((e.values?.length ?? 0) >= 3) {
      const k = e.values!.join("|");
      bySet.set(k, [...(bySet.get(k) ?? []), e]);
    }
  const score = [...bySet.values()].find((g) => g.length >= 3) ?? [];
  const inScore = new Set(score.map((p) => p.key));

  const refs = preds.filter((p) => p.type === "ref");
  const toPerson = (p: Pred) => (p.to ?? []).some((x: string) => people.has(x));
  const s = {
    preds,
    money: preds.find((p) => p.type === "money"),
    flags: enums.filter((p) => !inScore.has(p.key)),
    score,
    scorePrefix: score.length ? commonPrefix(score.map((p) => p.key)) : "",
    onePerson: refs.filter((p) => toPerson(p) && p.card !== "many"),
    manyPeople: refs.filter((p) => toPerson(p) && p.card === "many"),
    otherRefs: refs.filter((p) => !toPerson(p)),
    times: preds.filter((p) => p.type === "timestamp" || p.type === "date"),
    text: preds.filter((p) => p.type === "text"),
    strings: preds.filter((p) => p.type === "string"),
    ends: enums.find((p) => (p.values ?? []).some((v: string) => ENDED.test(v))),
  };
  const pick = (layout: LayoutId, why: string): Shape => ({
    ...s,
    layout,
    label: LABELS[layout],
    why,
  });

  const contact = s.strings.filter((p) => CONTACT.test(p.key));
  const stamp = s.times.find((p) => p.type === "timestamp");
  if (s.money && s.flags.length)
    return pick("pipeline", `a money amount (${s.money.key}) and enums (${names(s.flags)})`);
  if (contact.length) return pick("person", `contact details (${names(contact)})`);
  if (s.text.length && s.onePerson.length && s.times.length && s.ends)
    return pick(
      "task",
      `an action (${s.text[0]!.key}), an owner (${s.onePerson[0]!.key}), a date (${s.times[0]!.key}) and a status that ends (${s.ends.key})`,
    );
  if (stamp && s.manyPeople.length && !s.onePerson.length)
    return pick("event", `a time (${stamp.key}) and many people (${names(s.manyPeople)})`);
  if (s.text.length && s.ends)
    return pick("issue", `text (${s.text[0]!.key}) and a status that ends (${s.ends.key})`);
  if (preds.length && preds.every((p) => p.type === "text"))
    return pick("notes", `only text (${names(preds)})`);
  return pick("grid", "predicates no other layout fits");
}

/**
 * Where a scorecard value stands: its rank in the shared set, the first value best (`confirmed` 3 of 3,
 * `none` 0). Null for a value outside the set.
 */
export function scoreLevel(p: Pred, value: string): { level: number; top: number } | null {
  const values = p.values ?? [];
  const i = values.indexOf(value);
  return i < 0 ? null : { level: values.length - 1 - i, top: values.length - 1 };
}
