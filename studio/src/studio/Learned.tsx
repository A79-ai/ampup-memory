import "./advice-page.css";
import "./learned.css";
import { useEffect, useMemo, useState } from "react";
import { ApiError, errorText } from "../transport";
import { call } from "../memory/api";
import { ago, when } from "../memory/format";
import { useOption } from "../memory/hashOptions";
import type { Selection } from "../memory/types";

// Studio's Learned page: each lesson AmpUp Memory learned from how runs went (memstore's `/lessons`, through
// `Transport.call(sel, "lessons")`), its text first, then the runs it was learned from and the sessions it was
// handed in since, with how each went and who said so. A person reads their own lessons and the team's; an org's
// admin, everyone's (memstore decides). The Advice page is the other side of the team's lessons: how often each is
// served, to whom, and ratifying it; a team lesson links there, and Advice links back here.

/** How a run or a session went (`/lessons`). */
type Outcome = "right" | "wrong" | "good" | "bad" | "unclear" | "open";
/** Who said how it went. */
type SaidBy = "person" | "pr" | "reward" | "judge" | "none" | "unknown";

/** A run or session a lesson was learned from. */
interface LearnedFrom {
  id: string;
  kind: "run" | "session";
  user: string;
  question: string;
  at: number;
  runs: number;
  right: number;
  wrong: number;
  outcome: Outcome;
  by: SaidBy;
}

/** A PR a session opened or pushed to, as `/lessons` names it. */
interface ServedPr {
  repo: string;
  number: number | null;
  state: string;
}

/** A session a lesson was served in. */
interface ServedIn {
  id: string;
  user: string;
  question: string;
  at: number;
  moment: string | null;
  harness: string | null;
  applied: boolean | null;
  helped: boolean | null;
  outcome: Outcome;
  by: SaidBy;
  reward: number | null;
  prs: ServedPr[];
  ended_at: number | null;
}

/** One learned advice note with its trails. */
interface Lesson {
  key: string;
  space: "mine" | "team";
  owner: string | null;
  category: string;
  situation: string;
  when: string;
  title: string;
  lesson: string;
  note_id: string;
  learned_at: number;
  /** `served`, `held` or `not served`. */
  status: string;
  /** A team lesson that outcomes or people's responses don't back yet: why it isn't ratified. */
  needs_backing?: string;
  /** A team lesson's: ratified by an admin, or backed and past the evidence bar. */
  ratified?: boolean;
  learned_from: {
    runs: number;
    right: number;
    wrong: number;
    people: number;
    trail: LearnedFrom[];
    /** Older entries the trail leaves out. */
    more: number;
    /** Other people's, counted and not listed. */
    hidden: number;
  };
  served_in: {
    sessions: number;
    people: number;
    helped: number;
    good: number;
    bad: number;
    trail: ServedIn[];
    more: number;
    hidden: number;
  };
  evidence?: { ratified?: boolean };
}

/** The lessons of one whose (the team's, `owner` null, or a person's own), with their trails' totals. */
interface Group {
  owner: string | null;
  lessons: number;
  runs: number;
  sessions: number;
  helped: number;
}

interface Lessons {
  /** `everyone`: the reader reads every person's lessons and trails (an org admin). */
  reader: { user: string; everyone: boolean };
  /** The people with lessons of their own. */
  people: string[];
  /** How many lessons the view shows, over every page. */
  count: number;
  lessons: Lesson[];
  /** Every lesson the reader reads, by whose (an engine that pages lessons says it). */
  groups?: Group[];
  /** Where the next page starts; null on the last (an engine that pages lessons says it). */
  next?: string | null;
}

/** Lessons a page asks for at a time. */
const PAGE = 30;

/**
 * A lesson as a link names it: a team lesson by its key, a person's own by its key and its owner (two people's
 * lessons for the same kind of task share a key).
 */
export const lessonId = (l: { key: string; owner: string | null }) =>
  l.owner ? `${l.key}@${l.owner}` : l.key;

