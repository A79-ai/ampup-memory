// A thin client for an AmpUp Memory server's HTTP API.
import { loadProfile } from "./credentials.js";

export { loadProfile, type Profile } from "./credentials.js";

export const INGEST_BATCH = 500;

export interface Episode {
  source: string;
  body: string;
  occurred_at: number; // microseconds since the epoch, see micros()
  acl: string[]; // at least one reader, never "*"
  about?: string[];
  index_text?: boolean;
  document?: string;
}

export interface Hit {
  entity: string;
  predicate: string;
  value: unknown;
  valid_from?: string | null;
  valid_to?: string | null;
  fact_id: string;
  source_episodes: string[];
  quote?: string | null;
  score: number;
  written_by?: string | null;
  [k: string]: unknown;
}

export type Json = Record<string, any>;

/** What the server served: the situation it matched and its advice, both null when it has none yet. */
export interface Advice {
  situation: string | null;
  advice: string | null;
  evidence?: unknown; // how the advice has fared, when it was learned from runs
  skills?: unknown[];
  own_situation?: string; // your own note, served beside the team's
  advice_pending?: string; // still being found: ask again at the next step
  session_id?: string; // on a session's opening
  [k: string]: unknown;
}

/** Where a session is when it asks: moving to the next step, choosing an approach, stuck, or done. */
export type Moment = "next" | "fork" | "stuck" | "done";

export class AmpUpMemoryError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly retryAfter?: number,
  ) {
    super(`HTTP ${status}: ${message}`);
    this.name = "AmpUpMemoryError";
  }
}

/** `occurred_at` for an episode: microseconds since the epoch. */
export function micros(d: Date): number {
  return d.getTime() * 1000;
}

export interface Options {
  org?: string;
  team?: string;
  timeoutMs?: number;
}

export class AmpUpMemory {
  readonly server: string;
  private headers: Record<string, string> = { "content-type": "application/json" };
  private timeoutMs: number;

  constructor(server: string, token: string | null, opts: Options = {}) {
    this.server = server.replace(/\/+$/, "");
    if (token) this.headers.authorization = `Bearer ${token}`;
    // The wire names keep the engine's original prefix: headers are an interface, not what a person types.
    if (opts.org) this.headers["x-memstore-org"] = opts.org;
    if (opts.team) {
      if (!opts.org) throw new Error("team needs an org");
      this.headers["x-memstore-team"] = opts.team;
    }
    this.timeoutMs = opts.timeoutMs ?? 60_000;
  }

  /** The server, org, team and sign-in that `ampup-memory-client join` saved for a context. */
  static async fromProfile(context?: string, opts: { timeoutMs?: number } = {}): Promise<AmpUpMemory> {
    const p = await loadProfile(context);
    return new AmpUpMemory(p.server, p.token, { org: p.org, team: p.team, ...opts });
  }

  private async call(method: "GET" | "POST", path: string, body?: unknown): Promise<any> {
    const r = await fetch(this.server + path, {
      method,
      headers: this.headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(this.timeoutMs),
    });
    const text = await r.text();
    let data: any;
    try {
      data = JSON.parse(text);
    } catch {
      data = undefined;
    }
    if (!r.ok) {
      const retry = data?.retry_after_secs ?? r.headers.get("retry-after");
      throw new AmpUpMemoryError(r.status, data?.error ?? (text || r.statusText), retry ? Number(retry) : undefined);
    }
    return data;
  }

  /** The row protocol: send one row `[0, ...args]`, get back that row's value. */
  private async row(path: string, ...args: unknown[]): Promise<any> {
    const out = (await this.call("POST", path, { data: [[0, ...args]] })).data[0][1];
    if (out && typeof out === "object" && !Array.isArray(out) && Object.keys(out).length === 1 && "error" in out) {
      throw new AmpUpMemoryError(200, out.error);
    }
    return out;
  }

