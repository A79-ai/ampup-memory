import { entityName } from "./format";
import type { Focus } from "./types";
import { Icon } from "./ui";
import "./learn.css";

// A receipt: what the memory learned from something it was just given (files added by hand, an answer kept, and
// later a source's sync or a dream pass), each item linked to where it now lives in the Studio, and what was
// refused or skipped, with why. Any surface that can say what it read and what came of it passes the same shape.

/** One fact on a receipt. */
export type ReceiptFact = {
  fact_id: string;
  entity: string;
  predicate: string;
  value: string;
};

/** What one thing given to the memory became. */
export type ReceiptItem = {
  /** What it was, as the person would name it: a file's name, "Pasted text", the question an answer was to. */
  name: string;
  /** The episode it is kept as, once kept: opened from the receipt. */
  episodeId?: string;
  /**
   * `pending`: kept, still being read for facts. `read`: read. `failed`: the reader gave up on it. `not_read`:
   * kept, but nothing here reads it for facts. `refused`: not kept at all. `unknown`: not found, or not yours to read.
   */
  status: "pending" | "read" | "failed" | "not_read" | "refused" | "unknown";
  /** Why it was refused, failed or not read. */
  reason?: string;
  /** False when the same text was already in memory, so nothing new was kept. */
  new?: boolean;
  /** The facts it gave. */
  facts: ReceiptFact[];
  /** The entities those facts are about, with how many each, and whether memory knew of it before. */
  entities: { entity: string; facts: number; new: boolean }[];
  /** What it said that memory already held: the facts it stated again. */
  known: ReceiptFact[];
  /** Facts the reader found and dropped (a schema refused them, say), and why the first was. */
  dropped?: { facts: number; why: string };
};

export type ReceiptProps = {
  /** What happened, as a heading: "Added to memory", "Synced from Slack", "Dream pass". */
  title?: string;
  /** Where it all came from, in a phrase ("notes.md"); by default the items' names. */
  source?: string;
  items: ReceiptItem[];
  /** Whether the memory reads for facts what was given: `on` where any of it is read or will be, else `off` (its
   * admins turned it off, and its schema reads none of it) or `none` (no model). */
  extraction?: "on" | "off" | "none";
  /** Still not read after waiting a while: said so, rather than as never to be read. */
  stalled?: boolean;
  /** Opens an entity or an episode in the Studio; without it, the receipt's links move the page's hash. */
  onOpen?: (f: Focus) => void;
  /** Fewer details: for a receipt inline under an answer. */
  compact?: boolean;
  onDismiss?: () => void;
};

/** An entity's place in the Explorer, as the Studio's links name it. */
export function entityFocus(entity: string): Focus {
  const i = entity.indexOf(":");
  const path =
    i < 0 ? ["entities", "_", entity] : ["entities", entity.slice(0, i), entity.slice(i + 1)];
  return { kind: "entity", path };
}

function hashOf(f: Focus): string {
  if (f.kind === "entity") return `#/entity/${f.path.map(encodeURIComponent).join("/")}`;
  if (f.kind === "episode") return `#/episode/${encodeURIComponent(f.id)}`;
  return "";
}