/** Trail entries shown per side before "show all". */
const SHOWN = 4;

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
const same = (a: string | null | undefined, b: string | null | undefined) =>
  (a ?? "").toLowerCase() === (b ?? "").toLowerCase();

/** Who said how it went, in words. */
const SAID: Record<SaidBy, string> = {
  person: "graded by the person",
  pr: "by what came of its PR",
  reward: "scored by its reward",
  judge: "judged by a model",
  none: "",
  unknown: "",
};

/** Where in a session a lesson was handed. */
const MOMENT: Record<string, string> = {
  start: "at the start",
  prompt: "on a prompt",
  agent: "when the agent asked",
  next: "at the next step",
  fork: "at a fork",
  stuck: "when stuck",
  done: "before finishing",
};

function good(o: Outcome): boolean | null {
  if (o === "right" || o === "good") return true;
  if (o === "wrong" || o === "bad") return false;
  return null;
}

/** How one run or session went: ✓, ✕, open (a hollow ring) or unclear (a dot). */
function Mark({ outcome, by }: { outcome: Outcome; by: SaidBy }) {
  const g = good(outcome);
  const tone = g === true ? "ok" : g === false ? "bad" : outcome === "open" ? "open" : "none";
  const said = SAID[by] ? `, ${SAID[by]}` : "";
  return (
    <span className={`lr-mark lr-mark-${tone}`} title={`${outcome}${said}`}>
      <span aria-hidden>{g === true ? "✓" : g === false ? "✕" : ""}</span>
      <span className="st-sr">{outcome}</span>
    </span>
  );
}

/** A row of dots, one per run or session, as they went. */
function Dots({ ok, bad, other }: { ok: number; bad: number; other: number }) {
  const cap = 24;
  const all = [
    ...Array<string>(ok).fill("ok"),
    ...Array<string>(bad).fill("bad"),
    ...Array<string>(Math.max(0, other)).fill("other"),
  ];
  if (!all.length) return null;
  return (
    <span className="lr-dots" aria-hidden>
      {all.slice(0, cap).map((k, i) => (
        <i key={i} className={`lr-dot lr-dot-${k}`} />
      ))}
      {all.length > cap && <span className="ap-muted ap-small">+{all.length - cap}</span>}
    </span>
  );
}

