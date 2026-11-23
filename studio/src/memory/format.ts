// How memory's names and times read on the page. Nothing here changes what is shown, only how: the raw id or
// predicate is always kept beside it (in a title, or in mono under it).

/** Words spelled their own way wherever they fall in a name: acronyms and product names. */
const SPELLED: Record<string, string> = {
  acl: "ACL",
  ai: "AI",
  aip: "AIP",
  ampup: "AmpUp",
  api: "API",
  apis: "APIs",
  arr: "ARR",
  aws: "AWS",
  b2b: "B2B",
  chatgpt: "ChatGPT",
  ci: "CI",
  crm: "CRM",
  csv: "CSV",
  gcp: "GCP",
  github: "GitHub",
  gitlab: "GitLab",
  hubspot: "HubSpot",
  id: "ID",
  ids: "IDs",
  json: "JSON",
  kpi: "KPI",
  linkedin: "LinkedIn",
  llm: "LLM",
  mcp: "MCP",
  meddpicc: "MEDDPICC",
  mrr: "MRR",
  okr: "OKR",
  openai: "OpenAI",
  pr: "PR",
  prs: "PRs",
  qa: "QA",
  roi: "ROI",
  saas: "SaaS",
  sdk: "SDK",
  sql: "SQL",
  sso: "SSO",
  ui: "UI",
  url: "URL",
  urls: "URLs",
  ux: "UX",
};
/** Names of more than one word, as their makers spell them. */
const PHRASES: [RegExp, string][] = [[/\bclaude code\b/gi, "Claude Code"]];

/** An ISO date or time (`2026-10-26`, a UTC day folder), or an email: shown as it is, never as words. */
const AS_IS = /^(\d{4}-\d{2}-\d{2}([T ].*)?|[^\s@]+@[^\s@]+\.[^\s@]+)$/;

/** `close_date` as "Close date", `seats-target` as "Seats target"; a compiled skill's `review-a-pr~847aa47c` as
 * "Review a PR" (the `~` and hash after it tell versions apart, not what it is). Acronyms and product names keep
 * their spelling (`claude_code` "Claude Code", `github` "GitHub", `meddpicc_champion` "MEDDPICC champion"); a date
 * or an email is left as it is. */
export function humanize(s: string): string {
  if (AS_IS.test(s.trim())) return s;
  const t = s
    .replace(/~[0-9a-f]{6,}$/, "")
    .replace(/[_-]+/g, " ")
    .trim();
  if (!t) return s;
  let out = t
    .split(/\s+/)
    .map((w) => (Object.hasOwn(SPELLED, w.toLowerCase()) ? SPELLED[w.toLowerCase()]! : w))
    .join(" ");
  for (const [re, name] of PHRASES) out = out.replace(re, name);
  return out.charAt(0).toUpperCase() + out.slice(1);
}

/** What the server says of how an entity reads (`title`: a name from a fact the reader may read; `since`: when its
 * first fact holds from, µs), on graph nodes, Explorer rows, an entity's page and its links. */
export type Named = { title?: string | null; since?: number | null };

// Entities' names the memory has given, for as long as the page is open, so an id that says nothing to a person
// (`meeting:7d3f…`) reads as its name (a call's title) wherever it is shown, even where a page holds only the id (a
// fact's line, a receipt, a review card). The memory sends them as `names` beside what it answers (`{id: name}`),
// and as `title` and `since` on graph nodes, Explorer rows, an entity's page and its links. Kept for one
// selection at a time: another org, team or branch starts over.
let namesOf = "";
const names = new Map<string, Named>();
const MAX_NAMES = 20_000;

/** Something an answer holds that names an entity by `entity` or `id`, with a `title` or `since`. */
type NamedRow = Named & { entity?: unknown; id?: unknown };

function keep(id: unknown, title: unknown, since: unknown) {
  if (typeof id !== "string") return;
  const t = typeof title === "string" && title.trim() ? title.trim() : undefined;
  const at = typeof since === "number" ? since : undefined;
  if (t === undefined && at === undefined) return;
  const had = names.get(id);
  names.set(id, { title: t ?? had?.title, since: at ?? had?.since });
}

/** Whether the memory has named `id`: a value that is a named entity's id is that entity. */
export const named = (id: string): boolean => !!names.get(id)?.title;

