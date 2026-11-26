import { useEffect, useMemo, useState } from "react";
import { call } from "./api";
import { Markdown } from "./markdown";
import type { Focus, Selection } from "./types";
import { transport } from "../transport";
import { Icon } from "./ui";

// Repo skills: a connected repository's own SKILL.md files (memstore's `/skills/review`), how the sessions that loaded
// each one went (what became of their pull requests, the person's next moves) and the votes people gave it, and the
// revision the learning model proposes from that. Nothing is written to the repository from here.

type Tally = { positive: number; negative: number; very_negative: number };
type Graded = {
  uses: number;
  people: number;
  succeeded: number;
  failed: number;
  unknown: number;
  last_used: number | null;
};
type Change = { what?: string; why?: string };
type Proposal = {
  verdict: "revise" | "keep";
  summary: string;
  stale?: boolean;
  proposed_at?: number;
  changes?: Change[];
  skill_md?: string | null;
  evidence?: { reviewed?: number; excerpts?: boolean };
  model?: string;
};
type Summary = {
  key: string;
  name: string;
  description?: string | null;
  origin: "repo" | "compiled";
  title?: string | null;
  repo: string | null;
  path: string | null;
  read_at: number;
  graded: Graded;
  feedback: Tally;
  proposal: Pick<
    Proposal,
    "verdict" | "summary" | "stale" | "proposed_at"
  > | null;
};
type Detail = Omit<Summary, "proposal"> & {
  body: string;
  proposal: Proposal | null;
  may_review: boolean;
};

const ago = (micros?: number | null) => {
  if (!micros) return "never";
  const s = Math.max(0, (Date.now() - micros / 1000) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};

/** Where the list groups compiled skills: they belong to no repository. */
const COMPILED = "Compiled by the memory";

const down = (t: Tally) => t.negative + t.very_negative;

/** The badge a skill's latest proposal shows on the list. */
function badge(p: Summary["proposal"]) {
  if (!p) return null;
  if (p.stale)
    return <span className="rs-badge">Skill changed since review</span>;
  return p.verdict === "revise" ? (
    <span className="rs-badge is-revise">Revision proposed</span>
  ) : (
    <span className="rs-badge is-keep">Stands</span>
  );
}

/** Lines of `a` and `b` as a unified diff: each line kept, removed or added (a longest-common-subsequence walk). */
function diffLines(
  a: string,
  b: string,
): { kind: " " | "-" | "+"; text: string }[] {
  const x = a.split("\n");
  const y = b.split("\n");
  const n = x.length;
  const m = y.length;
  // A skill is a few hundred lines at most: the table fits.
  const lcs: number[][] = Array.from({ length: n + 1 }, () =>
    new Array<number>(m + 1).fill(0),
  );
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--)
      lcs[i]![j] =
        x[i] === y[j]
          ? lcs[i + 1]![j + 1]! + 1
          : Math.max(lcs[i + 1]![j]!, lcs[i]![j + 1]!);
  const out: { kind: " " | "-" | "+"; text: string }[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (x[i] === y[j]) {
      out.push({ kind: " ", text: x[i]! });
      i++;
      j++;
    } else if (lcs[i + 1]![j]! >= lcs[i]![j + 1]!)
      out.push({ kind: "-", text: x[i++]! });
    else out.push({ kind: "+", text: y[j++]! });
  }
  while (i < n) out.push({ kind: "-", text: x[i++]! });
  while (j < m) out.push({ kind: "+", text: y[j++]! });
  return out;
}

/** The diff, with long runs of unchanged lines folded to a few on each side of a change. */
function Diff({ before, after }: { before: string; after: string }) {
  const lines = useMemo(() => diffLines(before, after), [before, after]);
  const CONTEXT = 3;
  const near = lines.map((_, i) =>
    lines
      .slice(Math.max(0, i - CONTEXT), i + CONTEXT + 1)
      .some((l) => l.kind !== " "),
  );
  const rows: React.ReactNode[] = [];
  let folded = 0;
  lines.forEach((l, i) => {
    if (!near[i]) {
      folded++;
      return;
    }
    if (folded) {
      rows.push(
        <div key={`f${i}`} className="rs-fold">
          ⋯ {folded} unchanged {folded === 1 ? "line" : "lines"}
        </div>,
      );
      folded = 0;
    }
    rows.push(
      <div
        key={i}
        className={`rs-line ${l.kind === "+" ? "is-add" : l.kind === "-" ? "is-del" : ""}`}
      >
        <span className="rs-sign">{l.kind}</span>
        {l.text || " "}
      </div>,
    );
  });
  if (folded)
    rows.push(
      <div key="f-end" className="rs-fold">
        ⋯ {folded} unchanged {folded === 1 ? "line" : "lines"}
      </div>,
    );
  return <div className="rs-diff">{rows}</div>;
}

