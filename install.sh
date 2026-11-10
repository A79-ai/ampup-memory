#!/usr/bin/env sh
# Installs the memstore CLI from A79-ai/ampup-memory's GitHub releases, for this machine (Apple Silicon macOS, or
# Linux on arm64 or x86_64). The repository is public: no GitHub account, CLI or sign-in is needed, only curl (or
# wget) and tar.
#
#   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh
#
#   MEMSTORE_INSTALL      client: memstore-client alone, all the Claude Code and Cursor plugins need (a few MB);
#                         default: memstore (the store and server too) and memstore-client
#   MEMSTORE_VERSION      a release tag such as v0.1.0, or client-v0.1.0 for the client alone (default: the newest
#                         server release, or with MEMSTORE_INSTALL=client the newest client release)
#   MEMSTORE_INSTALL_DIR  where the binaries go (default: ~/.local/bin)
#   MEMSTORE_REPO         the GitHub repository whose releases to install (default: A79-ai/ampup-memory), for a fork
#   MEMSTORE_INSTALL_FROM a directory holding the release's files, instead of downloading them (for tests)
#   GITHUB_TOKEN          optional, and only sent to GitHub's API when looking up the newest release: without it the
#                         API allows 60 lookups an hour from one address
set -eu
repo=${MEMSTORE_REPO:-A79-ai/ampup-memory}
dir=${MEMSTORE_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) echo "memstore builds for macOS and Linux only, not $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "memstore builds for arm64 and x86_64 only, not $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ]; then
  # A shell under Rosetta reports x86_64 on Apple Silicon; the arm64 build runs there natively.
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    arch=aarch64
  else
    echo "memstore builds for Apple Silicon Macs only: onnxruntime, which it embeds with, has no prebuilt Intel macOS library" >&2
    exit 1
  fi
fi
case "${MEMSTORE_INSTALL:-}" in
  client) asset=memstore-client-$arch-$os.tar.gz ;;
  "" | full) asset=memstore-$arch-$os.tar.gz ;;
  *) echo "MEMSTORE_INSTALL is client or full, not $MEMSTORE_INSTALL" >&2; exit 1 ;;
esac
full=memstore-$arch-$os.tar.gz

# A file from a URL, with curl or else wget.
fetch() {
  if command -v curl >/dev/null; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null; then
    wget -q -O "$2" "$1"
  else
    echo "install curl or wget" >&2
    exit 1
  fi
}

# A GitHub API response (the path after https://api.github.com/) on stdout, with GITHUB_TOKEN when it is set.
api() {
  url=https://api.github.com/$1
  if command -v curl >/dev/null; then
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' -H "Authorization: Bearer $GITHUB_TOKEN" "$url"
    else
      curl -fsSL --retry 3 -H 'Accept: application/vnd.github+json' "$url"
    fi
  elif command -v wget >/dev/null; then
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      wget -q -O - --header='Accept: application/vnd.github+json' --header="Authorization: Bearer $GITHUB_TOKEN" "$url"
    else
      wget -q -O - --header='Accept: application/vnd.github+json' "$url"
    fi
  else
    echo "install curl or wget" >&2
    exit 1
  fi
}

# The release tags whose assets include $2, among releases whose tag starts with $1, newest first, read from a
# releases listing on stdin. No jq: each asset's download URL names its tag (.../releases/download/<tag>/<asset>).
tags_with() {
  tr ',' '\n' |
    sed -n "s#.*\"browser_download_url\": *\"https://github.com/[^\"]*/releases/download/\\($1[^/\"]*\\)/$2\".*#\\1#p"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "${MEMSTORE_INSTALL_FROM:-}" ]; then
  cp "$MEMSTORE_INSTALL_FROM/$asset" "$MEMSTORE_INSTALL_FROM/$asset.sha256" "$tmp/" 2>/dev/null || {
    echo "$MEMSTORE_INSTALL_FROM has no $asset (a release from before memstore-client has memstore alone: leave MEMSTORE_INSTALL out)" >&2
    exit 1
  }
