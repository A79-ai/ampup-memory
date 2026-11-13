"use client";

import { useEffect, useMemo, useState } from "react";
import { browse, when } from "./api";
import type { Focus, LearnedFrom, Lesson, Lessons as LessonsView, Outcome, SaidBy, Selection, ServedIn } from "./types";

// The Learned tab: each lesson AmpUp Memory learned from how runs went (memstore's `/lessons`), its text first,
// then where it came from and where it has been handed since, with how each went. A person reads their own lessons
// and the team's; an org's admin, everyone's.

/** Trail entries shown per side before "show all". */
const SHOWN = 4;

/** A time as "today", "yesterday", "3 days ago", else a date. */
function ago(us: number | null | undefined): string {
  if (us == null) return "";
  const days = Math.floor((Date.now() - us / 1000) / 86_400_000);
  if (days <= 0) return "today";
  if (days === 1) return "yesterday";
  if (days < 7) return `${days} days ago`;
  return new Date(us / 1000).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

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

function Mark({ outcome, title }: { outcome: Outcome; title?: string }) {
  const g = good(outcome);
  const cls = g === true ? "mark mark-ok" : g === false ? "mark mark-bad" : outcome === "open" ? "mark mark-open" : "mark";
  return (
    <span className={cls} title={title ?? outcome} aria-label={outcome}>
      {g === true ? "✓" : g === false ? "✕" : ""}
    </span>
  );
}

/** A row of dots, one per run or session, as they went. */
function Dots({ ok, bad, other }: { ok: number; bad: number; other: number }) {
  const cap = 24;
  const all = [...Array(ok).fill("ok"), ...Array(bad).fill("bad"), ...Array(other).fill("other")];
  return (
    <span className="dots" aria-hidden>
      {all.slice(0, cap).map((k, i) => (
        <span key={i} className={`dot dot-${k}`} />
      ))}
      {all.length > cap && <span className="muted dots-more">+{all.length - cap}</span>}
    </span>
  );
}

/** Inline `code` and **bold**, as elements. */
function inline(s: string): React.ReactNode[] {
  return s.split(/(\*\*[^*]+\*\*|`[^`]+`)/).map((part, i) => {
    if (part.startsWith("**") && part.endsWith("**") && part.length > 4) return <b key={i}>{part.slice(2, -2)}</b>;
    if (part.startsWith("`") && part.endsWith("`") && part.length > 2) return <code key={i}>{part.slice(1, -1)}</code>;
    return part;
  });
}

type Item = { kind: string | null; text: string };
type Section = { name: string; items: Item[]; prose: string[] };

/** A note's sections and their items, each item's `[kind]` apart and its `(runs: …)` citation dropped. */
function sections(note: string): Section[] {
  const out: Section[] = [];
  let cur: Section | null = null;
  for (const raw of note.split("\n")) {
    const h = /^##\s+(.*)$/.exec(raw);
    if (h) {
      cur = { name: h[1].trim(), items: [], prose: [] };
      out.push(cur);
      continue;
    }
    if (!cur || !raw.trim() || /^#\s/.test(raw)) continue;
    const li = /^(?:\d+[.)]|[-*])\s+(.*)$/.exec(raw);
    const text = (li ? li[1] : raw.trim()).replace(/\s*\(runs:[^)]*\)\s*\.?$/i, "").trim();
    if (li) {
      const k = /^\[([a-z-]+)(?:\s[^\]]*)?\]\s*(.*)$/i.exec(text);
      cur.items.push(k ? { kind: k[1].toLowerCase(), text: k[2] } : { kind: null, text });
    } else if (/^\s/.test(raw) && cur.items.length) {
      cur.items[cur.items.length - 1].text += ` ${text}`;
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
  Triggers: "Watch for",
  "Withhold when": "Not for",
};

function NoteBody({ note }: { note: string }) {
  const all = sections(note);
  const main = all.filter((s) => ["Steps", "Pitfalls", "Verification"].includes(s.name));
  const rest = all.filter((s) => !["When", "Steps", "Pitfalls", "Verification"].includes(s.name));
  const list = (s: Section) => {
    const Tag = s.name === "Steps" ? "ol" : "ul";
    return (
      <Tag className={`lesson-items lesson-${s.name.toLowerCase().replace(/\s+/g, "-")}`}>
        {s.items.map((it, i) => (
          <li key={i}>
            {it.kind && <span className={`kind kind-${it.kind}`}>{it.kind}</span>}
            <span>{inline(it.text)}</span>
          </li>
        ))}
      </Tag>
    );
  };
  return (
    <div className="lesson-body">
      {main.map((s) => (
        <section key={s.name} className="lesson-sec">
          <h4>{HEADING[s.name] ?? s.name}</h4>
          {s.prose.map((p, i) => (
            <p key={i}>{inline(p)}</p>
          ))}
          {list(s)}
        </section>
      ))}
      {rest.length > 0 && (
        <details className="lesson-more">
          <summary className="muted">{rest.map((s) => HEADING[s.name] ?? s.name).join(" · ")}</summary>
          {rest.map((s) => (
            <section key={s.name} className="lesson-sec">
              <h4>{HEADING[s.name] ?? s.name}</h4>
              {s.prose.map((p, i) => (
                <p key={i}>{inline(p)}</p>
              ))}
              {list(s)}
            </section>
          ))}
        </details>
      )}
    </div>
  );
}

/** The person a trail entry names, as the reader knows them. */
function Who({ user, me }: { user: string; me: string }) {
  return <span className="who-name">{user.toLowerCase() === me.toLowerCase() ? "you" : user}</span>;
}

function FromRow({ e, me }: { e: LearnedFrom; me: string }) {
  const counts = e.runs > 1 ? ` · ${e.right} right, ${e.wrong} wrong of ${e.runs} requests` : "";
  return (
    <li className="trail-row">
      <Mark outcome={e.outcome} title={`${e.outcome}${SAID[e.by] ? `, ${SAID[e.by]}` : ""}`} />
      <div className="trail-text">
        <div className="trail-q">{e.question ? inline(e.question) : <span className="muted">(no task text)</span>}</div>
        <div className="trail-meta muted">
          {e.kind === "session" ? "session" : "run"} · <Who user={e.user} me={me} /> · {ago(e.at)}
          {SAID[e.by] ? ` · ${SAID[e.by]}` : ""}
          {counts}
        </div>
      </div>
    </li>
  );
}

function ServedRow({ e, me }: { e: ServedIn; me: string }) {
  return (
    <li className="trail-row">
      <Mark outcome={e.outcome} title={`${e.outcome}${SAID[e.by] ? `, ${SAID[e.by]}` : ""}`} />
      <div className="trail-text">
        <div className="trail-q">
          {e.question ? inline(e.question) : <span className="muted">(no request text)</span>}
          {e.helped === true && <span className="pill pill-ok">helped</span>}
          {e.helped === false && e.applied === true && <span className="pill">applied</span>}
        </div>
        <div className="trail-meta muted">
          <Who user={e.user} me={me} /> · {ago(e.at)}
          {e.moment ? ` · handed ${MOMENT[e.moment] ?? `at ${e.moment}`}` : ""}
          {e.outcome === "open" ? " · still open" : SAID[e.by] ? ` · ${SAID[e.by]}` : ""}
          {e.prs.map((p) => (
            <span key={`${p.repo}#${p.number}`} className="pr mono">
              {" "}
              {p.repo}
              {p.number != null ? `#${p.number}` : ""} {p.state}
            </span>
          ))}
        </div>
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
  empty: React.ReactNode;
  render: (r: T, i: number) => React.ReactNode;
}) {
  const [all, setAll] = useState(false);
  const shown = all ? rows : rows.slice(0, SHOWN);
  const total = dots.ok + dots.bad + dots.other;
  return (
    <section className="trail">
      <header className="trail-head">
        <h4>{title}</h4>
        <span className="muted">{summary}</span>
      </header>
      {total > 0 && <Dots {...dots} />}
      {rows.length ? <ul className="trail-list">{shown.map(render)}</ul> : !hidden && <p className="trail-empty muted">{empty}</p>}
      {(rows.length > SHOWN || more > 0 || hidden) && (
        <p className="trail-foot muted">
          {rows.length > SHOWN && (
            <button className="link" onClick={() => setAll(!all)}>
              {all ? "show fewer" : `show all ${rows.length}`}
            </button>
          )}
          {more > 0 && <span>{rows.length > SHOWN ? ", " : ""}{more} older not listed</span>}
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

function LessonCard({ l, me, focused }: { l: Lesson; me: string; focused: boolean }) {
  const f = l.learned_from;
  const s = l.served_in;
  const mine = l.space === "mine";
  const whose = mine ? (l.owner && l.owner.toLowerCase() !== me.toLowerCase() ? l.owner : "Yours") : "Team";
  const applies = sections(l.lesson).find((x) => x.name === "When");
  const whenText = applies ? [...applies.prose, ...applies.items.map((i) => i.text)].join(" ") : l.when;
  // Other people's runs and sessions are counted above and never listed, unless the reader may read everyone's.
  const others = (n: number, what: string) => (n ? `+ ${plural(n, what)} by other people (counted, not listed)` : null);
  return (
    <article className={`card lesson${focused ? " lesson-focused" : ""}`} id={`lesson-${l.key}`}>
      <div className="lesson-top">
        <span className={`badge ${mine ? "badge-mine" : "badge-team"}`}>{whose}</span>
        <span className="lesson-cat">{l.category}</span>
        {l.status !== "served" && (
          <span className="badge badge-warn" title={l.status_reason}>
            {l.status}
          </span>
        )}
        {l.evidence?.ratified && <span className="badge badge-ok">proven</span>}
        <span className="lesson-date muted" title={when(l.learned_at)}>
          learned {ago(l.learned_at)}
        </span>
      </div>
      <h2 className="lesson-title">{l.title}</h2>
      {whenText && <p className="lesson-when">{inline(whenText)}</p>}
      <NoteBody note={l.lesson} />
      <div className="flow">
        <Trail
          title="Learned from"
          summary={
            <>
              {plural(f.runs, "run")}
              {f.people > 1 ? ` · ${f.people} people` : ""}
              {f.runs ? ` · ${f.right} right, ${f.wrong} wrong` : ""}
            </>
          }
          dots={{ ok: f.right, bad: f.wrong, other: Math.max(0, f.runs - f.right - f.wrong) }}
          rows={f.trail}
          hidden={others(f.hidden, "run")}
          more={f.more}
          empty="No runs left in the log."
          render={(e) => <FromRow key={e.id} e={e} me={me} />}
        />
        <div className="flow-arrow" aria-hidden>
          <svg viewBox="0 0 24 24" width="22" height="22">
            <path d="M4 12h14M13 6l6 6-6 6" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
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
              ? "Not handed yet: it will be the next time you start this kind of task."
              : "Not handed yet: agents get it the next time someone starts this kind of task."
          }
          render={(e) => <ServedRow key={e.id} e={e} me={me} />}
        />
      </div>
    </article>
  );
}

/** No lessons yet: how they come to be. */
function Empty({ filtered }: { filtered: boolean }) {
  if (filtered) return <div className="card lessons-empty muted">No lessons here. Pick another view above.</div>;
  return (
    <div className="card lessons-empty">
      <h2 className="lesson-title">Nothing learned yet</h2>
      <p className="muted">
        Lessons come from how your agents&apos; work turned out. AmpUp Memory sorts each finished run or session by the
        kind of task it was, and once a kind has enough that went right and wrong, it writes down what separates them.
      </p>
      <ol className="how">
        <li>
          <b>Work.</b> Agents open a session (<code>start_session</code>) or record a run (<code>POST /runs</code>).
        </li>
        <li>
          <b>Say how it went.</b> End it with a reward, rate it, or just reply: your next message grades the request
          before it. What came of a PR counts too.
        </li>
        <li>
          <b>Learn.</b> Each round writes or updates a lesson per kind of task: yours from your own work, the team&apos;s
          once several people&apos;s runs back it. It is handed to agents the next time that task comes up, and shows here
          with where it came from.
        </li>
      </ol>
    </div>
  );
}

/** Every lesson the reader may read, with a picker for whose: all, the team's, their own, or (an admin) a person's. */
export function Lessons({
  sel,
  show,
  focusKey,
  onFocus,
}: {
  sel: Selection;
  show?: string;
  focusKey?: string;
  onFocus: (f: Focus) => void;
}) {
  const [view, setView] = useState<{ v?: LessonsView; error?: string } | null>(null);
  useEffect(() => {
    let live = true;
    browse<LessonsView>({ op: "lessons" }, sel).then(
      (v) => live && setView({ v }),
      (e) => live && setView({ error: e.message }),
    );
    return () => {
      live = false;
    };
  }, [sel]);
  // Scrolled to the lesson a link names, once it is on the page.
  useEffect(() => {
    if (focusKey && view?.v) document.getElementById(`lesson-${focusKey}`)?.scrollIntoView({ block: "start" });
  }, [focusKey, view]);

  const v = view?.v;
  const me = v?.reader.user ?? "";
  const shown = useMemo(() => {
    const all = v?.lessons ?? [];
    if (!show) return all;
    if (show === "team") return all.filter((l) => l.space === "team");
    if (show === "mine") return all.filter((l) => l.space === "mine" && (l.owner ?? "").toLowerCase() === me.toLowerCase());
    return all.filter((l) => l.space === "mine" && (l.owner ?? "").toLowerCase() === show.toLowerCase());
  }, [v, show, me]);

  if (!view) return <main className="schema-main muted pad">Loading what it learned…</main>;
  if (view.error) {
    const old = /unknown op/i.test(view.error);
    return (
      <main className="schema-main pad error">
        {old ? "This AmpUp Memory server predates lessons: redeploy it from main." : view.error}
      </main>
    );
  }
  const totals = shown.reduce(
    (t, l) => ({
      runs: t.runs + l.learned_from.runs,
      sessions: t.sessions + l.served_in.sessions,
      helped: t.helped + l.served_in.helped,
    }),
    { runs: 0, sessions: 0, helped: 0 },
  );
  const others = (v?.people ?? []).filter((p) => p.toLowerCase() !== me.toLowerCase());
  const tabs: { id?: string; label: string }[] = [
    { label: "All" },
    { id: "team", label: "Team" },
    { id: "mine", label: "Yours" },
    ...(v?.reader.everyone ? others.map((p) => ({ id: p, label: p })) : []),
  ];
  return (
    <main className="schema-main">
      <div className="lessons-wrap">
        <header className="schema-head">
          <div>
            <div className="schema-kicker">What it learned</div>
            <h1 className="schema-name">Lessons</h1>
            <p className="schema-desc">
              What separated the runs that went right from those that didn&apos;t, written down and handed to agents the
              next time the same kind of task comes up.
              {v?.reader.everyone ? " As an admin you see everyone's." : ""}
            </p>
          </div>
          <div className="schema-stats">
            <div>
              <b>{shown.length}</b>
              <span>{shown.length === 1 ? "lesson" : "lessons"}</span>
            </div>
            <div>
              <b>{totals.runs}</b>
              <span>runs learned from</span>
            </div>
            <div>
              <b>{totals.sessions}</b>
              <span>sessions served</span>
            </div>
            <div>
              <b>{totals.helped}</b>
              <span>helped</span>
            </div>
          </div>
        </header>
        <nav className="seg" aria-label="Whose lessons">
          {tabs.map((t) => (
            <button
              key={t.id ?? "all"}
              className={(show ?? undefined) === t.id ? "seg-on" : ""}
              onClick={() => onFocus({ kind: "lessons", show: t.id })}
            >
              {t.label}
            </button>
          ))}
        </nav>
        {shown.length ? (
          shown.map((l) => <LessonCard key={l.key + (l.owner ?? "")} l={l} me={me} focused={l.key === focusKey} />)
        ) : (
          <Empty filtered={!!show && (v?.lessons.length ?? 0) > 0} />
        )}
      </div>
    </main>
  );
}