function Stat({
  n,
  label,
  tone,
}: {
  n: number;
  label: string;
  tone?: "ok" | "bad";
}) {
  return (
    <div className={`rs-stat ${tone ? `is-${tone}` : ""}`}>
      <strong>{n}</strong>
      <span className="muted">{label}</span>
    </div>
  );
}

function SkillPage({
  sel,
  skill,
  onBack,
}: {
  sel: Selection;
  skill: string;
  onBack: () => void;
}) {
  const [d, setD] = useState<Detail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reviewing, setReviewing] = useState(false);
  const [tab, setTab] = useState<"diff" | "proposed" | "current">("diff");
  const [copied, setCopied] = useState(false);
  const load = () =>
    call<Detail>(sel, "skills", { op: "get", skill })
      .then((v) => {
        setD(v);
        setError(null);
      })
      .catch((e: Error) => setError(e.message));
  useEffect(() => {
    setD(null);
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sel.org, sel.team, sel.branch, skill]);

  const review = () => {
    setReviewing(true);
    setError(null);
    // The review answers with what it proposed: shown at once (a read right after it may be served the memory a
    // moment before the proposal was written).
    call<{ proposal: Proposal }>(sel, "skills", { op: "review", skill })
      .then((r) =>
        setD((was) =>
          was ? { ...was, proposal: { ...r.proposal, stale: false } } : was,
        ),
      )
      .catch((e: Error) => setError(e.message))
      .finally(() => setReviewing(false));
  };

  if (!d)
    return (
      <div className="rs-page">
        <button type="button" className="btn rs-back" onClick={onBack}>
          ← Back
        </button>
        {error ? (
          <p className="rs-error">{error}</p>
        ) : (
          <p className="muted">Reading…</p>
        )}
      </div>
    );
  const g = d.graded;
  const p = d.proposal;
  const revised = p?.verdict === "revise" && p.skill_md ? p.skill_md : null;
  return (
    <div className="rs-page">
      <button type="button" className="btn rs-back" onClick={onBack}>
        ← {d.origin === "repo" ? "Repo skills" : "The skill"}
      </button>
      <div className="rs-head">
        <div>
          <h2>{d.title ?? d.name}</h2>
          <div className="muted rs-where">
            {d.origin === "repo" ? (
              <>
                <Icon name="code" size={12} /> {d.repo} · {d.path}
              </>
            ) : (
              <>
                <Icon name="skill" size={12} /> Compiled by the memory · skill:
                {d.name}
              </>
            )}
          </div>
          {d.description && <p className="rs-desc">{d.description}</p>}
        </div>
        {d.may_review && (
          <button
            type="button"
            className="btn btn-primary"
            disabled={reviewing}
            onClick={review}
          >
            <Icon name="refresh" size={13} />{" "}
            {reviewing ? "Reviewing…" : p ? "Review again" : "Review now"}
          </button>
        )}
      </div>
      {reviewing && (
        <p className="muted rs-note">
          The learning model is reading the skill against how its sessions went.
          This takes about a minute.
        </p>
      )}
      {error && <p className="rs-error">{error}</p>}

      <div className="rs-stats">
        <Stat n={g.uses} label={g.uses === 1 ? "use" : "uses"} />
        <Stat n={g.people} label={g.people === 1 ? "person" : "people"} />
        <Stat n={g.succeeded} label="succeeded" tone="ok" />
        <Stat n={g.failed} label="failed" tone="bad" />
        <Stat n={g.unknown} label="not graded yet" />
        <Stat n={d.feedback.positive} label="👍 votes" tone="ok" />
        <Stat n={down(d.feedback)} label="👎 votes" tone="bad" />
      </div>
      <p className="muted rs-note">
        Last used {ago(g.last_used)}. A session succeeded or failed by what
        became of its pull requests, else by what the person did next. Agents
        vote with <code>feedback_memory</code> on <code>skill:{d.name}</code>.
      </p>

      {p ? (
        <section className="card rs-proposal">
          <div className="rs-proposal-head">
            {p.verdict === "revise" ? (
              <span className="rs-badge is-revise">Revision proposed</span>
            ) : (
              <span className="rs-badge is-keep">Stands as it is</span>
            )}
            {p.stale && (
              <span className="rs-badge">
                The skill changed since this review
              </span>
            )}
            <span className="muted">
              {ago(p.proposed_at)} · read {p.evidence?.reviewed ?? 0} sessions
              {p.evidence?.excerpts ? " and what happened in them" : ""}
            </span>
          </div>
          <p>{p.summary}</p>
          {p.changes && p.changes.length > 0 && (
            <ol className="rs-changes">
              {p.changes.map((c, i) => (
                <li key={i}>
                  <strong>{c.what}</strong>
                  {c.why && <div className="muted">{c.why}</div>}
                </li>
              ))}
            </ol>
          )}
          {revised && (
            <>
              <div className="rs-tabs">
                {(["diff", "proposed", "current"] as const).map((t) => (
                  <button
                    key={t}
                    type="button"
                    className={`rs-tab ${tab === t ? "is-on" : ""}`}
                    onClick={() => setTab(t)}
                  >
                    {t === "diff"
                      ? "Changes"
                      : t === "proposed"
                        ? "Proposed SKILL.md"
                        : "Current SKILL.md"}
                  </button>
                ))}
                <span className="rs-grow" />
                <button
                  type="button"
                  className="btn"
                  onClick={() =>
                    navigator.clipboard.writeText(revised).then(() => {
                      setCopied(true);
                      setTimeout(() => setCopied(false), 1500);
                    })
                  }
                >
                  <Icon name={copied ? "check" : "copy"} size={13} />{" "}
                  {copied ? "Copied" : "Copy SKILL.md"}
                </button>
              </div>
              {tab === "diff" ? (
                <Diff before={d.body} after={revised} />
              ) : (
                <div className="rs-md">
                  <Markdown text={tab === "proposed" ? revised : d.body} />
                </div>
              )}
              <p className="muted rs-note">
                {d.origin === "repo" ? (
                  <>
                    Nothing is written to {d.repo}: copy it into{" "}
                    <code>{d.path}</code> and open a pull request, or leave it.
                  </>
                ) : (
                  <>
                    The compiled skill stays as its compile wrote it: what it
                    proposes points at what its sources should say.
                  </>
                )}
              </p>
            </>
          )}
        </section>
      ) : (
        <section className="card rs-proposal">
          <p className="muted">
            No review yet.{" "}
            {d.may_review
              ? "Review it to have the learning model propose a revision from how its sessions went."
              : "The memory's admins can review it."}
          </p>
          <div className="rs-md">
            <Markdown text={d.body} />
          </div>
        </section>
      )}
    </div>
  );
}

