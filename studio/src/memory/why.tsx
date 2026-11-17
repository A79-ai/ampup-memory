import "./why.css";
import { useEffect, useState } from "react";
import { errorText } from "../transport";
import { call } from "./api";
import { entityName, humanize, sourceTitle, when } from "./format";
import { entityFocus, review } from "./review-api";
import type { Selection } from "./types";

// "Why this answer": what a memory read found, explained. A search is run again with `explain` (memstore's
// `search_memory` in its JSON form, as the person viewing, so only what they may read is ever shown): the facts that
// won with the signals the ranker scored them by and each one's part of the score, the best of those found but cut
// with why, and on a winner "This is wrong", which states the right value through Review's own path (`add_fact`,
// then the conflict it makes, settled with "Use this value").

/** One signal as an explained search scores it (`explain.signals`). */
interface Signals {
  keyword: { part: number; bm25: number; terms_matched: number; terms: number };
  meaning: { part: number; similarity: number | null };
  origin: { scale: number; origin: string | null };
  time: { in_window: boolean; scale: number; first: boolean };
  recency?: { scale: number };
}
interface Explain {
  rank?: number;
  score: number;
  fusion: "blend" | "rrf" | null;
  signals: Signals;
  from_tail: boolean;
}
interface Source {
  episode_id: string;
  source: string;
  occurred_at: number;
}
/** A fact as an explained search answers it: a hit, or (with `cut`) one found and not returned. */
export interface WhyFact {
  fact_id: string;
  entity: string;
  predicate: string;
  value: string;
  valid_from: number;
  valid_to?: number | null;
  quote?: string | null;
  source_episodes: string[];
  origin?: string;
  origin_note?: string;
  written_by?: string | null;
  scope?: string;
  freshness?: { state?: string; note?: string } & Record<string, unknown>;
  conflict?: Record<string, unknown>;
  corroboration?: { sources?: number } & Record<string, unknown>;
  facts?: WhyFact[];
  explain?: Explain;
  source?: Source;
  cut?: "below" | "superseded" | "older_record" | "duplicate" | "contradicted";
  by?: string | null;
}
interface Answer {
  hits: WhyFact[];
  missed?: WhyFact[];
  note?: string;
}

/** The `search_memory` arguments a replay keeps: what picks the facts, not how the answer is laid out. */
const KEPT = [
  "query",
  "as_of",
  "start_date",
  "end_date",
  "tags",
  "predicates",
  "type",
  "episode_type",
] as const;

/** A search's arguments from a trace's step: JSON when whole, else the query alone where the step was cut. */
export function searchArgs(text: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(text) as unknown;
    if (v && typeof v === "object" && typeof (v as { query?: unknown }).query === "string")
      return v as Record<string, unknown>;
  } catch {
    // A long call is kept shortened: its query is still at its start.
  }
  const m = /"query"\s*:\s*"((?:[^"\\]|\\.)*)"/.exec(text);
  if (!m) return null;
  try {
    return { query: JSON.parse(`"${m[1]}"`) as string };
  } catch {
    return { query: m[1] };
  }
}

/** Whether a tool's name is memstore's fact search (`mcp__<server>__search_memory` too). */
export const isSearch = (tool: string) => (tool.split("__").pop() ?? tool) === "search_memory";

const CUT: Record<NonNullable<WhyFact["cut"]>, string> = {
  below: "Ranked below the facts returned",
  superseded: "A newer value of the same attribute replaced it",
  older_record: "An older record of what happened: only the newest three are kept",
  duplicate: "Says what a better-ranked fact says, shown instead",
  contradicted: "A claim that nothing happened, which what arrived since contradicts",
};

const go = (hash: string) => {
  location.hash = hash;
};
const entityHash = (entity: string) => {
  const f = entityFocus(entity);
  return `#/e/${f.path.map(encodeURIComponent).join("/")}`;
};