  /** Who you are: user, groups, scope and memberships. */
  me(): Promise<Json> {
    return this.call("GET", "/me");
  }

  /** Commit episodes in batches of 500; the ids of all of them. Needs an ingest or admin key. */
  async ingest(episodes: Episode[]): Promise<string[]> {
    const ids: string[] = [];
    for (let i = 0; i < episodes.length; i += INGEST_BATCH) {
      ids.push(...(await this.call("POST", "/ingest", { episodes: episodes.slice(i, i + INGEST_BATCH) })).episode_ids);
    }
    return ids;
  }

  /** Add a note: `{episode_id, scope, readers}`, or `{queued, message}` when the server queues it. */
  remember(source: string, body: string, opts: { acl?: string[]; document?: string } = {}): Promise<Json> {
    return this.call("POST", "/remember", { source, body, ...opts });
  }

  /** Forget an episode you wrote; its id. */
  async forget(episodeId: string): Promise<string> {
    return (await this.call("POST", "/forget", { episode_id: episodeId })).forgotten;
  }

  /** The facts that best match `query`, with the episodes they came from. */
  search(query: string, k = 10): Promise<Hit[]> {
    return this.row("/search", query, k);
  }

  /** A query-language request, e.g. {find: "passages", text: "renewal"}. */
  query(q: Json): Promise<Json> {
    return this.row("/query", q);
  }

  /** An episode's text, or null if it doesn't exist or you can't read it. */
  episode(episodeId: string): Promise<string | null> {
    return this.row("/episode", episodeId);
  }

  // The learning loop: a session per task, advice as it goes, and how it went at the end. Advice is learned from
  // sessions that ended with a reward.

  /**
   * Start a session for `task`; its opening advice is `session.opening`. `harness` names what runs the session
   * (your app, say). A name ending in ":sdk" marks the session a program's: advice isn't learned from it, nor
   * served to it unless the org turns that on.
   */
  async session(opts: { task: string; harness?: string }): Promise<Session> {
    const opening: Advice = await this.call("POST", "/sessions", { context: opts.task, harness: opts.harness });
    return new Session(this, opening.session_id as string, opts.task, opening);
  }

  /** Advice for `question` outside any session. Needs a server with a routing model (503 without one). */
  advice(question: string): Promise<Advice> {
    return this.call("POST", "/advice", { question });
  }

  /**
   * Reward a session or run you recorded, from -1 to 1, after it ended (say, once its change merged). An ended
   * session can be rewarded once the server has written it, within a minute of its end by default (404 before
   * then). To reward it as it ends, pass `reward` to `session.end`.
   */
  async reward(runId: string, reward: number, reason?: string): Promise<string> {
    return (await this.call("POST", "/runs/reward", { run_id: runId, reward, reason })).run_id;
  }
}

/** One task's session: from `AmpUpMemory.session`. */
export class Session {
  constructor(
    private readonly memory: AmpUpMemory,
    readonly id: string,
    readonly task: string,
    readonly opening: Advice,
  ) {}

  /**
   * Advice for this session, recorded in it. `question` is the task (the default) or the step at hand; `moment`
   * says where you are; `conversation` is a condensed account of the session so far.
   */
  advice(opts: { question?: string; moment?: Moment; conversation?: string } = {}): Promise<Advice> {
    return this.memory["call"]("POST", "/advice", {
      session_id: this.id,
      question: opts.question || this.task,
      moment: opts.moment,
      conversation_so_far: opts.conversation,
    });
  }

  /**
   * End the session: what came of it, and a reward from -1 (failed) to 1 (succeeded) if you know it now (else
   * reward it later with `memory.reward(session.id, ...)`). `transcript` is what happened.
   */
  end(opts: { outcome?: string; reward?: number; transcript?: string; reason?: string } = {}): Promise<Json> {
    return this.memory["call"]("POST", "/sessions/end", { session_id: this.id, task: this.task, ...opts });
  }
}
