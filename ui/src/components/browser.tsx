"use client";

import { signIn, signOut, useSession } from "next-auth/react";
import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { browse, me as loadMe, namespaces as loadNamespaces } from "./api";
import { Inspector } from "./inspector";
import { Schema } from "./schema";
import { Tools } from "./tools";
import { Tree } from "./tree";
import type { Branch, Focus, Me, Namespace, Selection } from "./types";

// The focus lives in the address bar's hash, so a view can be linked to and survives a reload:
//   #/f/sources/slack/C09   a folder      #/e/entities/deal/42   an entity
//   #/x/<episode id>        an episode    #/q/<text>             a search
//   #/s/skills/<slug>       a compiled skill (or the index)
//   #/t/<tool>              the Tools tab, a tool picked
//   #/c                     the Schema tab
// Each segment is URI-encoded, so one may hold `/`. `?ns=` picks the memstore (a `memstore-*` namespace on
// Kubernetes), `?org=&team=` (or `?scope=`) the memory within it and `?branch=` the branch read (main by default).
function toHash(f: Focus | null): string {
  if (!f) return "";
  const enc = (p: string[]) => p.map(encodeURIComponent).join("/");
  switch (f.kind) {
    case "folder":
      return `#/f/${enc(f.path)}`;
    case "entity":
      return `#/e/${enc(f.path)}`;
    case "skill":
      return `#/s/${enc(f.path)}`;
    case "episode":
      return `#/x/${encodeURIComponent(f.id)}`;
    case "search":
      return `#/q/${encodeURIComponent(f.query)}`;
    case "tools":
      return `#/t/${encodeURIComponent(f.tool ?? "")}`;
    case "schema":
      return "#/c/";
  }
}

function fromHash(h: string): Focus | null {
  const m = /^#\/([fesxqtc])\/(.*)$/.exec(h);
  if (!m) return null;
  const segs = m[2] ? m[2].split("/").map(decodeURIComponent) : [];
  switch (m[1]) {
    case "f":
      return { kind: "folder", path: segs };
    case "e":
      return { kind: "entity", path: segs };
    case "s":
      return { kind: "skill", path: segs };
    case "x":
      return { kind: "episode", id: decodeURIComponent(m[2]) };
    case "t":
      return { kind: "tools", tool: decodeURIComponent(m[2]) || undefined };
    case "c":
      return { kind: "schema" };
    default:
      return { kind: "search", query: decodeURIComponent(m[2]) };
  }
}

// The URL is the state: subscribe to it, and change it to move.
const NAVIGATE = "memstore:navigate";
function subscribe(on: () => void) {
  window.addEventListener("hashchange", on);
  window.addEventListener("popstate", on);
  window.addEventListener(NAVIGATE, on);
  return () => {
    window.removeEventListener("hashchange", on);
    window.removeEventListener("popstate", on);
    window.removeEventListener(NAVIGATE, on);
  };
}
function navigate(url: string) {
  history.pushState(null, "", url);
  window.dispatchEvent(new Event(NAVIGATE));
}

