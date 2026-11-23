import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { ApiError, browse, me as loadMe } from "./api";
import { displayName } from "./format";
import { Agent } from "./agent";
import { ALL_PARTS, Ask, type AskParts } from "./ask";
import { Inspector } from "./inspector";
import { SchemaView, useOrgSchema } from "./schema";
import { Tools } from "./tools";
import { Tree } from "./tree";
import type { Branch, Focus, Me, Selection } from "./types";
import { Icon } from "./ui";
import { ReviewSection } from "./review";
import { GraphSection } from "./graph";
import { Home, type HomeSummary } from "./home";
import { AddToMemoryDialog } from "./addToMemory";
import { Advice, AdviceServing, Branches, NotAnswered, Owners, People } from "../studio/Admin";
import { AdvicePage } from "../studio/AdvicePage";
import { Learned } from "../studio/Learned";
import { Trace } from "../studio/Trace";
import { NAVIGATE, optionsOf, setOptions, subscribe, viewOf } from "./hashOptions";
import { given, probe, setTransport, type Resolved, type Transport } from "../transport";
import "../base.css";
import "./memory.css";
import "../theme.css";

// The focus lives in the address bar's hash, named for the page it opens, so a view can be linked to, read at a
// glance and survives a reload:
//   #/home                      Home (what changed, what it answered, what needs you)
//   #/explorer/sources/slack/C09  a folder    #/entity/entities/deal/42  an entity
//   #/sessions/…                the reader's sessions     #/skills/<goal>/<slug>  compiled skills (a folder)
//   #/episode/<episode id>      an episode    #/search/<text>  a search (Search › Facts)
//   #/skill/skills/<goal>/<slug>  a compiled skill read as a note (or its goal's index)
//   #/tools/<tool>              the Tools, a tool picked
//   #/schema/<type>             the Schema, a type picked
//   #/goals                     the Goals
//   #/ask/<parts>/<question>    Search › Look up: a question, and which of facts (f), advice (a), episodes (e) it uses
//   #/playground                Ask: an agent answers on arms (its conversation isn't in the link)
//   #/handover                  Handover      #/people/<person>  People (a person's activity, or everyone's)
//   #/advice                    Advice's trail  #/branches  Branches  #/owners  Owners
//   #/learned/<lesson>          Learned (what it learned, a lesson picked: studio/Learned.tsx's `lessonId`)
//   #/serving                   Serving (when advice is served, and why not)
//   #/not-answered              Not answered  #/conversation/<entry>/<view>  a conversation's trace, its filter
//   #/review/<proposal>         Review (owner review: the inbox, or one proposal)
//   #/graph/<entity>            Graph (the entities and their links, one picked)
//                               (People onwards: the org's admins')
// After the view, its options as a query (`#/not-answered?days=7&open=<entry>`: hashOptions.ts): its window, the
// tab, the row opened, the filters, so every one of them can be linked to.
// Each segment is URI-encoded, so one may hold `/`. The org is the host's (`org`); `?team=` picks one of its
// teams' memory and `?branch=` the branch read (main by default).
// Links from before the names (`#/v/`, `#/f/sessions`, …: one letter each) still open the same page.
const LETTERS: Record<string, string> = {
  w: "home",
  f: "explorer",
  e: "entity",
  s: "skill",
  x: "episode",
  q: "search",
  t: "tools",
  m: "schema",
  g: "goals",
  h: "handover",
  p: "people",
  v: "advice",
  k: "learned",
  l: "serving",
  b: "branches",
  o: "owners",
  u: "not-answered",
  c: "conversation",
  a: "ask",
  n: "playground",
  r: "review",
  d: "graph",
};

/** `#/<name>`, and `/<rest>` after it when there's more to say. */
const at = (name: string, rest = "") => `#/${name}${rest ? `/${rest}` : ""}`;

function toHash(f: Focus | null): string {
  if (!f) return "";
  const enc = (p: string[]) => p.map(encodeURIComponent).join("/");
  switch (f.kind) {
    case "home":
      return at("home");
    case "folder":
      // Sessions and Skills are pages of their own: their folders are linked by their names.
      if (f.path[0] === "sessions" || f.path[0] === "skills")
        return at(f.path[0], enc(f.path.slice(1)));
      return at("explorer", enc(f.path));
    case "entity":
      return at("entity", enc(f.path));
    case "skill":
      return at("skill", enc(f.path));
    case "episode":
      return at("episode", encodeURIComponent(f.id));
    case "search":
      return at("search", encodeURIComponent(f.query));
    case "tools":
      return at("tools", encodeURIComponent(f.tool ?? ""));
    case "schema":
      return at("schema", encodeURIComponent(f.type ?? ""));
    case "goals":
      return at("goals");
    case "handover":
      return at("handover");
    case "people":
      return at("people", encodeURIComponent(f.user ?? ""));
    case "advice":
      return at("advice");
    case "learned":
      return at("learned", encodeURIComponent(f.lesson ?? ""));
    case "serving":
      return at("serving");
    case "branches":
      return at("branches");
    case "owners":
      return at("owners");
    case "gaps":
      return at("not-answered");
    case "conversation":
      return at("conversation", encodeURIComponent(f.id));
    case "ask":
      return at("ask", `${encodeURIComponent(f.parts ?? "fae")}/${encodeURIComponent(f.question)}`);
    case "agent":
      return at("playground");
    case "review":
      return at("review", encodeURIComponent(f.proposal ?? ""));
    case "graph":
      return at("graph", encodeURIComponent(f.entity ?? ""));
  }
}