async function explained(sel: Selection, args: Record<string, unknown>): Promise<Answer> {
  const kept: Record<string, unknown> = {};
  for (const k of KEPT) if (args[k] != null) kept[k] = args[k];
  const k = Math.min(Math.max(Number(args.k) || 10, 1), 10);
  const r = await call<{
    result?: { content?: { text?: string }[]; isError?: boolean };
    error?: { message?: string };
  }>(sel, "mcp", {
    method: "tools/call",
    params: {
      name: "search_memory",
      arguments: { ...kept, k, format: "json", explain: true },
    },
  });
  const text = r.result?.content?.[0]?.text ?? "";
  if (r.error || r.result?.isError) throw new Error(r.error?.message ?? text ?? "search failed");
  const v = JSON.parse(text) as Answer | WhyFact[];
  return Array.isArray(v) ? { hits: v } : v;
}

/**
 * Why a memory read found what it did. `args` are the `search_memory` call's; `replay` says it is run again now
 * against today's memory (a trace's), so it may differ from what the agent read then.
 */
export function Why({
  sel,
  args,
  replay,
  title = "Why this answer",
}: {
  sel: Selection;
  args: Record<string, unknown>;
  replay?: { at?: number } | boolean;
  title?: string;
}) {
  const [got, setGot] = useState<{ a?: Answer; error?: unknown } | null>(null);
  const [n, setN] = useState(0);
  const key = JSON.stringify([sel.org, sel.team, sel.branch, args, n]);
  useEffect(() => {
    let live = true;
    setGot(null);
    explained(sel, args).then(
      (a) => live && setGot({ a }),
      (error: unknown) => live && setGot({ error }),
    );
    return () => {
      live = false;
    };
  }, [key]);
  const a = got?.a;
  const all = [...(a?.hits ?? []), ...(a?.missed ?? [])];
  // Bars share one scale: the best score shown (a blend's parts sum to at most 1).
  const top = Math.max(
    1e-9,
    ...all.map((f) => (f.explain ? part(f.explain, "keyword") + part(f.explain, "meaning") : 0)),
  );
  const byId = new Map((a?.hits ?? []).map((h, i) => [h.fact_id, i + 1]));
  const fusion = a?.hits.find((h) => h.explain?.fusion)?.explain?.fusion;
  return (
    <section className="why" aria-label={title}>
      <header className="why-head">
        <div>
          <div className="why-title">{title}</div>
          <div className="why-q">
            <span className="why-q-label">Asked memory</span> “{String(args.query ?? "")}”
          </div>
        </div>
        <button className="why-btn why-quiet" type="button" onClick={() => setN((x) => x + 1)}>
          Run again
        </button>
      </header>
      {replay && (
        <p className="why-replay">
          Replayed now against today's memory, as you
          {typeof replay === "object" && replay.at ? `; the agent read it ${when(replay.at)}` : ""}.
          What has been learned or corrected since shows here; what you may not read never does.
        </p>
      )}
      {!got ? (
        <p className="why-muted">Explaining…</p>
      ) : got.error ? (
        <p className="why-error" role="alert">
          {errorText(got.error)}
        </p>
      ) : !a?.hits.length ? (
        <p className="why-muted">Memory found nothing you may read for this.</p>
      ) : (
        <>
          <Legend fusion={fusion} />
          <ol className="why-list">
            {a.hits.map((h, i) => (
              <Winner key={h.fact_id} sel={sel} f={h} rank={i + 1} top={top} />
            ))}
          </ol>
          {(a.missed?.length ?? 0) > 0 && (
            <>
              <div className="why-sub">
                Found but not used <span className="why-muted">{a.missed!.length}</span>
              </div>
              <ul className="why-list why-lost">
                {a.missed!.map((m) => (
                  <Loser key={m.fact_id} f={m} top={top} rankOf={byId} hits={a.hits} />
                ))}
              </ul>
            </>
          )}
          {a.note && <p className="why-muted why-small">{a.note}</p>}
        </>
      )}
    </section>
  );
}

const part = (e: Explain, side: "keyword" | "meaning") => e.signals[side].part || 0;

