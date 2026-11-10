#!/usr/bin/env bash
# Storage and search verification on the local Docker stack (harness/docker-compose.yml), on synthetic corpora from
# `memstore gen`: runs every check and writes reports/m7.0.json.
# Exit code is 0 only when every check passes. Run from the repo root.
set -uo pipefail
# Many concurrent writers each hold HTTP connection pools; the default 1024 open files is
# not enough (the concurrent-commits check hits EMFILE on stock Ubuntu).
ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
B=./target/release/ampup-memory
C=corpora
mkdir -p reports "$C"
cargo build --release -q || exit 1
results=()
check() { # name, command...
  local name=$1; shift
  local t0=$SECONDS out
  if out=$("$@" 2>&1); then status=pass; else status=fail; fi
  printf '%-22s %s (%ss)\n' "$name" "$status" $((SECONDS - t0))
  results+=("$(python3 -c 'import json,sys; print(json.dumps({"check":sys.argv[1],"status":sys.argv[2],"output":sys.argv[3][-2000:]}))' "$name" "$status" "$out")")
}

# Up and initialised: the one-shot polaris-init container (which needs a healthy Polaris and
# bucket) has exited 0. `up --wait` would count that clean exit as a failure.
stack_up() {
  (cd harness && docker compose up -d >/dev/null 2>&1) || return 1
  for _ in $(seq 120); do
    case $(cd harness && docker compose ps -a polaris-init --format '{{.State}} {{.ExitCode}}') in
      "exited 0") return 0 ;;
      exited*) return 1 ;;
    esac
    sleep 1
  done
  return 1
}
stack_health() { stack_up && echo "stack initialised"; }
determinism() {
  local a b; a=$($B gen 42 10000 - | grep content_hash); b=$($B gen 42 10000 - | grep content_hash)
  echo "$a"; [ "$a" = "$b" ]
}
corpus_sizes() {
  $B gen 42 10000 "$C/c10000" >/dev/null && $B gen 42 100000 "$C/c100000" >/dev/null || return 1
  $B gen 42 1000000 - | python3 -c '
import json,sys; m=json.load(sys.stdin); print(m)
assert m["episodes"]==1000000 and 5_000_000 <= m["facts"] <= 10_000_000'
}
gold_traceability() { $B verify-corpus "$C/c10000" && $B verify-corpus "$C/c100000"; }
hub_coverage() {
  $B gen 42 1000000 - | python3 -c '
import json,sys; m=json.load(sys.stdin); print("hubs above p99.9:", m["hubs_above_p999"])
assert m["hubs_above_p999"] >= 100'
}
gold_eval_on_stack() {
  source harness/env.sh
  # A fresh warehouse per run: wipe and recreate the stack's catalog contents.
  (cd harness && docker compose down -v >/dev/null 2>&1) && stack_up || return 1
  $B load "$C/c10000" 5000 && $B eval "$C/c10000"
}
# A row deleted by another engine (an erasure) must vanish from search at once, before the
# index catches up, and the index update must drop its stale rows.
search_after_delete() {
  source harness/env.sh
  export AMPUP_MEMORY_INDEX_URI="$PWD/$C/index-verify"
  local fact before
  fact=$(grep '"entity":"p000042","predicate":"role"' "$C/c10000/facts.jsonl" | grep '"valid_to":null' | sed -E 's/.*"fact_id":"([^"]+)".*/\1/')
  before=$($B search org anyone p000042 role | head -1)
  echo "before: $before"; [[ "$before" == *"p000042 role"* ]] || return 1
  uv run -q --with 'pyiceberg[pyarrow,s3fs]' python harness/delete_fact.py memory_org "$fact" || return 1
  ! $B search org anyone p000042 role | grep -q "p000042 role" || { echo "deleted row served from stale index"; return 1; }
  $B index org && ! $B search org anyone p000042 role | grep -q "p000042 role"
}
# Lexical search through the Lance index (plus unindexed tail): recall@10 >= 0.95 on the
# single-hop, as-of and freshness gold questions, p95 under 200 ms.
search_on_stack() {
  source harness/env.sh
  export AMPUP_MEMORY_INDEX_URI="$PWD/$C/index-verify"
  rm -rf "$AMPUP_MEMORY_INDEX_URI"
  $B index org && $B eval "$C/c10000" | python3 -c '
import json,sys; t=sys.stdin.read(); j=json.loads(t[t.index("{"):t.rindex("}")+1])
hit, n = j["search_recall_at_10"]; print("recall@10", hit, "/", n, "p95 ms", j["search_p95_ms"])
assert hit >= 0.95 * n and j["search_p95_ms"] < 200'
}

check stack-health stack_health
check generator-determinism determinism
check corpus-sizes corpus_sizes
check gold-traceability gold_traceability
check hub-coverage hub_coverage
check gold-eval-on-stack gold_eval_on_stack
check search-on-stack search_on_stack
check search-after-delete search_after_delete
# judge-calibration needs a 200-question human-labelled set; it is not automatable yet.

python3 - "${results[@]}" <<'PY'
import json, sys, datetime
checks = [json.loads(a) for a in sys.argv[1:]]
report = {"milestone": "M7.0", "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
          "passed": all(c["status"] == "pass" for c in checks), "checks": checks,
          "not_run": ["judge-calibration (needs the human-labelled set)"]}
open("reports/m7.0.json", "w").write(json.dumps(report, indent=2))
print("all passed" if report["passed"] else "FAILED")
sys.exit(0 if report["passed"] else 1)
PY
