import type { GraphEdge, GraphNode } from "./graphTypes.js";

export interface Sim extends GraphNode {
  x: number;
  y: number;
  vx: number;
  vy: number;
  r: number;
  pinned: boolean;
}

export interface View {
  x: number;
  y: number;
  k: number;
}

const goldenAngle = Math.PI * (3 - Math.sqrt(5));

/** Unique, deterministic starting positions, including hundreds of entities of the same type. */
export function seed(nodes: GraphNode[], w: number, h: number): Sim[] {
  const spread = Math.max(Math.min(w, h) * 0.32, Math.sqrt(nodes.length) * 30);
  return nodes.map((n, i) => {
    const a = i * goldenAngle;
    const r = spread * Math.sqrt((i + 0.5) / nodes.length);
    return {
      ...n,
      x: w / 2 + Math.cos(a) * r,
      y: h / 2 + Math.sin(a) * r,
      vx: 0,
      vy: 0,
      // Keep heavily recorded entities from covering their neighbours or the whole canvas.
      r: 7 + Math.min(25, Math.log2(1 + Math.max(0, n.facts)) * 2),
      pinned: false,
    };
  });
}

function separation(a: Sim, b: Sim, i: number, j: number) {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const d = Math.sqrt(dx * dx + dy * dy);
  if (d > 0.01) return { dx: dx / d, dy: dy / d, d };
  // An actual unit vector: using a made-up squared distance here amplified forces enormously.
  const angle = (i + j * 31) * goldenAngle;
  return { dx: Math.cos(angle), dy: Math.sin(angle), d: 0.01 };
}

/** Bounded forces, degree-normalized springs, and radius-aware collision separation. */
export function step(nodes: Sim[], edges: GraphEdge[], w: number, h: number, alpha: number) {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const links = new Map<string, [Sim, Sim]>();
  const degree = new Map<Sim, number>();
  for (const e of edges) {
    const a = byId.get(e.from);
    const b = byId.get(e.to);
    if (!a || !b || a === b) continue;
    // Multiple predicates between two entities should draw multiple links, but exert one spring.
    const key = JSON.stringify(a.id < b.id ? [a.id, b.id] : [b.id, a.id]);
    if (links.has(key)) continue;
    links.set(key, [a, b]);
    degree.set(a, (degree.get(a) ?? 0) + 1);
    degree.set(b, (degree.get(b) ?? 0) + 1);
  }
  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      const a = nodes[i]!;
      const b = nodes[j]!;
      const { dx, dy, d } = separation(a, b, i, j);
      const f = (2200 * alpha) / Math.max(100, d * d);
      a.vx -= dx * f;
      a.vy -= dy * f;
      b.vx += dx * f;
      b.vy += dy * f;
    }
  }
  for (const [a, b] of links.values()) {
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const d = Math.max(1, Math.hypot(dx, dy));
    const strength = 0.12 / Math.max(degree.get(a)!, degree.get(b)!);
    const f = ((d - (80 + a.r + b.r)) / d) * strength * alpha;
    a.vx += dx * f;
    a.vy += dy * f;
    b.vx -= dx * f;
    b.vy -= dy * f;
  }
  for (const n of nodes) {
    if (n.pinned) {
      n.vx = n.vy = 0;
      continue;
    }
    n.vx = (n.vx + (w / 2 - n.x) * 0.003 * alpha) * 0.6;
    n.vy = (n.vy + (h / 2 - n.y) * 0.003 * alpha) * 0.6;
    const speed = Math.hypot(n.vx, n.vy);
    if (speed > 12) {
      n.vx *= 12 / speed;
      n.vy *= 12 / speed;
    }
    n.x += n.vx;
    n.y += n.vy;
  }
  // Resolve overlap even as the simulation cools. Respect a node held by the pointer.
  for (let pass = 0; pass < 2; pass++) {
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const a = nodes[i]!;
        const b = nodes[j]!;
        if (a.pinned && b.pinned) continue;
        const distanceX = b.x - a.x;
        const distanceY = b.y - a.y;
        const gap = a.r + b.r + 10;
        if (distanceX * distanceX + distanceY * distanceY >= gap * gap) continue;
        const { dx, dy, d } = separation(a, b, i, j);
        const overlap = gap - d;
        if (overlap <= 0) continue;
        const shift = overlap / (a.pinned || b.pinned ? 1 : 2);
        if (!a.pinned) {
          a.x -= dx * shift;
          a.y -= dy * shift;
        }
        if (!b.pinned) {
          b.x += dx * shift;
          b.y += dy * shift;
        }
      }
    }
  }
}

/** Fit the visible nodes, including their rings, with room for the canvas caption. */
export function fitView(nodes: Sim[], w: number, h: number): View {
  if (nodes.length === 0) return { x: 0, y: 0, k: 1 };
  const left = Math.min(...nodes.map((n) => n.x - n.r - 8));
  const right = Math.max(...nodes.map((n) => n.x + n.r + 8));
  const top = Math.min(...nodes.map((n) => n.y - n.r - 8));
  const bottom = Math.max(...nodes.map((n) => n.y + n.r + 8));
  const k = Math.min(
    1.5,
    Math.max(1, w - 64) / (right - left),
    Math.max(1, h - 88) / (bottom - top),
  );
  return { x: w / 2 - ((left + right) / 2) * k, y: (h - 24) / 2 - ((top + bottom) / 2) * k, k };
}

/** A dot this large on screen, in pixels, may always get a label; smaller ones only if among the largest fifth. */
const MIN_LABELLED_R = 6;

/** Keep labels legible at every zoom, giving selected/search results first choice of space. */
export function labelIds(
  nodes: Sim[],
  view: View,
  w: number,
  h: number,
  name: (id: string) => string,
  priority: (id: string) => number,
): Set<string> {
  const labels = new Set<string>();
  const boxes: { x: number; y: number; w: number; h: number }[] = [];
  // Every dot on screen, so a label never sits over another entity's middle.
  const dots = nodes.map((n) => ({
    id: n.id,
    x: n.x * view.k + view.x,
    y: n.y * view.k + view.y,
    // Its middle: a label may cross a large dot's rim, never cover what marks it.
    core: Math.max(2, n.r * view.k * 0.5),
  }));
  const ranked = [...nodes].sort((a, b) => priority(b.id) - priority(a.id) || b.facts - a.facts);
  // Zoomed out, every dot is small: the largest fifth still get labels, and zooming in shows the rest.
  const radii = nodes.map((n) => n.r).sort((a, b) => b - a);
  const fifth = radii[Math.floor(radii.length / 5)] ?? 0;
  const minR = Math.min(MIN_LABELLED_R, fifth * view.k);
  for (const n of ranked) {
    // A small dot (one with few facts, or any while zoomed out) goes unlabelled unless it is picked or found.
    if (priority(n.id) === 0 && n.r * view.k < minR) continue;
    const width = Math.min(32, Array.from(name(n.id)).length) * 7 + 8;
    const x = n.x * view.k + view.x - width / 2;
    const y = (n.y + n.r) * view.k + view.y + 4;
    if (x < 4 || x + width > w - 4 || y < 4 || y + 16 > h - 30) continue;
    if (boxes.some((b) => x < b.x + b.w && x + width > b.x && y < b.y + b.h && y + 16 > b.y))
      continue;
    if (
      dots.some(
        (d) =>
          d.id !== n.id &&
          d.x + d.core > x &&
          d.x - d.core < x + width &&
          d.y + d.core > y &&
          d.y - d.core < y + 16,
      )
    )
      continue;
    labels.add(n.id);
    boxes.push({ x, y, w: width, h: 16 });
  }
  return labels;
}