/** The page: every connected skill, or one of them. */
export function RepoSkills({
  sel,
  skill,
  onOpen,
  proposed = false,
}: {
  sel: Selection;
  skill?: string;
  onOpen: (f: Focus) => void;
  /** Revisions proposed: every skill, a repository's or a compiled one, whose newest review proposes a change. */
  proposed?: boolean;
}) {
  const [list, setList] = useState<Summary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (skill) return;
    call<{ skills: Summary[] }>(
      sel,
      "skills",
      proposed ? { op: "list" } : { op: "list", origin: "repo" },
    )
      .then((r) => {
        setList(
          proposed
            ? r.skills.filter((s) => s.proposal?.verdict === "revise")
            : r.skills,
        );
        setError(null);
      })
      .catch((e: Error) => setError(e.message));
  }, [sel.org, sel.team, sel.branch, skill, proposed]);

  if (skill)
    return (
      <SkillPage
        sel={sel}
        skill={skill}
        onBack={() =>
          onOpen(
            proposed
              ? { kind: "skill-proposals" }
              : skill.startsWith("skills/")
                ? { kind: "skill", path: skill.split("/") }
                : { kind: "repo-skills" },
          )
        }
      />
    );

  const repos = new Map<string, Summary[]>();
  for (const s of list ?? [])
    repos.set(s.repo ?? COMPILED, [
      ...(repos.get(s.repo ?? COMPILED) ?? []),
      s,
    ]);
  return (
    <div className="rs-page">
      <h2>{proposed ? "Revisions proposed" : "Repo skills"}</h2>
      <p className="muted">
        {proposed
          ? "Skills whose latest review proposes a change, from how the sessions that used them went. Open one to see the change and the evidence for it."
          : "The SKILL.md files in your connected repositories, graded by how the sessions that loaded them went. Review one to get a proposed revision."}
      </p>
      {error && <p className="rs-error">{error}</p>}
      {proposed && list && list.length === 0 && (
        <div className="card rs-empty">
          <Icon name="check" size={18} />
          <strong>No revisions proposed</strong>
          <span className="muted">
            Skills are reviewed as sessions use them; a revision shows here when
            the evidence calls for one.
          </span>
        </div>
      )}
      {!proposed && list && list.length === 0 && <NoRepos sel={sel} />}
      {!list && !error && <p className="muted">Reading…</p>}
      {[...repos].map(([repo, skills]) => (
        <section key={repo} className="rs-repo">
          <div className="rs-repo-name">
            <Icon name={repo === COMPILED ? "skill" : "code"} size={13} />{" "}
            {repo}
          </div>
          <ul className="rs-list">
            {skills.map((s) => (
              <li key={s.key}>
                <button
                  type="button"
                  className="card rs-row"
                  onClick={() =>
                    onOpen({
                      kind: proposed ? "skill-proposals" : "repo-skills",
                      skill: s.key,
                    })
                  }
                >
                  <div className="rs-row-main">
                    <div className="rs-row-name">
                      {s.title ?? s.name} {badge(s.proposal)}
                    </div>
                    <div className="muted rs-row-path">
                      {s.path ?? `skill:${s.name}`}
                    </div>
                  </div>
                  <div className="rs-row-nums">
                    <span title="Sessions that loaded it">
                      {s.graded.uses} uses
                    </span>
                    <span className="is-ok" title="Succeeded">
                      ✓ {s.graded.succeeded}
                    </span>
                    <span className="is-bad" title="Failed">
                      ✗ {s.graded.failed}
                    </span>
                    <span title="Votes">
                      👍 {s.feedback.positive} · 👎 {down(s.feedback)}
                    </span>
                    <span className="muted">
                      used {ago(s.graded.last_used)}
                    </span>
                  </div>
                </button>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

/**
 * On a compiled skill's page: how the sessions it was served in went, and its newest proposal, linked to the
 * whole review. Says nothing while it reads, or where the memory doesn't answer it.
 */
export function SkillReviewCard({
  sel,
  skillKey,
  onOpen,
}: {
  sel: Selection;
  skillKey: string;
  onOpen: (f: Focus) => void;
}) {
  const [d, setD] = useState<Detail | null>(null);
  useEffect(() => {
    call<Detail>(sel, "skills", { op: "get", skill: skillKey }).then(setD, () =>
      setD(null),
    );
  }, [sel, skillKey]);
  if (!d) return null;
  const g = d.graded;
  const p = d.proposal;
  return (
    <div className="card rs-card">
      <div className="rs-card-nums">
        <span>
          <strong>{g.uses}</strong> {g.uses === 1 ? "session" : "sessions"}{" "}
          served it
        </span>
        <span className="is-ok">✓ {g.succeeded}</span>
        <span className="is-bad">✗ {g.failed}</span>
        <span>
          👍 {d.feedback.positive} · 👎 {down(d.feedback)}
        </span>
        {p &&
          (p.stale ? (
            <span className="rs-badge">Changed since review</span>
          ) : (
            badge(p)
          ))}
      </div>
      <button
        type="button"
        className="btn"
        onClick={() => onOpen({ kind: "repo-skills", skill: skillKey })}
      >
        {p ? "See the review" : d.may_review ? "Review it" : "See how it did"} →
      </button>
    </div>
  );
}

/** How many skills have a revision proposed, for Skills' tab; none until it is read, or where it can't be. */
export function useProposedCount(sel: Selection, on: boolean): number {
  const [n, setN] = useState(0);
  useEffect(() => {
    if (!on) return;
    call<{ skills: Summary[] }>(sel, "skills", { op: "list" }).then(
      (r) =>
        setN(r.skills.filter((s) => s.proposal?.verdict === "revise").length),
      () => setN(0),
    );
  }, [sel.org, sel.team, sel.branch, on]);
  return n;
}

/**
 * From repos with nothing in it: connect GitHub where the host has a connector for it (a console), else how a
 * local server reads a repository's skills.
 */
function NoRepos({ sel }: { sel: Selection }) {
  const href = transport().connectRepos?.(sel);
  return (
    <div className="card rs-empty">
      <Icon name="code" size={18} />
      <strong>No repository skills yet</strong>
      {href ? (
        <>
          <span className="muted">
            Connect GitHub and the SKILL.md files in your repos show here,
            graded by how the sessions that loaded them went.
          </span>
          <a className="btn btn-primary" href={href}>
            <Icon name="code" size={13} /> Connect GitHub
          </a>
        </>
      ) : (
        <span className="muted">
          Read a repository's SKILL.md files with{" "}
          <code>ampup-memory sources add skills OWNER/REPO</code>.
        </span>
      )}
    </div>
  );
}
