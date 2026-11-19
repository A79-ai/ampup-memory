import { useCallback, useMemo, useSyncExternalStore } from "react";

// A view's options (a tab, a window, a row opened, a filter) ride in the link after the view, as a query in the
// hash: `#/not-answered?days=7&open=<entry>`. The view's own segments are URI-encoded, so a bare `?` only ever starts the
// options. A view's options are its own: moving to another view starts from its defaults.

/** Fired when the link changes without a `hashchange` (pushState and replaceState fire none). */
export const NAVIGATE = "memstore:navigate";

export function subscribe(on: () => void) {
  addEventListener("hashchange", on);
  addEventListener("popstate", on);
  addEventListener(NAVIGATE, on);
  return () => {
    removeEventListener("hashchange", on);
    removeEventListener("popstate", on);
    removeEventListener(NAVIGATE, on);
  };
}

/** The view in a hash, without its options. */
export const viewOf = (h: string) => {
  const i = h.indexOf("?");
  return i < 0 ? h : h.slice(0, i);
};

/** The options in a hash. */
export const optionsOf = (h: string) => {
  const i = h.indexOf("?");
  return new URLSearchParams(i < 0 ? "" : h.slice(i + 1));
};

/** Sets options on the link in place: a change of option isn't a step back would undo. */
export function setOptions(change: Record<string, string | null>) {
  const h = location.hash;
  const p = optionsOf(h);
  for (const [k, v] of Object.entries(change)) {
    if (v == null || v === "") p.delete(k);
    else p.set(k, v);
  }
  p.sort();
  // A page opened on its first view has no hash yet: the options still go after a `#`.
  const next = `${viewOf(h) || "#"}${p.size ? `?${p}` : ""}`;
  if (next === h) return;
  history.replaceState(history.state, "", `${location.pathname}${location.search}${next}`);
  dispatchEvent(new Event(NAVIGATE));
}

/** Replaces the view in the link, keeping its options: a pick within a view (a node of the graph). */
export function replaceView(view: string) {
  const h = location.hash;
  const i = h.indexOf("?");
  const next = `${view}${i < 0 ? "" : h.slice(i)}`;
  if (next === h) return;
  history.replaceState(history.state, "", `${location.pathname}${location.search}${next}`);
  dispatchEvent(new Event(NAVIGATE));
}

const readHash = () => location.hash;

/** One option of the view in the link, and a setter; the fallback when the link doesn't name it, which is then
 * left out of the link. */
export function useOption(key: string, fallback = ""): [string, (v: string | null) => void] {
  const hash = useSyncExternalStore(subscribe, readHash);
  const value = optionsOf(hash).get(key) ?? fallback;
  const set = useCallback(
    (v: string | null) => setOptions({ [key]: v === fallback ? null : v }),
    [key, fallback],
  );
  return [value, set];
}

/** An option that is one of `allowed` (a link naming another reads as the fallback). */
export function useChoice<T extends string | number>(
  key: string,
  allowed: readonly T[],
  fallback: T,
): [T, (v: T) => void] {
  const [raw, set] = useOption(key, String(fallback));
  const value = allowed.find((a) => String(a) === raw) ?? fallback;
  return [value, useCallback((v: T) => set(String(v)), [set])];
}

/** An on/off option. */
export function useFlag(key: string, fallback = false): [boolean, (v: boolean) => void] {
  const [raw, set] = useOption(key, fallback ? "1" : "0");
  return [raw === "1", useCallback((v: boolean) => set(v ? "1" : "0"), [set])];
}

/** An option holding a set, comma-separated in the link; the same set until the link changes it. */
export function useSetOption(key: string): [Set<string>, (v: Set<string>) => void] {
  const [raw, set] = useOption(key);
  const value = useMemo(() => new Set(raw ? raw.split(",") : []), [raw]);
  return [value, useCallback((v: Set<string>) => set([...v].sort().join(",")), [set])];
}

/**
 * The Explorer's link to what the memory learned from one source, or from one person (an org admin's), as a hash:
 * `#/explorer/entities?from=<source prefix>`, `#/explorer/entities?by=<person>`, both narrowing together when both are given, and
 * `#/explorer/entities/<type>?from=…` within one entity type. A host links to it after the path its Studio is on
 * (`/memory#/explorer/entities?from=slack%3AC0123%3A`). The prefix is what the source's episodes' sources start with, as
 * `/browse {op: "sources"}` names it.
 */
export function learnedLink(narrow: { from?: string; by?: string }, type?: string): string {
  const p = new URLSearchParams();
  if (narrow.by) p.set("by", narrow.by);
  if (narrow.from) p.set("from", narrow.from);
  p.sort();
  const at = ["entities", ...(type ? [type] : [])].map(encodeURIComponent).join("/");
  return `#/explorer/${at}${p.size ? `?${p}` : ""}`;
}