function Legend({ fusion }: { fusion?: string | null }) {
  return (
    <p className="why-legend why-small">
      <span>
        <i className="why-sw why-sw-kw" /> Keyword
      </span>
      <span>
        <i className="why-sw why-sw-mn" /> Meaning
      </span>
      <span className="why-muted">
        {fusion === "rrf"
          ? "Fused by rank (too few matches on one side to blend scores)"
          : "Score: 40% meaning, 60% keyword, each against the best match; then origin and time"}
      </span>
    </p>
  );
}

/** The two sides of a score as one bar, on the scale of the best shown, with what scaled it after. */
function Bars({ e, top }: { e?: Explain; top: number }) {
  if (!e) return null;
  const kw = part(e, "keyword");
  const mn = part(e, "meaning");
  const s = e.signals;
  const flags: string[] = [];
  if (s.origin.scale !== 1) flags.push(`×${+s.origin.scale.toFixed(2)} only an agent wrote it`);
  if (s.time.first) flags.push("in the time asked about: ranked first");
  if (s.time.scale < 1) flags.push(`×${+s.time.scale.toFixed(2)} names another time`);
  if (s.time.scale > 1) flags.push(`×${s.time.scale.toFixed(2)} in the time asked about`);
  if (s.recency && s.recency.scale !== 1) flags.push(`×${s.recency.scale.toFixed(2)} recency`);
  const tip =
    `keyword ${kw.toFixed(3)} (BM25 ${s.keyword.bm25.toFixed(2)}, ${s.keyword.terms_matched} of ` +
    `${s.keyword.terms} words) · meaning ${mn.toFixed(3)}` +
    (s.meaning.similarity != null ? ` (similarity ${s.meaning.similarity.toFixed(3)})` : "") +
    ` · score ${e.score.toFixed(3)}`;
  return (
    <div className="why-bars" title={tip}>
      <span className="why-bar" aria-hidden="true">
        <i className="why-sw-kw" style={{ width: `${(kw / top) * 100}%` }} />
        <i className="why-sw-mn" style={{ width: `${(mn / top) * 100}%` }} />
      </span>
      <span className="why-nums why-small">
        <span>
          words {s.keyword.terms_matched}/{s.keyword.terms}
        </span>
        <span>meaning {s.meaning.similarity != null ? s.meaning.similarity.toFixed(2) : "—"}</span>
        <span>score {e.score.toFixed(2)}</span>
      </span>
      {flags.length > 0 && <span className="why-flags why-small">{flags.join(" · ")}</span>}
      {e.from_tail && (
        <span
          className="why-tail why-small"
          title="Its keyword score is estimated until the index holds it"
        >
          not indexed yet
        </span>
      )}
    </div>
  );
}

function FactLine({ f }: { f: WhyFact }) {
  const e = entityName(f.entity);
  return (
    <span className="why-fact">
      <a className="why-entity" href={entityHash(f.entity)} title={f.entity}>
        {e.name}
      </a>{" "}
      <span className="why-pred">{humanize(f.predicate)}</span>{" "}
      <span className="why-value">{f.value}</span>
    </span>
  );
}

/** Where a fact came from: its source and when it became true, who wrote it, the scope it is from. */
function Provenance({ f }: { f: WhyFact }) {
  const ep = f.source?.episode_id ?? f.source_episodes[0];
  const origin =
    f.origin === "agent"
      ? "an agent wrote it"
      : f.origin === "stated"
        ? "a person stated it"
        : f.origin === "observed"
          ? "observed in the source"
          : null;
  return (
    <span className="why-prov why-small">
      {ep ? (
        <a href={`#/x/${encodeURIComponent(ep)}`} title={f.source?.source ?? ep}>
          {f.source ? sourceTitle(f.source.source) : `episode ${ep.slice(0, 10)}`}
        </a>
      ) : (
        <span>no source</span>
      )}
      <span>since {when(f.valid_from)}</span>
      {origin && <span title={f.origin_note}>{origin}</span>}
      {f.written_by && f.written_by !== "posted" && <span>{f.written_by}</span>}
      {f.scope && <span>from {f.scope}</span>}
      {typeof f.corroboration?.sources === "number" && f.corroboration.sources > 1 && (
        <span>{f.corroboration.sources} sources agree</span>
      )}
    </span>
  );
}