function fromHash(hash: string): Focus | null {
  const h = viewOf(hash);
  const m = /^#\/([a-z][a-z-]*)(?:\/(.*))?$/.exec(h);
  if (!m) return null;
  const name = LETTERS[m[1]!] ?? m[1]!;
  const rest = m[2] ?? "";
  const segs = rest ? rest.split("/").map(decodeURIComponent) : [];
  switch (name) {
    case "home":
      return { kind: "home" };
    case "explorer":
      return { kind: "folder", path: segs };
    case "sessions":
    case "skills":
      return { kind: "folder", path: [name, ...segs] };
    case "entity":
      return { kind: "entity", path: segs };
    case "skill":
      return { kind: "skill", path: segs };
    case "episode":
      return { kind: "episode", id: decodeURIComponent(rest) };
    case "tools":
      return { kind: "tools", tool: decodeURIComponent(rest) || undefined };
    case "schema":
      return { kind: "schema", type: decodeURIComponent(rest) || undefined };
    case "goals":
      return { kind: "goals" };
    case "handover":
      return { kind: "handover" };
    case "people":
      return { kind: "people", user: decodeURIComponent(rest) || undefined };
    case "advice":
      return { kind: "advice" };
    case "learned":
      return { kind: "learned", lesson: decodeURIComponent(rest) || undefined };
    case "serving":
      return { kind: "serving" };
    case "branches":
      return { kind: "branches" };
    case "owners":
      return { kind: "owners" };
    case "not-answered":
      return { kind: "gaps" };
    case "conversation":
      // What follows the entry is the trace's filter, which Trace keeps in the link itself.
      return { kind: "conversation", id: segs[0] ?? "" };
    case "playground":
      return { kind: "agent" };
    case "review":
      return { kind: "review", proposal: decodeURIComponent(rest) || undefined };
    case "graph":
      return { kind: "graph", entity: decodeURIComponent(rest) || undefined };
    case "ask": {
      const i = rest.indexOf("/");
      return i < 0
        ? { kind: "ask", question: "", parts: decodeURIComponent(rest) || "fae" }
        : {
            kind: "ask",
            parts: decodeURIComponent(rest.slice(0, i)),
            question: decodeURIComponent(rest.slice(i + 1)),
          };
    }
    case "search":
      return { kind: "search", query: decodeURIComponent(rest) };
    default:
      return null;
  }
}

// The URL is the state: subscribe to it, and change it to move. The host's own `popstate` (its navigation)
// reaches here too.
function navigate(url: string) {
  history.pushState(history.state, "", url);
  dispatchEvent(new Event(NAVIGATE));
}

/**
 * The sections of the sidebar. A focus belongs to one: a skill to Skills, a search or a look-up to Search, a
 * conversation's trace to Sessions.
 */
type Section =
  | "home"
  | "handover"
  | "people"
  | "advice"
  | "learned"
  | "serving"
  | "branches"
  | "owners"
  | "gaps"
  | "review"
  | "explorer"
  | "graph"
  | "search"
  | "ask"
  | "sessions"
  | "schema"
  | "skills"
  | "tools"
  | "goals";
/** The sections only the org's admins see. Advice is everyone's: a member sees the counts and their own sessions. */
const ADMIN: Section[] = ["people", "gaps", "branches", "owners"];

/** The branches the server makes for a change while it waits on its owners' review: what extraction holds for them
 * (`held-`), a fact an admin added (`fact-`) or a conflict they settled (`resolve-`). Nobody reads one: the branch
 * picker leaves them out, and Branches lists them for the admin who settles one. */
const WORKING_BRANCH = /^(held|fact|resolve)-/;

