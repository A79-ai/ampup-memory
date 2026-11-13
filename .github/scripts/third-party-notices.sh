#!/usr/bin/env bash
# The third-party notices of the release binaries (memstore and memstore-client): every crate they link on the
# release platforms with its license text (cargo-about, about.toml and about.hbs), the NOTICE files those crates ship,
# and the texts in licenses/: ONNX Runtime, which ort links into memstore (on Intel macOS, built from the same source
# by onnxruntime-x86_64-apple-darwin.sh), and the default embedding model.
#
#   .github/scripts/third-party-notices.sh <out-file>
#
# Writes <out-file> and nothing else in the repository; THIRD-PARTY-NOTICES.txt at the root is this script's output,
# committed (CI checks it is current). Run from anywhere; needs cargo (the repo's pinned toolchain), python3, curl
# and the network (crates.io, and GitHub for cargo-about and the license files clarified from crates' repositories).
#
# Fails, rather than writing an incomplete file, when:
#   - a crate's license isn't accepted in about.toml (a new dependency under an unusual license),
#   - a clarification in about.toml no longer matches its crate (a new version: look at its license files again),
#   - ort now downloads another ONNX Runtime than licenses/onnxruntime/VERSION names,
#   - the default embedding model is no longer the one licenses/models describes.
#
# cargo-about: $CARGO_ABOUT if set, else `cargo-about` on PATH if it is the pinned version, else the pinned release
# downloaded (checksum checked) into ~/.cache/memstore, else built with `cargo install`.
set -euo pipefail

out=${1:?usage: .github/scripts/third-party-notices.sh <out-file>}
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

ABOUT_VERSION=0.9.3
about_sha256() {
  case "$1" in
    x86_64-unknown-linux-musl) echo 3ad1e94ea1e63d69d1961f4fb15ccbbe9575d906111f96d2cb4dce6df48f71b4 ;;
    aarch64-unknown-linux-musl) echo eb4d9ff6ebd2518152a8308729431a04b3836e71d2bd5ead7aea69f91a71162e ;;
    aarch64-apple-darwin) echo 45520df8680978a6e7c4023ace0e3f311c73efc208cd68e5a7f4ed4822fa0349 ;;
  esac
}
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

cargo_about() {
  if [ -n "${CARGO_ABOUT:-}" ]; then echo "$CARGO_ABOUT"; return; fi
  if command -v cargo-about >/dev/null && [ "$(cargo-about --version 2>/dev/null)" = "cargo-about $ABOUT_VERSION" ]; then
    command -v cargo-about; return
  fi
  local dir="${XDG_CACHE_HOME:-$HOME/.cache}/memstore/cargo-about-$ABOUT_VERSION" triple=""
  if [ -x "$dir/bin/cargo-about" ]; then echo "$dir/bin/cargo-about"; return; fi
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) triple=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) triple=aarch64-unknown-linux-musl ;;
    Darwin-arm64) triple=aarch64-apple-darwin ;;
  esac
  mkdir -p "$dir/bin"
  if [ -n "$triple" ]; then
    local name="cargo-about-$ABOUT_VERSION-$triple" tmp
    tmp=$(mktemp -d)
    curl -fsSL --retry 3 -o "$tmp/$name.tar.gz" \
      "https://github.com/EmbarkStudios/cargo-about/releases/download/$ABOUT_VERSION/$name.tar.gz"
    [ "$(sha256 "$tmp/$name.tar.gz")" = "$(about_sha256 "$triple")" ] \
      || { echo "cargo-about $ABOUT_VERSION for $triple doesn't match its pinned checksum" >&2; exit 1; }
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    mv "$tmp/$name/cargo-about" "$dir/bin/cargo-about"
    rm -rf "$tmp"
  else
    cargo install --locked --quiet --root "$dir" "cargo-about@$ABOUT_VERSION" >&2
  fi
  echo "$dir/bin/cargo-about"
}

# The ONNX Runtime the ort-sys in Cargo.lock downloads for the release platforms (its dist.tsv names one version for
# all of them), and the default local embedding model: what licenses/ describes must still be what ships.
cargo fetch --locked --quiet
ortsys=$(cargo metadata --format-version 1 --locked |
  python3 -I -c 'import json, sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "ort-sys"))')
