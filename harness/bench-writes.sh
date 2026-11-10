#!/usr/bin/env bash
# Write throughput sweep on the harness stack: commits/s vs concurrent writers, and facts/s vs
# facts per commit. Appends one JSON line per run to reports/writes.jsonl and prints a table.
set -uo pipefail
ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
B=./target/release/memstore
mkdir -p reports
cargo build --release -q || exit 1
source harness/env.sh
out=reports/writes.jsonl
: > "$out"
run() { # label writers commits facts [shared]
  local line
  line=$($B check throughput "team:tp_$1" "$2" "$3" "$4" ${5:+--shared} 2>&1 | tail -1)
  echo "$line" | grep -q '^{' || { echo "$1: $line"; return; }
  echo "$line" >> "$out"; echo "$1: $line"
}
# Iceberg metadata grows with every snapshot; start from expired metadata so runs compare.
$B maintain team:tp_maint 10 >/dev/null 2>&1 || true
# Separate stores (separate processes): ledger contention, 1 fact per commit.
for w in 1 5 10; do run "sep$w" "$w" $(( (30 + w - 1) / w )) 1; done
# One shared store (one engine serving many agents): group commit batches concurrent writers.
for w in 1 10 50 200; do run "shared$w" "$w" $(( (400 + w - 1) / w )) 1 shared; done
# Facts per commit: one writer.
for f in 100 1000 10000; do run "f$f" 1 5 "$f"; done
# Both: 50 agents on one store, 100 facts per commit each.
run "shared50f100" 50 8 100 shared
python3 - "$out" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
print(f"{'shared':>6} {'writers':>7} {'facts/commit':>12} {'commits':>7} {'commits/s':>9} {'facts/s':>9} {'p50 ms':>7} {'p95 ms':>7}")
for r in rows:
    print(f"{str(r.get('shared_store', False)):>6} {r['writers']:>7} {r['facts_per_commit']:>12} {r['commits']:>7} {r['commits_per_s']:>9} {r['facts_per_s']:>9} {r['commit_p50_ms']:>7} {r['commit_p95_ms']:>7}")
PY
