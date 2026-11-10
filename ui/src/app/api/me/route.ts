import { guard } from "@/lib/gateway";
import { memstore, selection } from "@/lib/memstore";

// POST {ns, sel?}: who memstore says the caller is, the scope selected and the orgs and teams they belong to.
export async function POST(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const b = await req.json().catch(() => ({}));
  return memstore(req, b?.ns, "/me", { method: "GET", sel: selection(b?.sel) });
}