export function Browser() {
  const hash = useSyncExternalStore(subscribe, () => window.location.hash, () => "");
  const search = useSyncExternalStore(subscribe, () => window.location.search, () => "");
  const focus = useMemo(() => fromHash(hash), [hash]);
  const sel = useMemo<Selection>(() => {
    const q = new URLSearchParams(search);
    return {
      ns: q.get("ns") ?? undefined,
      scope: q.get("scope") ?? undefined,
      org: q.get("org") ?? undefined,
      team: q.get("team") ?? undefined,
      branch: q.get("branch") ?? undefined,
    };
  }, [search]);
  const [scopes, setScopes] = useState<{ ns: string; list: string[] } | null>(null);
  const [who, setWho] = useState<{ sel: string; me?: Me; error?: string } | null>(null);
  const [nss, setNss] = useState<{ list?: Namespace[]; hosted?: boolean; error?: string } | null>(null);
  const { data: session, status: signedIn } = useSession();
  // Hosted, nothing is read until someone signs in.
  const waiting = !!nss?.hosted && signedIn !== "authenticated";
  const [query, setQuery] = useState("");
  // The scope's branches with their skills (null: a memstore that can't name them), for the memory selected.
  const [branches, setBranches] = useState<{ key: string; list: Branch[] | null } | null>(null);
  const memoryKey = JSON.stringify([sel.ns, sel.scope, sel.org, sel.team]);

  // The memstore- namespaces on Kubernetes; with none picked, the first one serving.
  useEffect(() => {
    loadNamespaces<{ namespaces: Namespace[]; hosted: boolean; error?: string }>().then(
      (r) => {
        setNss({ list: r.namespaces, hosted: r.hosted, error: r.error });
        const first = r.namespaces.find((n) => n.serving);
        if (!new URLSearchParams(window.location.search).get("ns") && first) {
          navigate(`${window.location.pathname}?ns=${first.namespace}`);
        }
      },
      (e) => setNss({ error: e.message }),
    );
  }, []);

  // The scopes in the picked namespace's catalog.
  useEffect(() => {
    const ns = sel.ns;
    if (!ns || waiting) return;
    browse<{ scopes: string[] }>({ op: "scopes" }, { ns }).then(
      (r) => setScopes({ ns, list: r.scopes }),
      () => setScopes({ ns, list: [] }),
    );
  }, [sel.ns, waiting]);

  useEffect(() => {
    if (!sel.ns || waiting) return;
    let live = true;
    const { ns, scope, org, team } = sel;
    browse<{ branches: Branch[] | null }>({ op: "branches" }, { ns, scope, org, team }).then(
      (r) => live && setBranches({ key: memoryKey, list: r.branches }),
      () => live && setBranches({ key: memoryKey, list: null }),
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [memoryKey, waiting]);

  useEffect(() => {
    if (!sel.ns || waiting) return;
    let live = true;
    loadMe<Me>(sel).then(
      (me) => live && setWho({ sel: search, me }),
      (e) => live && setWho({ sel: search, error: e.message }),
    );
    return () => {
      live = false;
    };
  }, [sel, search, waiting]);

  const go = useCallback((f: Focus) => {
    const h = toHash(f);
    if (h !== window.location.hash) navigate(`${window.location.pathname}${window.location.search}${h}`);
  }, []);

  const choose = (s: Selection) => {
    const q = new URLSearchParams();
    if (s.ns) q.set("ns", s.ns);
    if (s.scope) q.set("scope", s.scope);
    if (s.org) q.set("org", s.org);
    if (s.team) q.set("team", s.team);
    if (s.branch && s.branch !== "main") q.set("branch", s.branch);
    navigate(`${window.location.pathname}${q.size ? `?${q}` : ""}`);
  };

  const current = who?.sel === search ? who : null;
  const whoError = current?.error ?? null;
  const me = current?.me;
  const orgs = useMemo(() => {
    const m = (me?.memberships ?? {}) as Record<string, { teams?: string[] }>;
    return Object.entries(m).map(([org, a]) => ({ org, teams: a?.teams ?? [] }));
  }, [me]);
  const scope = me?.scope ?? null;
  const reveal =
    focus && (focus.kind === "folder" || focus.kind === "entity" || focus.kind === "skill") ? focus.path : null;
  const branchList = branches?.key === memoryKey ? branches.list : undefined;
  const branch = sel.branch ?? "main";
  // A build often compiles skills on a branch of its own: say where, when there are none on the one read.
  const elsewhere =
    branchList && !branchList.some((b) => b.name === branch && b.skills > 0)
      ? branchList.filter((b) => b.name !== branch && b.skills > 0)
      : [];
  const pickBranch = (b: string) =>
    choose({ ns: sel.ns, scope: sel.scope, org: sel.org, team: sel.team, branch: b || undefined });

  return (
    <div className="app">
      <header className="bar">
        <div className="brand">
          <span className="logo" aria-hidden>
            ◆
          </span>
          memstore
        </div>
        {nss?.hosted && (nss.list?.length ?? 0) <= 1 ? (
          <div className="scope mono" title="The memstore this UI reads">
            {nss.list?.[0]?.namespace}
          </div>
        ) : (
        <select
          className="picker mono"
          aria-label="Namespace"
          value={sel.ns ?? ""}
          onChange={(e) => choose({ ns: e.target.value || undefined })}
        >
          <option value="" disabled>
            {nss?.list ? "pick a namespace" : "loading namespaces…"}
          </option>
          {nss?.list?.map((n) => (
            <option key={n.namespace} value={n.namespace}>
              {n.namespace}
              {n.serving ? "" : " (not serving)"}
            </option>
          ))}
        </select>
        )}
        {scopes && scopes.ns === sel.ns && scopes.list.length > 1 ? (
          <select
            className="picker mono"
            aria-label="Scope"
            title="The memory you are reading: each scope is its own episodes and facts"
            value={scope ?? ""}
            onChange={(e) => choose({ ns: sel.ns, scope: e.target.value, branch: sel.branch })}
          >
            {scope && !scopes.list.includes(scope) && <option value={scope}>{scope}</option>}
            {scopes.list.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        ) : (
          <div className="scope mono" title="The memory you are reading">
            memstore://{scope ?? "…"}
          </div>
        )}
        {orgs.length > 0 && (
          <select
            className="picker"
            aria-label="Org and team"
            value={JSON.stringify({ org: sel.org ?? "", team: sel.team ?? "" })}
            onChange={(e) => {
              const v = JSON.parse(e.target.value);
              choose({
                ns: sel.ns,
                scope: sel.scope,
                org: v.org || undefined,
                team: v.team || undefined,
                branch: sel.branch,
              });
            }}
          >
            <option value={JSON.stringify({ org: "", team: "" })}>default selection</option>
            {orgs.flatMap(({ org, teams }) => [
              <option key={org} value={JSON.stringify({ org, team: "" })}>
                {org}
              </option>,
              ...teams.map((t) => (
                <option key={`${org}/${t}`} value={JSON.stringify({ org, team: t })}>
                  {org} / {t}
                </option>
              )),
            ])}
          </select>
        )}
        {branchList === undefined ? null : branchList ? (
          <select
            className="picker mono"
            aria-label="Branch"
            title="The branch you are reading: a build may compile skills on a branch of its own"
            value={branch}
            onChange={(e) => pickBranch(e.target.value)}
          >
            {!branchList.some((b) => b.name === branch) && <option value={branch}>{branch}</option>}
            {branchList.map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}
                {b.skills ? ` (${b.skills} skills)` : ""}
              </option>
            ))}
          </select>
        ) : (
          <BranchInput key={branch} branch={branch} onPick={pickBranch} />
        )}
        <nav className="tabs" aria-label="View">
          <button
            className={focus?.kind !== "tools" && focus?.kind !== "schema" ? "tab tab-on" : "tab"}
            onClick={() => go({ kind: "folder", path: [] })}
          >
            Browse
          </button>
          <button className={focus?.kind === "schema" ? "tab tab-on" : "tab"} onClick={() => go({ kind: "schema" })}>
            Schema
          </button>
          <button className={focus?.kind === "tools" ? "tab tab-on" : "tab"} onClick={() => go({ kind: "tools" })}>
            Tools
          </button>
        </nav>
        <form
          className="search"
          onSubmit={(e) => {
            e.preventDefault();
            if (query.trim()) go({ kind: "search", query: query.trim() });
          }}
        >
          <input
            type="search"
            placeholder="Search facts…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-label="Search facts"
          />
        </form>
        <div className="who muted" title={me?.groups?.join(", ")}>
          {me?.user ?? session?.user?.email ?? ""}
        </div>
        {nss?.hosted && signedIn === "authenticated" && (
          <button className="link" onClick={() => signOut()}>
            sign out
          </button>
        )}
      </header>
      {(nss?.error || whoError || me?.refused) && (
        <div className="banner error">{nss?.error ?? whoError ?? me?.refused}</div>
      )}
      {!waiting && sel.ns && elsewhere.length > 0 && (
        <div className="banner banner-note">
          No compiled skills on <span className="mono">{branch}</span>; they are on{" "}
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
      {waiting ? (
        <SignIn loading={signedIn === "loading"} />
      ) : !sel.ns ? (
        <Namespaces list={nss?.list} onPick={(ns) => choose({ ns })} />
      ) : focus?.kind === "schema" ? (
        <Schema key={search} sel={sel} />
      ) : focus?.kind === "tools" ? (
        <main className="split">
          <Tools key={search} sel={sel} tool={focus.tool} onFocus={go} />
        </main>
      ) : (
      <main className="split">
        <aside className="side">
          <Tree key={search} sel={sel} focus={focus} onFocus={go} reveal={reveal} />
        </aside>
        <section className="pane">
          <div className="crumbs mono">
            <button className="link" onClick={() => go({ kind: "folder", path: [] })}>
              memstore://{scope ?? "…"}
            </button>
            {reveal?.map((seg, i) => (
              <span key={i}>
                <span className="muted">/</span>
                <button
                  className="link"
                  onClick={() =>
                    go(
                      (focus?.kind === "entity" || focus?.kind === "skill") && i === reveal.length - 1
                        ? focus
                        : { kind: "folder", path: reveal.slice(0, i + 1) },
                    )
                  }
                >
                  {seg || "∅"}
                </button>
              </span>
            ))}
          </div>
          <Inspector focus={focus} sel={sel} onFocus={go} />
        </section>
      </main>
      )}
    </div>
  );
}

/** The branch to read, by name, for a memstore that can't list its branches. */
function BranchInput({ branch, onPick }: { branch: string; onPick: (b: string) => void }) {
  const [v, setV] = useState(branch);
  return (
    <form
      className="branch"
      onSubmit={(e) => {
        e.preventDefault();
        onPick(v.trim());
      }}
    >
      <input
        className="picker mono"
        aria-label="Branch"
        title="The branch you are reading (this memstore can't list them): a build may compile skills on a branch of its own"
        placeholder="branch: main"
        value={v}
        onChange={(e) => setV(e.target.value)}
        onBlur={() => v.trim() !== branch && onPick(v.trim())}
      />
    </form>
  );
}

/** Hosted: who may read is memstore's to decide, so everyone signs in with Google first. */
function SignIn({ loading }: { loading: boolean }) {
  return (
    <div className="signin">
      <h2 className="title">Sign in to read memory</h2>
      <p className="muted">
        memstore shows you what your Google account may read: your orgs and teams, and the episodes shared with
        you.
      </p>
      <button className="run" disabled={loading} onClick={() => signIn("google")}>
        {loading ? "…" : "Sign in with Google"}
      </button>
    </div>
  );
}

/** Every memstore on Kubernetes, before one is picked. */
function Namespaces({ list, onPick }: { list?: Namespace[]; onPick: (ns: string) => void }) {
  if (!list) return <div className="pad muted">Looking for memstore- namespaces on Kubernetes…</div>;
  if (!list.length) return <div className="pad muted">No memstore- namespaces on the cluster.</div>;
  return (
    <div className="pad">
      <h2 className="title">Memstores on Kubernetes</h2>
      <p className="muted">
        One per <span className="mono">memstore-*</span> namespace. A namespace whose memstore-serve isn&apos;t
        running has nothing to show until it is (trigger it in its Tilt).
      </p>
      <table className="tbl">
        <tbody>
          {list.map((n) => (
            <tr key={n.namespace} className="clickable" onClick={() => onPick(n.namespace)}>
              <td className="mono">{n.namespace}</td>
              <td className={n.serving ? "current" : "muted"}>{n.serving ? "serving" : "not serving"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
