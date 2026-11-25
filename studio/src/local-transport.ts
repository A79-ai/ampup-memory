import type { Selection } from "./memory/types";
import { ApiError, failedWithout, type Area, type Capabilities, type Transport } from "./transport";

/**
 * The memory of the server that serves these pages (`ampup-memory serve --local`), on its own routes and from
 * the same origin: no sign-in, no CORS. That server reads every request as its one caller and serves one memory,
 * and refuses a request naming an org (`?org=`, `x-memstore-org`): none is ever sent, and the selection's org is
 * only the pages' name for it. The branch read goes as `?branch=`.
 *
 * Admin reads (People, Branches, traces) and owner review are probed once (the server's `/me` says): they appear
 * when the server lets its caller administer its own memory.
 */
export class LocalTransport implements Transport {
  readonly capabilities: Capabilities = {
    admin: "probe",
    review: "probe",
    lessons: "probe",
    agent: true,
    teams: false,
    // The org's settings are the hosted directory's: a local server keeps none to change.
    settings: false,
    learn: true,
  };

  /** `base`: where the server answers, when not the pages' own origin (a dev server proxies it instead). */
  constructor(private readonly base = "") {}

  async call<T>(sel: Selection, area: Area, body: Record<string, unknown> = {}): Promise<T> {
    switch (area) {
      case "browse":
        return this.send<T>("POST", "/browse", sel, body);
      case "search": {
        // The best 30 facts, current and past (memstore's `/search` takes a batch of calls).
        const r = await this.send<{ replies?: unknown[][] }>("POST", "/search", sel, {
          calls: [[body.query, 30, -1]],
        });
        return { facts: r.replies?.[0] ?? [] } as T;
      }
      case "me":
        return this.send<T>("GET", "/me", sel);
      case "lessons":
        return this.send<T>("POST", "/lessons", sel, body);
      case "mcp":
        return this.send<T>("POST", "/mcp", sel, {
          jsonrpc: "2.0",
          id: 1,
          method: body.method,
          params: body.params ?? {},
        });
      case "agent": {
        const { maxToolCalls, ...ask } = body;
        return this.send<T>("POST", "/agent/run", sel, { ...ask, max_tool_calls: maxToolCalls });
      }
      case "admin":
        // The org admins' ops, as memstore's `/admin` names them; the memory is the server's, so no org.
        return this.send<T>("POST", "/admin", {}, adminOp(body));
      case "review":
        return this.send<T>("POST", "/admin", {}, body);
      case "learn": {
        const { op, ...rest } = body;
        return this.send<T>("POST", op === "status" ? "/learn/status" : "/learn", sel, rest);
      }
    }
  }

  private async send<T>(
    method: "GET" | "POST",
    path: string,
    sel: { branch?: string },
    body?: unknown,
  ): Promise<T> {
    const q =
      sel.branch && sel.branch !== "main" ? `?branch=${encodeURIComponent(sel.branch)}` : "";
    const res = await fetch(`${this.base}${path}${q}`, {
      method,
      headers: body === undefined ? {} : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let data: Record<string, unknown>;
    try {
      data = text ? (JSON.parse(text) as Record<string, unknown>) : {};
    } catch {
      // A gateway's own page (HTML) says nothing the pages can show: its status says it all.
      data = {
        error: [502, 503, 504].includes(res.status)
          ? failedWithout(res.status)
          : text.slice(0, 500) || res.statusText,
      };
    }
    if (!res.ok) {
      const error = typeof data.error === "string" ? data.error : failedWithout(res.status);
      throw new ApiError(res.status, error, data);
    }
    return data as T;
  }
}

/** An `admin` call's body as memstore's `/admin` takes it (the pages' names for two of its ops differ). */
function adminOp(body: Record<string, unknown>): Record<string, unknown> {
  if (body.op === "set_activity") return { op: "set_settings", settings: { activity: body.on } };
  if (body.op === "activity") {
    const { sinceDays, ...rest } = body;
    return typeof sinceDays === "number" ? { ...rest, since_secs: sinceDays * 86_400 } : rest;
  }
  return body;
}