ort=$(grep -E '^(x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|aarch64-apple-darwin)	' "$(dirname "$ortsys")/build/download/dist.tsv" |
  sed -E 's|.*/ms@([0-9.]+)/.*|\1|' | sort -u)
want=$(cat licenses/onnxruntime/VERSION)
[ "$ort" = "$want" ] || {
  echo "ort downloads ONNX Runtime $ort, but licenses/onnxruntime holds $want's notices: fetch LICENSE and" >&2
  echo "ThirdPartyNotices.txt from https://github.com/microsoft/onnxruntime/tree/v$ort and update VERSION" >&2
  exit 1
}
model=$(sed -nE 's/^pub const DEFAULT_LOCAL_MODEL: &str = "(.*)";/\1/p' crates/memstore-core/src/search/embed.rs)
[ "$model" = "$(cat licenses/models/MODEL)" ] || {
  echo "the default embedding model is now '$model', not the one licenses/models describes: update licenses/models" >&2
  exit 1
}

about=$(cargo_about)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# --fail: a crate whose license can't be read or isn't accepted. A clarification that no longer matches is only a
# warning to cargo-about (it falls back to scanning the crate), so it is checked here.
if ! "$about" generate --locked --fail -c about.toml -o "$work/crates.txt" about.hbs 2> "$work/log"; then
  cat "$work/log" >&2
  exit 1
fi
if grep -q "failed to validate all files specified in clarification" "$work/log"; then
  grep "clarification" "$work/log" >&2
  echo "a clarification in about.toml no longer matches its crate: check the crate's license files and update it" >&2
  exit 1
fi

python3 -I - "$work/crates.txt" "$out" <<'EOF'
import hashlib
import os
import re
import sys

crates, out = sys.argv[1:3]
text = open(crates, encoding="utf-8").read()
text, _, manifests = text.partition("@@THIRD-PARTY-NOTICES-MANIFESTS@@\n")
text = text.rstrip() + "\n"
rule = "=" * 80


def section(title, body):
    return f"\n\n{rule}\n{title}\n{rule}\n\n{body.rstrip()}\n"


def read(path):
    return open(path, encoding="utf-8").read()


# 4. NOTICE files: the Apache License 2.0 (4(d)) has a work's NOTICE file travel with it. One copy of each distinct
# text, with the crates that ship it.
notices = {}
for line in sorted(set(filter(None, manifests.splitlines()))):
    name, version, manifest = line.split("\t")
    root = os.path.dirname(manifest)
    for f in sorted(os.listdir(root)):
        if re.match(r"(?i)^notice(\.(txt|md))?$", f):
            body = read(os.path.join(root, f)).strip()
            notices.setdefault(hashlib.sha256(body.encode()).hexdigest(), (body, []))[1].append(f"{name} {version}")
parts = []
for body, users in sorted(notices.values(), key=lambda n: sorted(n[1])[0]):
    used = "\n".join(f"  {u}" for u in sorted(users))
    parts.append(f"{'-' * 80}\nUsed by:\n{used}\n{'-' * 80}\n\n{body}\n")
text += section("4. NOTICE FILES OF THE RUST CRATES", "\n".join(parts))

ort = read("licenses/onnxruntime/VERSION").strip()
text += section(
    "5. ONNX RUNTIME",
    f"memstore statically links ONNX Runtime {ort} (Copyright (c) Microsoft Corporation, MIT License), the build\n"
    "the ort crate downloads for each release platform (for Intel macOS, which it has none for, built from the same\n"
    "source by .github/scripts/onnxruntime-x86_64-apple-darwin.sh).\n"
    f"Source: https://github.com/microsoft/onnxruntime/tree/v{ort}\n"
    "Its license, then its own third-party notices (the libraries compiled into it), follow.\n\n"
    f"{'-' * 80}\nONNX Runtime LICENSE\n{'-' * 80}\n\n{read('licenses/onnxruntime/LICENSE').rstrip()}\n\n"
    f"{'-' * 80}\nONNX Runtime ThirdPartyNotices.txt\n{'-' * 80}\n\n{read('licenses/onnxruntime/ThirdPartyNotices.txt').rstrip()}",
)
text += section("6. EMBEDDING MODEL", read("licenses/models/README.txt").rstrip() + "\n\n" + read("licenses/models/LICENSE").rstrip())

open(out, "w", encoding="utf-8").write(text)
EOF
echo "wrote $out ($(grep -c '' "$out") lines)" >&2
