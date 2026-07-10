#!/usr/bin/env bash
# Build Spock.app from a release binary.
# Usage: packaging/macos/bundle.sh [path/to/spock] [out/dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${1:-$ROOT/target/release/spock}"
OUT="${2:-$ROOT/dist}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
APP_NAME="Spock"
APP="$OUT/${APP_NAME}.app"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found or not executable: $BIN" >&2
  echo "build first: cargo build --release --features tray" >&2
  exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/spock"
chmod +x "$APP/Contents/MacOS/spock"
cp "$ROOT/packaging/macos/Info.plist" "$APP/Contents/Info.plist"
# Patch version into Info.plist
if command -v sed >/dev/null; then
  sed -i.bak "s#<string>0.1.0</string>#<string>${VERSION}</string>#g" "$APP/Contents/Info.plist"
  rm -f "$APP/Contents/Info.plist.bak"
fi

# Ad-hoc codesign (Gatekeeper may still warn without Developer ID)
if command -v codesign >/dev/null; then
  codesign --force --deep --sign - "$APP" 2>/dev/null || true
fi

echo "built $APP"
echo "  drag to /Applications, then right-click → Open on first launch if Gatekeeper blocks"