function Winner({ sel, f, rank, top }: { sel: Selection; f: WhyFact; rank: number; top: number }) {
  const [fixing, setFixing] = useState(false);
  const stale = typeof f.freshness?.state === "string" ? f.freshness.state : null;
  return (
    <li className="why-item">
      <span className="why-rank">{rank}</span>
      <div className="why-body">
        <div className="why-line">
          <FactLine f={f} />
          {stale && <span className="why-tag why-tag-warn">{humanize(stale)}</span>}
          {f.conflict && <span className="why-tag why-tag-warn">in conflict</span>}
          <button
            className="why-btn why-wrong"
            type="button"
            aria-expanded={fixing}
            onClick={() => setFixing(!fixing)}
          >
            {fixing ? "Cancel" : "This is wrong"}
          </button>
        </div>
        {f.quote && f.quote !== f.value && <q className="why-quote">{f.quote}</q>}
        <Provenance f={f} />
        <Bars e={f.explain} top={top} />
        {(f.facts?.length ?? 0) > 0 && (
          <details className="why-with why-small">
            <summary>Came with it: {f.facts!.length} more about the same thing</summary>
            <ul>
              {f.facts!.map((g) => (
                <li key={g.fact_id}>
                  {humanize(g.predicate)} <b>{g.value}</b>
                </li>
              ))}
            </ul>
          </details>
        )}
        {fixing && <Correct sel={sel} f={f} />}
      </div>
    </li>
  );
}

function Loser({
  f,
  top,
  rankOf,
  hits,
}: {
  f: WhyFact;
  top: number;
  rankOf: Map<string, number>;
  hits: WhyFact[];
}) {
  const by = f.by ? rankOf.get(f.by) : undefined;
  const byHit = f.by ? hits.find((h) => h.fact_id === f.by) : undefined;
  const why =
    f.cut === "superseded" && byHit
      ? `Replaced by the newer value “${byHit.value}” (#${by})`
      : f.cut === "duplicate" && by
        ? `Says what #${by} says, shown instead`
        : CUT[f.cut ?? "below"];
  return (
    <li className="why-item">
      <span className="why-rank why-x" aria-label="Not used">
        ×
      </span>
      <div className="why-body">
        <div className="why-line why-struck">
          <FactLine f={f} />
        </div>
        <div className="why-reason why-small">{why}</div>
        <Provenance f={f} />
        {f.cut !== "superseded" && <Bars e={f.explain} top={top} />}
      </div>
    </li>
  );
}

/**
 * "This is wrong": the right value, stated through Review's own path. `add_fact` puts it beside the wrong one; a
 * conflict check then records the two in conflict where the attribute holds one value, and "Use this value" keeps
 * the new one, which ends the wrong one (history keeps it). Where it holds many, or the fact is owned, Review settles
 * it.
 */