/** Inline `code` and **bold**, as elements: the text is data, never HTML. */
function inline(s: string): React.ReactNode[] {
  return s.split(/(\*\*[^*]+\*\*|`[^`]+`)/).map((part, i) => {
    if (part.startsWith("**") && part.endsWith("**") && part.length > 4)
      return <b key={i}>{part.slice(2, -2)}</b>;
    if (part.startsWith("`") && part.endsWith("`") && part.length > 2)
      return <code key={i}>{part.slice(1, -1)}</code>;
    return part;
  });
}

interface Item {
  kind: string | null;
  text: string;
}
interface NoteSection {
  name: string;
  items: Item[];
  prose: string[];
}

/** A note's sections and their items, each item's `[kind]` apart and its `(runs: …)` citation dropped. */
export function sections(note: string): NoteSection[] {
  const out: NoteSection[] = [];
  let cur: NoteSection | null = null;
  for (const raw of note.split("\n")) {
    const h = /^##\s+(.*)$/.exec(raw);
    if (h) {
      cur = { name: (h[1] ?? "").trim(), items: [], prose: [] };
      out.push(cur);
      continue;
    }
    if (!cur || !raw.trim() || /^#\s/.test(raw)) continue;
    const li = /^(?:\d+[.)]|[-*])\s+(.*)$/.exec(raw);
    const text = (li ? (li[1] ?? "") : raw.trim()).replace(/\s*\(runs:[^)]*\)\s*\.?$/i, "").trim();
    const last = cur.items[cur.items.length - 1];
    if (li) {
      const k = /^\[([a-z-]+)(?:\s[^\]]*)?\]\s*(.*)$/i.exec(text);
      cur.items.push(
        k ? { kind: (k[1] ?? "").toLowerCase(), text: k[2] ?? "" } : { kind: null, text },
      );
    } else if (/^\s/.test(raw) && last) {
      last.text += ` ${text}`;
    } else {
      cur.prose.push(text);
    }
  }
  return out.filter((s) => s.items.length || s.prose.length);
}

/** What a section is called on the page. */
const HEADING: Record<string, string> = {
  Steps: "Do",
  Pitfalls: "Avoid",
  Verification: "Check",
  "Withhold when": "Not for",
  Triggers: "Watch for",
};
/** Shown open; the rest fold away under their names. */
const MAIN = ["Steps", "Pitfalls", "Verification", "Withhold when"];

function NoteSec({ s }: { s: NoteSection }) {
  const Tag = s.name === "Steps" ? "ol" : "ul";
  return (
    <section className={`lr-sec lr-sec-${s.name.toLowerCase().replace(/\s+/g, "-")}`}>
      <h4>{HEADING[s.name] ?? s.name}</h4>
      {s.prose.map((p, i) => (
        <p key={i}>{inline(p)}</p>
      ))}
      {s.items.length > 0 && (
        <Tag className="lr-items">
          {s.items.map((it, i) => (
            <li key={i}>
              {it.kind && <span className={`lr-kind lr-kind-${it.kind}`}>{it.kind}</span>}
              <span>{inline(it.text)}</span>
            </li>
          ))}
        </Tag>
      )}
    </section>
  );
}

function NoteBody({ note }: { note: string }) {
  const all = sections(note);
  const main = MAIN.flatMap((n) => all.filter((s) => s.name === n));
  const rest = all.filter((s) => s.name !== "When" && !MAIN.includes(s.name));
  return (
    <div className="lr-body">
      {main.map((s) => (
        <NoteSec key={s.name} s={s} />
      ))}
      {rest.length > 0 && (
        <details className="lr-more">
          <summary className="ap-muted ap-small">
            {rest.map((s) => HEADING[s.name] ?? s.name).join(" · ")}
          </summary>
          {rest.map((s) => (
            <NoteSec key={s.name} s={s} />
          ))}
        </details>
      )}
    </div>
  );
}

/** The person a trail entry names, as the reader knows them. */
const who = (user: string, me: string) => (same(user, me) ? "you" : user);

/** A session's PRs, each once: a PR whose number wasn't caught has none, so two can read the same. */
export function distinctPrs(prs: ServedPr[]): ServedPr[] {
  return prs.filter(
    (p, i) =>
      prs.findIndex((q) => q.repo === p.repo && q.number === p.number && q.state === p.state) === i,
  );
}

function PrChip({ p }: { p: ServedPr }) {
  const name = `${p.repo}${p.number != null ? `#${p.number}` : ""}`;
  const tone =
    p.state === "merged" ? "ok" : p.state === "reverted" || p.state === "regression" ? "bad" : "";
  const label = (
    <>
      <span className="mono">{name}</span> {p.state}
    </>
  );
  return p.number != null ? (
    <a
      className={`lr-pr ${tone && `lr-pr-${tone}`}`}
      href={`https://github.com/${p.repo}/pull/${p.number}`}
      target="_blank"
      rel="noreferrer"
    >
      {label}
    </a>
  ) : (
    <span className={`lr-pr ${tone && `lr-pr-${tone}`}`}>{label}</span>
  );
}

function Question({
  text,
  id,
  onTrace,
}: {
  text: string;
  id: string;
  onTrace: (id: string) => void;
}) {
  return (
    <button className="ap-link lr-q" onClick={() => onTrace(id)} title="Open its transcript">
      <span className="lr-q-text">
        {text ? inline(text) : <span className="ap-muted">(no request text)</span>}
      </span>
    </button>
  );
}

function FromRow({
  e,
  me,
  onTrace,
}: {
  e: LearnedFrom;
  me: string;
  onTrace: (id: string) => void;
}) {
  return (
    <li className="lr-row">
      <Mark outcome={e.outcome} by={e.by} />
      <div className="lr-row-text">
        <Question text={e.question} id={e.id} onTrace={onTrace} />
        <div className="ap-muted ap-small">
          {e.kind === "session" ? "session" : "run"} · {who(e.user, me)} ·{" "}
          <span title={when(e.at)}>{ago(e.at)}</span>
          {SAID[e.by] ? ` · ${SAID[e.by]}` : ""}
          {e.runs > 1 ? ` · ${e.right} right, ${e.wrong} wrong of ${e.runs} requests` : ""}
        </div>
      </div>
    </li>
  );
}

