// Review: one inbox for everything that changes the memory without a person typing it. "Needs you" (changes
// waiting for your approval, the dream's decisions it wasn't sure of, facts that disagree), "Done automatically"
// (what the dream and the team share did on their own, each with why and Undo), "History" (what was taken back
// or decided). Then the owners' review: one proposal (each row as before → after, the evidence you may read, who else must approve,
// your decision), the owned entities (an admin assigns, the owner hands on), the facts that disagree
// (the owner or an admin keeps one or writes the right value), and the owner panel an entity's page
// shows. Modelled on an approvals inbox: requests, not rows; completed ones stay as the record.
import { useCallback, useEffect, useMemo, useState } from "react";
import { ApiError } from "./api";
import { useChoice, useFlag } from "./hashOptions";
import { ago, entityName, humanize, when } from "./format";
import {
  entityFocus,
  needsMe,
  review,
  STATUS_BADGE,
  STATUS_WORD,
  titleOf,
  type Change,
  type Conflict,
  type ConflictCheck,
  type ConflictPage,
  type ConflictSide,
  type DreamDecision,
  type DreamKind,
  type DreamList,
  type DreamPass,
  type DreamStatus,
  type EvidenceRef,
  type Owner,
  type ProposalStatus,
  type ProposalSummary,
  type Resolution,
  type Review as ReviewView,
} from "./review-api";
import { closedValues, ValueField } from "./schema";
import type { Focus, OrgType, Selection } from "./types";
import { CopyId, EmptyState, ErrorState, Icon, Skeleton } from "./ui";
import "./review.css";

const errorOf = (e: unknown) =>
  e instanceof ApiError && e.status === 404
    ? "It isn't here, or you can't read it."
    : e instanceof Error
      ? e.message
      : String(e);

/** A read, done again on `reload`; `data` stays while a fresh read replaces it. */
function useRead<T>(load: () => Promise<T>, deps: unknown[]) {
  const [s, setS] = useState<{ data?: T; error?: string; loading: boolean }>({ loading: true });
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    setS((x) => ({ ...x, loading: true }));
    load().then(
      (data) => live && setS({ data, loading: false }),
      (e: unknown) => live && setS((x) => ({ ...x, error: errorOf(e), loading: false })),
    );
    return () => {
      live = false;
    };
  }, [...deps, n]);
  return { ...s, reload: useCallback(() => setN((x) => x + 1), []) };
}

/* ---------- small pieces ---------- */

function StatusBadge({ status }: { status: ProposalStatus }) {
  return (
    <span className={`badge ${STATUS_BADGE[status] ?? ""}`}>{STATUS_WORD[status] ?? status}</span>
  );
}

/** A person: "you", or their address. */
function Who({ user, me }: { user: string; me?: string }) {
  return user === me ? <b>you</b> : <span>{user}</span>;
}

/** Who wrote the draft: a person, or an actor such as an extractor (mono: it is an id). */
function Author({ by, me }: { by: string; me?: string }) {
  if (/^[^@\s:/]+@[^@\s/]+$/.test(by)) return <Who user={by} me={me} />;
  const model = /^extract:([^@/]+)/.exec(by)?.[1];
  return (
    <span className="mono" title={by}>
      {model ? `extract:${model}…` : by}
    </span>
  );
}

const STALE_TEXT: Record<string, string> = {
  draft_changed: "The draft changed after the review opened.",
  base_changed: "The memory under this draft changed on main.",
  ownership_changed: "The owners of what it changes were changed.",
};

/* ---------- the section ---------- */

export function ReviewSection({
  sel,
  me,
  admin,
  proposal,
  schema,
  onFocus,
}: {
  sel: Selection;
  /** The signed-in person as memstore names them; undefined while unknown. */
  me?: string;
  admin: boolean;
  /** The proposal open, or none for the inbox. */
  proposal?: string;
  /** The org's schema: the right value of an attribute it closes is picked from its values. */
  schema?: Map<string, OrgType>;
  onFocus: (f: Focus) => void;
}) {
  // Keyed by proposal (and org/team): a decision's result, a half-written note and the last read belong to the
  // proposal they were made on. Without the key, following a link from one proposal to another kept showing the
  // first one, decided, under the second one's address.
  if (proposal)
    return (
      <ProposalPane
        key={JSON.stringify([sel.org, sel.team, proposal])}
        sel={sel}
        id={proposal}
        me={me}
        admin={admin}
        onFocus={onFocus}
      />
    );
  return <Inbox sel={sel} me={me} admin={admin} schema={schema} onFocus={onFocus} />;
}

type Filter = "mine" | "others" | "published" | "rejected" | "stale" | "all";
const FILTERS: [Filter, string][] = [
  ["mine", "Needs you"],
  ["others", "Waiting on others"],
  ["published", "Published"],
  ["rejected", "Rejected"],
  ["stale", "Stale"],
  ["all", "All"],
];

