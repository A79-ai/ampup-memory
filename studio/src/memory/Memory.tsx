import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { ApiError, browse, me as loadMe } from "./api";
import { displayName } from "./format";
import { Agent } from "./agent";
import { ALL_PARTS, Ask, type AskParts } from "./ask";
import { Inspector } from "./inspector";
import { SchemaView, useOrgSchema } from "./schema";
import { Tools } from "./tools";
import { Tree } from "./tree";
import type { Focus, Me, Selection } from "./types";
import { Icon } from "./ui";
import { ReviewSection } from "./review";
import { GraphSection } from "./graph";
import { Home, type HomeSummary } from "./home";
import { AddToMemory, AddToMemoryDialog } from "./addToMemory";
import { ImportActivity } from "./importActivity";
import { RepoSkills, SkillReviewCard, useProposedCount } from "./repoSkills";
import { Advice, AdviceServing, NotAnswered, Owners, People } from "../studio/Admin";
import { AdvicePage } from "../studio/AdvicePage";
import { SCOPE_OPTIONS } from "../studio/adviceScope";
import { Learned } from "../studio/Learned";
import { Trace } from "../studio/Trace";
import { NAVIGATE, optionsOf, setOptions, subscribe, useOption, viewOf } from "./hashOptions";
import { given, probe, setTransport, type Resolved, type Transport } from "../transport";
import { CanWrite } from "./writes";
import "../base.css";
import "./memory.css";
import "../theme.css";

// The focus lives in the address bar's hash, named for the page it opens, so a view can be linked to, read at a
// glance and survives a reload:
//   #/home                      Home (what changed, what it answered, what needs you)
//   #/import                    Import (add files or text to the memory: Add to memory as a page)
//   #/repo-skills[/<key>]       Repo skills (a connected repository's SKILL.md files, graded; one, and its proposal)
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
    case "import":
      return at("import");
    case "repo-skills":
    case "skill-proposals":
      return at(f.kind, f.skill ? encodeURIComponent(f.skill) : "");
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
    case "import":
      return { kind: "import" };
    case "repo-skills":
    case "skill-proposals":
      return { kind: name, skill: rest ? decodeURIComponent(rest) : undefined };
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
  | "import"
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
  | "repo-skills"
  | "skill-proposals"
  | "tools"
  | "goals";
