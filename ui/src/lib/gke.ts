import "server-only";

import { execFile, spawn, type ChildProcess } from "node:child_process";
import { promisify } from "node:util";

// The memstores of a Kubernetes cluster: one per `memstore-*` namespace, each serving on its pod's
// own 127.0.0.1 with no sign-in, so the only way in is a port-forward. This runs `kubectl` with the
// developer's own credentials: they see the namespaces their cluster access allows, nothing more.
//
//   MEMSTORE_KUBE_CONTEXT  the kubectl context to use (default: kubectl's current context)
// Inside the cluster (the gateway) kubectl uses the pod's service account instead of a context.
const CONTEXT = process.env.KUBERNETES_SERVICE_HOST ? null : (process.env.MEMSTORE_KUBE_CONTEXT ?? null);
const ctx = (): string[] => (CONTEXT ? ["--context", CONTEXT] : []);
const PREFIX = "memstore-";
const run = promisify(execFile);

async function kubectl(args: string[]): Promise<string> {
  const { stdout } = await run("kubectl", [...ctx(), ...args], {
    timeout: 30_000,
    maxBuffer: 16 << 20,
  });
  return stdout;
}

export type Namespace = { namespace: string; serving: boolean };

/** Every `memstore-*` namespace, and whether its memstore-serve has a ready pod. */
export async function namespaces(): Promise<Namespace[]> {
  const [ns, deploys] = await Promise.all([
    kubectl(["get", "namespaces", "-o", "jsonpath={.items[*].metadata.name}"]),
    kubectl([
      "get",
      "deployments",
      "--all-namespaces",
      "--field-selector",
      "metadata.name=memstore-serve",
      "-o",
      "json",
    ]),
  ]);
  const ready = new Set<string>(
    JSON.parse(deploys)
      .items.filter((d: { status?: { readyReplicas?: number } }) => (d.status?.readyReplicas ?? 0) > 0)
      .map((d: { metadata: { namespace: string } }) => d.metadata.namespace),
  );
  return ns
    .split(/\s+/)
    .filter((n) => n.startsWith(PREFIX))
    .sort()
    .map((namespace) => ({ namespace, serving: ready.has(namespace) }));
}

/** A namespace name we would pass to kubectl: a `memstore-` DNS label. */
export function validNamespace(ns: unknown): ns is string {
  return typeof ns === "string" && ns.startsWith(PREFIX) && /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/.test(ns);
}

type Forward = { port: Promise<number>; proc: ChildProcess };

// One port-forward per namespace, kept for as long as it lives. On globalThis so the dev server's module
// reloads don't leak them.
const g = globalThis as unknown as { memstoreForwards?: Map<string, Forward> };
const forwards = (g.memstoreForwards ??= new Map());

/** The local base URL of a namespace's memstore-serve, starting a port-forward to it if none is open. */
export function baseUrl(ns: string): Promise<string> {
  let f = forwards.get(ns);
  if (!f) {
    const proc = spawn(
      "kubectl",
      [...ctx(), "-n", ns, "port-forward", "deployment/memstore-serve", "0:8080"],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    const port = new Promise<number>((resolve, reject) => {
      let err = "";
      const timer = setTimeout(() => reject(new Error(`port-forward to ${ns} timed out`)), 20_000);
      proc.stdout!.on("data", (b: Buffer) => {
        const m = /Forwarding from 127\.0\.0\.1:(\d+)/.exec(b.toString());
        if (m) {
          clearTimeout(timer);
          resolve(Number(m[1]));
        }
      });
      proc.stderr!.on("data", (b: Buffer) => (err += b.toString()));
      proc.on("exit", () => {
        clearTimeout(timer);
        forwards.delete(ns);
        reject(new Error(err.trim().split("\n").at(-1) || `port-forward to ${ns} ended`));
      });
    });
    port.catch(() => forwards.delete(ns));
    f = { port, proc };
    forwards.set(ns, f);
  }
  return f.port.then((p: number) => `http://127.0.0.1:${p}`);
}

/** Drops a namespace's port-forward (its pod went away): the next call opens a new one. */
export function forget(ns: string) {
  forwards.get(ns)?.proc.kill();
  forwards.delete(ns);
}
