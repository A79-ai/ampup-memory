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
mkdir "$ctx/bin"
cp target/release/ampup-memory "$ctx/bin/" && strip "$ctx/bin/ampup-memory"
# the command's name before the rename, a link the image keeps
ln -s ampup-memory "$ctx/bin/memstore"
# fetch the model into the context (hf-hub cache layout; -L turns its symlinks into files)
AMPUP_MEMORY_MODEL_DIR="$ctx/fetch" "$ctx/bin/ampup-memory" embed warmup >/dev/null
cp -rL "$ctx/fetch" "$ctx/models" && rm -rf "$ctx/fetch"
find "$ctx/models" -name '*.lock' -delete
mkdir "$ctx/doc" && cp LICENSE NOTICE THIRD-PARTY-NOTICES.txt "$ctx/doc/"
docker build -q -t "$T" -f deploy/spcs/Dockerfile "$ctx" >/dev/null
docker run --rm --network none --user 65532:65532 --read-only "$T" embed offline check
# and by its old name
docker run --rm --network none --user 65532:65532 --read-only --entrypoint /usr/local/bin/memstore "$T" --version
echo "built $T"
