#!/usr/bin/env bash
# W1: freshness. Commit a fact with a unique token (`memstore put`), then poll a running `memstore serve`
# until /search returns it. Prints one JSON line per write: commit time, and visible-after-ack time.
#   harness/evals/w1_fresh.sh <scope> <serve url> [writes] [group]
# group: who the probe facts are shared with (default local, the group `memstore serve --local` reads as).
set -euo pipefail
scope=$1 url=$2 n=${3:-30} group=${4:-local}
for i in $(seq "$n"); do
  tok="fresh$(date +%s%N | tail -c 10)x$i"
  t0=$(date +%s%3N)
  ./target/release/memstore put "$scope" "group:$group" "freshness-probe-$i" note "token $tok written for the freshness eval" >/dev/null
  t1=$(date +%s%3N)
  until curl -s -X POST "$url" -d "{\"data\":[[0,\"$tok\",3]]}" | grep -q "$tok"; do
    if [ $(( $(date +%s%3N) - t1 )) -gt 60000 ]; then echo "{\"i\":$i,\"timeout\":true}"; continue 2; fi
    sleep 0.05
  done
  t2=$(date +%s%3N)
  echo "{\"i\":$i,\"commit_ms\":$((t1 - t0)),\"visible_after_ack_ms\":$((t2 - t1))}"
done
