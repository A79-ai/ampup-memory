#!/usr/bin/env bash
# No new code reads a setting by its old name (`MEMSTORE_X`, now `AMPUP_MEMORY_X`): each binary's startup reads the
# old names as the new ones (crates/memstore-client/src/naming.rs), so the rest of the code names only the new
# ones. A `"MEMSTORE_…` string in src/ or crates/*/src fails this check, outside the files below: the names module
# itself, and the variables other programs read or write by the old names (the deployment agent's flags and
# embeddings, which the cluster's admission policy fixes; the Cloud Run services provision creates, whose image
# may be from before the rename; the agent's support bundle and its tests). Comments are not checked.
set -euo pipefail
cd "$(dirname "$0")/../.."
allowed=(
  crates/memstore-client/src/naming.rs
  src/agent.rs
  src/agent/support.rs
  src/provision.rs
)
found=$(grep -rnE '"MEMSTORE_|MEMSTORE_[A-Z_]+=' --include='*.rs' src crates/*/src \
  | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
  | grep -vF "${allowed[@]/#/-e}" || true)
if [ -n "$found" ]; then
  while IFS=: read -r f l _; do
    echo "::error file=$f,line=$l::read the setting by its new name (AMPUP_MEMORY_…); startup maps the old one"
  done <<<"$found"
  echo "$found"
  exit 1
fi