/** Keep the names an answer carries, at its top or under `result`; `sel` is the selection it answered for. */
export function rememberNames(answer: unknown, sel?: string): void {
  if (sel !== undefined && sel !== namesOf) {
    names.clear();
    namesOf = sel;
  }
  if (names.size > MAX_NAMES) names.clear();
  for (const at of [answer, (answer as { result?: unknown } | null)?.result]) {
    if (!at || typeof at !== "object") continue;
    const a = at as NamedRow & {
      names?: unknown;
      facts?: unknown;
      children?: (NamedRow & { kind?: string })[];
      nodes?: NamedRow[];
      links?: { out?: NamedRow[]; in?: NamedRow[] };
    };
    if (a.names && typeof a.names === "object")
      for (const [id, name] of Object.entries(a.names as Record<string, unknown>))
        keep(id, name, undefined);
    // An entity's page names itself.
    if (Array.isArray(a.facts)) keep(a.entity, a.title, a.since);
    const rows: NamedRow[] = [
      ...(Array.isArray(a.children) ? a.children.filter((c) => c?.kind === "entity") : []),
      ...(Array.isArray(a.nodes) ? a.nodes : []),
      ...(Array.isArray(a.links?.out) ? a.links.out : []),
      ...(Array.isArray(a.links?.in) ? a.links.in : []),
    ];
    for (const r of rows) keep(typeof r.entity === "string" ? r.entity : r.id, r.title, r.since);
  }
}

/** A key that says nothing to a reader: a number, a UUID or other hex, a random token, or one with no word in it. */
export function opaqueKey(key: string): boolean {
  if (!key || /^\d+$/.test(key)) return true;
  if (/^[0-9a-f-]{8,}$/i.test(key) && /\d/.test(key)) return true;
  // Base64-ish: long, cased and numbered at random (a CamelCase name changes case only at its words).
  if (
    /^[A-Za-z0-9+/=]{16,}$/.test(key) &&
    /\d/.test(key) &&
    /[a-z]/.test(key) &&
    /[A-Z]/.test(key)
  ) {
    const cls = (c: string) => (/\d/.test(c) ? 0 : c === c.toUpperCase() ? 1 : 2);
    let flips = 0;
    for (let i = 1; i < key.length; i++) if (cls(key[i]!) !== cls(key[i - 1]!)) flips++;
    if (flips / key.length > 0.35) return true;
  }
  return !/[A-Za-z]{3,}/.test(key);
}

/** An entity's name to show: the server's, else its slug key read as words (`deal:globex-expansion` is "Globex
 * expansion"), else its key as it is (an email, a word); never an opaque id, which reads as its type and a date
 * ("Meeting · Nov 12, 2026") or its type and the id's first characters ("Meeting 6f3a9c…"). The full id is the
 * caller's to show beside it (mono, or a title). */
export function displayName(entity: string, known?: Named | null): string {
  const i = entity.indexOf(":");
  const type = i < 0 ? "" : entity.slice(0, i);
  const key = i < 0 ? entity : entity.slice(i + 1);
  const kept = names.get(entity);
  const title = (known?.title ?? kept?.title)?.trim();
  const since = known?.since ?? kept?.since;
  if (title) return title;
  if (!opaqueKey(key)) {
    const slug = /^[a-z][a-z0-9]*(?:[-_][a-z0-9]+)+$/.test(key);
    return slug ? humanize(key) : key;
  }
  const t = type && type !== "_" ? humanize(type) : "Entity";
  if (since) return `${t} · ${day(since)}`;
  return key.length > 8 ? `${t} ${key.slice(0, 6)}…` : `${t} ${key}`;
}

/** An entity id, `deal:globex-expansion`, as its type, its key and the name to show ([`displayName`]). */
export function entityName(
  entity: string,
  known?: Named | null,
): { type: string; key: string; name: string } {
  const i = entity.indexOf(":");
  const type = i < 0 ? "" : entity.slice(0, i);
  const key = i < 0 ? entity : entity.slice(i + 1);
  return { type, key, name: displayName(entity, known) };
}

/** A person's id as their name: `rep:jane_doe` reads "Jane Doe", `rep:sam.l@example.com` "Sam L". */
export function personName(entity: string): string {
  const { key } = entityName(entity);
  const words = (key.split("@")[0] ?? key).split(/[._\s-]+/).filter(Boolean);
  return words.length ? words.map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(" ") : key;
}

export type SourceKind = { kind: string; label: string };

/** What kind of source an episode came from, by its source's prefix (as memstore's episode types read it). */
export function sourceKind(source: string): SourceKind {
  const p = source.split(":")[0]?.toLowerCase() ?? "";
  if (addedBy(source)) return { kind: "notes", label: "Studio" };
  if (p === "slack") return { kind: "chat", label: "Slack" };
  if (["ampup", "meeting", "gong", "zoom", "recall"].includes(p))
    return { kind: "call", label: "Call" };
  if (["hubspot", "salesforce", "dynamicscrm", "crm"].includes(p))
    return { kind: "crm", label: "CRM" };
  if (p === "github") return { kind: "code", label: "GitHub" };
  if (p === "granola") return { kind: "notes", label: "Notes" };
  if (p === "doc") return { kind: "doc", label: "Document" };
  if (p === "skill" || p === "aip") return { kind: "skill", label: "Skill" };
  return { kind: "doc", label: p ? humanize(p) : "Episode" };
}

