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

/** A question to start from, and the situation whose advice it comes from (none for the fixed questions). */
export type Starter = {
  question: string;
  /** What is asked when it's picked, where the question shown is cut short. */
  asks?: string;
  situation?: string;
  served?: number;
  helped?: number;
};

/** The longest a starter's task runs, in characters: a question on one line of the composer's width. */
export const TASK_CHARS = 60;

/**
 * The task a situation is about, short: its title ("Review a pull request"), or one written as a key
 * ("mine:code-and-file-changes/merge-a-finished-change") read as words; a description of when it applies
 * stands in only where it is that short. Cut at a word, with an ellipsis, past `max`; none without either.
 */
export function situationTask(
  e: { situation?: string; when?: string },
  max = TASK_CHARS,
): string | undefined {
  const keyed = (k: string) => k.replace(/^.*[/:]/, "").replace(/[-_]+/g, " ");
  const title = e.situation?.trim();
  let t =
    title && (/\s/.test(title) || !/[-_/:]/.test(title))
      ? title
      : title
        ? keyed(title)
        : (e.when?.trim() ?? "");
  t = t.replace(/^when\s+/i, "").replace(/[\s.?!:;,]+$/, "");
  if (!t || (!title && t.length > TASK_CHARS)) return undefined;
  if (t.length > max) {
    const cut = t.slice(0, max);
    t = `${cut.slice(0, Math.max(cut.lastIndexOf(" "), max / 2)).replace(/[\s,;:]+$/, "")}…`;
  }
  return /^[A-Z]{2}/.test(t) ? t : t.charAt(0).toLowerCase() + t.slice(1);
}

/** The question a starter asks about a task: how to do it ("How do I review a pull request?"). */
export const adviceQuestion = (task: string) => `How do I ${task}${task.endsWith("…") ? "" : "?"}`;

/**
 * The starter questions from the advice learned so far: the situations whose advice is served (not withheld, held,
 * merged, or a team's conversation note), best backed first (most often helped, ratified, most often served,
 * newest), one question each, at most `n`.
 */
export function adviceStarters(entries: AdviceEntry[], n = 3): Starter[] {
  const live = entries.filter(
    (e) => e.withheld == null && !e.held && !e.conversation && e.merged_into == null,
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
    const task = situationTask(e);
    if (!task) continue;
    const question = adviceQuestion(task);
    if (out.some((s) => s.question === question)) continue;
    const whole = adviceQuestion(situationTask(e, Infinity)!);
    out.push({
      question,
      ...(whole !== question ? { asks: whole } : {}),
      situation: e.situation,
      served: e.evidence?.served,
      helped: e.evidence?.helped,
    });
    if (out.length === n) break;
  }
  return out;
}
