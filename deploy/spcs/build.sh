#!/usr/bin/env bash
# Build the engine image (on a linux/amd64 host with Docker) with the embedding
# model baked in, and prove the model loads with no network as the image's non-root user.
#   deploy/spcs/build.sh [image tag]   (default memstore:local)
set -euo pipefail
cd "$(dirname "$0")/../.."
T=${1:-memstore:local}
cargo build --release -q
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
cp target/release/memstore "$ctx/" && strip "$ctx/memstore"
# fetch the model into the context (hf-hub cache layout; -L turns its symlinks into files)
MEMSTORE_MODEL_DIR="$ctx/fetch" "$ctx/memstore" embed warmup >/dev/null
cp -rL "$ctx/fetch" "$ctx/models" && rm -rf "$ctx/fetch"
find "$ctx/models" -name '*.lock' -delete
mkdir "$ctx/doc" && cp LICENSE NOTICE THIRD-PARTY-NOTICES.txt "$ctx/doc/"
docker build -q -t "$T" -f deploy/spcs/Dockerfile "$ctx" >/dev/null
docker run --rm --network none --user 65532:65532 --read-only "$T" embed offline check
echo "built $T"
