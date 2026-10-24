#!/bin/sh
# Re-sign a built DSH Desktop .app with the local self-signed
# "DSH Desktop Local Signing" certificate.
#
# Why: Tauri's default build is ad-hoc signed, whose designated requirement is
# tied to the binary content hash, so it changes on every rebuild and macOS
# Local Network / privacy grants stop matching. Signing with this stable
# self-signed cert gives a content-independent DR, so a single Local Network
# approval keeps working across rebuilds (on this machine).
#
# The cert lives in ~/Library/Keychains/dsh-sign.keychain-db (password: dshsign).
# It is trusted for code signing in the user trust domain and is NOT for
# distribution (use a Developer ID + notarization for that).
#
# Usage:
#   ./scripts/sign-local.sh "/Applications/DSH Desktop.app"
#   ./scripts/sign-local.sh "target/release/bundle/macos/DSH Desktop.app"
set -eu

APP="${1:?usage: sign-local.sh <path-to-.app>}"
KC="$HOME/Library/Keychains/dsh-sign.keychain-db"
CN="DSH Desktop Local Signing"

if [ ! -d "$APP" ]; then
  echo "not an .app bundle: $APP" >&2
  exit 1
fi

security unlock-keychain -p dshsign "$KC" >/dev/null 2>&1 || true

codesign --force --deep --sign "$CN" --keychain "$KC" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
echo "--- designated requirement ---"
codesign -d -r- "$APP" 2>&1 | tail -1