function Inbox({
  sel,
  me,
  admin,
  schema,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  admin: boolean;
  schema?: Map<string, OrgType>;
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team]);
  const ps = useRead(() => review.proposals(sel), [key]);
  // One page: its total counts the tab, and Needs you lists those on it the reader may settle.
  const cs = useRead(() => review.conflicts(sel, { limit: 50 }), [key]);
  // The dream's decisions are its builders' (an org admin's): counts and the last pass, for the tabs and banner.
  const ds = useRead(
    () => (admin ? review.dream(sel, { status: "waiting", limit: 200 }) : Promise.resolve(null)),
    [key, admin],
  );
  const [view, setView] = useChoice(
    "tab",
    ["needs", "done", "history", "proposals", "conflicts"] as const,
    "needs",
  );
  const [filter, setFilter] = useChoice(
    "filter",
    FILTERS.map(([k]) => k),
    "mine",
  );
  const buckets = useMemo(() => {
    const all = [...(ps.data ?? [])].sort((a, b) => (b.updated_at ?? 0) - (a.updated_at ?? 0));
    const mine = needsMe(all, me);
    const mineIds = new Set(mine.map((p) => p.id));
    return {
      mine,
      others: all.filter(
        (p) => (p.status === "pending_review" || p.status === "approved") && !mineIds.has(p.id),
      ),
      published: all.filter((p) => p.status === "published"),
      rejected: all.filter((p) => p.status === "rejected"),
      stale: all.filter((p) => p.status === "stale"),
      all,
    };
  }, [ps.data, me]);
  // Land on what needs the reader; when nothing does, on everything.
  useEffect(() => {
    if (ps.data && me && buckets.mine.length === 0 && filter === "mine") setFilter("all");
  }, [ps.data, me, buckets.mine.length, filter]);

  const waitingDream = ds.data?.decisions ?? [];
  const resolvable = (cs.data?.conflicts ?? []).filter((c) => c.can_resolve);
  const needsN = buckets.mine.length + waitingDream.length + resolvable.length;
  const tab = (id: typeof view, label: string, n?: number, nTitle?: string) => (
    <button
      role="tab"
      aria-selected={view === id}
      className={view === id ? "rv-view-on" : ""}
      onClick={() => setView(id)}
    >
      {label}
      {n ? (
        <span className="rv-n" title={nTitle}>
          {n.toLocaleString()}
        </span>
      ) : null}
    </button>
  );
  const reloadAll = () => {
    ps.reload();
    cs.reload();
    ds.reload();
  };
  // Refresh also reads the dream's lists afresh (a card acted on otherwise stays put, showing its new state).
  const [fresh, setFresh] = useState(0);
  const views = (
    <div className="rv-views" role="tablist" aria-label="Review">
      {tab("needs", "Needs you", needsN)}
      {tab(
        "done",
        "Done automatically",
        ds.data?.counts.applied,
        ds.data ? `Applied on its own: ${appliedSaid(dreamState(ds.data))}` : undefined,
      )}
      {tab("history", "History")}
      <span className="rv-views-gap" aria-hidden />
      <button
        role="tab"
        aria-selected={view === "proposals"}
        className={view === "proposals" ? "rv-view-on" : ""}
        onClick={() => setView("proposals")}
      >
        Proposals
        {buckets.mine.length > 0 && <span className="rv-n">{buckets.mine.length}</span>}
      </button>
      <button
        role="tab"
        aria-selected={view === "conflicts"}
        className={view === "conflicts" ? "rv-view-on" : ""}
        onClick={() => setView("conflicts")}
      >
        Conflicts
        {cs.data && cs.data.total > 0 && <span className="rv-n">{cs.data.total}</span>}
      </button>
    </div>
  );
  const banner = admin && (
    <DreamBanner sel={sel} list={ds.data ?? undefined} onChanged={reloadAll} />
  );
  if (view === "needs" || view === "done" || view === "history")
    return (
      <section className="rv">
        <Head
          title={
            view === "needs"
              ? "What needs you"
              : view === "done"
                ? "What the memory did on its own"
                : "What was decided"
          }
          tools={
            <button className="btn" onClick={() => {
                reloadAll();
                setFresh((x) => x + 1);
              }} disabled={ps.loading || cs.loading}>
              <Icon name="refresh" size={14} /> Refresh
            </button>
          }
        />
        <p className="lede">
          {view === "needs"
            ? "Changes waiting for your approval, decisions the dream wasn't sure of, and facts that disagree. Everything else the memory did on its own is under Done automatically, each with Undo."
            : view === "done"
              ? "The dream tidies the memory as facts arrive: it ends facts a newer one replaced, links facts that say the same thing and learns the other names things go by. When several people say the same thing, it's shared with the team. Each change says why; Undo takes back that one, and the dream won't make it again."
              : "Decisions taken back or let go, and proposals decided. Apply brings one back."}
        </p>
        {views}
        {banner}
        {view === "needs" ? (
          <NeedsYou
            sel={sel}
            me={me}
            admin={admin}
            mine={buckets.mine}
            proposalsLoading={!ps.data}
            dream={ds}
            conflicts={cs}
            schema={schema}
            onSeeConflicts={() => setView("conflicts")}
            onChanged={reloadAll}
            onFocus={onFocus}
          />
        ) : !admin ? (
          <EmptyState icon="lock" title="Only the org's admins see this">
            What the dream did, and Undo, are for the people who set up and look after this
            memory.
          </EmptyState>
        ) : view === "done" ? (
          <DreamList
            key={`done:${key}:${fresh}`}
            sel={sel}
            me={me}
            status="applied"
            onChanged={reloadAll}
            onFocus={onFocus}
          />
        ) : (
          <>
            <DreamList
              key={`history:${key}:${fresh}`}
              sel={sel}
              me={me}
              status={["undone", "dismissed", "released"]}
              onChanged={reloadAll}
              onFocus={onFocus}
            />
            <div className="h-section rv-tight">Proposals decided</div>
            {!ps.data ? (
              <Skeleton rows={2} title={false} />
            ) : buckets.published.length + buckets.rejected.length === 0 ? (
              <p className="muted rv-small">No proposal has been decided yet.</p>
            ) : (
              <div className="card rv-rows">
                {[...buckets.published, ...buckets.rejected]
                  .sort((a, b) => (b.updated_at ?? 0) - (a.updated_at ?? 0))
                  .slice(0, 20)
                  .map((p) => (
                    <ProposalRow key={p.id} p={p} me={me} onFocus={onFocus} />
                  ))}
              </div>
            )}
          </>
        )}
      </section>
    );
  if (view === "conflicts")
    return (
      <section className="rv">
        <Head
          title="Facts that disagree"
          tools={
            <button className="btn" onClick={cs.reload} disabled={cs.loading}>
              <Icon name="refresh" size={14} /> Refresh
            </button>
          }
        />
        <p className="lede">
          Two sources say different things about one attribute that holds a single value. The
          entity's owner, or an org admin, keeps one or writes the right value; the other ends and
          stays as history.
        </p>
        {views}
        <Conflicts
          sel={sel}
          me={me}
          admin={admin}
          count={cs}
          schema={schema}
          onFocus={onFocus}
        />
      </section>
    );
  if (ps.error && !ps.data) {
    const old = /unknown op|not supported|unexpected op/i.test(ps.error);
    return (
      <section>
        <Head />
        {old ? (
          <EmptyState icon="lock" title="Owner review isn't on this server yet">
            It arrives with the next engine release; until then nothing is held back for review.
          </EmptyState>
        ) : (
          <ErrorState message={ps.error} onRetry={ps.reload} />
        )}
      </section>
    );
  }
  const shown = buckets[filter];
  return (
    <section className="rv">
      <Head
        tools={
          <button className="btn" onClick={ps.reload} disabled={ps.loading}>
            <Icon name="refresh" size={14} /> Refresh
          </button>
        }
      />
      <p className="lede">
        A change to an owned entity waits here until each of its owners approves that exact
        revision. Until then main is unchanged and agents keep answering from what was approved.
      </p>
      {views}
      <div className="rv-layout">
        <div className="rv-main">
          <div className="rv-filters" role="tablist" aria-label="Filter proposals">
            {FILTERS.map(([k, label]) => (
              <button
                key={k}
                role="tab"
                aria-selected={filter === k}
                className={`chip ${filter === k ? "chip-on" : ""}`}
                onClick={() => setFilter(k)}
              >
                {label}
                {buckets[k].length > 0 && <span className="rv-n">{buckets[k].length}</span>}
              </button>
            ))}
          </div>
          {!ps.data ? (
            <Skeleton rows={3} title={false} />
          ) : shown.length === 0 ? (
            <EmptyState
              icon="check"
              title={
                filter === "mine"
                  ? "Nothing waits on you"
                  : filter === "all"
                    ? "No proposals yet"
                    : `Nothing ${FILTERS.find(([k]) => k === filter)?.[1].toLowerCase()}`
              }
            >
              {filter === "all" && "The first change to an owned entity opens one."}
            </EmptyState>
          ) : (
            <div className="card rv-rows">
              {shown.map((p) => (
                <ProposalRow key={p.id} p={p} me={me} onFocus={onFocus} />
              ))}
            </div>
          )}
        </div>
        <aside className="rv-rail" aria-label="Owned entities">
          <OwnersCard sel={sel} me={me} admin={admin} proposals={ps.data} onFocus={onFocus} />
          <div className="card card-pad rv-how">
            <div className="h-section">What approval means</div>
            <ul>
              <li>
                <Icon name="lock" size={13} /> An owner approves one exact revision. If the draft,
                the memory under it or the owners change, the approval is void and a new review
                opens.
              </li>
              <li>
                <Icon name="user" size={13} /> Approving doesn't widen access: a reviewer sees only
                the evidence they could already read.
              </li>
            </ul>
          </div>
        </aside>
      </div>
    </section>
  );
}

/* ---------- needs you ---------- */

