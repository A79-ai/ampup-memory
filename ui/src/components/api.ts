import type { Selection } from "./types";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function call<T>(url: string, body?: unknown): Promise<T> {
  const r = await fetch(url, {
    method: body === undefined ? "GET" : "POST",
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const v = await r.json().catch(() => ({}));
  if (!r.ok) throw new ApiError(r.status, v?.error ?? r.statusText);
  return v as T;
}

export const browse = <T>(req: Record<string, unknown>, sel: Selection) =>
  call<T>("/api/browse", { ...req, ns: sel.ns, sel });
export const me = <T>(sel: Selection) => call<T>("/api/me", { ns: sel.ns, sel });
export const search = <T>(query: string, sel: Selection) =>
  call<T>("/api/search", { query, ns: sel.ns, sel });
export const namespaces = <T>() => call<T>("/api/namespaces");

/** Microseconds since the epoch, as a short local date and time. */
export function when(us: number | null | undefined): string {
  if (us == null) return "";
  const d = new Date(us / 1000);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
