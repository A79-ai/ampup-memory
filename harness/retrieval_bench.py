#!/usr/bin/env python3
"""Retained local indexes and paired HTTP retrieval timings; no generation or paid APIs.

The DolphinBench-derived questions have one known supporting episode each. Metrics
measure known-source retrieval, not exhaustive relevance or official DolphinBench scores.
"""

import argparse
import collections
import datetime as dt
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import platform
import random
import socket
import statistics
import subprocess
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + "\n")


def jsonl(path):
    with Path(path).open() as f:
        return [json.loads(line) for line in f if line.strip()]


def write_jsonl(path, rows):
    with Path(path).open("w") as f:
        for row in rows:
            f.write(json.dumps(row, sort_keys=True) + "\n")


def prepare(args):
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    if (root / "fixture.json").exists():
        raise SystemExit("Fixture already exists; reuse it with run, or choose a new root.")
    cases = json.loads(args.cases.read_text())
    originals = [e for e in jsonl(args.corpus / "episodes.jsonl")
                 if e["source"].startswith("note:dolphinbench/morgan/")]
    by_session = {e["source"].rsplit("/", 1)[1]: e for e in originals}
    required = {by_session[c["session"]]["episode_id"] for c in cases}
    ranked = sorted(originals, key=lambda e: hashlib.sha256(
        f"{args.seed}:{e['episode_id']}".encode()).hexdigest())
    if args.episodes < len(required) or args.distractors < 0:
        raise SystemExit("Sample must hold every case; distractor count must be non-negative.")
    selected = required | {e["episode_id"] for e in
                           [e for e in ranked if e["episode_id"] not in required]
                           [:max(0, args.episodes - len(required))]}
    episodes = [dict(e, scope="org") for e in originals if e["episode_id"] in selected]
    facts = [dict(f, scope="org") for f in jsonl(args.corpus / "facts.jsonl")
             if f["source_episodes"] and set(f["source_episodes"]) <= selected]
    fact_ids = {f["fact_id"] for f in facts}
    for f in facts:
        if f.get("supersedes") not in fact_ids:
            f["supersedes"] = None
    queries = []
    for c in cases:
        ep = by_session[c["session"]]
        day = dt.datetime.fromtimestamp(ep["occurred_at"] / 1e6, dt.timezone.utc)
        for variant in ["broad", "doc", "doc_date"]:
            arguments = {"query": c["query"], "format": "json", "k": 10}
            if variant != "broad":
                arguments["episode_type"] = "doc"
            if variant == "doc_date":
                arguments["start_date"] = (day - dt.timedelta(days=7)).date().isoformat()
                arguments["end_date"] = (day + dt.timedelta(days=7)).date().isoformat()
            queries.append({"id": f"{c['id']}/{variant}", "variant": variant,
                            "arguments": arguments, "gold_episode_ids": [ep["episode_id"]],
                            "gold_source": ep["source"], "supporting_text": ep["body"]})
    # Explicit stress fixture: exact query wording but no answer, in another episode type.
    # Repeated content has distinct source IDs; identical fact text reuses its embedding.
    noise_episodes, noise_facts = [], []
    for c in cases:
        at = by_session[c["session"]]["occurred_at"]
        for i in range(args.distractors):
            eid = hashlib.sha256(f"retrieval-bench-v1:{c['id']}:{i}".encode()).hexdigest()
            noise_episodes.append({"episode_id": eid, "scope": "org", "acl": ["*"],
                                   "source": f"email:retrieval-bench/{c['id']}/{i}",
                                   "occurred_at": at, "body": c["query"]})
            noise_facts.append({"fact_id": f"noise-{c['id']}-{i}", "scope": "org",
                                "entity": f"topic:{c['id']}", "predicate": "question",
                                "value": c["query"], "valid_from": at, "valid_to": None,
                                "source_episodes": [eid], "supersedes": None})
    fixtures = {}
    for name, es, fs in [("dolphin", episodes, facts),
                         ("mixed", episodes + noise_episodes, facts + noise_facts)]:
        directory = root / "corpora" / name
        directory.mkdir(parents=True)
        write_jsonl(directory / "episodes.jsonl", es)
        write_jsonl(directory / "facts.jsonl", fs)
        fixtures[name] = {"episodes": len(es), "facts": len(fs),
                          "synthetic_email_episodes": len(es) - len(episodes),
                          "episodes_sha256": digest(directory / "episodes.jsonl"),
                          "facts_sha256": digest(directory / "facts.jsonl")}
    write_json(root / "queries.json", queries)
    manifest = {"version": 1, "seed": args.seed, "persona": "morgan",
                "sample_episodes": len(episodes), "distractors_per_question": args.distractors,
                "source_corpus": str(args.corpus.resolve()),
                "source_episodes_sha256": digest(args.corpus / "episodes.jsonl"),
                "source_facts_sha256": digest(args.corpus / "facts.jsonl"),
                "cases_sha256": digest(args.cases), "queries_sha256": digest(root / "queries.json"),
                "fixtures": fixtures,
                "relevance_boundary": "One manually curated known supporting source per question; "
                "qrels are incomplete. Not an official DolphinBench evaluation. Mixed adds artificial "
                "email distractors; Dolphin content and facts are unchanged except the local scope."}
    write_json(root / "fixture.json", manifest)
    print(json.dumps(manifest, indent=2), flush=True)


