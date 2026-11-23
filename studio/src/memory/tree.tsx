import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, browse, keptBrowse } from "./api";
import { displayName } from "./format";
import type { Child, Focus, Listing, Selection } from "./types";
import { Icon } from "./ui";

/** `kept`: what the folder's first page last listed on this page, shown until it is read again. */
type Loaded = { children: Child[]; next: string | null; error?: string; kept?: boolean };

const key = (path: string[]) => JSON.stringify(path);

/** One page of a folder; a folder that can't be listed is shown with why. */
async function listing(path: string[], after: string | undefined, sel: Selection): Promise<Loaded> {
  try {
    const l = await browse<Listing>({ op: "ls", path, after }, sel);
    return { children: l.children, next: l.next };
  } catch (e) {
    const msg =
      e instanceof ApiError && e.status === 404 ? "nothing here" : String((e as Error).message);
    return { children: [], next: null, error: msg };
  }
}

/** A folder's first page as last listed on this page, if it was. */
function keptListing(path: string[], sel: Selection): Loaded | undefined {
  const l = keptBrowse<Listing>({ op: "ls", path }, sel);
  return l && { children: l.children, next: l.next, kept: true };
}

/** Whether `path` is the sessions' day folders, `sessions/ended/<day>`, each day as UTC names it. */
export const isSessionDays = (path: string[]) =>
  path.length === 2 && path[0] === "sessions" && path[1] === "ended";

/** A folder's children in the order to show them: the sessions' days newest first (the memory lists every folder
 * by name, so the oldest day comes first). Only a whole listing is turned; one still paging keeps the memory's
 * order, so "Show more" carries on from where it stopped. */
export function ordered(path: string[], children: Child[], next: string | null): Child[] {
  return isSessionDays(path) && !next ? [...children].reverse() : children;
}

/** The memory as a tree: folders load their children when first opened, a page at a time. Mounted
 * afresh for each org or team selected. Hidden (`shown` false: a closed drawer), it reads nothing: the folders on
 * the way to `reveal` are read once it shows. */