function ServedRow({ e, me, onTrace }: { e: ServedIn; me: string; onTrace: (id: string) => void }) {
  const prs = distinctPrs(e.prs);
  return (
    <li className="lr-row">
      <Mark outcome={e.outcome} by={e.by} />
      <div className="lr-row-text">
        <div className="lr-row-q">
          <Question text={e.question} id={e.id} onTrace={onTrace} />
          {e.helped === true && <span className="lr-pill lr-pill-ok">helped</span>}
          {e.helped !== true && e.applied === true && <span className="lr-pill">applied</span>}
        </div>
        <div className="ap-muted ap-small">
          {who(e.user, me)} · <span title={when(e.at)}>{ago(e.at)}</span>
          {e.moment ? ` · handed ${MOMENT[e.moment] ?? `at ${e.moment}`}` : ""}
          {e.outcome === "open" ? " · still open" : SAID[e.by] ? ` · ${SAID[e.by]}` : ""}
        </div>
        {prs.length > 0 && (
          <div className="lr-prs">
            {prs.map((p) => (
              <PrChip key={`${p.repo}#${p.number}#${p.state}`} p={p} />
            ))}
          </div>
        )}
      </div>
    </li>
  );
}

function Trail<T>({
  title,
  summary,
  dots,
  rows,
  hidden,
  more,
  empty,
  render,
}: {
  title: string;
  summary: React.ReactNode;
  dots: { ok: number; bad: number; other: number };
  rows: T[];
  hidden: string | null;
  more: number;
  empty: string;
  render: (r: T) => React.ReactNode;
}) {
  const [all, setAll] = useState(false);
  const shown = all ? rows : rows.slice(0, SHOWN);
  return (
    <section className="lr-trail">
      <header className="lr-trail-head">
        <h3>{title}</h3>
        <span className="ap-muted ap-small">{summary}</span>
      </header>
      <Dots {...dots} />
      {rows.length ? (
        <ul className="lr-rows">{shown.map(render)}</ul>
      ) : (
        !hidden && <p className="ap-muted ap-small">{empty}</p>
      )}
      {(rows.length > SHOWN || more > 0 || hidden) && (
        <p className="ap-muted ap-small lr-trail-foot">
          {rows.length > SHOWN && (
            <button className="ap-link" onClick={() => setAll(!all)}>
              {all ? "show fewer" : `show all ${rows.length}`}
            </button>
          )}
          {more > 0 && (
            <span>
              {rows.length > SHOWN ? " · " : ""}
              {more} older not listed
            </span>
          )}
          {hidden && (
            <span>
              {rows.length > SHOWN || more > 0 ? " · " : ""}
              {hidden}
            </span>
          )}
        </p>
      )}
    </section>
  );
}