function Correct({ sel, f }: { sel: Selection; f: WhyFact }) {
  const [value, setValue] = useState("");
  const [note, setNote] = useState(`Corrects “${f.value}”`);
  const [step, setStep] = useState<
    | { s: "edit" }
    | { s: "busy"; what: string }
    | { s: "added"; added: string; pair?: [string, string]; proposal?: string }
    | { s: "done"; text: string }
    | { s: "error"; text: string }
  >({ s: "edit" });
  const fail = (e: unknown) => setStep({ s: "error", text: errorText(e) });
  const add = async () => {
    setStep({ s: "busy", what: "Adding the right value…" });
    try {
      const r = await review.addFact(sel, {
        entity: f.entity,
        predicate: f.predicate,
        value: value.trim(),
        note: note.trim() || undefined,
      });
      if (r.review && r.review.status !== "published") {
        setStep({ s: "added", added: r.fact_id, proposal: r.review.proposal.id });
        return;
      }
      setStep({ s: "busy", what: "Checking it against what memory holds…" });
      await review.checkConflicts(sel);
      const found = await review.conflicts(sel, { entity: f.entity, limit: 200 });
      // The conflict the new value makes with the wrong one: with it, or with another account of the same value
      // (a check compares one fact per value).
      const c = found.conflicts.find((c) => {
        const other = c.facts.find((x) => x.fact_id !== r.fact_id);
        return (
          c.predicate === f.predicate &&
          c.facts.some((x) => x.fact_id === r.fact_id) &&
          (other?.fact_id === f.fact_id || other?.value === f.value)
        );
      });
      setStep({
        s: "added",
        added: r.fact_id,
        pair: c ? [c.facts[0].fact_id, c.facts[1].fact_id] : undefined,
      });
    } catch (e) {
      fail(e);
    }
  };
  const keep = async (pair: [string, string], added: string) => {
    setStep({ s: "busy", what: "Using the right value…" });
    try {
      const r = await review.resolve(sel, pair, { keep: added });
      setStep({
        s: "done",
        text:
          r.status === "published"
            ? `Done: “${value.trim()}” answers now, and “${f.value}” has ended (history keeps it).`
            : "Sent for review: the entity's owners approve it before it reaches main.",
      });
    } catch (e) {
      fail(e);
    }
  };
  return (
    <form
      className="why-fix"
      aria-label={`Correct ${f.entity} ${f.predicate}`}
      onSubmit={(e) => {
        e.preventDefault();
        if (value.trim() && step.s === "edit") void add();
      }}
    >
      <div className="why-fix-head why-small">
        <b>Correct it</b> through Review: the right value is added beside the wrong one, then the
        two are settled as a conflict.
      </div>
      <dl className="why-fix-kv why-small">
        <dt>Entity</dt>
        <dd className="why-mono">{f.entity}</dd>
        <dt>Attribute</dt>
        <dd className="why-mono">{f.predicate}</dd>
        <dt>Wrong</dt>
        <dd className="why-struck">{f.value}</dd>
      </dl>
      {step.s === "edit" || step.s === "error" ? (
        <>
          <label className="why-fix-field">
            <span className="why-small">The right value</span>
            <input
              autoFocus
              value={value}
              maxLength={2000}
              onChange={(e) => {
                setValue(e.target.value);
                if (step.s === "error") setStep({ s: "edit" });
              }}
            />
          </label>
          <label className="why-fix-field">
            <span className="why-small">Note</span>
            <input value={note} maxLength={2000} onChange={(e) => setNote(e.target.value)} />
          </label>
          <div className="why-fix-row">
            <button className="why-btn why-primary" type="submit" disabled={!value.trim()}>
              Add the right value
            </button>
            {step.s === "error" && (
              <span className="why-error why-small" role="alert">
                {step.text}
              </span>
            )}
          </div>
        </>
      ) : step.s === "busy" ? (
        <p className="why-muted why-small">{step.what}</p>
      ) : step.s === "added" ? (
        step.proposal ? (
          <p className="why-small">
            Added; it waits for the entity's owners.{" "}
            <a href={`#/r/${encodeURIComponent(step.proposal)}`}>Open its review</a>
          </p>
        ) : step.pair ? (
          <div className="why-fix-row">
            <span className="why-small">
              “{value.trim()}” now conflicts with “{f.value}”.
            </span>
            <button
              className="why-btn why-primary"
              type="button"
              onClick={() => void keep(step.pair!, step.added)}
            >
              Use this value
            </button>
            <button className="why-btn why-quiet" type="button" onClick={() => go("#/r/")}>
              Settle it in Review
            </button>
          </div>
        ) : (
          <p className="why-small">
            Added beside it. This attribute isn't one answer two sources disagree on, so nothing was
            ended: <a href="#/r/">open Review</a> to settle what stays.
          </p>
        )
      ) : (
        <p className="why-small why-ok">{step.text}</p>
      )}
    </form>
  );
}