/** Who added something themselves in Studio, from its episode's source: a fact (Review's "Add a fact", or "This is
 * wrong" on a fact), `mcp:remember_memory:<them>:memstore:fact:<label>`, the server's `origin::ADDED`; or text they
 * pasted or uploaded in "Add to memory", `mcp:remember_memory:<them>:memstore:learn:paste:<words>` (or
 * `…:upload:<file>`), its `origin::LEARNED`. Undefined for any other source, and for a service key's
 * (`key:<org>/<name>`), which is a program's. */
export function addedBy(source: string): string | undefined {
  const m = /^mcp:remember_memory:(.+?):memstore:(?:fact|learn)(?::|$)/.exec(source);
  return m?.[1] && !m[1].startsWith("key:") ? m[1] : undefined;
}

/** A Slack source's segments after `slack:` as a title: its channel's name where the source carries one
 * (`C04DEALS:#deals:1727790000.1` reads "Slack #deals · Oct 1, 2024, 01:40 PM"), and when it was posted from the
 * message's timestamp, never the channel's id or the raw timestamp (`ws:thread:C04DEALS:1727790000.1` reads "Slack
 * thread · Oct 1, 2024, 01:40 PM"). */
function slackTitle(rest: string[]): string {
  const channel = rest.find((s) => s.startsWith("#") && s.length > 1);
  const ts = [...rest].reverse().find((s) => /^\d{9,11}\.\d+$/.test(s));
  const thread = rest.includes("thread");
  const head = ["Slack", channel, thread ? "thread" : channel ? "" : "message"]
    .filter(Boolean)
    .join(" ");
  const at = ts ? when(Number(ts) * 1_000_000) : "";
  return at ? `${head} · ${at}` : head;
}

/** A source as a title: `slack:C04DEALS:#deals:1727790000.1` reads "Slack #deals · Oct 1, 2024, 01:40 PM",
 * `hubspot:deal:globex-expansion` reads "HubSpot deal · globex-expansion". */
export function sourceTitle(source: string): string {
  const by = addedBy(source);
  if (by) {
    const learned = /:memstore:learn:(paste|upload)(?::|$)/.exec(source)?.[1];
    if (learned === "paste") return `${by} pasted it in Studio`;
    if (learned === "upload") return `${by} uploaded it in Studio`;
    return `${by} added it in Studio`;
  }
  const [p = "", ...rest] = source.split(":");
  if (p.toLowerCase() === "slack" && rest.length) return slackTitle(rest);
  const brand: Record<string, string> = {
    slack: "Slack",
    hubspot: "HubSpot",
    salesforce: "Salesforce",
    github: "GitHub",
    granola: "Granola",
    ampup: "AmpUp",
    gong: "Gong",
    zoom: "Zoom",
    doc: "Document",
  };
  const head = brand[p.toLowerCase()] ?? humanize(p);
  if (!rest.length) return head;
  if (rest.length >= 2 && /^[a-z]+$/.test(rest[0] ?? ""))
    return `${head} ${rest[0]} · ${rest.slice(1).join(":")}`;
  return `${head} · ${rest.join(":")}`;
}

const toDate = (us: number | null | undefined): Date | null => {
  if (us == null) return null;
  const d = new Date(us / 1000);
  return Number.isNaN(d.getTime()) ? null : d;
};

/** Microseconds since the epoch, as a short local date and time. */
export function when(us: number | null | undefined): string {
  const d = toDate(us);
  if (!d) return "";
  return d.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Microseconds since the epoch, as a date. */
export function day(us: number | null | undefined): string {
  const d = toDate(us);
  if (!d) return "";
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

/** "3 days ago", "in 2 months"; the date itself past a year. */
export function ago(us: number | null | undefined): string {
  const d = toDate(us);
  if (!d) return "";
  const s = (d.getTime() - Date.now()) / 1000;
  const abs = Math.abs(s);
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (abs < 60) return "just now";
  if (abs < 3600) return rtf.format(Math.round(s / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(s / 3600), "hour");
  if (abs < 86400 * 30) return rtf.format(Math.round(s / 86400), "day");
  if (abs < 86400 * 365) return rtf.format(Math.round(s / (86400 * 30)), "month");
  return day(us);
}

/** An id cut short for a chip: its first characters. */
export const short = (id: string, n = 8) => (id.length > n + 2 ? id.slice(0, n) : id);
