#!/usr/bin/env bash
# Signs macOS release binaries with the Developer ID certificate and notarizes them with Apple, so Gatekeeper runs
# them with no warning and install.sh leaves their quarantine flag alone. Run on the macOS build, after strip (a
# stripped binary loses its signature) and before the build is checked and packaged:
#
#   .github/scripts/macos-sign.sh target/release/ampup-memory target/release/ampup-memory-client
#
# From the environment (the release workflow passes the repository's secrets of the same names):
#
#   MACOS_SIGNING_CERT_P12        the Developer ID Application certificate and its private key, a .p12, base64
#   MACOS_SIGNING_CERT_PASSWORD   the .p12's password
#   APPLE_NOTARY_KEY_P8           an App Store Connect API key for notarytool, the AuthKey_<id>.p8, base64
#   APPLE_NOTARY_KEY_ID           that key's id
#   APPLE_NOTARY_ISSUER_ID        its issuer id
#
# None set: the binaries stay unsigned, as before signing existed, and it says so. Some but not all: it fails.
#
# Each binary is signed with the hardened runtime and a secure timestamp, checked (a valid, strict signature from a
# Developer ID, nothing linked from outside the system: such a library would ship unsigned), then all are notarized
# in one zip. A rejection fails it, with notarytool's log. A bare Mach-O can't be stapled: Gatekeeper looks the
# notarization up online on first run. The certificate goes into a keychain of its own, deleted on exit with the key
# files.
set -euo pipefail

[ "$#" -gt 0 ] || { echo "usage: $0 <binary>..." >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "macos-sign: not macOS, nothing to sign"; exit 0; }

have="" missing=""
for v in MACOS_SIGNING_CERT_P12 MACOS_SIGNING_CERT_PASSWORD APPLE_NOTARY_KEY_P8 APPLE_NOTARY_KEY_ID APPLE_NOTARY_ISSUER_ID; do
  if [ -n "${!v:-}" ]; then have="$have $v"; else missing="$missing $v"; fi
done
if [ -z "$have" ]; then
  echo "::notice::macOS signing secrets aren't set: these binaries ship unsigned and not notarized (install.sh clears the quarantine flag)"
  exit 0
fi
if [ -n "$missing" ]; then
  echo "::error::macOS signing is half set up: missing$missing (set:$have)"
  exit 1
fi

tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/macos-sign.XXXXXX")
kc=$tmp/signing.keychain-db
cleanup() {
  security delete-keychain "$kc" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# The certificate, in a keychain of its own that codesign can use with no prompt.
kcpass=$(openssl rand -hex 24)
security create-keychain -p "$kcpass" "$kc"
security set-keychain-settings -lut 21600 "$kc"
security unlock-keychain -p "$kcpass" "$kc"
printf '%s' "$MACOS_SIGNING_CERT_P12" | base64 --decode > "$tmp/cert.p12"
security import "$tmp/cert.p12" -k "$kc" -f pkcs12 -P "$MACOS_SIGNING_CERT_PASSWORD" -T /usr/bin/codesign > /dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$kcpass" "$kc" > /dev/null
# On the search list too (ahead of the others, which stay): codesign finds the certificate's chain there.
# shellcheck disable=SC2046 # one keychain path per word
security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
identity=$(security find-identity -v -p codesigning "$kc" | awk '/"Developer ID Application: / { print $2; exit }')
if [ -z "$identity" ]; then
  echo "::error::MACOS_SIGNING_CERT_P12 holds no valid Developer ID Application identity:"
  security find-identity -v -p codesigning "$kc"
  exit 1
fi
security find-identity -v -p codesigning "$kc" | grep "$identity"

for b in "$@"; do
  # Anything linked from outside the system would ship beside the binary, unsigned.
  extra=$(otool -L "$b" | tail -n +2 | awk '{ print $1 }' | grep -vE '^(/System/Library/|/usr/lib/)' || true)
  if [ -n "$extra" ]; then
    echo "::error::$b links libraries the release doesn't sign: $extra"
    exit 1
  fi
  codesign --force --sign "$identity" --keychain "$kc" --options runtime --timestamp \
    --identifier "ai.ampup.$(basename "$b")" "$b"
  codesign --verify --strict --verbose=2 "$b"
  # Signed by a Developer ID, with the hardened runtime: what notarization requires.
  codesign --verify --strict -R='anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13]' "$b"
  info=$(codesign -dvv "$b" 2>&1)
  echo "$info" | grep -E '^(Identifier|Authority=Developer ID Application|Timestamp|TeamIdentifier)'
  echo "$info" | grep -q 'flags=.*runtime' || { echo "::error::$b is signed without the hardened runtime"; exit 1; }
done

# Notarized together, in one zip (notarytool takes a zip, a .dmg or a .pkg).
printf '%s' "$APPLE_NOTARY_KEY_P8" | base64 --decode > "$tmp/AuthKey.p8"
mkdir "$tmp/payload"
cp "$@" "$tmp/payload/"
ditto -c -k "$tmp/payload" "$tmp/notarize.zip"
auth=(--key "$tmp/AuthKey.p8" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID")
result=$(xcrun notarytool submit "$tmp/notarize.zip" "${auth[@]}" --wait --timeout 30m --output-format json || true)
field() { printf '%s' "$result" | python3 -c "import json, sys; print(json.load(sys.stdin).get('$1', ''))" 2>/dev/null || true; }
id=$(field id) status=$(field status)
echo "notarization ${id:-(no submission id)}: ${status:-(no status)}"
if [ -n "$id" ]; then
  xcrun notarytool log "$id" "${auth[@]}" || true
fi
if [ "$status" != Accepted ]; then
  echo "::error::Apple didn't notarize the binaries (${status:-no status}): ${result:-notarytool gave no answer}"
  exit 1
fi

# What Gatekeeper will see on first run: the notarization, looked up online. Apple can take a minute to serve it,
# so a miss here is a warning, not a failure (notarytool has already accepted the binaries).
for b in "$@"; do
  ok=""
  for _ in 1 2 3 4 5 6; do
    if codesign --verify --strict --check-notarization -R=notarized "$b" 2> /dev/null; then ok=1; break; fi
    sleep 10
  done
  if [ -n "$ok" ]; then
    echo "$b: signed and notarized"
  else
    echo "::warning::$b: notarized, but Apple's servers don't report it yet"
  fi
done