def environment(root, store):
    # Carry executable/library paths, but no inherited model or cloud configuration.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("MEMSTORE_", "OPENROUTER_", "OPENAI_", "ANTHROPIC_", "SENTRY_"))}
    env.update(MEMSTORE_EMBED="local", MEMSTORE_INDEX_URI=str(store / "index"),
               MEMSTORE_EMBED_CACHE=str(root / "embeddings"),
               MEMSTORE_CONFIG_DIR=str(root / "config"), MEMSTORE_EXTRACT="off",
               MEMSTORE_COMPILE="off", MEMSTORE_ADVICE_LEARN="off", MEMSTORE_DREAM="off",
               MEMSTORE_MAINTAIN_SECS="0", MEMSTORE_KEEP_WARM_SECS="0",
               MEMSTORE_CATALOG_REFRESH_SECS="86400", LANCE_CPU_THREADS="2")
    return env


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, binary, store, root, log):
        self.port = free_port()
        env = environment(root, store)
        env["MEMSTORE_METRICS_PORT"] = str(free_port())
        self.log = log.open("w")
        self.process = subprocess.Popen([str(binary), "serve", "--local", str(store),
                                         "org", str(self.port)], env=env,
                                        stdout=self.log, stderr=self.log)
        self.http = None
        try:
            for _ in range(1200):
                if self.process.poll() is not None:
                    raise RuntimeError(f"Server exited; see {log}")
                try:
                    h = http.client.HTTPConnection("127.0.0.1", self.port, timeout=1)
                    h.request("GET", "/")
                    h.getresponse().read()
                    h.close()
                    self.http = http.client.HTTPConnection("127.0.0.1", self.port, timeout=120)
                    return
                except OSError:
                    time.sleep(0.1)
            raise RuntimeError(f"Server not ready; see {log}")
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.http:
            self.http.close()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()

    def query(self, arguments):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                           "params": {"name": "search_memory", "arguments": arguments}})
        started = time.perf_counter()
        self.http.request("POST", "/mcp", body, {"Content-Type": "application/json",
                                                "Accept": "application/json, text/event-stream"})
        response = self.http.getresponse()
        raw = response.read()
        elapsed = (time.perf_counter() - started) * 1000
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}: {raw[:500]!r}")
        outer = json.loads(raw)
        if "error" in outer or outer.get("result", {}).get("isError"):
            raise RuntimeError(json.dumps(outer)[:1000])
        text = "".join(b.get("text", "") for b in outer["result"]["content"])
        return elapsed, json.loads(text)


def source_metrics(payload, gold):
    hits = payload if isinstance(payload, list) else payload.get("hits", [])
    passages = [] if isinstance(payload, list) else payload.get("passages", [])
    sources = []

    def add(eid):
        if eid not in sources:
            sources.append(eid)

    for hit in hits:
        for row in [hit] + hit.get("facts", []):
            for eid in row.get("source_episodes", []):
                add(eid)
    fact_sources = list(sources)
    for p in passages:
        add(p["episode_id"])
    # The endpoint returns ranked facts and supplemental passages, not one unified ranking.
    # Rank metrics therefore use the first ten distinct sources in returned fact order.
    ranks = [i + 1 for i, eid in enumerate(fact_sources[:10]) if eid in gold]
    return {"known_source_hit_at_10": bool(ranks),
            "known_source_rr_at_10": 1 / min(ranks) if ranks else 0,
            "known_source_ndcg_at_10": 1 / math.log2(min(ranks) + 1) if ranks else 0,
            "evidence_hit": bool(set(sources) & set(gold)),
            "fact_ids": [h["fact_id"] for h in hits], "fact_sources": fact_sources,
            "passage_sources": [p["episode_id"] for p in passages]}