/** The sections only the org's admins see. Advice is everyone's: a member sees the counts and their own sessions. */
const ADMIN: Section[] = ["people", "gaps", "branches", "owners"];

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
  "repo-skills": "repoSkills",
  "skill-proposals": "repoSkills",
};
function sectionOf(f: Focus | null, start: Section): Section {
  if (!f) return start;
  switch (f.kind) {
    case "home":
      return "home";
    case "import":
      return "import";
    case "repo-skills":
    case "skill-proposals":
      return f.kind;
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
/** `hide`: a page that still answers at its link but isn't listed (the Graph is a tab of the Explorer). */
type Item = { id: Section; label: string; icon: string; to: Focus; reads: boolean; hide?: boolean };
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
      // Feeding it comes first: files and text added by hand, before (or beside) a connected source.
      { id: "import", label: "Import", icon: "upload", to: { kind: "import" }, reads: true },
      {
        id: "explorer",
        label: "Explorer",
        icon: "explorer",
        to: { kind: "folder", path: [] },
        reads: true,
      },
      // The Graph is the Explorer's second tab: lit as the Explorer.
      { id: "graph", label: "Graph", icon: "entity", to: { kind: "graph" }, reads: true, hide: true },
      // Facts (a search) and Look up (a question through the agent tools): one page, two modes.
      {
        id: "search",
        label: "Search",
        icon: "search",
        to: { kind: "search", query: "" },
        reads: true,
      },
      { id: "ask", label: "Ask", icon: "chat", to: { kind: "agent" }, reads: true },
      // What the memory answered: the reader's own sessions, and for the org's admins everyone's (where Traces opens
      // for them) and the team's person by person, tabs of the same section with each conversation's trace.
      {
        id: "sessions",
        label: "Traces",
        icon: "notes",
        to: { kind: "folder", path: ["sessions"] },
        reads: true,
      },
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
  // Whether it pays off: the advice served and how it went, what it learned, what it couldn't answer, who uses it.
  {
    group: "Impact",
    items: [
      // Advice opens on its first tab, the lessons (`#/learned`); `#/advice` is its Serving tab.
      { id: "advice", label: "Advice", icon: "skill", to: { kind: "learned" }, reads: true },
      // What it learned: every lesson the person may read (their own and the team's; an admin's, everyone's). The
      // first tab of Advice, lit as it.
      {
        id: "learned",
        label: "Learned",
        icon: "history",
        to: { kind: "learned" },
        reads: true,
        hide: true,
      },
      { id: "gaps", label: "Not answered", icon: "quote", to: { kind: "gaps" }, reads: true },
      {
        id: "handover",
        label: "Handover",
        icon: "alert",
        to: { kind: "handover" },
        reads: false,
      },
      // The team's traces: the second tab of Traces, lit as it.
      { id: "people", label: "Team traces", icon: "user", to: { kind: "people" }, reads: true, hide: true },
    ],
  },
  // What shapes what it learns: the types, the goals, the compiled skills, the tools agents call.
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
      // A connected repository's own skills, graded and reviewed: Skills' second tab, lit as Skills.
      {
        id: "repo-skills",
        label: "Repo skills",
        icon: "code",
        to: { kind: "repo-skills" },
        reads: true,
        hide: true,
      },
      // Every skill whose newest review proposes a change: Skills' third tab, lit as Skills.
      {
        id: "skill-proposals",
        label: "Revisions proposed",
        icon: "refresh",
        to: { kind: "skill-proposals" },
        reads: true,
        hide: true,
      },
      // The memory's tools, called one at a time: not listed, as the Playground runs them for a question.
      { id: "tools", label: "Tools", icon: "tools", to: { kind: "tools" }, reads: true, hide: true },
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
  /**
   * With `host` navigation, an element of the host's (its sidebar's foot, say) where the memory and branch pickers
   * are drawn instead of in a bar above the section.
   */
  pickersIn?: HTMLElement | null;
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

/** The pages' outer box, telling every page below whether the reader may change the memory (`CanWrite`). */
function Frame({
  root,
  write,
  children,
}: {
  root: React.HTMLAttributes<HTMLDivElement> & Record<string, unknown>;
  write: boolean;
  children: React.ReactNode;
}) {
  return (
    <CanWrite.Provider value={write}>
      <div {...root}>{children}</div>
    </CanWrite.Provider>
  );
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
  pickersIn,
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
  // Adding to the memory: where the transport can, and not for a viewer, who only reads.
  const learns = transport.capabilities.learn === true && can.write;
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
    items: g.items
      // An admin's Traces opens on everyone's sessions (All); anyone else's, on their own.
      .map((i) => (i.id === "sessions" && admin ? { ...i, to: { kind: "people" } as Focus } : i))
      .filter(
      (i) =>
        (canBrowse || !i.reads) &&
        (i.id !== "goals" || goals) &&
        (i.id !== "handover" || handover) &&
        (i.id !== "import" || learns) &&
        (!ADMIN.includes(i.id) || admin) &&
        (!NEEDS[i.id] || can[NEEDS[i.id]!]),
    ),
  })).filter((g) => g.items.length);
  const section = visible.some((g) => g.items.some((i) => i.id === wanted))
    ? wanted
    : (visible[0]?.items[0]?.id ?? "schema");
  // The sections the sidebar lists (and the host is told of); `lit`: the one shown as open.
  const listed = visible
    .map((g) => ({ ...g, items: g.items.filter((i) => !i.hide) }))
    .filter((g) => g.items.length);
  const lit =
    section === "graph"
      ? "explorer"
      : section === "people"
        ? "sessions"
        : section === "learned"
          ? "advice"
          : section === "repo-skills" || section === "skill-proposals"
            ? "skills"
            : section;
  const sel = useMemo<Selection>(() => {
    const q = new URLSearchParams(search);
    // Studio reads main only: it names no branch, and a link's `branch` is not followed.
    return { org, team: q.get("team") ?? undefined };
  }, [org, search]);
  const [who, setWho] = useState<{ key: string; me?: Me; error?: ApiError | Error } | null>(null);
  const [query, setQuery] = useState("");
  // Bumped when Import adds something, so its Processing panel reads again at once.
  const [imported, setImported] = useState(0);
  const memoryKey = JSON.stringify([sel.org, sel.team]);
  const selKey = JSON.stringify([sel.org, sel.team, sel.branch]);
  // On Home, its Home read counts the schema's types: it isn't read here while that is out, nor after it answered. undefined: not answered yet; null: a server with no such read.
  const [homeSaid, setHomeSaid] = useState<{ key: string; h: HomeSummary | null } | null>(null);
  const onHome = section === "home";
  const said = homeSaid?.key === selKey ? homeSaid.h : undefined;
  const [schema, retrySchema] = useOrgSchema(sel, canBrowse && !(onHome && said !== null));
  const onSummary = useCallback(
    (h: HomeSummary | null) => setHomeSaid({ key: selKey, h }),
    [selKey],
  );


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

  // Below 900px the tree is a drawer over the page, closed on every move. Wider, it is a column beside the page
  // (hosted, shown by the "Folders" button), and stays open while the user moves through it.
  const [drawer, setDrawer] = useState(false);
  // Whether the tree is on screen: always when wide, unless the host draws the sections (then the "Folders" button
  // shows it); narrow, only while its drawer is open. Hidden, it reads nothing.
  const wide = useMedia("(min-width: 901px)");
  const searchRef = useRef<HTMLInputElement>(null);
  const go = useCallback(
    (f: Focus) => {
      if (!wide) setDrawer(false);
      const h = toHash(f);
      if (h !== location.hash) navigate(`${location.pathname}${location.search}${h}`);
    },
    [wide],
  );
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
    </div>
  );

  // For a host that draws the sections: tell it what they are whenever that changes.
  const typeCount = schema?.types?.size ?? (onHome && said ? said.schema?.types : undefined);
  const navigationKey = JSON.stringify([
    listed.map((g) => [g.group, g.items.map((i) => i.id)]),
    lit,
    typeCount,
  ]);
  useEffect(() => {
    if (!onNavigation) return;
    onNavigation({
      groups: listed.map((g) => ({
        group: g.group,
        items: g.items.map((i) => ({
          id: i.id,
          label: i.label,
          hash: toHash(i.to),
          current: lit === i.id,
          count: i.id === "schema" && typeCount ? typeCount : undefined,
        })),
      })),
    });
    // `visible` is rebuilt every render: its key says when it changed.
  }, [navigationKey, onNavigation]);

  const nav = hostNav ? (
    pickersIn ? (
      pickers ? (
        createPortal(
          <div className="memory-studio studio-pickers-root" data-studio-theme={theme}>
            <div className="memory studio-pickers-host" aria-label="Memory being read">
              {pickers}
            </div>
          </div>,
          pickersIn,
        )
      ) : null
    ) : pickers ? (
      <div className="memory studio-bar" aria-label="Memory being read">
        {pickers}
      </div>
    ) : null
  ) : (
    <aside className="memory studio-nav" aria-label="Memory sections">
      {pickers}
      {listed.map((g) => (
        <div key={g.group} className="studio-group">
          {g.group && <div className="side-section">{g.group}</div>}
          {g.items.map((i) => (
            <button
              key={i.id}
              className={`studio-item ${lit === i.id ? "on" : ""}`}
              aria-current={lit === i.id ? "page" : undefined}
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
      <Frame root={root} write={can.write}>
        {nav}
        <section className="studio-goals">{handover}</section>
      </Frame>
    );
  }

  // Owner review and the graph: any member's, read as them (memstore decides who may decide).
  if (section === "review" || section === "graph") {
    return (
      <Frame root={root} write={can.write}>
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
            <>
            <ExplorerTabs on="graph" go={go} />
            <GraphSection
              key={memoryKey}
              sel={sel}
              me={me?.user}
              focus={focus?.kind === "graph" ? focus.entity : undefined}
              onFocus={go}
            />
            </>
          )}
        </section>
      </Frame>
    );
  }

  if (
    (section === "sessions" && focus?.kind === "conversation") ||
    section === "people" ||
    section === "advice" ||
    section === "learned" ||
    section === "serving" ||
    section === "owners" ||
    section === "gaps"
  ) {
    // A person's activity opens over the same window as the list it was picked from.
    const toPerson = (user: string | undefined) => {
      const days = optionsOf(location.hash).get("days");
      go({ kind: "people", user });
      if (days) setOptions({ days });
    };
    // Within Advice, a move to another tab or note keeps what every total is counted over: the window and whose
    // own notes (adviceScope.tsx), plus any of `keep`.
    const inAdvice = (f: Focus, keep: Record<string, string | null> = {}) => {
      const now = optionsOf(location.hash);
      const scope = Object.fromEntries(SCOPE_OPTIONS.map((k) => [k, now.get(k)]));
      go(f);
      setOptions({ ...scope, ...keep });
    };
    return (
      <Frame root={root} write={can.write}>
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
            <>
            <TracesTabs on={optionsOf(hash).get("show") === "team" ? "team" : "all"} go={go} team />
            <People
              sel={sel}
              user={focus?.kind === "people" ? focus.user : undefined}
              everyone={optionsOf(hash).get("show") !== "team"}
              onUser={toPerson}
              onEpisode={(id) => go({ kind: "episode", id })}
              onTrace={(id) => go({ kind: "conversation", id })}
            />
            </>
          ) : section === "learned" ? (
            <>
            <AdviceTabs on="learned" go={inAdvice} admin={admin} />
            <ShowTabs />
            <Learned
              key={memoryKey}
              sel={sel}
              admin={admin}
              lesson={focus?.kind === "learned" ? focus.lesson : undefined}
              onLesson={(lesson) => {
                // Picking a lesson keeps whose lessons are shown.
                const show = optionsOf(location.hash).get("show");
                inAdvice({ kind: "learned", lesson }, { show });
              }}
              onTrace={(id) => go({ kind: "conversation", id })}
              onAdvice={(note) => inAdvice({ kind: "advice" }, { note })}
            />
            </>
          ) : section === "advice" || section === "serving" ? (
            // Advice's tabs: Serving (each team note), and for the org's admins Impact (`#/serving`): whether advice
            // helps, and when it is given.
            focus?.kind === "serving" && admin ? (
              <>
                <AdviceTabs on="impact" go={inAdvice} admin />
                <ImpactScope />
                <Advice sel={sel} onUser={toPerson} />
                <AdviceServing sel={sel} onUser={toPerson} />
              </>
            ) : (
              <>
                <AdviceTabs on="served" go={inAdvice} admin={admin} />
                <ShowTabs />
                <AdvicePage
                  sel={sel}
                  admin={admin}
                  everyone={can.everyone}
                  onTrace={(id) => go({ kind: "conversation", id })}
                  onLearned={(lesson) => inAdvice({ kind: "learned", lesson })}
                />
              </>
            )
          ) : section === "gaps" ? (
            <NotAnswered
              sel={sel}
              onUser={toPerson}
              onEpisode={(id) => go({ kind: "episode", id })}
              onTrace={(id) => go({ kind: "conversation", id })}
            />
          ) : (
            <Owners sel={sel} />
          )}
        </section>
      </Frame>
    );
  }

  if (section === "goals") {
    return (
      <Frame root={root} write={can.write}>
        {nav}
        <section className="panel studio-goals">{goals}</section>
      </Frame>
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
        {/* One line: where you are (the crumbs), then the search, the folders' toggle and the two views. */}
        <header className="bar explorer-bar">
          <div className="bar-title">
            {crumbs || (
              <nav className="crumbs" aria-label="Location">
                <button className="crumb-on" onClick={() => go({ kind: "folder", path: [] })}>
                  Memory
                </button>
              </nav>
            )}
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
          {/* The folders' drawer: a quiet toggle before the views. */}
          <button
            className="icon-btn menu-btn"
            aria-label={drawer ? "Hide the folders" : "Show the folders"}
            aria-expanded={drawer}
            onClick={() => setDrawer((d) => !d)}
          >
            <Icon name={drawer ? "close" : "menu"} size={16} />
            {hostNav && <span className="menu-label">{drawer ? "Hide folders" : "Folders"}</span>}
          </button>
          <ExplorerTabs on="tree" go={go} />
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
          {/* The playground fills the pane: its conversation scrolls inside, under a composer that stays. */}
          <div
            className={`pane-inner ${section === "ask" ? "pane-fill" : section === "import" ? "pane-hold" : ""}`}
          >
            {section === "repo-skills" || section === "skill-proposals" ? (
              <>
                <SkillsTabs
                  on={section === "repo-skills" ? "repo" : "proposed"}
                  go={go}
                  repo={can.repoSkills}
                  sel={sel}
                />
                <RepoSkills
                  key={`${selKey}:${section}`}
                  sel={sel}
                  skill={focus?.kind === section ? focus.skill : undefined}
                  proposed={section === "skill-proposals"}
                  onOpen={go}
                />
              </>
            ) : section === "import" ? (
              <div className="import-page">
                <h2>Import</h2>
                <p className="muted">
                  What a document says is learned as facts, and the procedures it describes are compiled into skills
                  where this memory compiles them.
                </p>
                <div className="import-grid">
                  <AddToMemory
                    key={selKey}
                    sel={sel}
                    onOpen={go}
                    onAdded={() => setImported((n) => n + 1)}
                    title="Add files, pages or text"
                  />
                  <ImportActivity sel={sel} bump={imported} onOpen={go} />
                </div>
              </div>
            ) : section === "home" ? (
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
                // The "Add to memory" control, where the transport can learn and the reader may write. The dream's
                // summary is Home's own read (an admin's).
                addToMemory={learns ? <AddToMemoryDialog sel={sel} /> : undefined}
              />
            ) : section === "ask" ? (
              <Agent
                sel={sel}
                onFocus={go}
                teams={can.teams ? teams : []}
                onScope={(team, then) => {
                  choose({ team, branch: sel.branch });
                  if (then) go(then);
                }}
                connect={connect}
                addToMemory={learns ? <AddToMemoryDialog sel={sel} /> : undefined}
              />
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
                {section === "sessions" && <TracesTabs on="mine" go={go} team={admin} />}
                {section === "skills" && (
                  <SkillsTabs on="compiled" go={go} repo={can.repoSkills} sel={sel} />
                )}
                {crumbs}
                {focus?.kind === "skill" && can.repoSkills && focus.path[0] === "skills" && (
                  <SkillReviewCard
                    key={`${selKey}:${focus.path.join("/")}`}
                    sel={sel}
                    skillKey={focus.path.join("/")}
                    onOpen={go}
                  />
                )}
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
    <Frame root={root} write={can.write}>
      {nav}
      <div className="memory">{body}</div>
    </Frame>
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

/** The Explorer's two views: the context tree and the graph of the same memory. */
function ExplorerTabs({ on, go }: { on: "tree" | "graph"; go: (f: Focus) => void }) {
  const tab = (id: "tree" | "graph", label: string, icon: string, to: Focus) => (
    <button
      role="tab"
      aria-selected={on === id}
      className={`explorer-tab ${on === id ? "on" : ""}`}
      onClick={() => on !== id && go(to)}
    >
      <Icon name={icon} size={14} /> {label}
    </button>
  );
  return (
    <div className="explorer-tabs" role="tablist" aria-label="Explorer">
      {tab("tree", "Context tree", "explorer", { kind: "folder", path: [] })}
      {tab("graph", "Graph", "entity", { kind: "graph" })}
    </div>
  );
}

/** A section's views, as tabs above its page (Traces: yours and the team's; Advice: served and learned). */
function PageTabs({
  label,
  on,
  tabs,
  go,
}: {
  label: string;
  on: string;
  tabs: { id: string; label: string; icon: string; to: Focus }[];
  go: (f: Focus) => void;
}) {
  return (
    // In Studio's styles wherever it is drawn: some of these pages are drawn outside them.
    <div className="memory traces-bar">
      <div className="explorer-tabs" role="tablist" aria-label={label}>
        {tabs.map((t) => (
          <button
            key={t.id}
            role="tab"
            aria-selected={on === t.id}
            className={`explorer-tab ${on === t.id ? "on" : ""}`}
            onClick={() => on !== t.id && go(t.to)}
          >
            <Icon name={t.icon} size={14} /> {t.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/** Whose a page shows, as one pill: everyone's (the default), the reader's own, or the team's. */
const WHOSE = [
  { id: "all", label: "All", icon: "explorer" },
  { id: "mine", label: "Mine", icon: "notes" },
  { id: "team", label: "Team", icon: "user" },
] as const;
type Whose = (typeof WHOSE)[number]["id"];

function WhoseTabs({ on, pick }: { on: Whose; pick: (w: Whose) => void }) {
  return (
    <div className="memory traces-bar">
      <div className="explorer-tabs" role="group" aria-label="Whose">
        {WHOSE.map((w) => (
          <button
            key={w.id}
            aria-pressed={on === w.id}
            className={`explorer-tab ${on === w.id ? "on" : ""}`}
            onClick={() => on !== w.id && pick(w.id)}
          >
            <Icon name={w.icon} size={14} /> {w.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/** Advice's and Learned's pill: `show` in the link (none: all), kept as the reader moves between the tabs. */
function ShowTabs() {
  const [show, setShow] = useOption("show");
  const on: Whose = show === "mine" || show === "team" ? show : "all";
  return <WhoseTabs on={on} pick={(w) => setShow(w === "all" ? null : w)} />;
}

/**
 * Skills' views: the skills the memory compiled (the Explorer's `skills` folder) and a connected repository's own
 * (Repo skills), where the host reaches them.
 */
function SkillsTabs({
  on,
  go,
  repo,
  sel,
}: {
  on: "compiled" | "repo" | "proposed";
  go: (f: Focus) => void;
  repo: boolean;
  sel: Selection;
}) {
  const proposed = useProposedCount(sel, repo);
  if (!repo) return null;
  return (
    <PageTabs
      label="Skills"
      on={on}
      go={go}
      tabs={[
        { id: "compiled", label: "Compiled", icon: "skill", to: { kind: "folder", path: ["skills"] } },
        { id: "repo", label: "From repos", icon: "code", to: { kind: "repo-skills" } },
        {
          id: "proposed",
          label: proposed ? `Revisions proposed (${proposed})` : "Revisions proposed",
          icon: "refresh",
          to: { kind: "skill-proposals" },
        },
      ]}
    />
  );
}

/**
 * Traces' views: every session the answer log holds (the default), the reader's own sessions, and the team's,
 * person by person. Only the org's admins read anyone else's: anyone else gets their own, with no pill.
 */
function TracesTabs({ on, go, team }: { on: Whose; go: (f: Focus) => void; team: boolean }) {
  if (!team) return null;
  return (
    <WhoseTabs
      on={on}
      pick={(w) => {
        if (w === "mine") return go({ kind: "folder", path: ["sessions"] });
        go({ kind: "people" });
        if (w === "team") setOptions({ show: "team" });
      }}
    />
  );
}

/**
 * What Impact counts, which isn't what Advice and Serving count: whether advice helps reads every judged session of
 * the team's notes, and when advice is given reads every lookup in its own window (7, 30 or 90 days; 30 for a link
 * that names all of them).
 */
function ImpactScope() {
  const days = optionsOf(useSyncExternalStore(subscribe, () => location.hash)).get("days");
  const span = days === "7" || days === "90" ? days : "30";
  return (
    <p className="ap-scope ap-muted ap-small">
      Counted apart from Advice and Serving: &ldquo;Does advice help&rdquo; reads the team&apos;s notes and every
      judged session; &ldquo;When advice is given&rdquo; reads every lookup in the last {span} days.
    </p>
  );
}

/**
 * Advice's views, in this order: Advice (every lesson with the runs it was learned from, `#/learned`), Serving (each
 * team note and how it was served, `#/advice`), and for the org's admins Impact (whether advice helps, and when it is
 * given, `#/serving`).
 */
function AdviceTabs({
  on,
  go,
  admin,
}: {
  on: "served" | "learned" | "impact";
  go: (f: Focus) => void;
  admin: boolean;
}) {
  return (
    <PageTabs
      label="Advice"
      on={on}
      go={go}
      tabs={[
        { id: "learned", label: "Advice", icon: "history", to: { kind: "learned" } },
        { id: "served", label: "Serving", icon: "skill", to: { kind: "advice" } },
        ...(admin
          ? [{ id: "impact", label: "Impact", icon: "goal", to: { kind: "serving" } as Focus }]
          : []),
      ]}
    />
  );
}