export function Tree({
  sel,
  focus,
  onFocus,
  reveal,
  shown = true,
}: {
  sel: Selection;
  focus: Focus | null;
  onFocus: (f: Focus) => void;
  /** A path to open down to (when the inspector jumps somewhere). */
  reveal: string[] | null;
  /** Whether the tree is on screen. */
  shown?: boolean;
}) {
  const [loaded, setLoaded] = useState<Record<string, Loaded>>(() => {
    const seen: Record<string, Loaded> = {};
    for (const p of [[], ...(reveal ?? []).map((_, i) => reveal!.slice(0, i))]) {
      const l = keptListing(p, sel);
      if (l) seen[key(p)] = l;
    }
    return seen;
  });
  // Folders opened or closed by hand; the rest are open when on the way to `reveal`.
  const [toggled, setToggled] = useState<Record<string, boolean>>({});
  const onTheWay = useMemo(
    () => new Set((reveal ?? []).map((_, i) => key(reveal!.slice(0, i)))),
    [reveal],
  );
  const isOpen = (path: string[]) => toggled[key(path)] ?? onTheWay.has(key(path));

  // A folder's listing (or the next page of it), merged into what is loaded once it arrives. Opened by hand
  // (`chain`), a folder whose only child is a folder opens that one too, and so on down (`slack` → its workspace →
  // `thread`), so a run of single folders takes one click, not one each.
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);
  const load = useCallback(
    (path: string[], after?: string, chain = false): Promise<void> =>
      listing(path, after, sel).then((l) => {
        if (!live.current) return;
        setLoaded((m) => {
          const prev = after ? (m[key(path)]?.children ?? []) : [];
          return { ...m, [key(path)]: { ...l, children: [...prev, ...l.children] } };
        });
        const only = !after && !l.next && l.children.length === 1 ? l.children[0] : undefined;
        if (chain && only?.kind === "folder") {
          setToggled((t) => (key(only.path) in t ? t : { ...t, [key(only.path)]: true }));
          return load(only.path, undefined, true);
        }
      }),
    [sel],
  );

  // The root, then every folder on the way to `reveal` not read yet on this mount: a kept one shown
  // meanwhile is read again, and replaced unless more of it was paged in since.
  useEffect(() => {
    if (!shown) return;
    const want = [[], ...(reveal ?? []).map((_, i) => reveal!.slice(0, i))].filter(
      (p) => !loaded[key(p)] || loaded[key(p)]!.kept,
    );
    Promise.all(want.map((p) => listing(p, undefined, sel).then((l) => [key(p), l] as const))).then(
      (got) =>
        setLoaded((m) => ({
          ...m,
          ...Object.fromEntries(got.filter(([k]) => !m[k] || m[k].kept)),
        })),
    );
  }, [reveal, sel, shown]);

  const toggle = (path: string[]) => {
    const open = !isOpen(path);
    setToggled((t) => ({ ...t, [key(path)]: open }));
    if (open && (!loaded[key(path)] || loaded[key(path)]!.kept)) {
      const l = !loaded[key(path)] && keptListing(path, sel);
      if (l) setLoaded((m) => ({ ...m, [key(path)]: l }));
      load(path, undefined, true);
    }
  };

  const selected = (c: Child) =>
    focus != null &&
    ((focus.kind === "episode" && c.kind === "episode" && focus.id === c.episode_id) ||
      ((focus.kind === "folder" || focus.kind === "entity" || focus.kind === "skill") &&
        key(focus.path) === key(c.path)));

  const render = (path: string[], depth: number): React.ReactNode => {
    const l = loaded[key(path)];
    if (!l)
      return (
        <Row depth={depth} muted>
          Loading…
        </Row>
      );
    if (l.error)
      return (
        <Row depth={depth} muted>
          {l.error}
        </Row>
      );
    if (l.children.length === 0)
      return (
        <Row depth={depth} muted>
          Empty
        </Row>
      );
    return (
      <>
        {ordered(path, l.children, l.next).map((c) => {
          const k = key(c.path) + (c.kind === "episode" ? c.episode_id : "");
          const opened = c.kind === "folder" && isOpen(c.path);
          return (
            <div key={k}>
              <button
                className={`tree-row ${selected(c) ? "tree-row-on" : ""}`}
                style={{ paddingLeft: 6 + depth * 14 }}
                onClick={() => {
                  if (c.kind === "folder") {
                    toggle(c.path);
                    onFocus({ kind: "folder", path: c.path });
                  } else if (c.kind === "entity") onFocus({ kind: "entity", path: c.path });
                  else if (c.kind === "skill" || c.kind === "skill_index" || c.kind === "advice")
                    onFocus({ kind: "skill", path: c.path });
                  else onFocus({ kind: "episode", id: c.episode_id! });
                }}
                title={
                  c.when
                    ? `${c.name}: ${c.when}`
                    : isSessionDays(path)
                      ? `${c.name}, a day in UTC`
                      : c.name
                }
              >
                <span className={`tree-caret ${opened ? "open" : ""}`}>
                  {c.kind === "folder" && <Icon name="chevron" size={12} />}
                </span>
                <Icon name={c.kind} size={15} />
                <span className="tree-name">{label(c)}</span>
                {c.count != null && <span className="tree-count">{c.count.toLocaleString()}</span>}
              </button>
              {opened && render(c.path, depth + 1)}
            </div>
          );
        })}
        {l.next && (
          <button
            className="tree-more"
            style={{ paddingLeft: 28 + depth * 14 }}
            onClick={() => load(path, l.next!)}
          >
            Show more
          </button>
        )}
      </>
    );
  };

  return (
    <nav className="tree" aria-label="Memory tree">
      {render([], 0)}
    </nav>
  );
}

function Row({
  depth,
  muted,
  children,
}: {
  depth: number;
  muted?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div className={`tree-note ${muted ? "muted" : ""}`} style={{ paddingLeft: 28 + depth * 14 }}>
      {children}
    </div>
  );
}

/** How a child is named in the tree: an episode listed by its id shows the id's start and its date; an entity, its
 * name (never a bare opaque id). */
export function label(c: Child): string {
  if (c.title) return c.title;
  if (c.kind === "entity" && c.entity) return displayName(c.entity, c);
  if (c.kind === "episode" && c.name === c.episode_id) {
    const d = c.occurred_at ? new Date(c.occurred_at / 1000).toISOString().slice(0, 10) : "";
    return d ? `${d} · ${c.name.slice(0, 8)}` : `${c.name.slice(0, 10)}…`;
  }
  return c.name || "∅";
}
