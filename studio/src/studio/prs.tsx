import { when } from "../memory/format";

// The pull requests a session opened or pushed to (memstore's `crate::session_prs`), read from its transcript and
// kept on its row, each with what later came of it. What came of them is the session's outcome advice learns from
// first: a PR reverted, or one a later PR fixed a regression of, is a failure; one merged a success.

/** What came of a PR. */
export interface PrEvent {
  /** `merged`, `closed` (unmerged), `reverted` or `regression`. */
  kind: string;
  /** When (µs). */
  at: number;
  /** The PR that reverted it, or fixed a regression it caused. */
  by?: number;
}

/** A PR a session opened or pushed to. */
export interface SessionPr {
  /** `owner/repo`. */
  repo: string;
  number?: number;
  /** The branch pushed to, for a push that named no PR. */
  branch?: string;
  /** `created`, `pushed` or `said` (the agent said it opened it). */
  how: string;
  events?: PrEvent[];
}

const HOW: Record<string, string> = {
  created: "opened",
  pushed: "pushed to",
  said: "said it opened",
};

function eventText(e: PrEvent): string {
  const by = e.by != null ? ` #${e.by}` : "";
  switch (e.kind) {
    case "merged":
      return `merged ${when(e.at)}`;
    case "closed":
      return `closed unmerged ${when(e.at)}`;
    case "reverted":
      return `reverted${by ? ` by${by}` : ""} ${when(e.at)}`;
    case "regression":
      return `a regression fixed${by ? ` by${by}` : ""} ${when(e.at)}`;
    default:
      return `${e.kind} ${when(e.at)}`;
  }
}

const bad = (e: PrEvent) => e.kind === "reverted" || e.kind === "regression";

/** A session's PRs, for a trace's side. */
export function PrList({ prs }: { prs: SessionPr[] }) {
  if (!prs.length) {
    return <p className="st-quiet st-small">None found in its transcript.</p>;
  }
  return (
    <ul className="st-uses">
      {prs.map((p) => {
        const name = p.number != null ? `${p.repo}#${p.number}` : `${p.repo} (${p.branch ?? "?"})`;
        const url =
          p.number != null
            ? `https://github.com/${p.repo}/pull/${p.number}`
            : p.branch
              ? `https://github.com/${p.repo}/tree/${p.branch}`
              : undefined;
        return (
          <li key={`${p.repo}-${p.number ?? p.branch}`}>
            <div>
              <span className="st-quiet">{HOW[p.how] ?? p.how}</span>{" "}
              {url ? (
                <a className="mono st-small" href={url} target="_blank" rel="noreferrer">
                  {name}
                </a>
              ) : (
                <span className="mono st-small">{name}</span>
              )}
            </div>
            {(p.events ?? []).length > 0 ? (
              (p.events ?? []).map((e, i) => (
                <div
                  key={`${e.kind}-${i}`}
                  className={`st-small ${bad(e) ? "st-out" : e.kind === "merged" ? "st-within" : "st-quiet"}`}
                >
                  {eventText(e)}
                </div>
              ))
            ) : (
              <div className="st-quiet st-small">nothing has come of it yet</div>
            )}
          </li>
        );
      })}
    </ul>
  );
}