/** The sections that need more of the transport than reading the memory: each hidden where it lacks it. */
const NEEDS: Partial<Record<Section, keyof Resolved>> = {
  people: "admin",
  gaps: "admin",
  branches: "admin",
  owners: "admin",
  review: "review",
  graph: "review",
  ask: "agent",
  learned: "lessons",
};
function sectionOf(f: Focus | null, start: Section): Section {
  if (!f) return start;
  switch (f.kind) {
    case "home":
      return "home";
    case "tools":
      return "tools";
    case "search":
    case "ask":
      return "search";
    case "schema":
      return "schema";
    case "goals":
      return "goals";
    case "serving":
      // Serving's lookup tables sit under the Advice page's diagnostics.
      return "advice";
    case "handover":
    case "people":
    case "advice":
    case "learned":
    case "branches":
    case "owners":
    case "gaps":
      return f.kind;
    case "conversation":
      // A trace is an answer's, read by the org's admins (Memory below sends anyone else elsewhere).
      return "sessions";
    case "agent":
      return "ask";
    case "review":
    case "graph":
      return f.kind;
    case "skill":
      return "skills";
    case "folder":
      return f.path[0] === "skills" ? "skills" : f.path[0] === "sessions" ? "sessions" : "explorer";
    default:
      return "explorer";
  }
}
type Item = { id: Section; label: string; icon: string; to: Focus; reads: boolean };
// The sidebar follows a builder's journey: feed the memory and see what it learned (Memory), fix it (Review),
// see it used (Answers), know it pays off (Impact), and shape what it learns (Build). Home sums them up.
const SECTIONS: { group: string; items: Item[] }[] = [
  {
    group: "",
    items: [{ id: "home", label: "Home", icon: "home", to: { kind: "home" }, reads: true }],
  },
  {
    group: "Memory",
    items: [
      {
        id: "explorer",
        label: "Explorer",
        icon: "explorer",
        to: { kind: "folder", path: [] },
        reads: true,
      },
      { id: "graph", label: "Graph", icon: "entity", to: { kind: "graph" }, reads: true },
      // Facts (a search) and Look up (a question through the agent tools): one page, two modes.
      {
        id: "search",
        label: "Search",
        icon: "search",
        to: { kind: "search", query: "" },
        reads: true,
      },
      { id: "ask", label: "Ask", icon: "chat", to: { kind: "agent" }, reads: true },
    ],
  },
  // Who decides what reaches main: the proposals and conflicts waiting, and who owns what.
  {
    group: "Review",
    items: [
      { id: "review", label: "Review", icon: "check", to: { kind: "review" }, reads: true },
      { id: "owners", label: "Owners", icon: "lock", to: { kind: "owners" }, reads: true },
    ],
  },
  // What the memory answered: the reader's sessions, and (for the org's admins) each conversation's trace.
  {
    group: "Answers",
    items: [
      {
        id: "sessions",
        label: "Sessions",
        icon: "notes",
        to: { kind: "folder", path: ["sessions"] },
        reads: true,
      },
    ],
  },
  // Whether it pays off: the advice served and how it went, what it learned, what it couldn't answer, who uses it.
  {
    group: "Impact",
    items: [
      { id: "advice", label: "Advice", icon: "skill", to: { kind: "advice" }, reads: true },
      // What it learned: every lesson the person may read (their own and the team's; an admin's, everyone's).
      { id: "learned", label: "Learned", icon: "history", to: { kind: "learned" }, reads: true },
      { id: "gaps", label: "Not answered", icon: "quote", to: { kind: "gaps" }, reads: true },
      {
        id: "handover",
        label: "Handover",
        icon: "alert",
        to: { kind: "handover" },
        reads: false,
      },
      { id: "people", label: "People", icon: "user", to: { kind: "people" }, reads: true },
    ],
  },
  // What shapes what it learns: the types, the goals, the compiled skills, the branches, the tools agents call.
  {
    group: "Build",
    items: [
      { id: "schema", label: "Schema", icon: "schema", to: { kind: "schema" }, reads: true },
      { id: "goals", label: "Goals", icon: "goal", to: { kind: "goals" }, reads: false },
      {
        id: "skills",
        label: "Skills",
        icon: "skill",
        to: { kind: "folder", path: ["skills"] },
        reads: true,
      },
      {
        id: "branches",
        label: "Branches",
        icon: "branch",
        to: { kind: "branches" },
        reads: true,
      },
      { id: "tools", label: "Tools", icon: "tools", to: { kind: "tools" }, reads: true },
    ],
  },
];

/** What `MemoryStudio` takes. */
export interface MemoryStudioProps {
  /** How the pages reach the memory, and what else the host keeps (saved arms, area owners, members). */
  transport: Transport;
  /** The org whose memory is read, as the pages name it (the local server serves one, named as it likes). */
  org: string;
  /** Who may read the memory: false shows only the sections that don't (Goals, Handover). */
  canBrowse?: boolean;
  /** The section a link without a view opens on: Home unless given. */
  start?: "home" | "explorer" | "goals" | "handover";
  /** The host's Goals section (slot `goals`), drawn in its own styles beside the sidebar. */
  goals?: React.ReactNode;
  /** The host's Handover section (slot `handover`), for the org's admins: the first section when given. */
  handover?: React.ReactNode;
  /**
   * The org's admin (a member who may read people's activity, advice trails and branches). Unset, whoever the
   * transport's `admin` capability allows (the local caller, once their server lets them).
   */
  admin?: boolean;
  /** Light, dark, or the system's (`system`). The system's unless given. */
  theme?: "light" | "dark" | "system";
  /**
   * Where the host connects a source and an agent to the memory (a console's Sources and setup pages), for Home's
   * actions and a new memory's guide. Without them, Home says how instead.
   */
  connect?: { sources?: string; agents?: string };
  /**
   * Who draws the sections' sidebar: the pages (`sidebar`, the default), or the host (`host`), which is then told
   * the sections through `onNavigation` and draws them in its own navigation. With `host` the pages keep the memory
   * and branch pickers in a bar above the section, and leave ⌘K to the host (`/` still opens Search).
   */
  navigation?: "sidebar" | "host";
  /** The sections this person has, and the one open, each time they change (for a host that draws them). */
  onNavigation?: (nav: StudioNavigation) => void;
}

