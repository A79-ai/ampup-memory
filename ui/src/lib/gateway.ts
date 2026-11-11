import "server-only";

import { door, verifyGoogle } from "./google-token";

// The gateway: this same app, deployed in the cluster (see Dockerfile), where it reaches
// every memstore-* namespace through port-forwards as on a laptop. Exposed publicly, so every API call must
// carry a Google ID token minted for the UI's OAuth client, for an allowed domain; the hosted UI on Vercel
// forwards the signed-in person's own.
//
//   GATEWAY_AUDIENCE      the UI's OAuth client ID; set, this app is the gateway
//   AUTH_ALLOWED_DOMAINS  who may call it (e.g. example.com,example.org)
//
// And on the hosted UI:
//   AMPUP_MEMORY_GATEWAY_URL  the gateway's public URL (or MEMSTORE_GATEWAY_URL, its old name): its namespaces are
//                             listed next to AMPUP_MEMORY_URL's memstore

export const GATEWAY_AUDIENCE = process.env.GATEWAY_AUDIENCE;

/** As the gateway: null when the caller may proceed, else the refusal to send. Not in a cluster and not the
 * gateway (a laptop, Vercel): always null. In a cluster without its settings: refused, never open. */
export async function guard(req: Request): Promise<Response | null> {
  const d = door(process.env);
  if (d === "open") return null;
  if ("closed" in d) {
    console.log(`gateway closed: ${d.closed}`);
    return Response.json({ error: d.closed }, { status: 503 });
  }
  const r = await verifyGoogle(req.headers.get("authorization"), d.check);
  if ("email" in r) return null;
  console.log(`gateway refused: ${r.reason}`);
  return Response.json({ error: r.reason }, { status: r.status });
}

const GATEWAY_URL = (process.env.AMPUP_MEMORY_GATEWAY_URL ?? process.env.MEMSTORE_GATEWAY_URL)?.replace(/\/$/, "");

/** Whether the hosted UI also lists the gateway's namespaces. */
export const viaGateway = (): boolean => !!GATEWAY_URL;

/** A call through the gateway, with the person's ID token; its status and JSON passed back. */
export async function gatewayCall(path: string, token: string, body?: unknown): Promise<Response> {
  let r: Response;
  try {
    r = await fetch(`${GATEWAY_URL}${path}`, {
      method: body === undefined ? "GET" : "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
  } catch {
    return Response.json({ error: "the Kubernetes gateway isn't answering" }, { status: 502 });
  }
  const v = await r.json().catch(() => ({ error: `gateway answered ${r.status}` }));
  return Response.json(v, { status: r.status });
}