const plural = (n: number, one: string, many = `${one}s`) =>
  `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** What the memory learned from what it was given, as it learns it. */
export function Receipt({
  title = "Added to memory",
  source,
  items,
  extraction = "on",
  stalled = false,
  onOpen,
  compact = false,
  onDismiss,
}: ReceiptProps) {
  const kept = items.filter((i) => i.status !== "refused" && i.status !== "unknown");
  const pending = items.some((i) => i.status === "pending");
  // Read, or being read: then extraction is on for it, whatever the receipt was told before.
  const reading = items.some((i) => i.status === "read" || i.status === "pending");
  const unread = kept.length > 0 && kept.every((i) => i.status === "not_read" || i.status === "failed");
  const facts = items.flatMap((i) => i.facts);
  const entities = new Map<string, { facts: number; new: boolean }>();
  for (const i of items)
    for (const e of i.entities) {
      const was = entities.get(e.entity);
      entities.set(e.entity, {
        facts: (was?.facts ?? 0) + e.facts,
        new: (was?.new ?? true) && e.new,
      });
    }
  const known = items.flatMap((i) => i.known);
  const from =
    source ??
    (kept.length === 1
      ? kept[0]!.name
      : kept.length > 1
        ? `${kept[0]!.name} and ${plural(kept.length - 1, "more", "more")}`
        : "");
  const link = (f: Focus, label: React.ReactNode, cls: string, title?: string) =>
    onOpen ? (
      <button type="button" className={cls} title={title} onClick={() => onOpen(f)}>
        {label}
      </button>
    ) : (
      <a className={cls} title={title} href={hashOf(f)}>
        {label}
      </a>
    );
  const headline = pending
    ? facts.length
      ? `Learning… ${plural(facts.length, "fact")} so far`
      : "Reading it for facts…"
    : kept.length === 0
      ? "Nothing was added"
      : facts.length === 0 && stalled
        ? "Kept; still waiting to be read for facts"
        : facts.length === 0 && unread
          ? "Kept, not read for facts"
          : facts.length === 0
            ? "Kept: nothing new learned"
            : `Learned ${plural(entities.size, "entity", "entities")}, ${plural(facts.length, "fact")}`;

  return (
    <section className={`card receipt ${compact ? "receipt-compact" : ""}`} aria-live="polite">
      <header className="receipt-head">
        <span className={`receipt-mark ${pending ? "is-pending" : kept.length ? "is-done" : ""}`}>
          <Icon name={pending ? "clock" : kept.length ? "check" : "alert"} size={14} />
        </span>
        <div className="receipt-head-text">
          <div className="receipt-eyebrow">{title}</div>
          <div className="receipt-title">
            {headline}
            {from && <span className="receipt-from"> from {from}</span>}
          </div>
        </div>
        {onDismiss && (
          <button type="button" className="icon-btn" aria-label="Dismiss" onClick={onDismiss}>
            <Icon name="close" size={14} />
          </button>
        )}
      </header>

      {extraction !== "on" && !reading && facts.length === 0 && (
        <div className="receipt-note">
          {extraction === "off"
            ? "Extraction is turned off for this memory: what you add is kept and searchable as text, but no facts are drawn from it."
            : "This server has no extraction model, so what you add is kept and searchable as text, but no facts are drawn from it."}
        </div>
      )}

      {entities.size > 0 && (
        <div className="receipt-block">
          <div className="receipt-label">Entities</div>
          <div className="chips">
            {[...entities].map(([e, v]) => {
              const n = entityName(e);
              return (
                <span key={e} className="receipt-entity">
                  {link(
                    entityFocus(e),
                    <>
                      <Icon name="entity" size={12} />
                      {n.type && <span className="chip-type">{n.type}</span>}
                      {n.name}
                      <span className="receipt-count">{v.facts}</span>
                    </>,
                    "chip",
                    e,
                  )}
                  {!v.new && <span className="receipt-tag">known</span>}
                </span>
              );
            })}
          </div>
        </div>
      )}

      {facts.length > 0 && !compact && (
        <div className="receipt-block">
          <div className="receipt-label">Facts</div>
          <ul className="receipt-facts">
            {facts.slice(0, 50).map((f) => (
              <FactLine key={f.fact_id} f={f} link={link} />
            ))}
          </ul>
          {facts.length > 50 && (
            <div className="muted receipt-more">and {plural(facts.length - 50, "more fact")}</div>
          )}
        </div>
      )}

      {known.length > 0 && (
        <div className="receipt-block">
          <div className="receipt-label">Already known</div>
          <ul className="receipt-facts receipt-known">
            {known.slice(0, compact ? 5 : 30).map((f) => (
              <FactLine key={f.fact_id} f={f} link={link} />
            ))}
          </ul>
        </div>
      )}

      <ul className="receipt-items">
        {items.map((i, n) => (
          <li key={`${i.episodeId ?? ""}${n}`} className={`receipt-item is-${i.status}`}>
            <Icon
              name={
                i.status === "refused" || i.status === "failed" || i.status === "unknown"
                  ? "alert"
                  : i.status === "pending"
                    ? "clock"
                    : "doc"
              }
              size={13}
            />
            <span className="receipt-item-name">
              {i.episodeId && i.status !== "unknown"
                ? link({ kind: "episode", id: i.episodeId }, i.name, "link", "Open what was kept")
                : i.name}
            </span>
            <span className="receipt-item-what">{itemLine(i)}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function FactLine({
  f,
  link,
}: {
  f: ReceiptFact;
  link: (f: Focus, label: React.ReactNode, cls: string, title?: string) => React.ReactNode;
}) {
  const n = entityName(f.entity);
  return (
    <li className="receipt-fact">
      {link(entityFocus(f.entity), n.name, "link receipt-fact-entity", f.entity)}
      <span className="receipt-fact-pred">{f.predicate.replace(/_/g, " ")}</span>
      <span className="receipt-fact-value">{f.value}</span>
    </li>
  );
}

/** What became of one item, in a few words. */
function itemLine(i: ReceiptItem): string {
  switch (i.status) {
    case "refused":
      return `not added: ${i.reason ?? "refused"}`;
    case "unknown":
      return "not found, or not yours to read";
    case "pending":
      return i.new === false ? "already in memory; reading…" : "kept; reading it for facts…";
    case "failed":
      return `kept; ${i.reason ?? "it couldn't be read for facts"}`;
    case "not_read":
      return `kept; ${i.reason ?? "not read for facts"}`;
    case "read": {
      const parts = [
        i.facts.length
          ? plural(i.facts.length, "fact")
          : i.known.length
            ? "no new facts"
            : "nothing new: already known, or nothing a fact could hold",
        ...(i.known.length ? [`${plural(i.known.length, "fact")} already known`] : []),
        ...(i.dropped ? [`${plural(i.dropped.facts, "fact")} dropped (${i.dropped.why})`] : []),
      ];
      return `${i.new === false ? "already in memory · " : ""}${parts.join(" · ")}`;
    }
  }
}