/** A section the host may draw: its link is the hash to set on the page that shows the studio. */
export interface StudioNavItem {
  id: string;
  label: string;
  /** The view's hash (`#/…`), to put after the host page's path. */
  hash: string;
  /** The section open now. */
  current: boolean;
  /** A count to show beside it (the Schema's types), when there is one. */
  count?: number;
}
export interface StudioNavGroup {
  /** The group's heading ("" for the first, ungrouped one). */
  group: string;
  items: StudioNavItem[];
}
export interface StudioNavigation {
  groups: StudioNavGroup[];
}

/**
 * An org's memory, laid out like OpenViking Studio. A sidebar of sections (Home, then the memory, its review, what
 * it answered, its impact and what builds it, as the transport and the host allow) beside the one picked. The
 * memory is read as the caller, the schema too; who can't read it sees only the host's sections (`goals`,
 * `handover`), drawn in the host's own styles beside this.
 */
export function MemoryStudio({
  transport,
  org,
  canBrowse = true,
  start = "home",
  goals,
  handover,
  admin: isAdmin,
  theme = "system",
  connect,
  navigation = "sidebar",
  onNavigation,
}: MemoryStudioProps) {
  const hostNav = navigation === "host";
  // Every call the pages make below goes through it.
  setTransport(transport);
  // What the transport reaches: what it says it does at once, what it probes once the server answers.
  const [caps, setCaps] = useState<{ of: Transport; org: string; caps: Resolved } | null>(null);
  useEffect(() => {
    let live = true;
    void probe(transport, { org }).then((c) => live && setCaps({ of: transport, org, caps: c }));
    return () => {
      live = false;
    };
  }, [transport, org]);
  const can: Resolved = caps?.of === transport && caps.org === org ? caps.caps : given(transport);
  const admin = (isAdmin ?? true) && can.admin;
  const root = {
    className: "memory-studio",
    "data-studio-theme": theme,
    "data-navigation": hostNav ? "host" : undefined,
  };
  const hash = useSyncExternalStore(subscribe, () => location.hash);
  const search = useSyncExternalStore(subscribe, () => location.search);
  // A view's options (`?…` after it) are read by the view itself: changing one doesn't move the focus.
  const view = viewOf(hash);
  const focus = useMemo(() => fromHash(view), [view]);
  // A trace is the org's admins' to read: anyone else lands on the first section they have.
  const wanted =
    focus?.kind === "conversation" && !admin
      ? "people"
      : sectionOf(focus, canBrowse || start === "handover" ? start : "goals");
  const visible = SECTIONS.map((g) => ({
    ...g,
    items: g.items.filter(
      (i) =>
        (canBrowse || !i.reads) &&
        (i.id !== "goals" || goals) &&
        (i.id !== "handover" || handover) &&
        (!ADMIN.includes(i.id) || admin) &&
        (!NEEDS[i.id] || can[NEEDS[i.id]!]),
    ),
  })).filter((g) => g.items.length);
  const section = visible.some((g) => g.items.some((i) => i.id === wanted))
    ? wanted
    : (visible[0]?.items[0]?.id ?? "schema");
  const sel = useMemo<Selection>(() => {
    const q = new URLSearchParams(search);
    return { org, team: q.get("team") ?? undefined, branch: q.get("branch") ?? undefined };
  }, [org, search]);
  const [who, setWho] = useState<{ key: string; me?: Me; error?: ApiError | Error } | null>(null);
  const [query, setQuery] = useState("");
  // The memory's branches with their skills (null: a memstore that can't name them).
  const [branches, setBranches] = useState<{
    key: string;
    list: Branch[] | null | undefined;
  } | null>(null);
  const memoryKey = JSON.stringify([sel.org, sel.team]);
  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);
  // On Home (main read), its one read names the branches and counts the schema's types: neither is read here
  // while it is out, nor after it answered them. undefined: not answered yet; null: a server with no such read.
  const [homeSaid, setHomeSaid] = useState<{ key: string; h: HomeSummary | null } | null>(null);
  const onHome = section === "home" && (sel.branch ?? "main") === "main";
  const said = homeSaid?.key === selKey ? homeSaid.h : undefined;
  const homeNames = onHome && (said === undefined || (said !== null && said.branches !== null));
  const [schema, retrySchema] = useOrgSchema(sel, canBrowse && !(onHome && said !== null));
  // The memory whose branches were named: not read again for it.
  const named = useRef<string | null>(null);
  const onSummary = useCallback(
    (h: HomeSummary | null) => {
      setHomeSaid({ key: selKey, h });
      if (h?.branches && (sel.branch ?? "main") === "main") {
        named.current = memoryKey;
        setBranches({ key: memoryKey, list: h.branches });
      }
    },
    [selKey, memoryKey],
  );

  useEffect(() => {
    if (!canBrowse || homeNames || named.current === memoryKey) return;
    let live = true;
    browse<{ branches: Branch[] | null }>(
      { op: "branches" },
      { org: sel.org, team: sel.team },
    ).then(
      (r) => {
        if (!live) return;
        named.current = memoryKey;
        setBranches({ key: memoryKey, list: r.branches });
      },
      // Unreachable is not "can't name them": pick nothing until it answers.
      () => live && setBranches({ key: memoryKey, list: undefined }),
    );
    return () => {
      live = false;
    };
  }, [memoryKey, canBrowse, homeNames]);

  useEffect(() => {
    if (!canBrowse) return;
    let live = true;
    loadMe<Me>(sel).then(
      (me) => live && setWho({ key: selKey, me }),
      (e: Error) => live && setWho({ key: selKey, error: e }),
    );
    return () => {
      live = false;
    };
  }, [selKey, canBrowse]);

  // The search box shows the search being read, also when it was opened from a link.
  const searched = focus?.kind === "search" ? focus.query : null;
  const [shown, setShown] = useState<string | null>(null);
  if (searched !== shown) {
    setShown(searched);
    if (searched != null) setQuery(searched);
  }

  // Below 900px the tree is a drawer, closed on every move.
  const [drawer, setDrawer] = useState(false);
  // Whether the tree is on screen: always when wide, unless the host draws the sections (then the "Folders" button
  // shows it); narrow, only while its drawer is open. Hidden, it reads nothing.
  const wide = useMedia("(min-width: 901px)");
  const searchRef = useRef<HTMLInputElement>(null);
  const go = useCallback((f: Focus) => {
    setDrawer(false);
    const h = toHash(f);
    if (h !== location.hash) navigate(`${location.pathname}${location.search}${h}`);
  }, []);
  // ⌘K (or Ctrl+K, or /) opens Search with its box focused, from anywhere but a text field.
  useEffect(() => {
    const on = (e: KeyboardEvent) => {
      const typing =
        e.target instanceof HTMLElement && /^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName);
      if (
        canBrowse &&
        ((!hostNav && (e.metaKey || e.ctrlKey) && e.key === "k") || (e.key === "/" && !typing))
      ) {
        e.preventDefault();
        if (fromHash(location.hash)?.kind !== "search") go({ kind: "search", query: "" });
        requestAnimationFrame(() => {
          searchRef.current?.focus();
          searchRef.current?.select();
        });
      } else if (e.key === "Escape") setDrawer(false);
    };
    addEventListener("keydown", on);
    return () => removeEventListener("keydown", on);
  }, [canBrowse, go, hostNav]);

  const choose = (s: { team?: string; branch?: string }) => {
    const q = new URLSearchParams();
    if (s.team) q.set("team", s.team);
    if (s.branch && s.branch !== "main") q.set("branch", s.branch);
    navigate(`${location.pathname}${q.size ? `?${q}` : ""}${location.hash}`);
  };

  const current = who?.key === selKey ? who : null;
  const me = current?.me;
  const teams = useMemo(() => {
    const m = (me?.memberships ?? {}) as Record<string, { teams?: string[] } | undefined>;
    return m[org]?.teams ?? [];
  }, [me, org]);
  const reveal =
    focus && (focus.kind === "folder" || focus.kind === "entity" || focus.kind === "skill")
      ? focus.path
      : null;
  const branchList = branches?.key === memoryKey ? branches.list : undefined;
  // What a person reads: not the branches a change waits on for its owners (Branches lists those, to settle).
  const readable = branchList && branchList.filter((b) => !WORKING_BRANCH.test(b.name));
  const branch = sel.branch ?? "main";
  // Skills usually live on a goal's branch: say where, when there are none on the one read.
  const elsewhere =
    readable && !readable.some((b) => b.name === branch && (b.skills ?? 0) > 0)
      ? readable.filter((b) => b.name !== branch && (b.skills ?? 0) > 0)
      : [];
  const pickBranch = (b: string) => choose({ team: sel.team, branch: b || undefined });

  const pickers = canBrowse && (
    <div className="studio-pickers">
      {can.teams && (teams.length > 0 || sel.team) ? (
        <select
          className="field"
          aria-label="Memory"
          title="The memory you are reading: the org's own, or one of its teams'"
          value={sel.team ?? ""}
          onChange={(e) => choose({ team: e.target.value || undefined, branch: sel.branch })}
        >
          <option value="">All of {org}</option>
          {sel.team && !teams.includes(sel.team) && <option value={sel.team}>{sel.team}</option>}
          {teams.map((t) => (
            <option key={t} value={t}>
              Team: {t}
            </option>
          ))}
        </select>
      ) : hostNav ? null : (
        // A host names the org itself: the label says nothing more there.
        <div className="context-label mono" title="The memory you are reading">
          {me?.scope ?? `org:${org}`}
        </div>
      )}
      {readable === undefined ? null : readable ? (
        readable.length > 1 || branch !== "main" ? (
          <select
            className="field mono"
            aria-label="Branch"
            title="The branch you are reading: compiled skills usually live on their goal's branch"
            value={branch}
            onChange={(e) => pickBranch(e.target.value)}
          >
            {!readable.some((b) => b.name === branch) && <option value={branch}>{branch}</option>}
            {readable.map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}
                {b.skills ? ` (${b.skills} skills)` : ""}
              </option>
            ))}
          </select>
        ) : hostNav ? null : (
          <div className="context-label" title="The branch you are reading">
            <Icon name="branch" size={14} />
            main
          </div>
        )
      ) : (
        <BranchInput key={branch} branch={branch} onPick={pickBranch} />
      )}
    </div>
  );

  // For a host that draws the sections: tell it what they are whenever that changes.
  const typeCount = schema?.types?.size ?? (onHome && said ? said.schema?.types : undefined);
  const navigationKey = JSON.stringify([
    visible.map((g) => [g.group, g.items.map((i) => i.id)]),
    section,
    typeCount,
  ]);
  useEffect(() => {
    if (!onNavigation) return;
    onNavigation({
      groups: visible.map((g) => ({
        group: g.group,
        items: g.items.map((i) => ({
          id: i.id,
          label: i.label,
          hash: toHash(i.to),
          current: section === i.id,
          count: i.id === "schema" && typeCount ? typeCount : undefined,
        })),
      })),
    });
    // `visible` is rebuilt every render: its key says when it changed.
  }, [navigationKey, onNavigation]);

  const nav = hostNav ? (
    pickers ? (
      <div className="memory studio-bar" aria-label="Memory being read">
        {pickers}
      </div>
    ) : null
  ) : (
    <aside className="memory studio-nav" aria-label="Memory sections">
      {pickers}
      {visible.map((g) => (
        <div key={g.group} className="studio-group">
          {g.group && <div className="side-section">{g.group}</div>}
          {g.items.map((i) => (
            <button
              key={i.id}
              className={`studio-item ${section === i.id ? "on" : ""}`}
              aria-current={section === i.id ? "page" : undefined}
              onClick={() => go(i.to)}
            >
              <Icon name={i.icon} size={16} />
              <span>{i.label}</span>
              {i.id === "schema" && typeCount !== undefined && (
                <span className="studio-count">{typeCount}</span>
              )}
            </button>
          ))}
        </div>
      ))}
    </aside>
  );

  if (section === "handover") {
    return (
      <div {...root}>
        {nav}
        <section className="studio-goals">{handover}</section>
      </div>
    );
  }

  // Owner review and the graph: any member's, read as them (memstore decides who may decide).
  if (section === "review" || section === "graph") {
    return (
      <div {...root}>
        {nav}
        <section className="memory studio-goals rv-shell">
          {section === "review" ? (
            <ReviewSection
              key={memoryKey}
              sel={sel}
              me={me?.user}
              admin={admin}
              proposal={focus?.kind === "review" ? focus.proposal : undefined}
              schema={schema?.types}
              onFocus={go}
            />
          ) : (
            <GraphSection
              key={memoryKey}
              sel={sel}
              me={me?.user}
              focus={focus?.kind === "graph" ? focus.entity : undefined}
              onFocus={go}
            />
          )}
        </section>
      </div>
    );
  }

  if (
    (section === "sessions" && focus?.kind === "conversation") ||
    section === "people" ||
    section === "advice" ||
    section === "learned" ||
    section === "serving" ||
    section === "branches" ||
    section === "owners" ||
    section === "gaps"
  ) {
    // A person's activity opens over the same window as the list it was picked from.
    const toPerson = (user: string | undefined) => {
      const days = optionsOf(location.hash).get("days");
      go({ kind: "people", user });
      if (days) setOptions({ days });
    };
    return (
      <div {...root}>
        {nav}
        <section className="studio-goals">
          {focus?.kind === "conversation" ? (
            <Trace
              sel={sel}
              id={focus.id}
              onUser={toPerson}
              onEpisode={(id) => go({ kind: "episode", id })}
              onOpen={(id) => go({ kind: "conversation", id })}
            />
          ) : section === "people" ? (
            <People
              sel={sel}
              user={focus?.kind === "people" ? focus.user : undefined}
              onUser={toPerson}
              onEpisode={(id) => go({ kind: "episode", id })}
              onTrace={(id) => go({ kind: "conversation", id })}
            />
          ) : section === "learned" ? (
            <Learned
              key={memoryKey}
              sel={sel}
              lesson={focus?.kind === "learned" ? focus.lesson : undefined}
              onLesson={(lesson) => {
                // Picking a lesson keeps whose lessons are shown.
                const show = optionsOf(location.hash).get("show");
                go({ kind: "learned", lesson });
                if (show) setOptions({ show });
              }}
              onTrace={(id) => go({ kind: "conversation", id })}
              onAdvice={(note) => {
                go({ kind: "advice" });
                setOptions({ note });
              }}
            />
          ) : section === "advice" || section === "serving" ? (
            // Serving's lookup tables now sit under the Advice page's diagnostics, with the evidence.
            <AdvicePage
              sel={sel}
              admin={admin}
              onTrace={(id) => go({ kind: "conversation", id })}
              onLearned={(lesson) => go({ kind: "learned", lesson })}
              diagnostics={
                <>
                  <Advice sel={sel} onUser={toPerson} />
                  <AdviceServing sel={sel} onUser={toPerson} />
                </>
              }
            />
          ) : section === "gaps" ? (
            <NotAnswered
              sel={sel}
              onUser={toPerson}
              onEpisode={(id) => go({ kind: "episode", id })}
              onTrace={(id) => go({ kind: "conversation", id })}
            />
          ) : section === "owners" ? (
            <Owners sel={sel} />
          ) : (
            <Branches sel={sel} branches={branchList} />
          )}
        </section>
      </div>
    );
  }

  if (section === "goals") {
    return (
      <div {...root}>
        {nav}
        <section className="panel studio-goals">{goals}</section>
      </div>
    );
  }

  const error = current?.error;
  const refused = me && (me as Me & { refused?: string | null }).refused;
  const banners = (
    <div className="banners">
      {(error || refused) && (
        <div className="banner banner-error" role="alert">
          <Icon name="alert" size={14} /> {error?.message ?? refused}
        </div>
      )}
      {elsewhere.length > 0 && (section === "explorer" || section === "skills") && (
        <div className="banner banner-note">
          <Icon name="branch" size={14} /> No compiled skills on{" "}
          <span className="mono">{branch}</span>. They are on{" "}
          {elsewhere.map((b, i) => (
            <span key={b.name}>
              {i > 0 && ", "}
              <button className="link mono" onClick={() => pickBranch(b.name)}>
                {b.name}
              </button>{" "}
              ({b.skills})
            </span>
          ))}
          .
        </div>
      )}
    </div>
  );
  const crumbs = reveal && reveal.length > 0 && (
    <nav className="crumbs" aria-label="Location">
      <button onClick={() => go({ kind: "folder", path: [] })}>Memory</button>
      {reveal.map((seg, i) => (
        <span key={i} style={{ display: "contents" }}>
          <Icon name="chevron" size={12} />
          <button
            className={i === reveal.length - 1 ? "crumb-on" : ""}
            onClick={() =>
              go(
                (focus?.kind === "entity" || focus?.kind === "skill") && i === reveal.length - 1
                  ? focus
                  : { kind: "folder", path: reveal.slice(0, i + 1) },
              )
            }
          >
            {focus?.kind === "entity" && i === reveal.length - 1 && focus.path.length === 3
              ? displayName(focus.path[1] === "_" ? seg : `${focus.path[1]}:${seg}`)
              : seg || "∅"}
          </button>
        </span>
      ))}
    </nav>
  );
  const types = schema?.types;

  let body: React.ReactNode;
  if (section === "explorer") {
    body = (
      <div className={`app ${hostNav ? "hosted" : ""} ${drawer ? "drawer-open" : ""}`}>
        <header className="bar">
          <button
            className="icon-btn menu-btn"
            aria-label={drawer ? "Close the tree" : "Open the tree"}
            onClick={() => setDrawer((d) => !d)}
          >
            <Icon name={drawer ? "close" : "menu"} />
            {hostNav && <span className="menu-label">Folders</span>}
          </button>
          <div className="bar-title">
            <Icon name="explorer" size={16} /> Context tree
          </div>
          <button
            className="search-box search-open"
            hidden={hostNav}
            onClick={() => {
              go({ kind: "search", query: "" });
              requestAnimationFrame(() => searchRef.current?.focus());
            }}
          >
            <Icon name="search" size={15} />
            <span>Search facts</span>
            <span className="kbd">⌘K</span>
          </button>
        </header>
        {banners}
        <main className="main">
          <aside className="side" aria-label="Memory">
            <div className="side-scroll">
              <div className="side-section">Memory</div>
              <Tree
                key={selKey}
                sel={sel}
                focus={focus}
                onFocus={go}
                reveal={reveal}
                shown={drawer || (wide && !hostNav)}
              />
            </div>
          </aside>
          <div className="scrim" onClick={() => setDrawer(false)} />
          <section className="pane">
            <div className="pane-inner">
              {crumbs}
              <Inspector
                focus={focus}
                sel={sel}
                onFocus={go}
                schema={types}
                admin={admin}
                me={me?.user}
              />
            </div>
          </section>
        </main>
      </div>
    );
  } else if (section === "tools") {
    body = (
      <div className={`app app-page app-tools ${hostNav ? "hosted" : ""}`}>
        {banners}
        <main className="main">
          <Tools
            key={selKey}
            sel={sel}
            tool={focus?.kind === "tools" ? focus.tool : undefined}
            onFocus={go}
          />
        </main>
      </div>
    );
  } else {
    body = (
      <div className="app app-page">
        {banners}
        <section className="pane">
          <div className="pane-inner">
            {section === "home" ? (
              <Home
                key={selKey}
                sel={sel}
                scope={sel.team ? `Team ${sel.team}` : org}
                me={me?.user}
                admin={admin}
                can={can}
                href={(f, options) => {
                  const o = new URLSearchParams(options);
                  return `${toHash(f)}${o.size ? `?${o}` : ""}`;
                }}
                connect={connect}
                onSummary={onSummary}
                // The "Add to memory" control, where the transport can learn. The dream's summary is Home's own
                // read (an admin's).
                addToMemory={transport.capabilities.learn ? <AddToMemoryDialog sel={sel} /> : undefined}
              />
            ) : section === "ask" ? (
              <Agent sel={sel} />
            ) : section === "search" ? (
              <>
                <div className="ask-modes" role="tablist" aria-label="Search">
                  <button
                    className="ask-mode"
                    role="tab"
                    aria-selected={focus?.kind !== "ask"}
                    onClick={() => go({ kind: "search", query: query.trim() })}
                  >
                    Facts
                  </button>
                  <button
                    className="ask-mode"
                    role="tab"
                    aria-selected={focus?.kind === "ask"}
                    onClick={() => go({ kind: "ask", question: "", parts: "fae" })}
                  >
                    Look up
                  </button>
                </div>
                {focus?.kind === "ask" ? (
                  <Ask
                    sel={sel}
                    question={focus.question}
                    parts={partsOf(focus.parts)}
                    onAsk={(question, p) => go({ kind: "ask", question, parts: lettersOf(p) })}
                    onFocus={go}
                  />
                ) : (
                  <>
                    <form
                      className="retrieval"
                      role="search"
                      onSubmit={(e) => {
                        e.preventDefault();
                        if (query.trim()) go({ kind: "search", query: query.trim() });
                      }}
                    >
                      <div className="search-box retrieval-box">
                        <Icon name="search" size={16} />
                        <input
                          ref={searchRef}
                          type="search"
                          placeholder="Search facts: what memory knows, with the quote and episode behind each"
                          value={query}
                          onChange={(e) => setQuery(e.target.value)}
                          aria-label="Search facts"
                        />
                        <span className="kbd">⌘K</span>
                      </div>
                      <button className="btn btn-primary" type="submit" disabled={!query.trim()}>
                        Search
                      </button>
                      <button
                        type="button"
                        className="btn"
                        title="Call search_memory as an agent would"
                        onClick={() => go({ kind: "tools", tool: "search_memory" })}
                      >
                        <Icon name="tools" size={14} /> As an agent
                      </button>
                    </form>
                    {focus?.kind === "search" && focus.query ? (
                      <Inspector
                        focus={focus}
                        sel={sel}
                        onFocus={go}
                        schema={types}
                        admin={admin}
                        me={me?.user}
                      />
                    ) : (
                      <div className="retrieval-start">
                        <div className="eyebrow">Search</div>
                        <h1 className="title">Search the org's memory</h1>
                        <p className="muted">
                          The 30 facts that best match, current and past, each with the quote it
                          rests on and the episodes that say it. Searches read as you, so they find
                          what you can read.
                        </p>
                        <p className="muted">
                          To see what an agent gets for a question (facts, advice and the episodes
                          behind them, through its tools), use{" "}
                          <button
                            className="link"
                            onClick={() =>
                              go({ kind: "ask", question: query.trim(), parts: "fae" })
                            }
                          >
                            Look up
                          </button>
                          .
                        </p>
                      </div>
                    )}
                  </>
                )}
              </>
            ) : section === "schema" ? (
              <SchemaView
                schema={schema}
                retry={retrySchema}
                sel={sel}
                type={focus?.kind === "schema" ? focus.type : undefined}
                canBrowse={canBrowse}
                onFocus={go}
              />
            ) : (
              // Skills and Sessions: the Explorer's own folder, on a page of its own.
              <>
                {section === "sessions" && (
                  <div className="banner banner-note">
                    <Icon name="notes" size={14} />
                    <span>
                      Your sessions, as the memory kept them.
                      {admin ? (
                        <>
                          {" "}
                          Everyone&rsquo;s, with each conversation&rsquo;s trace:{" "}
                          <button className="link" onClick={() => go({ kind: "people" })}>
                            People
                          </button>
                          .
                        </>
                      ) : null}{" "}
                      The advice served in them:{" "}
                      <button className="link" onClick={() => go({ kind: "advice" })}>
                        Advice
                      </button>
                      .
                    </span>
                  </div>
                )}
                {crumbs}
                <Inspector
                  focus={
                    focus?.kind === "skill" || focus?.kind === "folder"
                      ? focus
                      : { kind: "folder", path: [section === "sessions" ? "sessions" : "skills"] }
                  }
                  sel={sel}
                  onFocus={go}
                  schema={types}
                />
              </>
            )}
          </div>
        </section>
      </div>
    );
  }

  return (
    <div {...root}>
      {nav}
      <div className="memory">{body}</div>
    </div>
  );
}

