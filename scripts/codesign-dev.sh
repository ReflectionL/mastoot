#!/bin/sh
# Sign the release binary with a stable local identity so macOS Keychain
# keeps trusting it across rebuilds (an ad-hoc signature changes with
# every build, which re-triggers the "mastoot wants to access keychain"
# prompt).
#
# One-time setup, done by you in Keychain Access (not scripted — it
# touches your login keychain):
#   Keychain Access → Certificate Assistant → Create a Certificate…
#   Name: mastoot-dev · Identity Type: Self Signed Root ·
#   Certificate Type: Code Signing
# Then: cargo build --release && scripts/codesign-dev.sh
set -eu
IDENTITY="${MASTOOT_SIGN_IDENTITY:-mastoot-dev}"
BIN="${1:-target/release/mastoot}"
if ! security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
  echo "no code-signing identity named '$IDENTITY' — see the comment at the top of this script" >&2
  exit 1
fi
codesign --force --sign "$IDENTITY" "$BIN"
echo "signed $BIN with $IDENTITY"