function NeedsYou({
  sel,
  me,
  admin,
  mine,
  proposalsLoading,
  dream,
  conflicts,
  schema,
  onSeeConflicts,
  onChanged,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  admin: boolean;
  mine: ProposalSummary[];
  proposalsLoading: boolean;
  dream: Read<DreamList | null>;
  conflicts: Read<ConflictPage>;
  schema?: Map<string, OrgType>;
  onSeeConflicts: () => void;
  onChanged: () => void;
  onFocus: (f: Focus) => void;
}) {
  // What you decided here stays in view, as decided, until you leave: the list read again holds only what waits.
  const [decided, setDecided] = useState<DreamDecision[]>([]);
  const fresh = dream.data?.decisions ?? [];
  const waiting = [...fresh, ...decided.filter((d) => !fresh.some((w) => w.id === d.id))];
  const mineToSettle = (conflicts.data?.conflicts ?? []).filter((c) => c.can_resolve);
  const total = conflicts.data?.total ?? 0;
  const loading = proposalsLoading || (admin && !dream.data && !dream.error) || !conflicts.data;
  if (!loading && mine.length + waiting.length + mineToSettle.length === 0)
    return (
      <EmptyState icon="check" title="Nothing needs you">
        {total > 0
          ? `${total} ${total === 1 ? "conflict waits" : "conflicts wait"} for someone else to settle.`
          : "When a change waits for your approval, the dream isn't sure of something, or two sources disagree, it shows here."}
      </EmptyState>
    );
  return (
    <div className="rv-needs">
      {(proposalsLoading || mine.length > 0) && (
        <div className="rv-group">
          <div className="h-section">Changes waiting for your approval</div>
          {proposalsLoading ? (
            <Skeleton rows={2} title={false} />
          ) : (
            <div className="card rv-rows">
              {mine.map((p) => (
                <ProposalRow key={p.id} p={p} me={me} onFocus={onFocus} />
              ))}
            </div>
          )}
        </div>
      )}
      {admin && (waiting.length > 0 || !dream.data) && (
        <div className="rv-group">
          <div className="h-section">The dream wasn't sure</div>
          <p className="muted rv-small rv-group-lede">
            These aren't applied. Apply one if it's right; dismiss it and the dream won't suggest
            it again.
          </p>
          {dream.error && !dream.data ? (
            <ErrorState message={dream.error} onRetry={dream.reload} />
          ) : !dream.data ? (
            <Skeleton rows={2} title={false} />
          ) : (
            <div className="rv-dreams">
              {waiting.map((d) => (
                <DreamCard
                  key={d.id}
                  sel={sel}
                  d={d}
                  me={me}
                  onChanged={onChanged}
                  onDone={(x) => setDecided((ds) => [...ds.filter((y) => y.id !== x.id), x])}
                  onFocus={onFocus}
                />
              ))}
            </div>
          )}
        </div>
      )}
      {(!conflicts.data || mineToSettle.length > 0) && (
        <div className="rv-group">
          <div className="h-section">
            Facts that disagree
            {total > mineToSettle.length && (
              <button className="link rv-small rv-group-more" onClick={onSeeConflicts}>
                all {total}
              </button>
            )}
          </div>
          {!conflicts.data ? (
            <Skeleton rows={2} title={false} />
          ) : (
            <div className="rv-conflicts">
              {mineToSettle.map((c) => (
                <ConflictCard
                  key={`${c.facts[0].fact_id}:${c.facts[1].fact_id}`}
                  sel={sel}
                  c={c}
                  me={me}
                  values={closedValues(schema, c.entity, c.predicate)}
                  onFocus={onFocus}
                />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/* ---------- the dream ---------- */

const counted = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** How long one pass lasts at most, for a last run whose records name no pass: is it the one that last changed
 * something? */
const PASS_US = 3_600_000_000;

/** What the dream did over this memory, as Home and Review both say it, from one read of its decisions. */
export interface DreamState {
  /** When a pass last ran, a change or not; null when none has. */
  ran: number | null;
  /** What that last run did, where known. */
  did: DreamPass | null;
  /** The newest pass that changed something, when it ran before the last run. */
  changed: DreamPass | null;
  /** The memory it ran over, in words ("org acme", "team acme/eng"). */
  scope: string | null;
  /** Decisions applied on their own: in the last seven days (a server before it counts none), and in all. */
  week: number | undefined;
  applied: number;
}

const NOTHING = { ended: 0, linked: 0, named: 0, waiting: 0 };

/**
 * When the dream last ran and what it did. The last run is the newest of the dreamer's last commit (`last_run`, the
 * same whichever process ran it), this server's last pass and the newest pass that decided something. What it did:
 * this server's count when that is the last run; else the deciding pass's when it is the last run, and nothing when
 * the last run is another pass (it decided nothing, or it would be the deciding one).
 */
export function dreamState(list: DreamList): DreamState {
  const last = list.dreaming.last;
  const here: DreamPass | null = last
    ? {
        at: last.at,
        ended: Number(last.counts.ended ?? 0),
        linked: Number(last.counts.linked ?? 0),
        named: Number(last.counts.aliased ?? 0),
        waiting: Number(last.counts.waiting ?? 0),
      }
    : null;
  const deciding = list.last_pass;
  const run = list.last_run ?? null;
  const times = [run?.at, here?.at, deciding?.at].filter(
    (t): t is number => typeof t === "number" && t > 0,
  );
  const ran = times.length ? Math.max(...times) : null;
  let did: DreamPass | null = null;
  if (ran === null) did = null;
  else if (here && here.at >= ran) did = here;
  else if (deciding && (run?.pass != null ? run.pass === deciding.at : ran - deciding.at < PASS_US))
    did = deciding;
  else if (run?.pass != null) did = { at: ran, ...NOTHING };
  return {
    ran,
    did: did && ran !== null ? { ...did, at: ran } : null,
    // Shown only when the last run changed nothing, or what it did isn't known.
    changed:
      deciding && (did === null || did.ended + did.linked + did.named + did.waiting === 0)
        ? deciding
        : null,
    scope: list.scope ? list.scope.replace(":", " ") : null,
    week: list.counts.applied_week,
    applied: list.counts.applied,
  };
}

/** "ended 2, linked 1, named 1, 1 waiting for you", or that nothing needed changing. */
export function dreamDid(p: DreamPass): string {
  return p.ended + p.linked + p.named + p.waiting === 0
    ? "nothing needed changing."
    : [
        `ended ${p.ended}`,
        `linked ${p.linked}`,
        `named ${p.named}`,
        p.waiting ? `${p.waiting} waiting for you` : null,
      ]
        .filter(Boolean)
        .join(", ") + ".";
}

/**
 * "Dreamt 3 hours ago (Nov 16, 05:29) over org acme: nothing needed changing.", and, when the last run changed
 * nothing, when the dream last did: one sentence for Home and Review alike. "Hasn't run yet" only when no pass has.
 */
export function DreamSaid({ st, whenClass }: { st: DreamState; whenClass?: string }) {
  if (st.ran === null)
    return <span className={whenClass}>The dream hasn't run over this memory yet.</span>;
  return (
    <>
      <b>Dreamt {ago(st.ran)}</b>
      <span className={whenClass}> ({when(st.ran)})</span> over {st.scope ?? "this memory"}
      {st.did ? `: ${dreamDid(st.did)}` : "."}
      {st.changed && (
        <span className={whenClass}>
          {" "}
          Last changed something {ago(st.changed.at)}: {dreamDid(st.changed)}
        </span>
      )}
    </>
  );
}

/** "50 this week, 983 in all": what the dream applied on its own (the week where the server counts it). */
export function appliedSaid(st: DreamState): string {
  const all = `${st.applied.toLocaleString()} in all`;
  return st.week === undefined ? all : `${st.week.toLocaleString()} this week, ${all}`;
}

/** "Dreamt 14:02: ended 2, linked 1, named 1, 1 waiting for you", and Dream now. */
function DreamBanner({
  sel,
  list,
  onChanged,
}: {
  sel: Selection;
  list?: DreamList;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const live = running || !!list?.dreaming.running;
  // While a pass runs, look again every few seconds; when it ends, read everything again.
  useEffect(() => {
    if (!live) return;
    let stop = false;
    const t = setInterval(() => {
      review.dream(sel, { limit: 1 }).then(
        (r) => {
          if (stop || r.dreaming.running) return;
          setRunning(false);
          onChanged();
        },
        () => {},
      );
    }, 3000);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [live, sel.org, sel.team]);
  if (!list) return null;
  const st = dreamState(list);
  const start = () => {
    setBusy(true);
    setErr(null);
    review.dreamNow(sel).then(
      () => {
        setBusy(false);
        setRunning(true);
      },
      (e: unknown) => {
        setErr(errorOf(e));
        setBusy(false);
      },
    );
  };
  return (
    <div className={`rv-dreambar ${live ? "rv-dreambar-live" : ""}`} role="status">
      <span className="rv-dreambar-icon" aria-hidden>
        <Icon name={live ? "refresh" : "moon"} size={16} />
      </span>
      <div className="rv-dreambar-text">
        {live ? (
          <b>Dreaming over this memory…</b>
        ) : (
          <DreamSaid st={st} whenClass="muted" />
        )}
        {!live && st.applied > 0 && (
          <span className="muted"> Applied on its own: {appliedSaid(st)}.</span>
        )}
        {list.dreaming.failed && !live && (
          <span className="error-text rv-small"> The last pass failed: {list.dreaming.failed.error}</span>
        )}
        {err && (
          <span className="error-text rv-small" role="alert">
            {" "}
            {err}
          </span>
        )}
      </div>
      {list.dreaming.available && (
        <button className="btn" onClick={start} disabled={busy || live}>
          <Icon name="play" size={13} /> {live ? "Dreaming…" : busy ? "Starting…" : "Dream now"}
        </button>
      )}
    </div>
  );
}

const KIND_FILTERS: [DreamKind | "all", string][] = [
  ["all", "All"],
  ["end", "Ended"],
  ["link", "Linked"],
  ["name", "Named"],
  ["share", "Shared with team"],
];

/** A page at a time of the dream's decisions in some status, with a filter by kind. */
function DreamList({
  sel,
  me,
  status,
  onChanged,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  status: DreamStatus | DreamStatus[];
  onChanged: () => void;
  onFocus: (f: Focus) => void;
}) {
  const [kind, setKind] = useState<DreamKind | "all">("all");
  const [pages, setPages] = useState<DreamDecision[][]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [state, setState] = useState<{ loading: boolean; error?: string }>({ loading: true });
  const load = (after?: string) => {
    setState({ loading: true });
    review
      .dream(sel, { status, kind: kind === "all" ? undefined : kind, limit: 30, after })
      .then(
        (r) => {
          setPages((p) => (after ? [...p, r.decisions] : [r.decisions]));
          setNext(r.next_page);
          setState({ loading: false });
        },
        (e: unknown) => setState({ loading: false, error: errorOf(e) }),
      );
  };
  useEffect(() => load(), [kind]);
  const all = pages.flat();
  // A card acted on stays where it is, showing what it is now; the list reads afresh when it is opened again.
  const done = (x: DreamDecision) =>
    setPages((ps) => ps.map((p) => p.map((y) => (y.id === x.id ? x : y))));
  return (
    <div className="rv-dreamlist">
      <div className="rv-filters" role="tablist" aria-label="Filter by what it did">
        {KIND_FILTERS.map(([k, label]) => (
          <button
            key={k}
            role="tab"
            aria-selected={kind === k}
            className={`chip ${kind === k ? "chip-on" : ""}`}
            onClick={() => setKind(k)}
          >
            {label}
          </button>
        ))}
      </div>
      {state.error && all.length === 0 ? (
        <ErrorState message={state.error} onRetry={() => load()} />
      ) : state.loading && all.length === 0 ? (
        <Skeleton rows={3} title={false} />
      ) : all.length === 0 ? (
        <EmptyState icon="check" title={Array.isArray(status) ? "Nothing taken back" : "Nothing yet"}>
          {Array.isArray(status)
            ? "Decisions you undo or dismiss, and those the dream lets go when what they rest on is forgotten, show here."
            : "Once enough new facts arrive, the dream tidies the memory and what it did shows here."}
        </EmptyState>
      ) : (
        <div className="rv-dreams">
          {all.map((d) => (
            <DreamCard key={d.id} sel={sel} d={d} me={me} onChanged={onChanged} onDone={done} onFocus={onFocus} />
          ))}
          {next && (
            <button className="btn rv-more" disabled={state.loading} onClick={() => load(next)}>
              {state.loading ? "Loading…" : "Show more"}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

const DREAM_KIND: Record<DreamKind, { label: string; icon: string }> = {
  end: { label: "Ended", icon: "history" },
  link: { label: "Linked", icon: "copy" },
  name: { label: "Named", icon: "entity" },
  share: { label: "Shared with team", icon: "user" },
};

const DREAM_STATUS: Record<DreamStatus, [string, string]> = {
  applied: ["applied", "badge-ok"],
  waiting: ["waiting for you", "rv-badge-warn"],
  undone: ["undone", ""],
  dismissed: ["dismissed", ""],
  released: ["let go", ""],
};

const ROLE: Record<string, string> = {
  old: "Was",
  by: "Replaced by",
  keep: "Kept",
  dup: "Same thing",
  alias: "Also called",
  name: "Named",
};

const REVIEWED: Record<string, string> = { apply: "Applied", undo: "Undone", dismiss: "Dismissed" };

/**
 * One decision: what it did and why, the facts it rests on, and Undo (or Apply / Dismiss while it waits). Review's
 * lists and an entity's page (the decisions touching it) both show it.
 */
export function DreamCard({
  sel,
  d: given,
  me,
  onChanged,
  onDone,
  onFocus,
}: {
  sel: Selection;
  d: DreamDecision;
  me?: string;
  onChanged: () => void;
  /** The decision as it stands after this card applied, undid or dismissed it. */
  onDone?: (d: DreamDecision) => void;
  onFocus: (f: Focus) => void;
}) {
  const [d, setD] = useState(given);
  const [busy, setBusy] = useState<"undo" | "apply" | "dismiss" | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => setD(given), [given]);
  const act = (what: "undo" | "apply" | "dismiss") => {
    setBusy(what);
    setErr(null);
    const call =
      what === "undo" ? review.undoDream : what === "apply" ? review.applyDream : review.dismissDream;
    call(sel, d.id).then(
      (r) => {
        const next = { ...d, ...r };
        setD(next);
        setBusy(null);
        onDone?.(next);
        onChanged();
      },
      (e: unknown) => {
        setErr(errorOf(e));
        setBusy(null);
      },
    );
  };
  const k = DREAM_KIND[d.kind];
  const [statusWord, statusClass] = DREAM_STATUS[d.status];
  return (
    <div className={`card rv-dream rv-dream-${d.status}`}>
      <div className="rv-dream-head">
        <span className={`rv-dream-kind rv-dk-${d.kind}`} title={k.label}>
          <Icon name={k.icon} size={13} />
          {k.label}
        </span>
        <span className={`badge ${statusClass}`}>{statusWord}</span>
        <span className="muted rv-small rv-dream-when" title={when(d.at)}>
          {ago(d.at)}
        </span>
      </div>
      <div className="rv-dream-summary">{d.summary}</div>
      {d.why && (
        <div className="rv-why">
          <span className="rv-why-label">Why</span> {d.why}
        </div>
      )}
      {d.facts.length > 0 && (
        <div className={`rv-dream-facts rv-df-${d.kind}`}>
          {d.facts.map((f) => (
            <div key={`${f.role}:${f.id}`} className={`rv-dfact rv-dfact-${f.role}`}>
              <span className="rv-dfact-role">{ROLE[f.role] ?? f.role}</span>
              <span className="rv-dfact-value">{f.value}</span>
              <span className="rv-dfact-meta muted rv-small">
                <button
                  className="link mono"
                  onClick={() => onFocus(entityFocus(f.entity))}
                  title={f.entity}
                >
                  {f.entity}
                </button>{" "}
                · {humanize(f.predicate)} · {when(f.valid_from)}
              </span>
            </div>
          ))}
        </div>
      )}
      <div className="rv-dream-foot">
        <span className="muted rv-small">
          {d.reviewed ? (
            <>
              {REVIEWED[d.reviewed.action] ?? d.reviewed.action} by{" "}
              <Who user={d.reviewed.by} me={me} /> {ago(d.reviewed.at)}
            </>
          ) : d.status === "released" ? (
            "Let go by the dream: what it rested on was forgotten or no longer stands."
          ) : (
            <span className="mono" title={d.by}>
              {d.kind === "share" ? "by the memory" : d.by.replace(/^dream:/, "dream · ")}
            </span>
          )}
        </span>
        <span className="rv-dream-actions">
          {d.can_dismiss && (
            <button className="btn" disabled={busy !== null} onClick={() => act("dismiss")}>
              <Icon name="close" size={13} /> {busy === "dismiss" ? "Dismissing…" : "Dismiss"}
            </button>
          )}
          {d.can_apply && (
            <button
              className={`btn ${d.status === "waiting" ? "btn-primary" : ""}`}
              disabled={busy !== null}
              onClick={() => act("apply")}
            >
              <Icon name="check" size={13} /> {busy === "apply" ? "Applying…" : "Apply"}
            </button>
          )}
          {d.can_undo && (
            <button className="btn" disabled={busy !== null} onClick={() => act("undo")}>
              <Icon name="history" size={13} /> {busy === "undo" ? "Undoing…" : "Undo"}
            </button>
          )}
        </span>
      </div>
      {err && (
        <p className="error-text rv-small" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function Head({
  tools,
  title = "Changes waiting for their owners",
}: {
  tools?: React.ReactNode;
  title?: string;
}) {
  return (
    <div className="head">
      <span className="kind k-entity">
        <Icon name="lock" size={22} />
      </span>
      <div className="head-text">
        <div className="eyebrow">Review</div>
        <h1 className="title">{title}</h1>
      </div>
      {tools}
    </div>
  );
}

export function ProposalRow({
  p,
  me,
  onFocus,
}: {
  p: ProposalSummary;
  me?: string;
  onFocus: (f: Focus) => void;
}) {
  const waiting = p.required_owners.filter((o) => !p.approvals.includes(o));
  return (
    <button
      className={`list-row rv-row rv-${p.status}`}
      onClick={() => onFocus({ kind: "review", proposal: p.id })}
    >
      <div className="rv-when">
        <span className="mono">{p.updated_at ? clock(p.updated_at) : ""}</span>
        <span>{p.updated_at ? ago(p.updated_at) : ""}</span>
      </div>
      <StatusBadge status={p.status} />
      <div className="list-main">
        <div className="list-title">{titleOf(p)}</div>
        {p.first?.predicate && p.first.predicate !== "schema" && (
          <div className="rv-first">
            {humanize(p.first.predicate)} <Icon name="arrow" size={12} />{" "}
            <b>{p.first.value == null ? "ended" : String(p.first.value)}</b>
          </div>
        )}
        <div className="list-sub rv-sub">
          <span className="mono" title="Its draft branch">
            {p.branch}
          </span>
          {p.entities.slice(0, 3).map((e) => (
            <span key={e} className="mono">
              {e}
            </span>
          ))}
          <span>
            by <Author by={p.created_by} me={me} />
          </span>
        </div>
      </div>
      <div className="list-meta rv-owners">
        {p.status === "rejected" && p.rejected_by ? (
          <span className="rv-owner rv-bad">
            <i /> <Who user={p.rejected_by} me={me} />
          </span>
        ) : p.status === "published" ? (
          <span>
            {p.required_owners.length === 1
              ? "owner approved"
              : `${p.required_owners.length} owners approved`}
          </span>
        ) : p.status === "stale" ? (
          <span>needs a new review</span>
        ) : (
          p.required_owners.map((o) => (
            <span key={o} className={`rv-owner ${waiting.includes(o) ? "" : "rv-ok"}`}>
              <i /> <Who user={o} me={me} />
            </span>
          ))
        )}
      </div>
    </button>
  );
}

const clock = (us: number) =>
  new Date(us / 1000).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  });

/* ---------- facts that disagree ---------- */

type Read<T> = ReturnType<typeof useRead<T>>;

/** How many conflicts one page shows. */
const CONFLICT_PAGE = 50;

function Conflicts({
  sel,
  me,
  admin,
  count,
  schema,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  admin: boolean;
  /** How many conflicts the reader may see in all, read by the inbox for its tab. */
  count: Read<ConflictPage>;
  schema?: Map<string, OrgType>;
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team]);
  // "Mine" asks memstore for the conflicts on what the reader owns.
  const [mine, setMine] = useFlag("mine");
  // The first page; "Show more" adds the next ones after it, and a reload starts again from the first.
  const r = useRead(
    () => review.conflicts(sel, { mine: mine || undefined, limit: CONFLICT_PAGE }),
    [key, mine],
  );
  const [more, setMore] = useState<{
    of?: ConflictPage;
    pages: ConflictPage[];
    busy: boolean;
    error?: string;
  }>({ pages: [], busy: false });
  const pages = r.data ? [r.data, ...(more.of === r.data ? more.pages : [])] : [];
  const last = pages[pages.length - 1];
  const list = r.data ? pages.flatMap((p) => p.conflicts) : undefined;
  const showMore = () => {
    if (!r.data || !last?.next) return;
    const of = r.data;
    setMore((m) => ({ ...m, busy: true, error: undefined }));
    review.conflicts(sel, { mine: mine || undefined, limit: CONFLICT_PAGE, after: last.next }).then(
      (p) =>
        setMore((m) => ({
          of,
          pages: [...(m.of === of ? m.pages : []), p],
          busy: false,
        })),
      (e: unknown) => setMore((m) => ({ ...m, busy: false, error: errorOf(e) })),
    );
  };
  const reload = () => {
    count.reload();
    r.reload();
  };

  if (r.error && !list) {
    const old = /unknown op|not supported|unexpected op/i.test(r.error);
    return old ? (
      <EmptyState icon="alert" title="Conflicting facts aren't on this server yet">
        They arrive with the next engine release.
      </EmptyState>
    ) : (
      <ErrorState message={r.error} onRetry={reload} />
    );
  }
  return (
    <div className="rv-layout">
      <div className="rv-main">
        <div className="rv-toolbar">
          <div className="rv-filters" role="tablist" aria-label="Filter conflicts">
            {(
              [
                [false, "All"],
                [true, "Mine"],
              ] as const
            ).map(([m, label]) => (
              <button
                key={label}
                role="tab"
                aria-selected={mine === m}
                className={`chip ${mine === m ? "chip-on" : ""}`}
                onClick={() => setMine(m)}
              >
                {label}
                {!m && count.data && count.data.total > 0 && (
                  <span className="rv-n">{count.data.total}</span>
                )}
              </button>
            ))}
          </div>
          {admin && <CheckConflicts sel={sel} onDone={reload} />}
        </div>
        {!list ? (
          <Skeleton rows={3} title={false} />
        ) : list.length === 0 ? (
          <EmptyState icon="check" title={mine ? "Nothing you own disagrees" : "No conflicts"}>
            {mine
              ? "Conflicts on the entities you own show here."
              : admin
                ? "Check for conflicts to compare what each source says now."
                : "When two sources disagree about something, it shows here."}
          </EmptyState>
        ) : (
          <div className="rv-conflicts">
            {list.map((c) => (
              <ConflictCard
                key={`${c.facts[0].fact_id}:${c.facts[1].fact_id}`}
                sel={sel}
                c={c}
                me={me}
                values={closedValues(schema, c.entity, c.predicate)}
                onFocus={onFocus}
              />
            ))}
            {last?.next && (
              <button className="btn rv-more" onClick={showMore} disabled={more.busy}>
                {more.busy
                  ? "Loading…"
                  : `Show more (${list.length} of ${last.total.toLocaleString()})`}
              </button>
            )}
            {more.error && more.of === r.data && (
              <p className="error-text rv-small" role="alert">
                {more.error}
              </p>
            )}
          </div>
        )}
      </div>
      <aside className="rv-rail" aria-label="Conflicts">
        {admin && <AddFact sel={sel} onFocus={onFocus} />}
        <div className="card card-pad rv-how">
          <div className="h-section">What resolving does</div>
          <ul>
            <li>
              <Icon name="check" size={13} /> Keeping one ends the other as of now; both stay in the
              entity's history.
            </li>
            <li>
              <Icon name="lock" size={13} /> Writing the right value ends both and starts a new
              fact. On an entity with several owners it waits for the others to approve.
            </li>
          </ul>
        </div>
      </aside>
    </div>
  );
}

function ConflictCard({
  sel,
  c,
  me,
  values,
  onFocus,
}: {
  sel: Selection;
  c: Conflict;
  me?: string;
  /** The values the conflict's attribute takes when the schema closes it: the right one is picked, not typed. */
  values?: string[];
  onFocus: (f: Focus) => void;
}) {
  const [writing, setWriting] = useState(false);
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [done, setDone] = useState<Resolution | null>(null);
  const ids: [string, string] = [c.facts[0].fact_id, c.facts[1].fact_id];

  const resolve = (how: { keep: string } | { value: string }) => {
    setBusy("keep" in how ? how.keep : "value");
    setErr(null);
    review.resolve(sel, ids, how).then(
      (r) => {
        setDone(r);
        setBusy(null);
      },
      (e: unknown) => {
        setErr(errorOf(e));
        setBusy(null);
      },
    );
  };
  const live = c.can_resolve && !done;
  return (
    <div className={`card rv-conflict ${done ? "rv-resolved" : ""}`}>
      <div className="rv-change-head rv-conflict-head">
        <button
          className="link mono"
          onClick={() => onFocus(entityFocus(c.entity))}
          title={c.entity}
        >
          {c.entity}
        </button>
        <span className="rv-pred" title={c.predicate}>
          {humanize(c.predicate)}
        </span>
        <span className="rv-conflict-owner muted rv-small">
          {c.owner ? (
            <>
              owner <Who user={c.owner} me={me} />
            </>
          ) : (
            "no owner"
          )}
        </span>
      </div>
      <div className="rv-sides">
        {c.facts.map((f, i) => (
          <ConflictSideView
            key={f.fact_id}
            f={f}
            label={i === 0 ? "Older" : "Newer"}
            kept={done ? done.kept === f.fact_id : undefined}
            onFocus={onFocus}
            action={
              live ? (
                <button
                  className="btn"
                  disabled={busy !== null}
                  onClick={() => resolve({ keep: f.fact_id })}
                >
                  <Icon name="check" size={14} />
                  {busy === f.fact_id ? "Keeping…" : "Keep this"}
                </button>
              ) : null
            }
          />
        ))}
      </div>
      {done ? (
        <ResolvedLine r={done} onFocus={onFocus} />
      ) : live ? (
        <div className="rv-resolve">
          {writing ? (
            <form
              className="rv-write"
              onSubmit={(ev) => {
                ev.preventDefault();
                if (value.trim()) resolve({ value: value.trim() });
              }}
            >
              <ValueField
                values={values}
                autoFocus
                value={value}
                onChange={setValue}
                placeholder={`The right ${humanize(c.predicate).toLowerCase()}`}
                label="The right value"
              />
              <button className="btn btn-primary" disabled={busy !== null || !value.trim()}>
                {busy === "value" ? "Saving…" : "Use this value"}
              </button>
              <button
                type="button"
                className="icon-btn"
                aria-label="Cancel"
                onClick={() => setWriting(false)}
              >
                <Icon name="close" size={13} />
              </button>
            </form>
          ) : (
            <button className="link rv-small" onClick={() => setWriting(true)}>
              Neither: write the right value
            </button>
          )}
        </div>
      ) : (
        <p className="muted rv-small rv-resolve">
          {c.owner ? `Only ${c.owner === me ? "you" : c.owner}` : "Only an org admin"}
          {c.owner ? " or an org admin" : ""} can resolve this.
        </p>
      )}
      {err && (
        <p className="error-text rv-small rv-resolve" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function ConflictSideView({
  f,
  label,
  kept,
  action,
  onFocus,
}: {
  f: ConflictSide;
  label: string;
  /** After a resolution: whether this side stands. */
  kept?: boolean;
  action: React.ReactNode;
  onFocus: (f: Focus) => void;
}) {
  return (
    <div
      className={`rv-side ${kept === true ? "rv-side-kept" : kept === false ? "rv-side-ended" : ""}`}
    >
      <div className="rv-side-label muted rv-small">
        {label}
        {kept === true ? " · kept" : kept === false ? " · ended" : ""}
      </div>
      <div className="rv-side-value">{f.value}</div>
      <div className="rv-side-meta muted rv-small">
        <span className="mono" title="Its source">
          {f.source ?? "no source label"}
        </span>
        <span title={when(f.since)}>since {f.since_day}</span>
      </div>
      {f.quote && <div className="rv-excerpt">“{f.quote}”</div>}
      {(f.episode_id || action) && (
        <div className="rv-side-actions">
          {action}
          {f.episode_id && (
            <button
              className="link rv-small"
              onClick={() => onFocus({ kind: "episode", id: f.episode_id! })}
            >
              Its episode
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function ResolvedLine({ r, onFocus }: { r: Resolution; onFocus: (f: Focus) => void }) {
  const published = r.status === "published";
  return (
    <div className={`banner rv-banner rv-resolve ${published ? "rv-banner-ok" : "banner-note"}`}>
      <Icon name={published ? "check" : "clock"} size={14} />
      <span>
        {published
          ? r.added
            ? "Resolved: the new value is on main, and both old facts ended."
            : "Resolved: published to main. Agents answer from the kept fact now."
          : "Resolved, and waiting for the entity's other owners to approve it."}
      </span>
      {r.review && !published && (
        <button
          className="btn"
          onClick={() => onFocus({ kind: "review", proposal: r.review!.proposal.id })}
        >
          Open its review
        </button>
      )}
    </div>
  );
}

/** An admin's check now, rather than on the next schedule: an action on the list it fills. */
function CheckConflicts({ sel, onDone }: { sel: Selection; onDone: () => void }) {
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<ConflictCheck | null>(null);
  const [err, setErr] = useState<string | null>(null);
  return (
    <div className="rv-check">
      <div className="rv-check-row">
        <span className="rv-small muted" aria-live="polite">
          {busy
            ? "Comparing what each source says…"
            : said
              ? `${said.checked.toLocaleString()} ${said.checked === 1 ? "fact" : "facts"} checked · ${said.recorded} new`
              : null}
        </span>
        <button
          className="btn"
          disabled={busy}
          aria-busy={busy}
          onClick={() => {
            setBusy(true);
            setErr(null);
            review.checkConflicts(sel).then(
              (r) => {
                setSaid(r);
                setBusy(false);
                onDone();
              },
              (e: unknown) => {
                setErr(errorOf(e));
                setBusy(false);
              },
            );
          }}
        >
          <span className={busy ? "rv-spin" : undefined}>
            <Icon name="refresh" size={14} />
          </span>
          {busy ? "Checking…" : "Check now"}
        </button>
      </div>
      {said && !said.marked_in_reads && (
        <p className="rv-small rv-hint">
          <Icon name="alert" size={13} /> The conflicts setting is off for this memory, so searches
          and agents don't flag these facts yet. Listing and resolving them here still works.
        </p>
      )}
      {err && (
        <p className="error-text rv-small" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

/** memstore's rule for an attribute's name. */
const PREDICATE = /^[A-Za-z][A-Za-z0-9_.-]{0,127}$/;
/** A source label: the account a fact comes from. */
const SOURCE = /^[A-Za-z0-9_.-]{1,64}$/;

/** An admin states a fact as a source would. On an owned entity their approval publishes it at once. */
function AddFact({ sel, onFocus }: { sel: Selection; onFocus: (f: Focus) => void }) {
  const [entity, setEntity] = useState("");
  const [predicate, setPredicate] = useState("");
  const [value, setValue] = useState("");
  const [source, setSource] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<{ ok: boolean; text: string; proposal?: string } | null>(null);
  const e = entity.trim();
  const p = predicate.trim();
  const src = source.trim();
  const bad =
    e && !ENTITY.test(e)
      ? "entity looks like type:name, e.g. account:northwind"
      : p && !PREDICATE.test(p)
        ? "attribute: letters, digits and _ . -, e.g. renewal_date"
        : src && !SOURCE.test(src)
          ? "source: a short label (letters, digits, - _ .)"
          : null;
  const ready = Boolean(e && p && value.trim() && !bad);
  return (
    <form
      className="card card-pad rv-assign rv-add"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (!ready) return;
        setBusy(true);
        review
          .addFact(sel, {
            entity: e,
            predicate: p,
            value: value.trim(),
            source: src || undefined,
            note: note.trim() || undefined,
          })
          .then(
            (r) => {
              const status = r.review?.status;
              setSaid({
                ok: true,
                text:
                  !r.review || status === "published"
                    ? "Added to main. Check for conflicts to compare it with what's there."
                    : "Added; it waits for the entity's owners to approve it.",
                proposal: r.review && status !== "published" ? r.review.proposal.id : undefined,
              });
              setValue("");
              setNote("");
              setBusy(false);
            },
            (err: unknown) => {
              setSaid({ ok: false, text: errorOf(err) });
              setBusy(false);
            },
          );
      }}
    >
      <div className="h-section">Add a fact</div>
      <label className="rv-label">
        <span>Entity</span>
        <input
          className="field mono"
          value={entity}
          onChange={(ev) => {
            setEntity(ev.target.value);
            setSaid(null);
          }}
          placeholder="e.g. account:northwind"
          aria-invalid={Boolean(e && !ENTITY.test(e))}
          spellCheck={false}
        />
      </label>
      <label className="rv-label">
        <span>Attribute</span>
        <input
          className="field mono"
          value={predicate}
          onChange={(ev) => setPredicate(ev.target.value)}
          placeholder="e.g. renewal_date"
          aria-invalid={Boolean(p && !PREDICATE.test(p))}
          spellCheck={false}
        />
      </label>
      <label className="rv-label">
        <span>Value</span>
        <input
          className="field"
          value={value}
          onChange={(ev) => setValue(ev.target.value)}
          placeholder="e.g. 2027-03-15"
          maxLength={2000}
        />
      </label>
      <label className="rv-label">
        <span>
          Source <em>optional</em>
        </span>
        <input
          className="field mono"
          value={source}
          onChange={(ev) => setSource(ev.target.value)}
          placeholder="e.g. crm"
          aria-invalid={Boolean(src && !SOURCE.test(src))}
          spellCheck={false}
        />
      </label>
      <label className="rv-label">
        <span>
          Note <em>optional</em>
        </span>
        <input
          className="field"
          value={note}
          onChange={(ev) => setNote(ev.target.value)}
          placeholder="Where it was said"
          maxLength={2000}
        />
      </label>
      <button className="btn btn-primary" disabled={busy || !ready}>
        {busy ? "Adding…" : "Add the fact"}
      </button>
      <p className={`rv-small ${bad || (said && !said.ok) ? "error-text" : "muted"}`}>
        {bad ??
          said?.text ??
          "Facts with one source label are one source revising itself: they never conflict."}
        {said?.proposal && (
          <>
            {" "}
            <button
              type="button"
              className="link"
              onClick={() => onFocus({ kind: "review", proposal: said.proposal })}
            >
              Open its review
            </button>
          </>
        )}
      </p>
    </form>
  );
}

/* ---------- owned entities ---------- */

/** memstore's rule for an entity id: a type, a colon, a name. */
const ENTITY = /^[a-z][a-z0-9_-]*:[^\s,]+$/;

function OwnersCard({
  sel,
  me,
  admin,
  proposals,
  onFocus,
}: {
  sel: Selection;
  me?: string;
  admin: boolean;
  proposals?: ProposalSummary[];
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team]);
  const ow = useRead(() => review.owners(sel), [key]);
  const owners = ow.data ? [...ow.data].sort((a, b) => a.entity.localeCompare(b.entity)) : null;
  const pending = (o: Owner) =>
    (proposals ?? []).filter(
      (p) =>
        p.status === "pending_review" &&
        p.entities.includes(o.entity) &&
        !p.approvals.includes(o.user),
    ).length;
  const [err, setErr] = useState<string | null>(null);
  const act = (f: Promise<unknown>) =>
    f.then(
      () => {
        setErr(null);
        ow.reload();
      },
      (e: unknown) => setErr(errorOf(e)),
    );
  return (
    <div className="card card-pad rv-owners-card">
      <div className="h-section">
        Owned entities {owners && <span className="count">{owners.length}</span>}
      </div>
      {ow.error && !owners ? (
        <p className="error-text">{ow.error}</p>
      ) : !owners ? (
        <Skeleton rows={3} title={false} />
      ) : owners.length === 0 ? (
        <p className="muted rv-small">
          No entity has an owner yet. Changes publish as they always have.
        </p>
      ) : (
        <ul className="rv-own-list">
          {owners.map((o) => (
            <li key={o.entity}>
              <button className="link mono" onClick={() => onFocus(entityFocus(o.entity))}>
                {o.entity}
              </button>
              <span className="rv-own-who">
                <Who user={o.user} me={me} />
                {pending(o) > 0 && (
                  <span className="badge badge-accent" title={`${pending(o)} waiting on ${o.user}`}>
                    {pending(o)}
                  </span>
                )}
                {o.user === me && (
                  <TransferButton
                    sel={sel}
                    owner={o}
                    onDone={() => act(Promise.resolve())}
                    onError={setErr}
                  />
                )}
                {admin && (
                  <button
                    className="icon-btn rv-remove"
                    title={`Remove the owner of ${o.entity}: its changes publish without review again`}
                    aria-label={`Remove the owner of ${o.entity}`}
                    onClick={() => {
                      if (
                        confirm(
                          `Remove ${o.user} as owner of ${o.entity}?\n\nIts changes publish without review again.`,
                        )
                      )
                        void act(review.removeOwner(sel, o.entity, o.revision));
                    }}
                  >
                    <Icon name="close" size={13} />
                  </button>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
      {err && (
        <p className="error-text rv-small" role="alert">
          {err}
        </p>
      )}
      {admin && owners && (
        <AssignOwner sel={sel} taken={owners.map((o) => o.entity)} onDone={ow.reload} />
      )}
    </div>
  );
}

/** An admin names an entity and a member. Owning protects future changes; it doesn't vouch for what's there. */
export function AssignOwner({
  sel,
  taken,
  preset,
  onDone,
}: {
  sel: Selection;
  taken: string[];
  /** The entity is fixed (its own page): only the owner is asked. */
  preset?: string;
  onDone: () => void;
}) {
  const [entity, setEntity] = useState(preset ?? "");
  const [owner, setOwner] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<{ ok: boolean; text: string } | null>(null);
  const e = entity.trim();
  const bad = e && !ENTITY.test(e) ? "looks like type:name, e.g. policy:refund-window" : null;
  const dup = e && taken.includes(e) ? "already owned: its owner hands it on" : null;
  return (
    <form
      className="rv-assign"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (bad || dup || !e || !owner.trim()) return;
        setBusy(true);
        review.setOwner(sel, e, owner.trim().toLowerCase()).then(
          () => {
            setSaid({ ok: true, text: `${e} is now ${owner.trim()}'s` });
            setEntity(preset ?? "");
            setOwner("");
            setBusy(false);
            onDone();
          },
          (err: unknown) => {
            setSaid({ ok: false, text: errorOf(err) });
            setBusy(false);
          },
        );
      }}
    >
      {!preset && <div className="h-section">Assign an owner</div>}
      {!preset && (
        <input
          className="field mono"
          value={entity}
          onChange={(ev) => {
            setEntity(ev.target.value);
            setSaid(null);
          }}
          placeholder="policy:refund-window"
          aria-label="Entity"
          aria-invalid={Boolean(bad || dup)}
          spellCheck={false}
        />
      )}
      <div className="rv-assign-row">
        <input
          className="field"
          type="email"
          value={owner}
          onChange={(ev) => setOwner(ev.target.value)}
          placeholder="owner@acme.com"
          aria-label="Owner"
        />
        <button
          className="btn btn-primary"
          disabled={busy || !e || !owner.trim() || Boolean(bad || dup)}
        >
          {busy ? "Assigning…" : "Assign"}
        </button>
      </div>
      <p className={`rv-small ${bad || dup || (said && !said.ok) ? "error-text" : "muted"}`}>
        {bad ??
          dup ??
          said?.text ??
          "Owning protects future changes; it doesn't vouch for what's there."}
      </p>
    </form>
  );
}

function TransferButton({
  sel,
  owner,
  onDone,
  onError,
}: {
  sel: Selection;
  owner: Owner;
  onDone: () => void;
  onError: (e: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  if (!open)
    return (
      <button className="link rv-small" onClick={() => setOpen(true)}>
        Transfer
      </button>
    );
  return (
    <form
      className="rv-transfer"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (!to.trim()) return;
        setBusy(true);
        review.setOwner(sel, owner.entity, to.trim().toLowerCase(), owner.revision).then(
          () => {
            setBusy(false);
            setOpen(false);
            onDone();
          },
          (e: unknown) => {
            setBusy(false);
            onError(errorOf(e));
          },
        );
      }}
    >
      <input
        className="field"
        type="email"
        autoFocus
        value={to}
        onChange={(ev) => setTo(ev.target.value)}
        placeholder="new owner's address"
        aria-label={`New owner of ${owner.entity}`}
      />
      <button className="btn" disabled={busy || !to.trim()}>
        {busy ? "…" : "Hand on"}
      </button>
      <button type="button" className="icon-btn" aria-label="Cancel" onClick={() => setOpen(false)}>
        <Icon name="close" size={13} />
      </button>
    </form>
  );
}

/* ---------- one proposal ---------- */

const KIND_WORD: Record<string, string> = {
  fact: "fact",
  closure: "ends",
  quote: "quote",
  retraction: "retracts",
  schema: "schema",
  schemas: "schema",
  document: "document",
  episode: "episode",
  aggregate: "aggregate",
};

function ProposalPane({
  sel,
  id,
  me,
  admin,
  onFocus,
}: {
  sel: Selection;
  id: string;
  me?: string;
  admin: boolean;
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team, id]);
  const r = useRead(() => review.proposal(sel, id), [key]);
  const [view, setView] = useState<ReviewView | null>(null);
  const v = view ?? r.data ?? null;
  const [busy, setBusy] = useState<"approve" | "reject" | "repropose" | null>(null);
  const [rejecting, setRejecting] = useState(false);
  const [note, setNote] = useState("");
  const [err, setErr] = useState<string | null>(null);

  if (r.error && !v) return <ErrorState message={r.error} onRetry={r.reload} />;
  if (!v) return <Skeleton rows={6} />;
  const p = v.proposal;
  const entities = [
    ...new Set(p.changes.map((c) => c.entity).filter((x): x is string => Boolean(x))),
  ];
  const schemaChange = p.changes.find((c) => c.kind === "schemas");
  const required = Boolean(me && p.required_owners.includes(me));
  const mine = Boolean(me && v.approvals.includes(me));
  const waiting = p.required_owners.filter((o) => !v.approvals.includes(o));
  const last = required && !mine && waiting.length === 1;
  // An org admin who isn't an owner may decide too: their approval publishes for every owner.
  const asAdmin = admin && !required;
  const canDecide = (required || asAdmin) && v.status === "pending_review" && !mine;
  const decisions = v.decisions ?? [];
  const rejection = decisions.find((d) => !d.approve);
  const byAdmin = decisions.find((d) => d.approve && d.admin);
  const evidence: EvidenceRef[] =
    v.evidence_details ?? p.evidence.map((id) => ({ id, readable: true }));
  const withheld = evidence.filter((e) => !e.readable).length;

  const decide = (decision: "approve" | "reject") => {
    setBusy(decision);
    setErr(null);
    review
      .decide(
        sel,
        p.id,
        p.digest,
        decision,
        decision === "reject" ? note.trim() || undefined : undefined,
      )
      .then(
        (next) => {
          setView(next);
          setBusy(null);
          setRejecting(false);
        },
        (e: unknown) => {
          setErr(errorOf(e));
          setBusy(null);
        },
      );
  };

  return (
    <section className="rv rv-proposal">
      <nav className="crumbs" aria-label="Location">
        <button onClick={() => onFocus({ kind: "review" })}>Review</button>
        <Icon name="chevron" size={12} />
        <button className="crumb-on mono">{p.id}</button>
      </nav>
      <div className="head">
        <span className="kind k-entity">
          <Icon name="lock" size={22} />
        </span>
        <div className="head-text">
          <div className="eyebrow">Proposal</div>
          <h1 className="title">
            {titleOf({
              entities,
              changes: p.changes.length,
              first: schemaChange && { predicate: "schema", value: schemaChange.value },
            })}
          </h1>
          <div className="subtitle">
            <StatusBadge status={v.status} />
            <span>
              by <Author by={p.created_by} me={me} />
            </span>
            {p.created_at ? (
              <span title={when(p.created_at)}>opened {ago(p.created_at)}</span>
            ) : null}
            <span className="mono">{p.branch}</span>
          </div>
        </div>
      </div>

      <div className="rv-layout">
        <div className="rv-main">
          {v.status === "stale" && (
            <div className="banner banner-note rv-banner">
              <Icon name="alert" size={14} />
              <span>
                {STALE_TEXT[v.stale_reason ?? ""] ?? "What this proposal reviewed has moved."}{" "}
                Approvals on it are void; the owners review what is there now.
              </span>
              <button
                className="btn"
                disabled={busy !== null}
                onClick={() => {
                  setBusy("repropose");
                  review.repropose(sel, p.branch).then(
                    (next) => {
                      setBusy(null);
                      onFocus({ kind: "review", proposal: next.proposal.id });
                    },
                    (e: unknown) => {
                      setErr(errorOf(e));
                      setBusy(null);
                    },
                  );
                }}
              >
                Review what's there now
              </button>
            </div>
          )}
          {v.status === "rejected" && rejection && (
            <div className="banner banner-error rv-banner">
              <Icon name="close" size={14} />
              <span>
                <Who user={rejection.user} me={me} /> rejected this
                {rejection.at ? ` ${ago(rejection.at)}` : ""}
                {rejection.note ? <>: “{rejection.note}”</> : "."}
              </span>
            </div>
          )}
          {v.status === "published" && (
            <div className="banner rv-banner rv-banner-ok">
              <Icon name="check" size={14} />
              <span>
                Published to main{v.published_at ? ` ${ago(v.published_at)}` : ""}. Agents answer
                from it now.
              </span>
            </div>
          )}

          <div className="h-section">
            Changes <span className="count">{p.changes.length}</span>
          </div>
          <div className="card rv-changes">
            {p.changes.map((c, i) => (
              <ChangeRow key={c.id ?? i} c={c} onFocus={onFocus} />
            ))}
          </div>

          <div className="h-section">
            Evidence <span className="count">{evidence.length}</span>
          </div>
          {evidence.length === 0 ? (
            <div className="card card-pad muted">The rows cite no episode.</div>
          ) : (
            <div className="card">
              {evidence.map((e) =>
                e.readable ? (
                  <button
                    key={e.id}
                    className="list-row"
                    onClick={() => onFocus({ kind: "episode", id: e.id })}
                  >
                    <Icon name="episode" size={16} className="icon-episode" />
                    <div className="list-main">
                      <div className="list-title mono">{e.source ?? e.id}</div>
                      {e.excerpt && <div className="rv-excerpt">“{e.excerpt}”</div>}
                    </div>
                    <div className="list-meta">{e.occurred_at ? ago(e.occurred_at) : ""}</div>
                    <Icon name="chevron" size={14} className="icon-chevron" />
                  </button>
                ) : (
                  <div key={e.id} className="list-row rv-hidden">
                    <Icon name="lock" size={16} />
                    <div className="list-main">
                      <div className="list-title muted">An episode you can't read</div>
                      <div className="list-sub">
                        It backs this change; approving doesn't let you read it.
                      </div>
                    </div>
                  </div>
                ),
              )}
            </div>
          )}
          {withheld > 0 && (
            <p className="muted rv-small">
              {withheld} of the {evidence.length} episodes are withheld from you.
            </p>
          )}
        </div>

        <aside className="rv-rail" aria-label="Review">
          <div className="card card-pad">
            <div className="h-section">Owners</div>
            <ul className="rv-reviewers">
              {p.required_owners.map((o) => {
                const ok = v.approvals.includes(o);
                return (
                  <li key={o}>
                    <span
                      className={`rv-owner ${ok ? "rv-ok" : v.status === "rejected" && rejection?.user === o ? "rv-bad" : ""}`}
                    >
                      <i /> <Who user={o} me={me} />
                    </span>
                    <span className="muted rv-small">
                      {ok
                        ? "approved"
                        : rejection?.user === o
                          ? "rejected"
                          : v.status === "stale"
                            ? "must review again"
                            : byAdmin && v.status === "published"
                              ? "an admin approved"
                              : "waiting"}
                    </span>
                  </li>
                );
              })}
            </ul>

            {canDecide ? (
              <div className="rv-decision">
                <p className="rv-small muted">
                  {asAdmin
                    ? "You aren't an owner of this, but you are an org admin: approving publishes this revision to main for every owner."
                    : last
                      ? "You are the last owner: approving publishes this revision to main."
                      : `Approving records your decision; ${waiting.length - 1} more ${waiting.length - 1 === 1 ? "owner" : "owners"} must approve before it publishes.`}
                </p>
                {!rejecting ? (
                  <div className="rv-buttons">
                    <button
                      className="btn btn-primary"
                      disabled={busy !== null}
                      onClick={() => decide("approve")}
                    >
                      <Icon name="check" size={14} />
                      {busy === "approve"
                        ? "Approving…"
                        : asAdmin
                          ? "Approve as admin"
                          : last
                            ? "Approve and publish"
                            : "Approve"}
                    </button>
                    <button
                      className="btn"
                      disabled={busy !== null}
                      onClick={() => setRejecting(true)}
                    >
                      Reject
                    </button>
                  </div>
                ) : (
                  <form
                    className="rv-reject"
                    onSubmit={(ev) => {
                      ev.preventDefault();
                      decide("reject");
                    }}
                  >
                    <textarea
                      className="field rv-note"
                      autoFocus
                      value={note}
                      onChange={(ev) => setNote(ev.target.value)}
                      placeholder="Why: the proposer reads this"
                      rows={3}
                      maxLength={2000}
                    />
                    <div className="rv-buttons">
                      <button className="btn rv-danger" disabled={busy !== null}>
                        {busy === "reject" ? "Rejecting…" : "Reject"}
                      </button>
                      <button type="button" className="btn" onClick={() => setRejecting(false)}>
                        Cancel
                      </button>
                    </div>
                  </form>
                )}
              </div>
            ) : required && mine && v.status === "pending_review" ? (
              <p className="rv-small muted">
                You approved. Waiting on {waiting.map((o) => o).join(", ")}.
              </p>
            ) : !required &&
              !admin &&
              (v.status === "pending_review" || v.status === "approved") ? (
              <p className="rv-small muted">Only its owners decide this one.</p>
            ) : null}
            {err && (
              <p className="error-text rv-small" role="alert">
                {err}
              </p>
            )}
            <div className="rv-digest">
              <span className="muted rv-small">Revision</span>
              <CopyId id={p.digest} n={12} />
            </div>
          </div>

          {decisions.length > 0 && (
            <div className="card card-pad">
              <div className="h-section">History</div>
              <ol className="rv-hist">
                {[...decisions].reverse().map((d, i) => (
                  <li key={i}>
                    <span className="mono muted">{d.at ? ago(d.at) : ""}</span>
                    <span>
                      <Who user={d.user} me={me} /> {d.approve ? "approved" : "rejected"}
                      {d.admin ? " as admin" : ""}
                      {d.note ? <>: “{d.note}”</> : ""}
                    </span>
                  </li>
                ))}
                {p.created_at && (
                  <li>
                    <span className="mono muted">{ago(p.created_at)}</span>
                    <span>
                      <Author by={p.created_by} me={me} /> proposed
                    </span>
                  </li>
                )}
              </ol>
            </div>
          )}
        </aside>
      </div>
    </section>
  );
}

/** A schema change: the schema main moves from and to, and what that breaks and adds. */
function SchemaChangeRow({ c }: { c: Change }) {
  const breaking = c.breaking ?? [];
  const adds = c.adds ?? [];
  return (
    <div className="rv-change">
      <div className="rv-change-head">
        <span className="badge">schema</span>
        {c.before === null && <span className="badge badge-accent">new</span>}
        {breaking.length > 0 && <span className="badge badge-error">breaking</span>}
      </div>
      <div className="rv-diff">
        {c.before != null && (
          <>
            <span className="rv-before">{String(c.before)}</span>
            <Icon name="arrow" size={14} />
          </>
        )}
        <span className="rv-after">{String(c.value ?? c.name ?? "")}</span>
      </div>
      {breaking.length > 0 && (
        <ul className="rv-schema-list">
          {breaking.map((b) => (
            <li key={b} className="rv-schema-break">
              {b}
            </li>
          ))}
        </ul>
      )}
      {adds.length > 0 && (
        <ul className="rv-schema-list muted">
          {adds.map((a) => (
            <li key={a}>{a}</li>
          ))}
        </ul>
      )}
      {c.before != null && breaking.length === 0 && adds.length === 0 && (
        <div className="rv-change-meta muted">No type or attribute changes.</div>
      )}
    </div>
  );
}

function ChangeRow({ c, onFocus }: { c: Change; onFocus: (f: Focus) => void }) {
  if (c.kind === "schemas") return <SchemaChangeRow c={c} />;
  const unchanged = c.before !== undefined && c.before !== null && c.before === c.value;
  const fresh = c.before === null;
  // A predicate that holds several values: the row adds one beside what main holds, never replaces it.
  const many = c.many === true;
  const n = c.entity ? entityName(c.entity) : null;
  return (
    <div className={`rv-change ${unchanged ? "rv-unchanged" : ""}`}>
      <div className="rv-change-head">
        <span className="badge">{KIND_WORD[c.kind] ?? c.kind}</span>
        {c.entity && (
          <button
            className="link mono"
            onClick={() => onFocus(entityFocus(c.entity!))}
            title={c.entity}
          >
            {c.entity}
          </button>
        )}
        {c.predicate && (
          <span className="rv-pred" title={c.predicate}>
            {humanize(c.predicate)}
          </span>
        )}
        {fresh && <span className="badge badge-accent">{many ? "adds" : "new"}</span>}
        {unchanged && (
          <span className="badge badge-past">{many ? "already held" : "unchanged"}</span>
        )}
        {n?.type && <span className="chip chip-type">{humanize(n.type)}</span>}
      </div>
      <div className="rv-diff">
        {c.before !== undefined && !fresh && !unchanged && (
          <>
            <span className="rv-before">{String(c.before)}</span>
            <Icon name="arrow" size={14} />
          </>
        )}
        <span className="rv-after">
          {c.value == null ? <span className="muted">ended</span> : String(c.value)}
        </span>
        {many && c.held && c.held.length > 0 && (
          <span className="muted rv-held">alongside {c.held.join(", ")}</span>
        )}
      </div>
      {(c.valid_from || (c.evidence && c.evidence.length > 0)) && (
        <div className="rv-change-meta muted">
          {c.valid_from ? <span>from {when(c.valid_from)}</span> : null}
          {c.evidence && c.evidence.length > 0 && (
            <span>
              {c.evidence.length} {c.evidence.length === 1 ? "episode" : "episodes"}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

/* ---------- an entity's owner panel (the Explorer's entity pane) ---------- */

export function EntityOwnership({
  entity,
  sel,
  me,
  admin,
  onFocus,
}: {
  entity: string;
  sel: Selection;
  me?: string;
  admin: boolean;
  onFocus: (f: Focus) => void;
}) {
  const key = JSON.stringify([sel.org, sel.team, entity]);
  const ow = useRead(() => review.owners(sel), [key]);
  const ps = useRead(() => review.proposals(sel, entity), [key]);
  const [err, setErr] = useState<string | null>(null);
  // A memstore without owner review: say nothing on the entity's page.
  if (ow.error && /unknown op|not supported|unexpected op/i.test(ow.error)) return null;
  const owner = ow.data?.find((o) => o.entity === entity) ?? null;
  const open = (ps.data ?? []).filter(
    (p) => p.status === "pending_review" || p.status === "approved" || p.status === "stale",
  );
  const done = (ps.data ?? []).filter((p) => p.status === "published" || p.status === "rejected");
  return (
    <div className="card card-pad rv-entity">
      <div className="rv-entity-head">
        <div className="h-section">
          <Icon name="lock" size={14} /> Owner
        </div>
        <button className="link rv-small" onClick={() => onFocus({ kind: "review" })}>
          Review inbox
        </button>
      </div>
      {!ow.data && !ow.error ? (
        <Skeleton rows={1} title={false} />
      ) : owner ? (
        <p className="rv-owner-line">
          <Who user={owner.user} me={me} />
          {owner.since ? <span className="muted"> since {when(owner.since)}</span> : null}
          <span className="muted">
            {" "}
            · every change to this entity waits for {owner.user === me ? "you" : "them"}
            {open.length ? `; ${open.length} ${open.length === 1 ? "is" : "are"} waiting now` : ""}.
          </span>
          {owner.user === me && (
            <>
              {" "}
              <TransferButton sel={sel} owner={owner} onDone={ow.reload} onError={setErr} />
            </>
          )}
        </p>
      ) : (
        <>
          <p className="muted rv-small">
            No owner: changes publish without review.
            {admin ? " Assign one to protect what is recorded here from now on." : ""}
          </p>
          {admin && <AssignOwner sel={sel} taken={[]} preset={entity} onDone={ow.reload} />}
        </>
      )}
      {err && (
        <p className="error-text rv-small" role="alert">
          {err}
        </p>
      )}
      {open.length > 0 && (
        <>
          <div className="h-section rv-tight">
            Waiting <span className="count">{open.length}</span>
          </div>
          <div className="rv-rows rv-rows-flat">
            {open.map((p) => (
              <ProposalRow key={p.id} p={p} me={me} onFocus={onFocus} />
            ))}
          </div>
        </>
      )}
      {done.length > 0 && (
        <details className="rv-done">
          <summary className="h-section rv-tight">
            <Icon name="chevron" size={12} /> Reviewed <span className="count">{done.length}</span>
          </summary>
          <div className="rv-rows rv-rows-flat">
            {done.map((p) => (
              <ProposalRow key={p.id} p={p} me={me} onFocus={onFocus} />
            ))}
          </div>
        </details>
      )}
    </div>
  );
}
