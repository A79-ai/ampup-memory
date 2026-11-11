import "server-only";

import { idToken } from "./auth";
import { gatewayCall, viaGateway } from "./gateway";
import { baseUrl, forget, validNamespace } from "./gke";

// The memstores this UI reads, called only from route handlers (the browser never talks to memstore, so
// memstore needs no CORS):
// - hosted (e.g. on Vercel), as the signed-in person with their own Google ID token (./auth.ts): the memstore
//   at `AMPUP_MEMORY_URL`, and a Kubernetes cluster's namespaces through the gateway at `AMPUP_MEMORY_GATEWAY_URL`
//   (./gateway.ts), either or both;
// - otherwise a Kubernetes cluster's, one per `memstore-*` namespace, through port-forwards from where this runs
//   (./gke.ts): a laptop, or the gateway itself.
// Each setting is AMPUP_MEMORY_*, still read under its old name (MEMSTORE_*) when the new one isn't set.
const HOSTED = (process.env.AMPUP_MEMORY_URL ?? process.env.MEMSTORE_URL)?.replace(/\/$/, "");

/** How this UI reaches memstore, as the page needs to know it. */
export function mode(): { hosted: boolean; label: string } {
  if (HOSTED) return { hosted: true, label: process.env.AMPUP_MEMORY_LABEL ?? process.env.MEMSTORE_LABEL ?? new URL(HOSTED).host };
  return { hosted: viaGateway(), label: "Kubernetes" };
}

/** The namespace value that names `AMPUP_MEMORY_URL`'s memstore, beside the gateway's memstore-* ones. */
export const hostedLabel = (): string | null => (HOSTED ? mode().label : null);

/** The paths a hosted UI may send through the gateway, and the gateway may forward. */
export const PATHS = ["/browse", "/me", "/search", "/mcp"];

export type Selection = { scope?: string; org?: string; team?: string; branch?: string };

/** Which memory to read: a scope by name (the dev loop's servers read any in their catalog), or memstore's
 * `?org=&team=` selection, and on which branch (main unless named). */
function query(sel: Selection | undefined): string {
  const q = new URLSearchParams();
  if (sel?.scope) q.set("scope", sel.scope);
  if (sel?.org) q.set("org", sel.org);
  if (sel?.team) q.set("team", sel.team);
  if (sel?.branch) q.set("branch", sel.branch);
  const s = q.toString();
  return s ? `?${s}` : "";
}

/** One call to memstore (hosted, or a namespace's), its status and JSON answer passed back as they are. */
export async function memstore(
  req: Request,
  ns: unknown,
  path: string,
  init: { body?: unknown; sel?: Selection; method?: "GET" | "POST" },
): Promise<Response> {
  if (HOSTED || viaGateway()) {
    const token = await idToken(req).catch(() => null);
    if (!token) return Response.json({ error: "sign in", signIn: true }, { status: 401 });
    if (HOSTED && (ns === mode().label || !viaGateway())) {
      return call(`${HOSTED}${path}${query(init.sel)}`, init, path, HOSTED, token);
    }
    if (!validNamespace(ns)) return Response.json({ error: "pick a memstore- namespace" }, { status: 400 });
    return gatewayCall("/api/raw", token, { ns, path, method: init.method ?? "POST", body: init.body, sel: init.sel });
  }
  if (!validNamespace(ns)) return Response.json({ error: "pick a memstore- namespace" }, { status: 400 });
  // A port-forward dies with its pod: one retry through a fresh one.
  for (let attempt = 0; ; attempt++) {
    let base: string;
    try {
      base = await baseUrl(ns);
    } catch (e) {
      return Response.json({ error: `can't reach ${ns}: ${(e as Error).message}` }, { status: 502 });
    }
    const r = await call(`${base}${path}${query(init.sel)}`, init, path, ns);
    if (r.status === 599) {
      forget(ns);
      if (attempt === 0) continue;
      return Response.json({ error: `${ns}'s AmpUp Memory server isn't answering` }, { status: 502 });
    }
    return r;
  }
}

/** The request, its answer as JSON with memstore's status (599: no answer at all). */
async function call(
  url: string,
  init: { body?: unknown; method?: "GET" | "POST" },
  path: string,
  name: string,
  bearer?: string,
): Promise<Response> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (bearer) headers.authorization = `Bearer ${bearer}`;
  let r: Response;
  try {
    r = await fetch(url, {
      method: init.method ?? "POST",
      headers,
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
      cache: "no-store",
    });
  } catch {
    return Response.json({ error: `${name} isn't answering` }, { status: bearer ? 502 : 599 });
  }
  const text = await r.text();
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch {
    json = { error: text.slice(0, 500) || r.statusText };
  }
  // A memstore built before /browse answers unknown paths with {"ok": true}.
  if (path === "/browse" && r.ok && (json as { ok?: boolean })?.ok === true) {
    return Response.json({ error: `${name} runs an AmpUp Memory server without /browse: redeploy it from main` }, { status: 501 });
  }
  return Response.json(json, { status: r.status });
}

/** A selection from a request body, keeping only names memstore would accept. */
export function selection(v: unknown): Selection | undefined {
  if (!v || typeof v !== "object") return undefined;
  const { scope, org, team, branch } = v as Record<string, unknown>;
  const ok = (s: unknown) => (typeof s === "string" && /^[A-Za-z0-9_.-]{1,64}$/.test(s) ? s : undefined);
  const okScope = (s: unknown) =>
    typeof s === "string" && /^(org|org:[a-z0-9_]+|team:[a-z0-9_/]+|user:[^\s&#]{1,128})$/.test(s) ? s : undefined;
  const okBranch = (s: unknown) => (typeof s === "string" && /^[A-Za-z0-9_.\/-]{1,128}$/.test(s) ? s : undefined);
  return { scope: okScope(scope), org: ok(org), team: ok(team), branch: okBranch(branch) };
}
