import { ApiError, transport, type Area } from "../transport";
import { rememberNames } from "./format";
import type { Selection } from "./types";

export { ApiError };

/**
 * One call to the memory through the host's transport (`Transport.call`), which reads it as the caller: the
 * org is the transport's to place, the team and branch come from the selection.
 */
export function call<T>(
  sel: Selection,
  area: Area,
  body: Record<string, unknown> = {},
): Promise<T> {
  return transport()
    .call<T>(sel, area, body)
    .then((answer) => {
      rememberNames(answer, JSON.stringify([sel.org, sel.team ?? "", sel.branch ?? "main"]));
      return answer;
    });
}

// The last answer to each read, for as long as the page is open: a pane opened again shows it at once
// while a fresh read replaces it, so going back to the Explorer doesn't wait on the memory again.
const kept = new Map<string, unknown>();
const KEEP = 300;
const keyOf = (area: string, body: Record<string, unknown>, sel: Selection) =>
  JSON.stringify([sel.org, sel.team ?? "", sel.branch ?? "main", area, body]);

/** The reads on their way: a second ask for one (two sections reading the root at once) waits on the first. */
const going = new Map<string, Promise<unknown>>();

function read<T>(
  area: "browse" | "search",
  body: Record<string, unknown>,
  sel: Selection,
): Promise<T> {
  const k = keyOf(area, body, sel);
  const on = going.get(k);
  if (on) return on as Promise<T>;
  const p = call<T>(sel, area, body)
    .then((data) => {
      kept.delete(k);
      kept.set(k, data);
      if (kept.size > KEEP) kept.delete(kept.keys().next().value!);
      return data;
    })
    .finally(() => going.delete(k));
  going.set(k, p);
  return p;
}

/** What `browse(req, sel)` last answered on this page, if it has. */
export const keptBrowse = <T>(req: Record<string, unknown>, sel: Selection) =>
  kept.get(keyOf("browse", req, sel)) as T | undefined;
/** What `search(query, sel)` last answered on this page, if it has. */
export const keptSearch = <T>(query: string, sel: Selection) =>
  kept.get(keyOf("search", { query }, sel)) as T | undefined;

export const browse = <T>(req: Record<string, unknown>, sel: Selection) =>
  read<T>("browse", req, sel);
export const me = <T>(sel: Selection) => call<T>(sel, "me");
export const search = <T>(query: string, sel: Selection) => read<T>("search", { query }, sel);

export { when } from "./format";
