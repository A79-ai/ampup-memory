"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { ApiError, browse } from "./api";
import type { Child, Focus, Listing, Selection } from "./types";

type Loaded = { children: Child[]; next: string | null; error?: string };

const key = (path: string[]) => JSON.stringify(path);

/** One page of a folder; a folder that can't be listed is shown with why. */
async function listing(path: string[], after: string | undefined, sel: Selection): Promise<Loaded> {
  try {
    const l = await browse<Listing>({ op: "ls", path, after }, sel);
    return { children: l.children, next: l.next };
  } catch (e) {
    const msg = e instanceof ApiError && e.status === 404 ? "nothing here" : String((e as Error).message);
    return { children: [], next: null, error: msg };
  }
}

/** The memory as a tree: folders load their children when first opened, a page at a time. Mounted
 * afresh for each org or team selected. */
export function Tree({
  sel,
  focus,
  onFocus,
  reveal,
}: {
  sel: Selection;
  focus: Focus | null;
  onFocus: (f: Focus) => void;
  /** A path to open down to (when the inspector jumps somewhere). */
  reveal: string[] | null;
}) {
  const [loaded, setLoaded] = useState<Record<string, Loaded>>({});
  // Folders opened or closed by hand; the rest are open when on the way to `reveal`.
  const [toggled, setToggled] = useState<Record<string, boolean>>({});
  const onTheWay = useMemo(
    () => new Set((reveal ?? []).map((_, i) => key(reveal!.slice(0, i)))),
    [reveal],
  );
  const isOpen = (path: string[]) => toggled[key(path)] ?? onTheWay.has(key(path));

  // A folder's listing (or the next page of it), merged into what is loaded once it arrives.
  const load = useCallback(
    (path: string[], after?: string) =>
      listing(path, after, sel).then((l) =>
        setLoaded((m) => {
          const prev = after ? (m[key(path)]?.children ?? []) : [];
          return { ...m, [key(path)]: { ...l, children: [...prev, ...l.children] } };
        }),
      ),
    [sel],
  );

  // The root, then every folder on the way to `reveal` not loaded yet.
  useEffect(() => {
    const want = [[], ...(reveal ?? []).map((_, i) => reveal!.slice(0, i))].filter((p) => !loaded[key(p)]);
    Promise.all(want.map((p) => listing(p, undefined, sel).then((l) => [key(p), l] as const))).then((got) =>
      setLoaded((m) => ({ ...Object.fromEntries(got), ...m })),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reveal, sel]);

  const toggle = (path: string[]) => {
    const open = !isOpen(path);
    setToggled((t) => ({ ...t, [key(path)]: open }));
    if (open && !loaded[key(path)]) load(path);
  };

  const selected = (c: Child) =>
    focus != null &&
    ((focus.kind === "episode" && c.kind === "episode" && focus.id === c.episode_id) ||
      ((focus.kind === "folder" || focus.kind === "entity" || focus.kind === "skill") &&
        key(focus.path) === key(c.path)));

  const render = (path: string[], depth: number): React.ReactNode => {
    const l = loaded[key(path)];
    if (!l) return <Row depth={depth} muted>loading…</Row>;
    if (l.error) return <Row depth={depth} muted>{l.error}</Row>;
    if (l.children.length === 0) return <Row depth={depth} muted>empty</Row>;
    return (
      <>
        {l.children.map((c) => {
          const k = key(c.path) + (c.kind === "episode" ? c.episode_id : "");
          const opened = c.kind === "folder" && isOpen(c.path);
          return (
            <div key={k}>
              <button
                className={`tree-row ${selected(c) ? "tree-row-on" : ""}`}
                style={{ paddingLeft: 8 + depth * 14 }}
                onClick={() => {
                  if (c.kind === "folder") {
                    toggle(c.path);
                    onFocus({ kind: "folder", path: c.path });
                  } else if (c.kind === "entity") onFocus({ kind: "entity", path: c.path });
                  else if (c.kind === "skill" || c.kind === "skill_index") onFocus({ kind: "skill", path: c.path });
                  else onFocus({ kind: "episode", id: c.episode_id! });
                }}
                title={c.when ? `${c.name}: ${c.when}` : c.name}
              >
                <span className="tree-caret">{c.kind === "folder" ? (opened ? "▾" : "▸") : ""}</span>
                <Icon kind={c.kind} />
                <span className="tree-name">{label(c)}</span>
                {c.count != null && <span className="tree-count">{c.count}</span>}
              </button>
              {opened && render(c.path, depth + 1)}
            </div>
          );
        })}
        {l.next && (
          <button className="tree-more" style={{ paddingLeft: 30 + depth * 14 }} onClick={() => load(path, l.next!)}>
            more…
          </button>
        )}
      </>
    );
  };

  return <nav className="tree" aria-label="Memory tree">{render([], 0)}</nav>;
}

function Row({ depth, muted, children }: { depth: number; muted?: boolean; children: React.ReactNode }) {
  return (
    <div className={`tree-note ${muted ? "muted" : ""}`} style={{ paddingLeft: 30 + depth * 14 }}>
      {children}
    </div>
  );
}

/** How a child is named in the tree: an episode listed by its id shows the id's start and its date. */
export function label(c: Child): string {
  if (c.kind === "episode" && c.name === c.episode_id) {
    const d = c.occurred_at ? new Date(c.occurred_at / 1000).toISOString().slice(0, 10) : "";
    return `${c.name.slice(0, 10)}…  ${d}`;
  }
  return c.name || "∅";
}

export function Icon({ kind }: { kind: string }) {
  const d =
    kind === "folder"
      ? "M2 4.5A1.5 1.5 0 0 1 3.5 3h3l1.5 1.5h4.5A1.5 1.5 0 0 1 14 6v5.5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5z"
      : kind === "entity"
        ? "M8 2.5 13.5 5.5v5L8 13.5 2.5 10.5v-5z M2.5 5.5 8 8.5l5.5-3 M8 8.5v5"
        : kind === "skill"
          ? "M9 1.5 3.5 9H8l-1 5.5L12.5 7H8z"
          : kind === "skill_index"
            ? "M3 3.5h10 M3 6.5h10 M3 9.5h10 M3 12.5h6"
            : "M4 2h5l3 3v9H4z M9 2v3h3 M6 8.5h4 M6 11h4";
  return (
    <svg className={`icon icon-${kind}`} viewBox="0 0 16 16" width="14" height="14" aria-hidden>
      <path d={d} fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
    </svg>
  );
}
