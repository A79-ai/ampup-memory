// The entity graph cut to what a page can lay out. Kept apart from graph.tsx so it is tested without a page.

/** The most entities drawn at once. The layout repels every pair, so thousands would freeze the page. */
export const GRAPH_MAX = 300;

interface Node {
  id: string;
  owner: string | null;
  facts: number;
  pending: string[];
}
interface Edge {
  from: string;
  to: string;
}

/** `g` cut to at most `max` entities: every owned one, every one a change waits on, and `keep`, then those
 * with the most facts; only the links between the ones kept. */
export function capGraph<N extends Node, E extends Edge>(
  g: { nodes: N[]; edges: E[] },
  max = GRAPH_MAX,
  keep?: string,
): { nodes: N[]; edges: E[] } {
  if (g.nodes.length <= max) return g;
  const must = (n: N) => n.owner != null || n.pending.length > 0 || n.id === keep;
  const ranked = [...g.nodes].sort(
    (a, b) => Number(must(b)) - Number(must(a)) || b.facts - a.facts || a.id.localeCompare(b.id),
  );
  const nodes = ranked.slice(0, max);
  const ids = new Set(nodes.map((n) => n.id));
  return { nodes, edges: g.edges.filter((e) => ids.has(e.from) && ids.has(e.to)) };
}
