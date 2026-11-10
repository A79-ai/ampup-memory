import { guard } from "@/lib/gateway";
import { memstore, selection } from "@/lib/memstore";

// POST {ns, query, sel?}: the best-matching facts, through memstore's /search (all of valid time).
export async function POST(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const b = await req.json().catch(() => ({}));
  const q = typeof b?.query === "string" ? b.query.slice(0, 500) : "";
  const r = await memstore(req, b?.ns, "/search", { body: { calls: [[q, 30, -1]] }, sel: selection(b?.sel) });
  if (!r.ok) return r;
  const v = await r.json();
  return Response.json({ facts: v?.replies?.[0] ?? [] });
}
