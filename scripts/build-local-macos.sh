#!/bin/sh
# Build a SIGNED macOS dmg for local use, and optionally install it.
#
# Signs the app with the local self-signed "DSH Desktop Local Signing" cert
# during bundling, so the dmg contains a properly signed app whose designated
# requirement is stable (identifier + certificate, not content). That keeps the
# macOS Local Network approval across rebuilds.
#
# Not for distribution: a self-signed cert is only trusted on this machine.
# For public releases use a Developer ID + notarization (release.yml APPLE_* secrets).
#
# Usage:
#   ./scripts/build-local-macos.sh              # build signed dmg
#   ./scripts/build-local-macos.sh --install    # build signed dmg + install to /Applications
set -eu
cd "$(dirname "$0")/.."

INSTALL=0
[ "${1:-}" = "--install" ] && INSTALL=1

KC="$HOME/Library/Keychains/dsh-sign.keychain-db"
security unlock-keychain -p dshsign "$KC" >/dev/null 2>&1 || true

APPLE_SIGNING_IDENTITY="DSH Desktop Local Signing" cargo tauri build --bundles dmg

DMG=$(ls -t target/release/bundle/dmg/*.dmg | head -1)
echo "--- built dmg ---"
ls -la "$DMG"

if [ "$INSTALL" = "1" ]; then
  MNT="/tmp/dsh-dmg-mnt"
  rm -rf "$MNT"; mkdir -p "$MNT"
  hdiutil attach "$DMG" -nobrowse -mountpoint "$MNT" >/dev/null
  APP=$(ls -d "$MNT"/*.app | head -1)
  NAME=$(basename "$APP")
  echo "--- installing $NAME -> /Applications ---"
  pkill -f "$NAME/Contents/MacOS/dsh-desktop" 2>/dev/null || true
  sleep 1
  ditto "$APP" "/Applications/$NAME"
  hdiutil detach "$MNT" >/dev/null
  echo "installed: /Applications/$NAME"
  echo "launch it with: open \"/Applications/$NAME\""
fi
