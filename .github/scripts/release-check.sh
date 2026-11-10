#!/usr/bin/env bash
# An ampup-memory binary as a user installs it, checked end to end: it serves a local store, and as a client of that
# server (`ampup-memory doctor`, the SessionStart hook, `ampup-memory mcp` over stdio) it finds it up, opens a session and
# lists the MCP tools. The release workflow runs it on each build and again on what install.sh installs.
#
#   .github/scripts/release-check.sh <ampup-memory binary> [<ampup-memory-client binary>]
#
# With an ampup-memory-client binary, the client checks run on it instead (what the Claude Code plugin runs).
#
# Everything it writes is under a temp directory; the developer's ~/.ampup-memory (or ~/.memstore) is never read or
# written.
set -uo pipefail
BIN=${1:?usage: .github/scripts/release-check.sh <ampup-memory binary>}
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
CLIENT=${2:-$BIN}
CLIENT=$(cd "$(dirname "$CLIENT")" && pwd)/$(basename "$CLIENT")
work=$(mktemp -d)
# Each setting by both names: the server may be a release from before the rename (a client release on macOS is
# checked against the newest server release built for it), which reads only MEMSTORE_*.
export AMPUP_MEMORY_CONFIG_DIR="$work/config" MEMSTORE_CONFIG_DIR="$work/config" TMPDIR="$work"
unset AMPUP_MEMORY_TOKEN AMPUP_MEMORY_SCOPE AMPUP_MEMORY_CONTEXT MEMSTORE_TOKEN MEMSTORE_SCOPE MEMSTORE_CONTEXT
fails=0
ok() { printf 'pass  %s\n' "$1"; }
no() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }

port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
metrics=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
export AMPUP_MEMORY_URL="http://127.0.0.1:$port" MEMSTORE_URL="http://127.0.0.1:$port"

# stdout only: onnxruntime warns on stderr on some CPUs ("Unknown CPU vendor" on ARM runners)
version=$("$BIN" --version 2>/dev/null)
case "$version" in ampup-memory\ * | memstore\ *) ok "$version" ;; *) no "--version: $version" ;; esac

AMPUP_MEMORY_METRICS_PORT=$metrics MEMSTORE_METRICS_PORT=$metrics "$BIN" serve --local "$work/store" org "$port" >"$work/serve.log" 2>&1 &
server=$!
# Gone before this exits, the server and every copy a hook left running (the start's detached opening): the
# workflow strips the binary next, which Linux refuses while it runs ("Text file busy").
cleanup() {
  kill $server 2>/dev/null
  wait $server 2>/dev/null
  for b in "$BIN" "$CLIENT"; do
    for _ in $(seq 1 20); do
      pgrep -f "^$b " >/dev/null || break
      sleep 0.5
    done
    pkill -9 -f "^$b " 2>/dev/null
  done
  rm -rf "$work"
}
trap cleanup EXIT
for _ in $(seq 1 120); do
  curl -sf "$AMPUP_MEMORY_URL/" >/dev/null && break
  kill -0 $server 2>/dev/null || break
  sleep 0.5
done
curl -sf "$AMPUP_MEMORY_URL/" >/dev/null && ok "serve --local is up on $port" || {
  no "serve --local never came up: $(tail -5 "$work/serve.log")"
  exit 1
}

out=$("$CLIENT" doctor 2>&1)
printf '%s' "$out" | grep -q "AmpUp Memory at $AMPUP_MEMORY_URL: up" && ok "doctor finds it up" || no "doctor: $out"

# The start opens the session in a copy of the hook; the first prompt once it is open hands the agent its line.
printf '{"session_id":"release-check-%s","cwd":"%s","hook_event_name":"SessionStart"}' "$$" "$work" |
  "$CLIENT" hook session-start >/dev/null 2>&1
for _ in $(seq 1 60); do
  [ -s "$work/memstore-hooks/session-release-check-$$" ] && break
  sleep 0.5
done
out=$(printf '{"session_id":"release-check-%s","cwd":"%s","hook_event_name":"UserPromptSubmit","prompt":"hello"}' \
  "$$" "$work" | "$CLIENT" hook prompt 2>&1)
printf '%s' "$out" | grep -q "AmpUp Memory session: " && ok "the SessionStart hook opens a session, the first prompt names it" ||
  no "session-start, then prompt: $out"

out=$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | "$CLIENT" mcp 2>&1)
printf '%s' "$out" | grep -q '"start_session"' && ok "ampup-memory mcp lists the tools over stdio" ||
  no "ampup-memory mcp: $out"

[ "$fails" -eq 0 ] && echo "all checks passed" || echo "$fails failed"
exit "$fails"
