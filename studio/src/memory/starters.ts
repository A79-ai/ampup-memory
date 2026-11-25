// No imports, so the tests (NodeNext, no JSX) can load it.

/** One entry of list_advice's answer, as far as the playground reads it. */
export type AdviceEntry = {
  situation?: string;
  when?: string;
  ratified?: boolean;
  learned_at?: number | null;
  evidence?: { served?: number; helped?: number } | null;
  withheld?: unknown;
  held?: boolean;
  conversation?: boolean;
  merged_into?: unknown;
};

/** A question to start from, and the advice it comes from (none for the fallback questions). */
export type Starter = { question: string; when?: string; served?: number; helped?: number };

/**
 * The question to ask about a situation the memory learned advice for: what to know before doing it ("deploy the
 * billing service"), or when it happens ("an alert fires…").
 */
export function adviceQuestion(when: string): string {
  let w = when
    .trim()
    .replace(/^when\s+/i, "")
    .replace(/[\s.?!:;,]+$/, "");
  if (!/^[A-Z]{2}/.test(w)) w = w.charAt(0).toLowerCase() + w.slice(1);
  const happens = /^(a|an|the|someone|somebody|you|we|it|they|there)\b/i.test(w);
  return happens ? `What should I know when ${w}?` : `What should I know before I ${w}?`;
}

/**
 * The starter questions from the advice learned so far: the situations whose advice is served (not withheld, held,
 * merged, or a team's conversation note), best backed first (most often helped, ratified, most often served,
 * newest), one question each, at most `n`.
 */
export function adviceStarters(entries: AdviceEntry[], n = 3): Starter[] {
  const live = entries.filter(
    (e) =>
      !!e.when?.trim() && e.withheld == null && !e.held && !e.conversation && e.merged_into == null,
  );
  const score = (e: AdviceEntry) => [
    e.evidence?.helped ?? 0,
    e.ratified ? 1 : 0,
    e.evidence?.served ?? 0,
    e.learned_at ?? 0,
  ];
  live.sort((a, b) => {
    const x = score(a);
    const y = score(b);
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return y[i]! - x[i]!;
    return 0;
  });
  const out: Starter[] = [];
  for (const e of live) {
    const question = adviceQuestion(e.when!);
    if (out.some((s) => s.question === question)) continue;
    out.push({ question, when: e.when, served: e.evidence?.served, helped: e.evidence?.helped });
    if (out.length === n) break;
  }
  return out;
}
