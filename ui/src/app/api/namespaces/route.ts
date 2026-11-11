import { idToken } from "@/lib/auth";
import { gatewayCall, guard, viaGateway } from "@/lib/gateway";
import { namespaces, type Namespace } from "@/lib/gke";
import { hostedLabel, mode } from "@/lib/memstore";

// GET: where this UI reads memory. Hosted: AMPUP_MEMORY_URL's memstore and, through the gateway, every memstore-
// namespace on the gateway's Kubernetes cluster (once signed in). Otherwise (a laptop, or the gateway itself) the namespaces,
// listed with kubectl, and whether each one's memstore is serving.
export async function GET(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const m = mode();
  if (m.hosted) {
    const list: Namespace[] = [];
    const label = hostedLabel();
    if (label) list.push({ namespace: label, serving: true });
    let error: string | undefined;
    const token = viaGateway() ? await idToken(req).catch(() => null) : null;
    if (token) {
      const r = await gatewayCall("/api/namespaces", token);
      const v = await r.json();
      if (r.ok) list.push(...(v.namespaces as Namespace[]));
      else error = `Kubernetes namespaces: ${v.error ?? r.status}`;
    }
    return Response.json({ hosted: true, namespaces: list, error });
  }
  try {
    return Response.json({ hosted: false, namespaces: await namespaces() });
  } catch (e) {
    return Response.json({ error: `can't list namespaces: ${(e as Error).message}` }, { status: 502 });
  }
}
