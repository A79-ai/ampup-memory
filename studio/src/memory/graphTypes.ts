// Shared by the browser API and the layout's Node-based regression tests, without browser imports.
export interface GraphNode {
  id: string;
  type: string;
  owner: string | null;
  facts: number;
  /** Open proposals that change it. */
  pending: string[];
  /** Its name, from a fact the reader may read; absent from a server before it named nodes, or when none does. */
  title?: string | null;
  /** When its first fact the reader may read holds from (µs). */
  since?: number | null;
}

export interface GraphEdge {
  from: string;
  to: string;
  predicate: string;
  /** The proposal this link exists in only, when it hasn't reached main. */
  pending: string | null;
}

export interface Graph {
  nodes: GraphNode[];
  edges: GraphEdge[];
  /** How many entities there are in all: `nodes` holds at most the limit asked for. Absent from a server before it
   * cut the graph. */
  total?: number;
}