function LessonCard({
  l,
  me,
  focused,
  onLesson,
  onTrace,
  onAdvice,
}: {
  l: Lesson;
  me: string;
  focused: boolean;
  onLesson: (id: string) => void;
  onTrace: (id: string) => void;
  onAdvice: (key: string) => void;
}) {
  const f = l.learned_from;
  const s = l.served_in;
  const mine = l.space === "mine";
  const id = lessonId(l);
  const whose = mine ? (l.owner && !same(l.owner, me) ? l.owner : "Yours") : "Team";
  const ratified = l.ratified ?? l.evidence?.ratified;
  const applies = sections(l.lesson).find((x) => x.name === "When");
  const whenText = applies
    ? [...applies.prose, ...applies.items.map((i) => i.text)].join(" ")
    : l.when;
  // Other people's runs and sessions are counted and never listed, unless the reader reads everyone's.
  const others = (n: number, what: string) =>
    n ? `+ ${plural(n, what)} by other people (counted, not listed)` : null;
  return (
    <article className={`lr-card${focused ? " lr-focused" : ""}`} id={`lesson-${id}`}>
      <div className="lr-top">
        <span className={`ap-badge ${mine ? "lr-badge-mine" : "lr-badge-team"}`}>{whose}</span>
        <span className="ap-muted ap-small ap-caps">{l.category}</span>
        {l.status !== "served" && <span className="ap-badge ap-badge-held">{l.status}</span>}
        {!mine &&
          (ratified ? (
            <span className="ap-badge ap-badge-ok">✓ Ratified</span>
          ) : (
            <span className="ap-badge" title={l.needs_backing}>
              Not ratified
            </span>
          ))}
        <span className="ap-muted ap-small lr-date" title={when(l.learned_at)}>
          learned {ago(l.learned_at)}
        </span>
      </div>
      <h2 className="lr-title">
        <button className="lr-title-link" onClick={() => onLesson(id)} title="Link to this lesson">
          {l.title || l.situation}
        </button>
      </h2>
      {whenText && <p className="lr-when">{inline(whenText)}</p>}
      {!mine && l.needs_backing && <p className="ap-muted ap-small">{l.needs_backing}</p>}
      <NoteBody note={l.lesson} />
      {!mine && (
        <p className="ap-small lr-advice">
          <button className="ap-link" onClick={() => onAdvice(l.key)}>
            How often it is served, and ratifying it: Advice →
          </button>
        </p>
      )}
      <div className="lr-flow">
        <Trail
          title="Learned from"
          summary={
            <>
              {plural(f.runs, "run")}
              {f.people > 1 ? ` · ${f.people} people` : ""}
              {f.runs ? ` · ${f.right} right, ${f.wrong} wrong` : ""}
            </>
          }
          dots={{ ok: f.right, bad: f.wrong, other: f.runs - f.right - f.wrong }}
          rows={f.trail}
          hidden={others(f.hidden, "run")}
          more={f.more}
          empty="No runs left in the log."
          render={(e) => <FromRow key={e.id} e={e} me={me} onTrace={onTrace} />}
        />
        <div className="lr-arrow" aria-hidden>
          <svg viewBox="0 0 24 24" width="22" height="22">
            <path
              d="M4 12h14M13 6l6 6-6 6"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.6"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </div>
        <Trail
          title="Served in"
          summary={
            s.sessions ? (
              <>
                {plural(s.sessions, "session")}
                {s.helped ? ` · helped in ${s.helped}` : ""}
                {s.good + s.bad ? ` · ${s.good} went well, ${s.bad} didn't` : ""}
              </>
            ) : (
              "not yet"
            )
          }
          dots={{ ok: s.good, bad: s.bad, other: s.sessions - s.good - s.bad }}
          rows={s.trail}
          hidden={others(s.hidden, "session")}
          more={s.more}
          empty={
            mine
              ? "Not handed yet: it will be the next time this kind of task starts."
              : "Not handed yet: agents get it the next time someone starts this kind of task."
          }
          render={(e) => <ServedRow key={e.id} e={e} me={me} onTrace={onTrace} />}
        />
      </div>
    </article>
  );
}

/** No lessons yet: how they come to be. */
function Empty({ filtered }: { filtered: boolean }) {
  if (filtered) return <p className="ap-pad ap-muted">No lessons here. Pick another view above.</p>;
  return (
    <div className="lr-card lr-empty">
      <h2 className="lr-title">Nothing learned yet</h2>
      <p className="ap-muted">
        Lessons come from how your agents&apos; work turned out. AmpUp Memory sorts each finished
        run or session by the kind of task it was, and once a kind has enough that went right and
        wrong, it writes down what separates them.
      </p>
      <ol>
        <li>
          <b>Work.</b> Agents open a session (<code>start_session</code>) or record a run (
          <code>POST /runs</code>).
        </li>
        <li>
          <b>Say how it went.</b> End it with a reward, rate it, or just reply: the next message
          grades the request before it. What came of a PR counts too.
        </li>
        <li>
          <b>Learn.</b> Each round writes or updates a lesson per kind of task: yours from your own
          work, the team&apos;s once several people&apos;s runs back it. It is handed to agents the
          next time that task comes up, and shows here with where it came from.
        </li>
      </ol>
    </div>
  );
}