def percentile(xs, p):
    return sorted(xs)[min(len(xs) - 1, math.ceil(len(xs) * p) - 1)]


def summarize(rows):
    groups = collections.defaultdict(list)
    for row in rows:
        if row["phase"] == "warm":
            groups[(row["fixture"], row["variant"], row["arm"])].append(row)
    out = []
    for (fixture, variant, arm), rs in sorted(groups.items()):
        times = [r["ms"] for r in rs]
        out.append({"fixture": fixture, "variant": variant, "arm": arm,
                    "requests": len(rs), "queries": len({r["query_id"] for r in rs}),
                    "p50_ms": statistics.median(times), "p95_ms": percentile(times, .95),
                    **{k: statistics.mean(r[k] for r in rs) for k in
                       ["known_source_hit_at_10", "known_source_rr_at_10",
                        "known_source_ndcg_at_10", "evidence_hit"]}})
    return out


def report(directory):
    provenance = json.loads((directory / "provenance.json").read_text())
    if provenance["status"] != "complete":
        raise RuntimeError("Only a complete, valid run can produce a comparison report.")
    rows = jsonl(directory / "requests.jsonl")
    fixture_manifest = json.loads((directory.parent.parent / "fixture.json").read_text())
    summary = summarize(rows)
    grouped = {(x["fixture"], x["variant"], x["arm"]): x for x in summary}
    pairs = []
    for fixture, variant in sorted({(x["fixture"], x["variant"]) for x in summary}):
        case_times = collections.defaultdict(lambda: collections.defaultdict(list))
        for r in rows:
            if r["phase"] == "warm" and (r["fixture"], r["variant"]) == (fixture, variant):
                case_times[r["query_id"]][r["arm"]].append(r["ms"])
        paired = [(statistics.median(t["baseline"]), statistics.median(t["prefilter"]))
                  for _, t in sorted(case_times.items())]
        rng = random.Random(436)
        deltas = []
        for _ in range(2000):
            sample = rng.choices(paired, k=len(paired))
            deltas.append(100 * (statistics.mean(b for a, b in sample)
                                 / statistics.mean(a for a, b in sample) - 1))
        pairs.append({"fixture": fixture, "variant": variant,
                      "paired_mean_median_change_percent": 100 *
                      (statistics.mean(b for a, b in paired) / statistics.mean(a for a, b in paired) - 1),
                      "bootstrap_95_percent": [percentile(deltas, .025), percentile(deltas, .975)]})
    write_json(directory / "paired.json", pairs)
    lines = ["# Episode-type prefilter benchmark", "",
             f"Run: `{directory.name}`. Baseline `{provenance['baseline_revision']}`; "
             f"prefilter `{provenance['prefilter_revision']}`.", "",
             "Warm local HTTP requests to the real `search_memory` JSON endpoint, k=10. "
             f"{len({r['query_id'] for r in rows})} fixed query/filter combinations, "
             f"{provenance['warm_repeats']} measured repeats per combination. "
             "A first pass warms caches and is excluded. Requests alternate before/after order.", "",
             "| Fixture | Filter | Before p50 / p95 ms | After p50 / p95 ms | p50 change | Known supporting evidence, before → after |",
             "|---|---|---:|---:|---:|---:|"]
    for fixture, variant in sorted({(x["fixture"], x["variant"]) for x in summary}):
        a, b = (grouped[(fixture, variant, arm)] for arm in ["baseline", "prefilter"])
        delta = 100 * (b["p50_ms"] / a["p50_ms"] - 1)
        lines.append(f"| {fixture} | {variant} | {a['p50_ms']:.1f} / {a['p95_ms']:.1f} | "
                     f"{b['p50_ms']:.1f} / {b['p95_ms']:.1f} | {delta:+.1f}% | "
                     f"{a['evidence_hit']:.1%} → {b['evidence_hit']:.1%} |")
    lines += ["", "A positive latency change is slower. `doc_date` intersects document type with a "
              "14-day window around the supporting source.", "",
              "## What the fixtures measure", "",
              f"- `dolphin`: {fixture_manifest['fixtures']['dolphin']['episodes']:,} fixed Morgan messages and "
              f"{fixture_manifest['fixtures']['dolphin']['facts']:,} existing extracted facts. Every original "
              "message has canonical type `doc`; the type filter therefore selects the whole corpus.",
              f"- `mixed`: the same sample plus {fixture_manifest['fixtures']['mixed']['synthetic_email_episodes']:,} "
              f"synthetic email episodes and facts ({fixture_manifest['distractors_per_question']} per question). "
              "Their text repeats the question without supplying its answer. This deliberately stresses "
              "candidate crowding; it is not a representative production type distribution.",
              "- Known supporting evidence means at least one preselected supporting episode appears among "
              "the returned fact citations or supplemental passages. This is incomplete source-level ground truth, "
              "not a semantic answer grade or an official DolphinBench task score. `summary.json` also records "
              "hit@10, reciprocal rank and binary nDCG for the deduplicated source order of returned facts.", "",
              "## Limits and reproducibility", "",
              "- Local filesystem, local BGE-small embeddings (384 dimensions), two Lance CPU threads, "
              "development build with dependencies optimized at level 1. Inspect preparation logs for index "
              "topology: the default small fixtures use the cached flat-vector path (fewer than 20,000 rows "
              "per part). No cloud-storage latency, concurrent "
              "load, unindexed tail, card formatting or answer generation was measured.",
              "- The operating system file cache was not flushed. First-pass timings are saved separately "
              "and must not be described as cold-cloud latency.",
              "- `paired.json` bootstraps paired per-question median latencies over the tested questions, not "
              "the repeated requests as independent observations. Its intervals describe only this small "
              "diagnostic query set, not new users or workloads.",
              "- `provenance.json` records executable hashes, revisions, harness hash and machine settings; "
              "the retained fixture manifest hashes both corpora and the query set. `requests.jsonl` keeps "
              "every duration and returned source ID; first-pass response files support error analysis.",
              "- No paid model calls or live organization mutations are part of this benchmark.", ""]
    (directory / "report.md").write_text("\n".join(lines))


