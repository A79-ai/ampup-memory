// The context and sign-in that `ampup-memory-client join` saved, renewed when it expires.
//
// They live where the CLI keeps them: AMPUP_MEMORY_CONFIG_DIR (or the older MEMSTORE_CONFIG_DIR), else
// ~/.ampup-memory, else ~/.memstore on a machine where the CLI hasn't moved it yet.
import { existsSync, lstatSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const SIGN_IN = "`ampup-memory-client join <server> --org <org>`";

export interface Profile {
  server: string;
  token: string;
  org?: string;
  team?: string;
}

interface Credentials {
  client_id?: string;
  client_secret?: string;
  token_endpoint: string;
  access_token: string;
  id_token?: string;
  refresh_token?: string;
  expires_at?: number;
  api_key?: string;
  [k: string]: unknown;
}

// AMPUP_MEMORY_<name>, else the older MEMSTORE_<name> (the new name wins, as in the CLI).
function env(name: string): string | undefined {
  return process.env[`AMPUP_MEMORY_${name}`] || process.env[`MEMSTORE_${name}`] || undefined;
}

function exists(p: string, follow: boolean): boolean {
  try {
    return follow ? statSync(p).isDirectory() : (lstatSync(p), true);
  } catch {
    return false;
  }
}

export function configDir(): string {
  const d = env("CONFIG_DIR");
  if (d) return d;
  const fresh = join(homedir(), ".ampup-memory");
  const old = join(homedir(), ".memstore");
  return !exists(fresh, false) && exists(old, true) ? old : fresh;
}

function read<T>(name: string): T | undefined {
  const p = join(configDir(), name);
  return existsSync(p) ? (JSON.parse(readFileSync(p, "utf8")) as T) : undefined;
}

function writePrivate(name: string, data: unknown): void {
  mkdirSync(configDir(), { recursive: true });
  const tmp = join(configDir(), `.${name}.${process.pid}.tmp`);
  writeFileSync(tmp, JSON.stringify(data, null, 2), { mode: 0o600 });
  renameSync(tmp, join(configDir(), name));
}

export async function loadProfile(context?: string): Promise<Profile> {
  const config =
    read<{ "current-context"?: string; contexts?: Record<string, Record<string, string>> }>("config.json") ?? {};
  const name = context || env("CONTEXT") || config["current-context"];
  const ctx = name ? config.contexts?.[name] : undefined;
  if (!ctx?.server) {
    throw new Error(`no context ${JSON.stringify(name)} with a server in ${configDir()}: run ${SIGN_IN}`);
  }
  const server = ctx.server.replace(/\/+$/, "");
  const all = read<Record<string, Credentials>>("credentials.json") ?? {};
  const key = ctx.org && all[`${server}#${ctx.org}`] ? `${server}#${ctx.org}` : server;
  if (!all[key]) throw new Error(`not signed in to ${server}: run ${SIGN_IN}`);
  return { server, token: await bearer(key), org: ctx.org, team: ctx.team };
}

// Renewals under way in this process, by sign-in: two calls at once renew once, not with the same refresh token
// twice (a provider that rotates refresh tokens can take that for a stolen one and sign the person out).
const renewing = new Map<string, Promise<string>>();

// The API key, else the ID (or access) token, renewed first if it expires within a minute.
//
// The CLI and the Python SDK renew holding a lock on credentials.lock (flock), so processes renew one at a time.
// Node has no flock without a native module, which this package doesn't take, and a lock file of its own would
// keep out other Node processes alone; so across processes this narrows the race rather than closing it: it decides
// from credentials.json as it reads just before renewing, and a renewal the provider refuses takes the sign-in
// another process renewed meanwhile, if one did, rather than failing.
//
// The token is read once: a client made from a profile (`AmpUpMemory.fromProfile`) that outlives its token is
// refused once the token expires, and is made again to renew it.
async function bearer(key: string): Promise<string> {
  const current = read<Record<string, Credentials>>("credentials.json")?.[key];
  if (!current) throw new Error(`no sign-in ${JSON.stringify(key)} in ${configDir()}: run ${SIGN_IN}`);
  if (current.api_key) return current.api_key;
  if (fresh(current)) return current.id_token || current.access_token;
  let r = renewing.get(key);
  if (!r) {
    r = renew(key).finally(() => renewing.delete(key));
    renewing.set(key, r);
  }
  return r;
}

function fresh(c: Credentials): boolean {
  return (c.expires_at ?? 0) > Date.now() / 1000 + 60;
}

async function renew(key: string): Promise<string> {
  // Read again: another process may have renewed since.
  const c = read<Record<string, Credentials>>("credentials.json")?.[key];
  if (!c) throw new Error(`no sign-in ${JSON.stringify(key)} in ${configDir()}: run ${SIGN_IN}`);
  if (fresh(c)) return c.id_token || c.access_token;
  if (!c.refresh_token) throw new Error(`signed-in token expired: run ${SIGN_IN}`);
  const form = new URLSearchParams({
    grant_type: "refresh_token",
    refresh_token: c.refresh_token,
    client_id: c.client_id ?? "",
  });
  if (c.client_secret) form.set("client_secret", c.client_secret);
  const r = await fetch(c.token_endpoint, { method: "POST", body: form, headers: { accept: "application/json" } });
  if (!r.ok) {
    // Its refresh token used by another process that renewed first: that process's sign-in is the one to use.
    const theirs = read<Record<string, Credentials>>("credentials.json")?.[key];
    if (theirs && fresh(theirs)) return theirs.id_token || theirs.access_token;
    throw new Error(`renewing the sign-in failed (${r.status}): run ${SIGN_IN}`);
  }
  const t = (await r.json()) as { access_token: string; id_token?: string; refresh_token?: string; expires_in?: number };
  c.access_token = t.access_token;
  if (t.id_token) c.id_token = t.id_token;
  if (t.refresh_token) c.refresh_token = t.refresh_token;
  c.expires_at = jwtExp(c.id_token) ?? Math.floor(Date.now() / 1000) + (t.expires_in ?? 3600);
  const all = read<Record<string, Credentials>>("credentials.json") ?? {};
  all[key] = c;
  writePrivate("credentials.json", all);
  return c.id_token || c.access_token;
}

function jwtExp(jwt?: string): number | undefined {
  try {
    const exp = JSON.parse(Buffer.from(jwt!.split(".")[1], "base64url").toString()).exp;
    return typeof exp === "number" ? exp : undefined;
  } catch {
    return undefined;
  }
}
