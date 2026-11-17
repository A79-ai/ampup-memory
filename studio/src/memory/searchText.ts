// No imports, so the tests (NodeNext, no JSX) can load it.

/** search_memory's answer, read back: one block per entity, and the episodes its markers (`[e1 m0]`) name. */
export type Found = {
  head: string;
  results: { entity: string; when: string; lines: string[] }[];
  sources: Map<string, { id: string; type: string }>;
};
export function readSearch(text: string): Found {
  const lines = text.split("\n");
  const sources = new Map<string, { id: string; type: string }>();
  for (const l of lines) {
    const m = /^(e\d+) ([0-9a-f]{16,}) (\S+)\s*$/.exec(l.trim());
    if (m) sources.set(m[1]!, { id: m[2]!, type: m[3]! });
  }
  const results: Found["results"] = [];
  for (const l of lines) {
    const start = /^\d+\. (\S+) · ([^[]+?)(?: \[e\d+\])?\s*$/.exec(l);
    if (start) results.push({ entity: start[1]!, when: start[2]!.trim(), lines: [] });
    else if (results.length && /^\s{2,}\S/.test(l))
      results[results.length - 1]!.lines.push(l.trim());
  }
  return { head: lines[0] ?? "", results, sources };
}
