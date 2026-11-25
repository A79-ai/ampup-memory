import { useEffect, useState } from "react";
import { call } from "./api";
import type { Focus, Selection } from "./types";
import { Icon } from "./ui";

// Import's Processing panel: the person's own latest imports (memstore's `/learn/status {recent}`), what is still in
// flight (read for facts, then compiled into skills where this memory compiles them), and what each one gave.

/** How many of the latest imports the panel shows. */
const RECENT = 12;
/** How often it reads again while something is in flight, and while nothing is. */
const BUSY_MS = 3000;
const IDLE_MS = 30_000;
/** How long it reads quickly while the same imports stay in flight; then at the idle pace, saying so. */
const BUSY_FOR_MS = 5 * 60_000;

type Item = {
  episode_id: string;
  name?: string;
  kind?: string;
  added_at?: number;
  status: "pending" | "read" | "failed" | "not_read" | "unknown";
  compile?: "compiled" | "pending" | "failed" | "off";
  facts?: unknown[];
  skills?: { slug: string; title: string }[];
  reason?: string;
};
type Recent = { done: boolean; compiling?: boolean; items: Item[] };

const inFlight = (i: Item) => i.status === "pending" || i.compile === "pending";

/** What an import is doing now, in a few words. */
function stage(i: Item): string {
  if (i.status === "pending") return "Reading for facts…";
  if (i.compile === "pending") return "Compiling into skills…";
  if (i.status === "failed") return i.reason ?? "Couldn't be read";
  if (i.compile === "failed") return "Couldn't be compiled into skills";
  const facts = i.facts?.length ?? 0;
  const parts = [`${facts} ${facts === 1 ? "fact" : "facts"}`];
  if (i.compile === "compiled") {
    const n = i.skills?.length ?? 0;
    parts.push(n ? `${n} ${n === 1 ? "skill" : "skills"}` : "no procedure");
  }
  return parts.join(" · ");
}

const ago = (micros?: number) => {
  if (!micros) return "";
  const s = Math.max(0, (Date.now() - micros / 1000) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};

/** The panel. `bump` changes when something was just added, to read again at once. */
export function ImportActivity({
  sel,
  bump,
  onOpen,
}: {
  sel: Selection;
  bump: number;
  onOpen: (f: Focus) => void;
}) {
  const [items, setItems] = useState<Item[] | null>(null);
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    // When what is in flight now first was: the same imports in flight for long are read at the idle pace.
    let since = 0;
    let busy = "";
    const poll = () =>
      call<Recent>(sel, "learn", { op: "status", recent: RECENT })
        .then((r) => {
          if (!live) return;
          setItems(r.items);
          const now = r.items
            .filter(inFlight)
            .map((i) => i.episode_id)
            .join(",");
          if (now !== busy) {
            busy = now;
            since = Date.now();
          }
          const long = !!now && Date.now() - since > BUSY_FOR_MS;
          setSlow(long);
          timer = setTimeout(poll, now && !long ? BUSY_MS : IDLE_MS);
        })
        .catch(() => {
          if (live) timer = setTimeout(poll, IDLE_MS);
        });
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [sel.org, sel.team, sel.branch, bump]);

  const flying = (items ?? []).filter(inFlight);
  const done = (items ?? []).filter((i) => !inFlight(i));
  const row = (i: Item) => (
    <li key={i.episode_id} className="imp-row">
      <Icon name={i.kind === "web" ? "search" : "doc"} size={13} />
      <div className="imp-main">
        <button
          type="button"
          className="imp-name"
          title={i.name}
          onClick={() => onOpen({ kind: "episode", id: i.episode_id })}
        >
          {(i.name ?? i.episode_id.slice(0, 12)).replace(/^https:\/\//, "")}
        </button>
        <div className={`imp-stage ${inFlight(i) ? "is-busy" : ""}`}>
          {inFlight(i) && <span className="imp-dot" aria-hidden />}
          {stage(i)}
          {!inFlight(i) && i.added_at ? (
            <span className="muted"> · {ago(i.added_at)}</span>
          ) : null}
        </div>
        {i.skills && i.skills.length > 0 && (
          <div className="imp-skills">
            {i.skills.map((s) => (
              <button
                key={s.slug}
                type="button"
                className="chip"
                onClick={() =>
                  onOpen({ kind: "skill", path: ["skills", s.slug] })
                }
              >
                <Icon name="skill" size={11} /> {s.title}
              </button>
            ))}
          </div>
        )}
      </div>
    </li>
  );
  return (
    <aside className="card imp-panel" aria-label="Processing">
      <div className="imp-head">Processing</div>
      <div className="imp-body">
        {flying.length ? (
          <>
            <ul className="imp-list">{flying.map(row)}</ul>
            {slow && (
              <p className="muted imp-slow">
                Taking longer than usual: this panel checks again every half minute.
              </p>
            )}
          </>
        ) : (
          <div className={`imp-empty ${done.length ? "is-compact" : ""}`}>
            <Icon name="skill" size={18} />
            <strong>Nothing in flight</strong>
            <span className="muted">
              Imports appear here while they process.
            </span>
          </div>
        )}
        {done.length > 0 && (
          <>
            <div className="imp-sub muted">Recent</div>
            <ul className="imp-list">{done.map(row)}</ul>
          </>
        )}
      </div>
    </aside>
  );
}
