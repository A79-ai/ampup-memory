#!/usr/bin/env python3
"""Compare independent search optimizations against one baseline on retained local fixtures."""

import argparse
import collections
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess

import retrieval_bench as bench


def canonical(payload):
    # Pagination cursors identify a server's snapshot; all retrieved fields remain exact.
    if isinstance(payload, dict):
        payload = {key: value for key, value in payload.items() if key != "cursor"}
    return json.dumps(payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def summary(rows, reference):
    groups = collections.defaultdict(list)
    by_query = collections.defaultdict(lambda: collections.defaultdict(list))
    for row in rows:
        if row["phase"] == "warm":
            groups[(row["fixture"], row["variant"], row["arm"])].append(row)
            by_query[(row["fixture"], row["variant"], row["arm"])][row["query_id"]].append(row["ms"])
    result = []
    for (fixture, variant, arm), group in sorted(groups.items()):
        times = [row["ms"] for row in group]
        base = groups[(fixture, variant, reference)]
        base_times = [row["ms"] for row in base]
        paired = [(statistics.median(by_query[(fixture, variant, reference)][query]), statistics.median(values))
                  for query, values in sorted(by_query[(fixture, variant, arm)].items())]
        rng = random.Random(436)
        deltas = []
        for _ in range(2000):
            sample = rng.choices(paired, k=len(paired))
            deltas.append(100 * (statistics.mean(b for a, b in sample) / statistics.mean(a for a, b in sample) - 1))
        result.append({"fixture": fixture, "variant": variant, "arm": arm,
                       "requests": len(group), "queries": len(paired),
                       "p50_ms": statistics.median(times), "p95_ms": bench.percentile(times, .95),
                       "p50_change_percent": 100 * (statistics.median(times) / statistics.median(base_times) - 1),
                       "p95_change_percent": 100 * (bench.percentile(times, .95) / bench.percentile(base_times, .95) - 1),
                       "paired_mean_median_change_percent": 100 * (statistics.mean(b for a, b in paired) / statistics.mean(a for a, b in paired) - 1),
                       "paired_bootstrap_95_percent": [bench.percentile(deltas, .025), bench.percentile(deltas, .975)],
                       "payload_mismatches": sum(not row["parity"] for row in group),
                       "evidence_hit": statistics.mean(row["evidence_hit"] for row in group)})
    return result


def report(directory):
    provenance = json.loads((directory / "provenance.json").read_text())
    if provenance["status"] != "complete":
        raise ValueError("Only complete runs can produce comparison reports")
    rows = bench.jsonl(directory / "requests.jsonl")
    grouped = summary(rows, provenance["reference"])
    bench.write_json(directory / "summary.json", grouped)
    lines = ["# Search CPU experiment", "", f"Run: `{directory.name}`. "
             f"Reference: `{provenance['reference']}`. Profile: {provenance['profile']}.", "",
             "Negative latency changes are faster. Full response equality includes content, ordering, "
             "citations and scores; only snapshot-specific pagination cursors are omitted.", "",
             "| Fixture | Filter | Arm | p50 ms | p95 ms | p50 change | p95 change | Payload mismatches |",
             "|---|---|---|---:|---:|---:|---:|---:|"]
    for item in grouped:
        lines.append(f"| {item['fixture']} | {item['variant']} | {item['arm']} | {item['p50_ms']:.2f} | "
                     f"{item['p95_ms']:.2f} | {item['p50_change_percent']:+.1f}% | "
                     f"{item['p95_change_percent']:+.1f}% | {item['payload_mismatches']} |")
    lines += ["", f"Measured requests: {sum(row['phase'] == 'warm' for row in rows)}. "
              f"All-phase payload mismatches: {sum(not row['parity'] for row in rows)}.", "",
              f"Host: {provenance['cpu_count']} logical CPUs. One-minute load during passes: "
              f"{min(p['load'][0] for p in provenance['pass_load']):.1f}–"
              f"{max(p['load'][0] for p in provenance['pass_load']):.1f}. "
              "This is a shared machine; small or inconsistent latency changes are inconclusive.", "",
              "Local filesystem and cached flat-vector indexes, BGE-small embeddings, two Lance threads. "
              "No concurrent traffic, cloud storage, ANN, generated answers or production release qualification. "
              "The mixed fixture is an artificial candidate-crowding stress test. Known-source labels are incomplete.", "",
              "Warmup passes are excluded. Requests rotate arm order with shuffled questions. Bootstrap intervals "
              "resample paired per-question medians, not individual repetitions. Raw rows and first-pass responses "
              "are retained; differing responses are retained separately. Artifact and executable hashes are in provenance.", ""]
    (directory / "report.md").write_text("\n".join(lines))


def run(args):
    root = args.root.resolve()
    manifest = json.loads((root / "fixture.json").read_text())
    if args.repeats < 1 or args.warmups < 1:
        raise ValueError("At least one warmup and measured repetition required")
    if bench.digest(root / "queries.json") != manifest["queries_sha256"]:
        raise ValueError("Frozen queries changed")
    queries = json.loads((root / "queries.json").read_text())
    binaries = {}
    for value in args.binary:
        name, path = value.split("=", 1)
        if not name.replace("_", "").isalnum() or name in binaries:
            raise ValueError("Arm names must be unique alphanumeric/underscore labels")
        binaries[name] = Path(path).resolve()
    if args.reference not in binaries or len(binaries) < 2:
        raise ValueError("Need a reference and at least one candidate")
    hashes = {name: bench.digest(path) for name, path in binaries.items()}
    if len(set(hashes.values())) != len(hashes):
        raise ValueError("Every arm needs a distinct executable")
    experiment = json.loads((root / "experiment.json").read_text())
    for arm, digest in hashes.items():
        if experiment.get("artifacts", {}).get(arm, {}).get("binary_sha256") != digest:
            raise ValueError(f"{arm}: executable does not match the recorded source/build provenance")
    directory = root / "runs" / dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    directory.mkdir(parents=True)
    (directory / "harness.py").write_bytes(Path(__file__).read_bytes())
    (directory / "retrieval_bench.py").write_bytes(Path(bench.__file__).read_bytes())
    provenance = {"status": "running", "reference": args.reference, "profile": args.profile,
                  "platform": platform.platform(), "seed": args.seed, "warmups": args.warmups,
                  "cpu_count": os.cpu_count(), "load_at_start": os.getloadavg(), "pass_load": [],
                  "repeats": args.repeats, "fixture_order": args.fixtures.split(","),
                  "binary_sha256": hashes, "binary_paths": {k: str(v) for k, v in binaries.items()},
                  "fixture_sha256": bench.digest(root / "fixture.json"),
                  "harness_sha256": bench.digest(__file__), "helper_sha256": bench.digest(bench.__file__),
                  "experiment": experiment}
    bench.write_json(directory / "provenance.json", provenance)
    rows = []
    try:
        with (directory / "requests.jsonl").open("w") as output:
            for fixture in args.fixtures.split(","):
                corpus = root / "corpora" / fixture
                for name in ["episodes", "facts"]:
                    if bench.digest(corpus / f"{name}.jsonl") != manifest["fixtures"][fixture][f"{name}_sha256"]:
                        raise ValueError(f"Frozen {fixture}/{name} changed")
                servers = {}
                try:
                    for arm, binary in binaries.items():
                        store = root / "stores" / fixture / arm
                        store.mkdir(parents=True, exist_ok=True)
                        marker = store / "benchmark-ready.json"
                        expected = {"binary_sha256": hashes[arm], "corpus": manifest["fixtures"][fixture]}
                        previous = json.loads(marker.read_text()) if marker.exists() else None
                        if previous and previous["corpus"] != expected["corpus"]:
                            raise ValueError("Store corpus changed; choose a new root")
                        if previous != expected:
                            print(f"Preparing {fixture}/{arm}", flush=True)
                            with (directory / f"{fixture}-{arm}-prepare.log").open("w") as log:
                                commands = [] if previous else [[str(binary), "load", str(corpus), "--local", str(store)]]
                                commands.append([str(binary), "index", "org", "--local", str(store)])
                                for command in commands:
                                    subprocess.run(command, env=bench.environment(root, store), stdout=log, stderr=log, check=True)
                            bench.write_json(marker, expected)
                        if not (store / "index" / "memory_org" / "v10" / "open.lance").exists():
                            raise ValueError("This experiment expects the unchanged v10 fact layout")
                        servers[arm] = bench.Server(binary, store, root, directory / f"{fixture}-{arm}-serve.log")
                    for repeat in range(args.warmups + args.repeats):
                        provenance["pass_load"].append({"fixture": fixture, "repeat": repeat,
                                                       "load": os.getloadavg()})
                        order = list(queries)
                        random.Random(args.seed + repeat).shuffle(order)
                        for number, query in enumerate(order):
                            arms = list(binaries)
                            rotate = (repeat + number) % len(arms)
                            arms = arms[rotate:] + arms[:rotate]
                            if repeat % 2:
                                arms.reverse()
                            replies = {arm: servers[arm].query(query["arguments"]) for arm in arms}
                            reference = canonical(replies[args.reference][1])
                            for arm, (ms, payload) in replies.items():
                                encoded = canonical(payload)
                                parity = encoded == reference
                                row = {"fixture": fixture, "query_id": query["id"], "variant": query["variant"],
                                       "arm": arm, "repeat": repeat, "phase": "warmup" if repeat < args.warmups else "warm",
                                       "ms": ms, "parity": parity, "response_sha256": hashlib.sha256(encoded.encode()).hexdigest(),
                                       **bench.source_metrics(payload, query["gold_episode_ids"])}
                                rows.append(row)
                                output.write(json.dumps(row) + "\n")
                                output.flush()
                                if repeat == 0 or not parity:
                                    suffix = "first" if repeat == 0 else f"mismatch-{repeat}"
                                    bench.write_json(directory / f"{fixture}-{arm}-{query['id'].replace('/', '-')}-{suffix}.json", payload)
                        print(f"{fixture}: pass {repeat + 1}/{args.warmups + args.repeats}", flush=True)
                finally:
                    for server in servers.values():
                        server.close()
        provenance["status"] = "complete"
        provenance["payload_parity_passed"] = all(row["parity"] for row in rows)
    except BaseException as error:
        provenance.update(status="failed", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        bench.write_json(directory / "provenance.json", provenance)
        print(f"Results: {directory}", flush=True)
    report(directory)
    if not provenance["payload_parity_passed"]:
        raise SystemExit("Payload parity failed; inspect the saved mismatches before accepting a speedup")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    command = sub.add_parser("run")
    command.add_argument("--root", type=Path, required=True)
    command.add_argument("--binary", action="append", required=True, help="NAME=/absolute/path")
    command.add_argument("--reference", default="baseline")
    command.add_argument("--profile", required=True)
    command.add_argument("--repeats", type=int, default=8)
    command.add_argument("--warmups", type=int, default=2)
    command.add_argument("--seed", type=int, default=436)
    command.add_argument("--fixtures", default="dolphin,mixed")
    command = sub.add_parser("report")
    command.add_argument("directory", type=Path)
    args = parser.parse_args()
    report(args.directory) if args.command == "report" else run(args)
