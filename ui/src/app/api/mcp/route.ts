import { guard } from "@/lib/gateway";
import { memstore, selection } from "@/lib/memstore";

// POST {ns, sel?, method, params?}: one MCP JSON-RPC call to the namespace's memstore (tools/list, tools/call).
export async function POST(req: Request) {
  const refused = await guard(req);
  if (refused) return refused;
  const b = await req.json().catch(() => ({}));
  const method = b?.method;
  if (method !== "tools/list" && method !== "tools/call") {
    return Response.json({ error: "method is tools/list or tools/call" }, { status: 400 });
  }
  const body = { jsonrpc: "2.0", id: 1, method, params: b?.params ?? {} };
  return memstore(req, b?.ns, "/mcp", { body, sel: selection(b?.sel) });
}