/** Which parts a question uses, as its link spells them: `f` facts, `a` advice, `e` episodes. */
function partsOf(letters: string | undefined): AskParts {
  if (!letters) return ALL_PARTS;
  return {
    facts: letters.includes("f"),
    advice: letters.includes("a"),
    episodes: letters.includes("e"),
  };
}
const lettersOf = (p: AskParts) =>
  `${p.facts ? "f" : ""}${p.advice ? "a" : ""}${p.episodes ? "e" : ""}`;

/** The branch to read, by name, for a memstore that can't list its branches. */
function BranchInput({ branch, onPick }: { branch: string; onPick: (b: string) => void }) {
  const [v, setV] = useState(branch);
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        onPick(v.trim());
      }}
    >
      <input
        className="field mono"
        aria-label="Branch"
        title="The branch you are reading (this server can't list them): compiled skills usually live on their goal's branch"
        placeholder="branch: main"
        value={v}
        onChange={(e) => setV(e.target.value)}
        onBlur={() => v.trim() !== branch && onPick(v.trim())}
      />
    </form>
  );
}

/** Whether a media query matches, kept up to date as the window changes. */
function useMedia(query: string): boolean {
  return useSyncExternalStore(
    (on) => {
      if (typeof matchMedia !== "function") return () => {};
      const m = matchMedia(query);
      m.addEventListener("change", on);
      return () => m.removeEventListener("change", on);
    },
    () => typeof matchMedia !== "function" || matchMedia(query).matches,
  );
}
