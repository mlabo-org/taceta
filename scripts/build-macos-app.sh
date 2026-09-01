#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
APP_NAME="Taceta"
APP_DIR="$REPO_ROOT/dist/$APP_NAME.app"
RELEASE_BINARY="$REPO_ROOT/target/release/taceta"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

cd -- "$REPO_ROOT"
cargo build --release

test -x "$RELEASE_BINARY"
rm -rf -- "$APP_DIR"
mkdir -p -- "$MACOS_DIR" "$RESOURCES_DIR"
install -m 0755 "$RELEASE_BINARY" "$MACOS_DIR/$APP_NAME"

cat > "$CONTENTS_DIR/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key>
	<string>Taceta</string>
	<key>CFBundleExecutable</key>
	<string>Taceta</string>
	<key>CFBundleIdentifier</key>
	<string>org.mlabo.taceta</string>
	<key>CFBundleName</key>
	<string>Taceta</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundleVersion</key>
	<string>0.1.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
</dict>
</plist>
PLIST

printf 'Built %s\n' "$APP_DIR"
