#!/bin/sh
# Installs memstore-client, what the memstore plugins run (the sign-in, the hooks and the MCP bridge, a few MB), from
# the public GitHub releases of A79-ai/ampup-memory, for this machine: Apple Silicon macOS, or Linux on arm64 or
# x86_64. Client releases are the tags client-v* (the server's are v*); with no version named, it installs the newest
# client release that has a build for this machine (pre-releases only when named).
#
#   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/memstore/bin/install.sh | sh
#   sh install.sh --latest   prints the newest client release's tag (client-v0.1.0) and installs nothing
#
#   MEMSTORE_VERSION      a client release tag such as client-v0.1.0 (v0.1.0 means the same; default: the newest)
#   MEMSTORE_INSTALL_DIR  where it goes (default: ~/.local/bin)
#   MEMSTORE_CLIENT_REPO  the repository whose releases hold it (default: MEMSTORE_REPO, else A79-ai/ampup-memory)
#   MEMSTORE_INSTALL_FROM a directory holding the release's files, instead of downloading them (for tests)
#   GITHUB_TOKEN          optional, and only sent to GitHub's API when looking up the newest release: without it the
#                         API allows 60 lookups an hour from one address
set -eu
repo=${MEMSTORE_CLIENT_REPO:-${MEMSTORE_REPO:-A79-ai/ampup-memory}}
dir=${MEMSTORE_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) echo "memstore-client builds for macOS and Linux only, not $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "memstore-client builds for arm64 and x86_64 only, not $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ]; then
  # A shell under Rosetta reports x86_64 on Apple Silicon; the arm64 build runs there natively.
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    arch=aarch64
  else
    echo "memstore-client builds for Apple Silicon Macs only" >&2
    exit 1
  fi
fi
asset=memstore-client-$arch-$os.tar.gz

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

# The newest client release (client-v<digits and dots>: no pre-release) with a build for this machine. No jq: each
# asset's download URL names its tag (.../releases/download/<tag>/<asset>), and GitHub lists releases newest first.
newest() {
  api "repos/$repo/releases?per_page=100" | tr ',' '\n' |
    sed -n "s#.*\"browser_download_url\": *\"https://github.com/[^\"]*/releases/download/\\(client-v[0-9][0-9.]*\\)/$asset\".*#\\1#p" |
    head -n1
}

if [ "${1:-}" = --latest ]; then
  tag=$(newest) && [ -n "$tag" ] && echo "$tag"
  exit $?
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "${MEMSTORE_INSTALL_FROM:-}" ]; then
  cp "$MEMSTORE_INSTALL_FROM/$asset" "$MEMSTORE_INSTALL_FROM/$asset.sha256" "$tmp/"
else
  version=${MEMSTORE_VERSION:-}
  case "$version" in
    "") version=$(newest) || version=""
      [ -n "$version" ] || {
        echo "found no client release of $repo with $asset (https://github.com/$repo/releases): is the repository public and spelled right (MEMSTORE_CLIENT_REPO)? Over GitHub's unauthenticated limit, set GITHUB_TOKEN or name a release with MEMSTORE_VERSION" >&2
        exit 1
      } ;;
    v*) version=client-$version ;;
  esac
  base=https://github.com/$repo/releases/download/$version
  for f in "$asset" "$asset.sha256"; do
    if command -v curl >/dev/null; then
      curl -fsSL --retry 3 -o "$tmp/$f" "$base/$f"
    else
      wget -q -O "$tmp/$f" "$base/$f"
    fi || { echo "couldn't download $base/$f" >&2; exit 1; }
  done
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
# Moved into place whole: a hook running the old copy keeps it, the next one runs the new.
mv "$tmp/out/memstore-client" "$dir/memstore-client.new"
chmod 755 "$dir/memstore-client.new"
# Not signed yet: a download is quarantined, and Gatekeeper would refuse it.
[ "$os" = apple-darwin ] && xattr -d com.apple.quarantine "$dir/memstore-client.new" 2>/dev/null || true
mv -f "$dir/memstore-client.new" "$dir/memstore-client"
echo "installed $("$dir/memstore-client" --version) to $dir/memstore-client"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "(the plugins find it there; to run it yourself, add $dir to your PATH)" ;;
esac
echo "next: memstore-client join <your team's server> --org <your org>, or run a local server (memstore serve --local), which needs no sign-in"
