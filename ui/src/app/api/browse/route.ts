import { guard } from "@/lib/gateway";
import { memstore, selection } from "@/lib/memstore";
import { withSkills } from "@/lib/skills";

// POST {ns, op, path?, id?, limit?, after?, sel?}: memstore's /browse for the selected org or team, on the
// selected branch; `skills/` read through the `skills` tool where that memstore's /browse predates it.
export async function POST(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const b = await req.json().catch(() => ({}));
  const { op, path, id, limit, after } = b ?? {};
  const sel = selection(b?.sel);
  const browse = (body: unknown) => memstore(req, b?.ns, "/browse", { body, sel });
  const skills = (args: Record<string, string>) =>
    memstore(req, b?.ns, "/mcp", {
      body: { jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "skills", arguments: args } },
      sel,
    });
  const r = await browse({ op, path, id, limit, after });
  if (op === "scopes") return r;
  return withSkills(r, { op, path, limit, after }, String(b?.ns ?? ""), { browse, skills });
}