else
  version=${MEMSTORE_VERSION:-}
  if [ -z "$version" ]; then
    # No version given: the newest release of its kind with a build for this machine, a pre-release too (GitHub's
    # own "latest" skips those). Clients are released as client-v*, the server as v*; a server release from before
    # the split has the client too.
    releases=$(api "repos/$repo/releases?per_page=100") || {
      echo "couldn't list $repo's releases on GitHub: is the repository public, and spelled right (MEMSTORE_REPO)? Over GitHub's unauthenticated limit (60 lookups an hour), set GITHUB_TOKEN or name a release with MEMSTORE_VERSION" >&2
      exit 1
    }
    if [ "${MEMSTORE_INSTALL:-}" = client ]; then
      version=$(printf '%s\n' "$releases" | tags_with client-v "$asset" | head -n1)
      [ -n "$version" ] || version=$(printf '%s\n' "$releases" | tags_with v "$full" | head -n1)
    else
      version=$(printf '%s\n' "$releases" | tags_with v "$asset" | head -n1)
    fi
    [ -n "$version" ] || {
      echo "$repo has no release with $asset yet (https://github.com/$repo/releases): build from source instead, as CONTRIBUTING.md describes (cargo build --release)" >&2
      exit 1
    }
  fi
  base=https://github.com/$repo/releases/download/$version
  if ! fetch "$base/$asset" "$tmp/$asset" 2>/dev/null || ! fetch "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
    # Why: which files the release has, if GitHub's API says.
    assets=$(api "repos/$repo/releases/tags/$version" 2>/dev/null |
      tr ',' '\n' | sed -n 's#.*"name": *"\([^"]*\.tar\.gz\)".*#\1#p') || assets=""
    if [ -z "$assets" ]; then
      echo "couldn't download $asset from $repo $version: is there such a release? (https://github.com/$repo/releases)" >&2
    # A release from before memstore-client has no client asset: say so, not that the download failed.
    elif [ "${MEMSTORE_INSTALL:-}" = client ] && echo "$assets" | grep -qx "$full"; then
      echo "$version has no memstore-client (it is from before it): leave MEMSTORE_INSTALL out to install memstore" >&2
    elif ! echo "$assets" | grep -qx "$asset"; then
      echo "$version has no $asset (it has no build for this machine): leave MEMSTORE_VERSION out for the newest one built for it" >&2
    else
      echo "couldn't download $base/$asset" >&2
    fi
    exit 1
  fi
fi

# The checksum file names the archive as it was built: compare the hashes alone.
want=$(cut -d' ' -f1 "$tmp/$asset.sha256")
if command -v sha256sum >/dev/null; then
  got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
[ "$want" = "$got" ] || { echo "$asset doesn't match its checksum ($got, not $want)" >&2; exit 1; }

mkdir -p "$tmp/out"
tar -xzf "$tmp/$asset" -C "$tmp/out"
mkdir -p "$dir"
# Releases before memstore-client hold memstore alone.
for bin in memstore memstore-client; do
  [ -f "$tmp/out/$bin" ] || continue
  mv "$tmp/out/$bin" "$dir/$bin"
  chmod 755 "$dir/$bin"
  # Not signed yet: a file the browser or AirDrop downloaded is quarantined, and Gatekeeper would refuse it.
  [ "$os" = apple-darwin ] && xattr -d com.apple.quarantine "$dir/$bin" 2>/dev/null || true
  echo "installed $("$dir/$bin" --version) to $dir/$bin"
done
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to your PATH: export PATH=\"$dir:\$PATH\"" ;;
esac
if [ "${MEMSTORE_INSTALL:-}" = client ]; then
  echo "next: memstore-client join <your team's server> --org <your org>, then install the Claude Code plugin"
  echo "  (/plugin marketplace add $repo, then /plugin install memstore@memstore)"
else
  echo "next: memstore setup <your team's server> --org <your org>"
  echo "  (signs you in and sets up Claude Code, Codex, Gemini CLI, Cursor and Copilot, whichever are installed)"
fi
