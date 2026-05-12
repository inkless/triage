#!/usr/bin/env bash
# Compile main.swift into triage-notify.app, a minimal .app bundle that can
# request and use UNUserNotificationCenter (the modern macOS notification API).
#
# Idempotent: rebuilds the binary every run; the .app dir gets re-populated
# in place. No external deps beyond Xcode CLI tools (swiftc + ad-hoc codesign).
#
# Also bakes AppIcon.png into a multi-resolution .icns using sips + iconutil
# (both ship with macOS). The PNG at scripts/triage-notify/AppIcon.png is
# the canonical source — swap that file to change the icon, no build flag
# changes needed.

set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
APP="$DIR/triage-notify.app"
MACOS_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
BIN="$MACOS_DIR/triage-notify"
PLIST_SRC="$DIR/Info.plist"
PLIST_DST="$APP/Contents/Info.plist"
SRC="$DIR/main.swift"
ICON_SRC="$DIR/AppIcon.png"
ICON_DST="$RES_DIR/AppIcon.icns"

if ! command -v swiftc >/dev/null 2>&1; then
  echo "build.sh: swiftc not found — install Xcode Command Line Tools (xcode-select --install)" >&2
  exit 1
fi

mkdir -p "$MACOS_DIR" "$RES_DIR"
cp "$PLIST_SRC" "$PLIST_DST"
swiftc -O -o "$BIN" "$SRC"

# Generate AppIcon.icns from the source PNG. Best-effort: missing source
# or a sips/iconutil failure prints a warning but doesn't abort the build
# — without the .icns, macOS falls back to the generic app icon.
if [[ -f "$ICON_SRC" ]] && command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
  ICONSET=$(mktemp -d)/AppIcon.iconset
  mkdir -p "$ICONSET"
  # Sizes per Apple's High Resolution Guidelines for macOS .icns.
  # `sips -z <h> <w>` resizes preserving aspect; AppIcon.png is square.
  for spec in \
      "16 icon_16x16.png" \
      "32 icon_16x16@2x.png" \
      "32 icon_32x32.png" \
      "64 icon_32x32@2x.png" \
      "128 icon_128x128.png" \
      "256 icon_128x128@2x.png" \
      "256 icon_256x256.png" \
      "512 icon_256x256@2x.png" \
      "512 icon_512x512.png" \
      "1024 icon_512x512@2x.png"; do
    size=${spec%% *}
    name=${spec#* }
    sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/$name" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$ICON_DST"
  rm -rf "$(dirname "$ICONSET")"
else
  echo "build.sh: skipping icon (need $ICON_SRC + sips + iconutil)" >&2
fi

# Ad-hoc codesign so the bundle has a stable identity for macOS's notification
# permission ledger. Without this, every build produces a "new" app from
# Gatekeeper's POV and the user gets re-prompted for permission. Re-sign
# AFTER icon insertion so the signature covers the new Resources/ tree.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $APP"
