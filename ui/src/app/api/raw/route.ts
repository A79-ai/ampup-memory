import { guard } from "@/lib/gateway";
import { PATHS, memstore, selection } from "@/lib/memstore";

// POST {ns, path, method?, body?, sel?}: one memstore call in a namespace, as the hosted UI sends it through the
// gateway. Only the paths the UI uses.
export async function POST(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const b = await req.json().catch(() => ({}));
  if (!PATHS.includes(b?.path)) return Response.json({ error: "no such path" }, { status: 404 });
  const method = b?.method === "GET" ? "GET" : "POST";
  return memstore(req, b?.ns, b.path, { method, body: method === "GET" ? undefined : b?.body, sel: selection(b?.sel) });
}