/**
 * Every lesson the reader may read, filtered by whose (`?show=`: `team`, `mine`, or for an admin a person), a
 * lesson picked by the link (`#/learned/<lessonId>`) scrolled to and marked.
 */
export function Learned({
  sel,
  lesson,
  onLesson,
  onTrace,
  onAdvice,
}: {
  sel: Selection;
  /** The lesson the link names. */
  lesson?: string;
  onLesson: (id: string) => void;
  /** Opens a run's or session's transcript. */
  onTrace: (id: string) => void;
  /** Opens a team lesson on the Advice page. */
  onAdvice: (key: string) => void;
}) {
  const [n, setN] = useState(0);
  const [show, setShow] = useOption("show");
  const [got, setGot] = useState<{ key: string; data?: Lessons; error?: unknown } | null>(null);
  const [more, setMore] = useState<{ key: string; busy: boolean; error?: unknown } | null>(null);
  // A page of the view (`show`) at a time: memstore keeps every lesson the reader reads and answers each page from
  // them, with how many of each whose there are for the tiles and tabs.
  const key = JSON.stringify([sel.org, sel.team, sel.branch, show, n]);
  useEffect(() => {
    let live = true;
    call<Lessons>(sel, "lessons", { page: PAGE, ...(show ? { show } : {}) }).then(
      (data) => live && setGot({ key, data }),
      (error: unknown) => live && setGot({ key, error }),
    );
    return () => {
      live = false;
    };
  }, [key]);
  const cur = got?.key === key ? got : null;
  const v = cur?.data;
  const me = v?.reader.user ?? "";
  const paged = !!v?.groups;
  const next = v?.next ?? null;
  const loadMore = () => {
    if (!v || !next || (more?.key === key && more.busy)) return;
    setMore({ key, busy: true });
    call<Lessons>(sel, "lessons", { page: PAGE, after: next, ...(show ? { show } : {}) }).then(
      (page) => {
        setGot((g) =>
          g?.key === key && g.data
            ? { key, data: { ...g.data, lessons: [...g.data.lessons, ...page.lessons], next: page.next } }
            : g,
        );
        setMore({ key, busy: false });
      },
      (error: unknown) => setMore({ key, busy: false, error }),
    );
  };

  const all = useMemo(() => v?.lessons ?? [], [v]);
  const whose = (owner: string | null, s: string) =>
    !s || (s === "team" ? owner === null : owner !== null && same(owner, s === "mine" ? me : s));
  // An engine that pages answers the view asked for; an older one, every lesson, filtered here.
  const shown = useMemo(
    () => (paged ? all : all.filter((l) => whose(l.owner, show))),
    [all, paged, show, me],
  );
  const groups: Group[] = useMemo(
    () =>
      v?.groups ??
      Object.values(
        all.reduce<Record<string, Group>>((by, l) => {
          const g = (by[l.owner ?? ""] ??= {
            owner: l.owner,
            lessons: 0,
            runs: 0,
            sessions: 0,
            helped: 0,
          });
          g.lessons += 1;
          g.runs += l.learned_from.runs;
          g.sessions += l.served_in.sessions;
          g.helped += l.served_in.helped;
          return by;
        }, {}),
      ),
    [v, all],
  );
  const inView = (s: string) => groups.filter((g) => whose(g.owner, s));

  // The lesson the link names: pages are read until it is on the page, then scrolled to.
  const found = !!lesson && shown.some((l) => lessonId(l) === lesson);
  useEffect(() => {
    if (lesson && v && !found && next) loadMore();
  }, [lesson, v, found, next]);
  useEffect(() => {
    if (lesson && v && found)
      document.getElementById(`lesson-${lesson}`)?.scrollIntoView({ block: "start" });
  }, [lesson, v, found]);

  const tooOld =
    cur?.error instanceof ApiError &&
    (cur.error.status === 404 || /unknown op/i.test(errorText(cur.error)))
      ? "This deployment's engine predates lessons: they show once the engine is updated."
      : null;
  const totals = inView(show).reduce(
    (t, g) => ({
      lessons: t.lessons + g.lessons,
      runs: t.runs + g.runs,
      sessions: t.sessions + g.sessions,
      helped: t.helped + g.helped,
    }),
    { lessons: 0, runs: 0, sessions: 0, helped: 0 },
  );
  const people = (v?.people ?? []).filter((p) => !same(p, me));
  const tabs: { id: string; label: string }[] = [
    { id: "", label: "All" },
    { id: "team", label: "Team" },
    { id: "mine", label: "Yours" },
    ...(v?.reader.everyone ? people.map((p) => ({ id: p, label: p })) : []),
  ];
  const picked = lesson && !found && !next ? lesson : null;
  const anyLessons = groups.some((g) => g.lessons > 0);

  return (
    <div className="studio ap lr">
      <header className="ap-head">
        <h1>Learned</h1>
        <span className="ap-muted">
          What separated the runs that went right from those that didn&apos;t, handed to agents the
          next time the same kind of task comes up
          {v?.reader.everyone ? " · as an admin you see everyone's" : ""}
        </span>
      </header>

      {cur?.error ? (
        <p className="ap-pad ap-error" role="alert">
          {tooOld ?? errorText(cur.error)}{" "}
          {!tooOld && (
            <button className="ap-btn2" onClick={() => setN((x) => x + 1)}>
              Try again
            </button>
          )}
        </p>
      ) : !cur ? (
        <p className="ap-pad ap-muted">Reading what it learned…</p>
      ) : (
        <>
          <dl className="ap-tiles">
            <div>
              <dt>Lessons</dt>
              <dd>{totals.lessons.toLocaleString()}</dd>
            </div>
            <div>
              <dt>Runs learned from</dt>
              <dd>{totals.runs.toLocaleString()}</dd>
            </div>
            <div>
              <dt>Sessions served</dt>
              <dd>{totals.sessions.toLocaleString()}</dd>
            </div>
            <div>
              <dt>Helped</dt>
              <dd>{totals.helped.toLocaleString()}</dd>
            </div>
          </dl>
          <nav className="ap-chips lr-chips" aria-label="Whose lessons">
            {tabs.map((t) => (
              <button
                key={t.id || "all"}
                className={show === t.id ? "on" : ""}
                aria-pressed={show === t.id}
                onClick={() => setShow(t.id || null)}
              >
                {t.label}{" "}
                <span className="ap-num">
                  {inView(t.id).reduce((k, g) => k + g.lessons, 0)}
                </span>
              </button>
            ))}
          </nav>
          {picked && (
            <p className="ap-pad ap-muted ap-small">
              The lesson this link names isn&apos;t in this view.{" "}
              <button className="ap-link" onClick={() => setShow(null)}>
                Show all
              </button>
            </p>
          )}
          <div className="lr-list">
            {shown.length ? (
              shown.map((l) => (
                <LessonCard
                  key={lessonId(l)}
                  l={l}
                  me={me}
                  focused={lessonId(l) === lesson}
                  onLesson={onLesson}
                  onTrace={onTrace}
                  onAdvice={onAdvice}
                />
              ))
            ) : (
              <Empty filtered={!!show && anyLessons} />
            )}
          </div>
          {next && (
            <p className="ap-pad">
              {more?.key === key && !!more.error && (
                <span className="ap-error" role="alert">
                  {errorText(more.error)}{" "}
                </span>
              )}
              <button
                className="ap-btn2"
                disabled={more?.key === key && more.busy}
                onClick={loadMore}
              >
                {more?.key === key && more.busy
                  ? "Reading more…"
                  : `Show more (${(v?.count ?? 0) - shown.length} left)`}
              </button>
            </p>
          )}
        </>
      )}
    </div>
  );
}