def run(args):
    root = args.root.resolve()
    manifest = json.loads((root / "fixture.json").read_text())
    if args.repeats < 1 or (args.limit_queries is not None and args.limit_queries < 1):
        raise SystemExit("Repeats and query limit must be positive.")
    if digest(root / "queries.json") != manifest["queries_sha256"]:
        raise SystemExit("Frozen queries changed; prepare a new fixture root.")
    queries = json.loads((root / "queries.json").read_text())
    if args.limit_queries:
        queries = queries[:args.limit_queries]
    binaries = {"baseline": args.baseline.resolve(), "prefilter": args.prefilter.resolve()}
    if digest(binaries["baseline"]) == digest(binaries["prefilter"]):
        raise SystemExit("Both executables have the same SHA-256; build distinct revisions first.")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    result = root / "runs" / stamp
    result.mkdir(parents=True)
    (result / "harness.py").write_bytes(Path(__file__).read_bytes())
    provenance = {"status": "running", "fixture_sha256": digest(root / "fixture.json"),
                  "harness_sha256": digest(__file__), "created_at": stamp,
                  "platform": platform.platform(), "cpu_count": os.cpu_count(),
                  "profile": "Cargo dev; dependencies opt-level=1", "lance_cpu_threads": 2,
                  "warm_repeats": args.repeats, "order_seed": args.seed,
                  "binary_sha256": {k: digest(v) for k, v in binaries.items()},
                  "baseline_revision": args.baseline_revision,
                  "prefilter_revision": args.prefilter_revision,
                  "cache_note": "One cache-warming first pass per arm, excluded from warm statistics. Warm requests alternate "
                  "AB/BA with shuffled query order. OS file cache is not flushed; first-pass is not cold-cloud latency."}
    write_json(result / "provenance.json", provenance)
    rows = []
    try:
        with (result / "requests.jsonl").open("w") as output:
            for fixture in args.fixtures.split(","):
                corpus = root / "corpora" / fixture
                for name in ["episodes", "facts"]:
                    if digest(corpus / f"{name}.jsonl") != manifest["fixtures"][fixture][f"{name}_sha256"]:
                        raise RuntimeError(f"Frozen {fixture}/{name} corpus changed; prepare a new root.")
                stores = {}
                for arm, binary in binaries.items():
                    store = root / "stores" / fixture / arm
                    store.mkdir(parents=True, exist_ok=True)
                    stores[arm] = store
                    marker = store / "benchmark-ready.json"
                    expected = {"binary_sha256": digest(binary), "corpus": manifest["fixtures"][fixture]}
                    if marker.exists():
                        previous = json.loads(marker.read_text())
                        if previous["corpus"] != expected["corpus"]:
                            raise RuntimeError(f"Different corpus at {store}; choose a new store root.")
                    else:
                        previous = None
                    if previous != expected:
                        print(f"Preparing {fixture}/{arm}", flush=True)
                        with (result / f"{fixture}-{arm}-prepare.log").open("w") as log:
                            env = environment(root, store)
                            commands = []
                            if previous is None:
                                commands.append([str(binary), "load", str(corpus), "--local", str(store)])
                            commands.append([str(binary), "index", "org", "--local", str(store)])
                            for command in commands:
                                subprocess.run(command, env=env, stdout=log, stderr=log, check=True)
                        write_json(marker, expected)
                    layout = args.baseline_layout if arm == "baseline" else args.prefilter_layout
                    if not (store / "index" / "memory_org" / layout / "open.lance").exists():
                        raise RuntimeError(f"Expected facts layout {layout} missing in {store}")
                servers = {}
                try:
                    for arm, binary in binaries.items():
                        servers[arm] = Server(binary, stores[arm], root, result / f"{fixture}-{arm}-serve.log")
                    for repeat in range(args.repeats + 1):
                        order = list(queries)
                        random.Random(args.seed + repeat).shuffle(order)
                        for number, q in enumerate(order):
                            arms = list(binaries)
                            if (number + repeat) % 2:
                                arms.reverse()
                            for arm in arms:
                                ms, payload = servers[arm].query(q["arguments"])
                                row = {"fixture": fixture, "arm": arm, "query_id": q["id"],
                                       "variant": q["variant"], "repeat": repeat,
                                       "phase": "first_pass" if repeat == 0 else "warm", "ms": ms,
                                       **source_metrics(payload, q["gold_episode_ids"])}
                                rows.append(row)
                                output.write(json.dumps(row) + "\n")
                                output.flush()
                                if repeat == 0:
                                    write_json(result / f"{fixture}-{arm}-{q['id'].replace('/', '-')}.json", payload)
                        print(f"{fixture}: pass {repeat}/{args.repeats} complete", flush=True)
                finally:
                    for server in servers.values():
                        server.close()
        provenance["status"] = "complete"
    except BaseException as error:
        provenance["status"] = "failed"
        provenance["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        write_json(result / "provenance.json", provenance)
        write_json(result / "summary.json", summarize(rows))
        print(f"Results: {result}", flush=True)
    report(result)
    print(json.dumps(summarize(rows), indent=2), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--cases", type=Path, default=Path(__file__).with_name("retrieval_bench_cases.json"))
    p.add_argument("--episodes", type=int, default=600)
    p.add_argument("--distractors", type=int, default=425)
    p.add_argument("--seed", type=int, default=436)
    p = sub.add_parser("run")
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--prefilter", type=Path, required=True)
    p.add_argument("--baseline-revision", default="unknown")
    p.add_argument("--prefilter-revision", default="unknown")
    p.add_argument("--baseline-layout", default="v10")
    p.add_argument("--prefilter-layout", default="v11")
    p.add_argument("--fixtures", default="dolphin,mixed")
    p.add_argument("--repeats", type=int, default=8)
    p.add_argument("--seed", type=int, default=436)
    p.add_argument("--limit-queries", type=int)
    p = sub.add_parser("report")
    p.add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.command == "report":
        report(args.directory)
    else:
        (prepare if args.command == "prepare" else run)(args)


if __name__ == "__main__":
    main()
