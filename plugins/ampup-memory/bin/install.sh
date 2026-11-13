#!/bin/sh
# Installs ampup-memory-client, what the AmpUp Memory plugins run (the sign-in, the hooks and the MCP bridge, a few
# MB), from the public GitHub releases of A79-ai/ampup-memory, for this machine: Apple Silicon macOS, or Linux on
# arm64 or x86_64. Client releases are the tags client-v* (the server's are v*); with no version named, it installs
# the newest client release that has a build for this machine (pre-releases only when named). Releases from before
# the rename name their archive memstore-client-<target>.tar.gz, and it takes those too.
#
#   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory/bin/install.sh | sh
#   sh install.sh --latest   prints the newest client release's tag (client-v0.1.0) and installs nothing
#
#   AMPUP_MEMORY_VERSION       a client release tag such as client-v0.1.0 (v0.1.0 means the same; default: the newest)
#   AMPUP_MEMORY_INSTALL_DIR   where it goes (default: ~/.local/bin)
#   AMPUP_MEMORY_CLIENT_REPO   the repository whose releases hold it (default: AMPUP_MEMORY_REPO, else
#                              A79-ai/ampup-memory)
#   AMPUP_MEMORY_INSTALL_FROM  a directory holding the release's files, instead of downloading them (for tests)
#   GITHUB_TOKEN               optional, and only sent to GitHub's API when looking up the newest release: without it
#                              the API allows 60 lookups an hour from one address
#
# Each AMPUP_MEMORY_* variable is also read under its old name (MEMSTORE_*) when the new one isn't set.
#
# It installs ampup-memory-client and leaves memstore-client next to it as a link to it, the old name that older
# plugins and scripts run.
set -eu
repo=${AMPUP_MEMORY_CLIENT_REPO:-${MEMSTORE_CLIENT_REPO:-${AMPUP_MEMORY_REPO:-${MEMSTORE_REPO:-A79-ai/ampup-memory}}}}
dir=${AMPUP_MEMORY_INSTALL_DIR:-${MEMSTORE_INSTALL_DIR:-$HOME/.local/bin}}
from=${AMPUP_MEMORY_INSTALL_FROM:-${MEMSTORE_INSTALL_FROM:-}}
want_version=${AMPUP_MEMORY_VERSION:-${MEMSTORE_VERSION:-}}

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) echo "ampup-memory-client builds for macOS and Linux only, not $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "ampup-memory-client builds for arm64 and x86_64 only, not $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ]; then
  # A shell under Rosetta reports x86_64 on Apple Silicon; the arm64 build runs there natively.
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    arch=aarch64
  else
    echo "ampup-memory-client builds for Apple Silicon Macs only" >&2
    exit 1
  fi
fi
target=$arch-$os
# The archive's name, and the binary's in it: ampup-memory-client from the rename on, memstore-client before.
asset=ampup-memory-client-$target.tar.gz
old_asset=memstore-client-$target.tar.gz

# A GitHub API response (the path after https://api.github.com/) on stdout, with GITHUB_TOKEN when it is set.
api() {
  url=https://api.github.com/$1
  if command -v curl >/dev/null; then
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      curl -fsSL --retry 3 --max-time 20 -H 'Accept: application/vnd.github+json' -H "Authorization: Bearer $GITHUB_TOKEN" "$url"
    else
      curl -fsSL --retry 3 --max-time 20 -H 'Accept: application/vnd.github+json' "$url"
    fi
  elif [ -n "${GITHUB_TOKEN:-}" ]; then
    wget -q -T 20 -O - --header='Accept: application/vnd.github+json' --header="Authorization: Bearer $GITHUB_TOKEN" "$url"
  else
    wget -q -T 20 -O - --header='Accept: application/vnd.github+json' "$url"
  fi
}

# The newest client release (client-v<digits and dots>: no pre-release) with a build for this machine, under either
# archive name. No jq: each asset's download URL names its tag (.../releases/download/<tag>/<asset>), and GitHub
# lists releases newest first.
newest() {
  api "repos/$repo/releases?per_page=100" | tr ',' '\n' |
    sed -En "s#.*\"browser_download_url\": *\"https://github.com/[^\"]*/releases/download/(client-v[0-9][0-9.]*)/(ampup-memory|memstore)-client-$target\\.tar\\.gz\".*#\\1#p" |
    head -n1
}

if [ "${1:-}" = --latest ]; then
  tag=$(newest) && [ -n "$tag" ] && echo "$tag"
  exit $?
fi

fetch() {
  if command -v curl >/dev/null; then
    curl -fsSL --retry 3 -o "$tmp/$2" "$1/$2"
  else
    wget -q -O "$tmp/$2" "$1/$2"
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "$from" ]; then
  [ -f "$from/$asset" ] || asset=$old_asset
  cp "$from/$asset" "$from/$asset.sha256" "$tmp/"
else
  version=$want_version
  case "$version" in
    "") version=$(newest) || version=""
      [ -n "$version" ] || {
        echo "found no client release of $repo with $asset (https://github.com/$repo/releases): is the repository public and spelled right (AMPUP_MEMORY_CLIENT_REPO)? Over GitHub's unauthenticated limit, set GITHUB_TOKEN or name a release with AMPUP_MEMORY_VERSION" >&2
        exit 1
      } ;;
    v*) version=client-$version ;;
  esac
  base=https://github.com/$repo/releases/download/$version
  # A release from before the rename has the archive under its old name only.
  fetch "$base" "$asset" 2>/dev/null || { asset=$old_asset; fetch "$base" "$asset"; } ||
    { echo "couldn't download $base/$asset" >&2; exit 1; }
  fetch "$base" "$asset.sha256" || { echo "couldn't download $base/$asset.sha256" >&2; exit 1; }
fi

# The checksum file names the archive as it was built: compare the hashes alone.
want=$(cut -d' ' -f1 "$tmp/$asset.sha256")
if command -v sha256sum >/dev/null; then
  got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
[ "$want" = "$got" ] || { echo "$asset doesn't match its checksum ($got, not $want)" >&2; exit 1; }

mkdir -p "$tmp/out" "$dir"
tar -xzf "$tmp/$asset" -C "$tmp/out"
bin=$tmp/out/ampup-memory-client
[ -f "$bin" ] || bin=$tmp/out/memstore-client
# Moved into place whole: a hook running the old copy keeps it, the next one runs the new.
mv "$bin" "$dir/ampup-memory-client.new"
chmod 755 "$dir/ampup-memory-client.new"
# Signed with a Developer ID and notarized (a release built with the signing secrets): any quarantine flag stays, for
# Gatekeeper to check. Unsigned (one built without them): a quarantined download would be refused, so its flag goes.
if [ "$os" = apple-darwin ] && ! codesign --verify -R='anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13]' "$dir/ampup-memory-client.new" 2>/dev/null; then
  xattr -d com.apple.quarantine "$dir/ampup-memory-client.new" 2>/dev/null || true
fi
mv -f "$dir/ampup-memory-client.new" "$dir/ampup-memory-client"
# The old name, for what still runs it (an older plugin, a script): a link, so it is always this one.
ln -sf ampup-memory-client "$dir/memstore-client.new" && mv -f "$dir/memstore-client.new" "$dir/memstore-client"
echo "installed $("$dir/ampup-memory-client" --version) to $dir/ampup-memory-client (and memstore-client, its old name)"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "(the plugins find it there; to run it yourself, add $dir to your PATH)" ;;
esac
echo "next: sign in with /ampup-memory:login in Claude Code, or ampup-memory-client join <your team's server> --org <your org>; with no team server, run a local one (ampup-memory serve --local), which needs no sign-in"
