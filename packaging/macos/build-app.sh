#!/usr/bin/env bash
# Build Spock.app = SwiftUI menu bar + embedded Rust proxy binary.
# Usage: ./packaging/macos/build-app.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
OUT="$ROOT/dist"
APP="$OUT/Spock.app"
MACOS_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
SWIFT_SRC="$ROOT/macos/SpockApp"

echo "==> cargo build --release (proxy)"
cargo build --release

PROXY_BIN="$ROOT/target/release/spock"
if [[ ! -x "$PROXY_BIN" ]]; then
  echo "missing $PROXY_BIN" >&2
  exit 1
fi

echo "==> compile SwiftUI app"
mkdir -p "$MACOS_DIR" "$RES_DIR"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RES_DIR"

# Compile all Swift sources into one binary named Spock
swiftc \
  -O \
  -target arm64-apple-macosx14.0 \
  -sdk "$(xcrun --show-sdk-path)" \
  -framework SwiftUI \
  -framework AppKit \
  -parse-as-library \
  -module-name Spock \
  -o "$MACOS_DIR/Spock" \
  "$SWIFT_SRC/SpockApp.swift" \
  "$SWIFT_SRC/AppModel.swift" \
  "$SWIFT_SRC/SettingsView.swift" \
  "$SWIFT_SRC/ChatView.swift"

# Embed Rust proxy next to the Swift binary
cp "$PROXY_BIN" "$MACOS_DIR/spock-proxy"
chmod +x "$MACOS_DIR/Spock" "$MACOS_DIR/spock-proxy"

# Info.plist — LSUIElement menu bar app, executable Spock
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>Spock</string>
	<key>CFBundleIdentifier</key>
	<string>com.satindergrewal.spock</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Spock</string>
	<key>CFBundleDisplayName</key>
	<string>Spock</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Copyright © 2026 Satinder Grewal</string>
</dict>
</plist>
EOF

# App icon (Vulcan salute)
if [[ -f "$ROOT/packaging/macos/AppIcon.icns" ]]; then
  cp "$ROOT/packaging/macos/AppIcon.icns" "$RES_DIR/AppIcon.icns"
fi

# PkgInfo
echo -n "APPL????" > "$APP/Contents/PkgInfo"

if command -v codesign >/dev/null; then
  codesign --force --deep --sign - "$APP" 2>/dev/null || true
fi

echo "built $APP"
echo "  open dist/Spock.app"
echo "  Menu bar: Settings… · Chat… · profiles"
