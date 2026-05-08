#!/usr/bin/env bash
# Compile main.swift into triage-notify.app, a minimal .app bundle that can
# request and use UNUserNotificationCenter (the modern macOS notification API).
#
# Idempotent: rebuilds the binary every run; the .app dir gets re-populated
# in place. No external deps beyond Xcode CLI tools (swiftc + ad-hoc codesign).

set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
APP="$DIR/triage-notify.app"
MACOS_DIR="$APP/Contents/MacOS"
BIN="$MACOS_DIR/triage-notify"
PLIST_SRC="$DIR/Info.plist"
PLIST_DST="$APP/Contents/Info.plist"
SRC="$DIR/main.swift"

if ! command -v swiftc >/dev/null 2>&1; then
  echo "build.sh: swiftc not found — install Xcode Command Line Tools (xcode-select --install)" >&2
  exit 1
fi

mkdir -p "$MACOS_DIR"
cp "$PLIST_SRC" "$PLIST_DST"
swiftc -O -o "$BIN" "$SRC"

# Ad-hoc codesign so the bundle has a stable identity for macOS's notification
# permission ledger. Without this, every build produces a "new" app from
# Gatekeeper's POV and the user gets re-prompted for permission.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $APP"
