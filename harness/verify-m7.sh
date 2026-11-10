#!/usr/bin/env bash
# Portable-core verification: runs every portable-core check (`memstore check`, src/harness/checks.rs) on synthetic
# `memstore gen` corpora against the local Docker stack (harness/docker-compose.yml) and writes
# reports/m7.json. Exit code is 0 only when every check passes. The default sizes (M7_EPISODES, a million
# episodes) want a large Linux machine (16 vCPU); they are env-tunable for a laptop.
set -uo pipefail
# Many concurrent writers each hold HTTP connection pools; the default 1024 open files is
# not enough (the concurrent-commits check hits EMFILE on stock Ubuntu).
ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
B=./target/release/memstore
C=corpora
N=${M7_EPISODES:-1000000}
mkdir -p reports "$C"
cargo build --release -q || exit 1
source harness/env.sh
results=()
check() { # name, command...
  local name=$1; shift
  local t0=$SECONDS out
  if out=$("$@" 2>&1); then status=pass; else status=fail; fi
  printf '%-24s %s (%ss)\n' "$name" "$status" $((SECONDS - t0))
  results+=("$(python3 -c 'import json,sys; print(json.dumps({"check":sys.argv[1],"status":sys.argv[2],"output":sys.argv[3][-3000:]}))' "$name" "$status" "$out")")
}
stack_reset() {
  (cd harness && docker compose down -v >/dev/null 2>&1 && docker compose up -d >/dev/null 2>&1) || return 1
  for _ in $(seq 120); do
    case $(cd harness && docker compose ps -a polaris-init --format '{{.State}} {{.ExitCode}}') in
      "exited 0") return 0 ;; exited*) return 1 ;; esac
    sleep 1
  done
  return 1
}
# Peak RSS via GNU time where available (Linux); plain run elsewhere.
with_rss() {
  if /usr/bin/time -f "%M" true >/dev/null 2>&1; then /usr/bin/time -f "max_rss_kb=%M" "$@"; else "$@"; fi
}

# Eval at scale: the gold questions of an N-episode corpus, every category, as their
# principals, after loading org plus every scope a question is asked in. Pass: every key
# answer right (incl. the owner control on permission questions), search recall@10 >= 0.95,
# warm p95 under 200 ms for search and lookups.
eval_corpus() { # episodes, label
  local n=$1 dir="$C/c$1"
  [ -f "$dir/manifest.json" ] || $B gen 42 "$n" "$dir" >/dev/null || return 1
  export MEMSTORE_INDEX_URI="$PWD/$C/index-m7-$n"; rm -rf "$MEMSTORE_INDEX_URI"
  stack_reset || { echo "stack failed"; return 1; }
  local t=$SECONDS
  $B load "$dir" 50000 questions || return 1
  echo "load_s=$((SECONDS - t))"
  t=$SECONDS
  for s in $(scopes_of "$dir"); do $B index "$s" >/dev/null || return 1; done
  echo "index_s=$((SECONDS - t))"
  with_rss $B eval "$dir" questions --warm > /tmp/m7eval.json 2>/tmp/m7eval.time
  grep max_rss /tmp/m7eval.time
  python3 - /tmp/m7eval.json <<'PY'
import json, sys
t = open(sys.argv[1]).read(); j = json.loads(t[t.index("{"):t.rindex("}") + 1])
print(json.dumps({k: j[k] for k in ["by_category", "p50_ms", "p95_ms", "search_recall_at_10", "search_p50_ms", "search_p95_ms"]}))
wrong = {c: n - ok for c, (ok, n) in j["by_category"].items() if ok != n}
hit, n = j["search_recall_at_10"]
assert not wrong, f"wrong answers: {wrong} {j['failures'][:5]}"
assert hit >= 0.95 * n, f"search recall {hit}/{n}"
assert j["search_p95_ms"] < 200 and j["p95_ms"] < 200, "p95 over 200 ms"
PY
}
scopes_of() { python3 -c "import json;print(' '.join(sorted({json.loads(l)['scope'] for l in open('$1/questions.jsonl')}|{'org'})))"; }

eval_at_scale() { eval_corpus "$N"; }

# Source permission fidelity, on the corpus eval-at-scale just loaded: for sampled
# facts (half with restricted sources) and their principals, visible exactly when ground
# truth says every source episode is visible to them.
source_acl() { $B check source-acl "$C/c$N" "${M7_ACL_SAMPLE:-2000}" 42; }

# Rebuild time: rebuild every derived index of the loaded corpus from the facts
# tables into an empty location. Pass: projects to under 24 hours.
rebuild_time() {
  export MEMSTORE_INDEX_URI="$PWD/$C/index-m7-rebuild"; rm -rf "$MEMSTORE_INDEX_URI"
  local t=$SECONDS
  for s in $(scopes_of "$C/c$N"); do $B index "$s" >/dev/null || return 1; done
  local secs=$((SECONDS - t))
  echo "full index rebuild of the $N-episode corpus: ${secs}s"
  [ "$secs" -lt 86400 ]
}

# The same eval on a larger corpus (about 10M current facts).
eval_at_10m() { eval_corpus "${M7_10M_EPISODES:-1400000}"; }

concurrent() { $B check concurrent team:m7conc "${M7_WRITERS:-50}" "${M7_COMMITS:-4}"; }
pinning() { $B check pinning team:m7pin "${M7_PIN_WRITES:-50}"; }
crash() { $B check crash team:m7crash "${M7_CRASHES:-1000}" 42; }
model() { $B check model "${M7_MODEL_SEQS:-100}" "${M7_MODEL_OPS:-15}" 42; }
skew() { $B check skew team:m7skew "${M7_SKEW_WRITERS:-10}" "${M7_SKEW_COMMITS:-5}" 300; }
freshness() { $B check freshness team:m7fresh "${M7_FRESH_SAMPLES:-20}"; }
equivalence() { $B check index-equivalence team:m7equiv "${M7_EQ_BATCHES:-10}" "${M7_EQ_PER_BATCH:-500}" 42; }

# M7_SKIP_EVALS=1 runs only the engine checks, on a fresh stack (for diagnosing those).
if [ "${M7_SKIP_EVALS:-}" ]; then
  stack_reset || { echo "stack failed"; exit 1; }
else
  check eval-at-scale eval_at_scale
  check source-acl source_acl
  check rebuild-time rebuild_time
fi
check concurrent-commits concurrent
check snapshot-pinning pinning
check ledger-atomicity crash
check branch-merge-model model
check clock-skew skew
check freshness freshness
check index-equivalence equivalence
[ "${M7_SKIP_10M:-}${M7_SKIP_EVALS:-}" ] || check eval-at-10m eval_at_10m

python3 - "${results[@]}" <<'PY'
import json, sys, datetime
checks = [json.loads(a) for a in sys.argv[1:]]
report = {"milestone": "M7", "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
          "passed": all(c["status"] == "pass" for c in checks), "checks": checks}
open("reports/m7.json", "w").write(json.dumps(report, indent=2))
print("all passed" if report["passed"] else "FAILED")
sys.exit(0 if report["passed"] else 1)
PY
