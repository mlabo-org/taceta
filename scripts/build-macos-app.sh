#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
APP_NAME="Taceta"
APP_DIR="$REPO_ROOT/dist/$APP_NAME.app"
BUILD_OUTPUT_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
RELEASE_BINARY="$BUILD_OUTPUT_DIR/release/taceta"
HOST_BINARY="$BUILD_OUTPUT_DIR/release/taceta-link-host"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
ICON_SOURCE="$REPO_ROOT/assets/Taceta.icns"

cd -- "$REPO_ROOT"
PACKAGE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
EXT_VERSION="$(tr -d '[:space:]' < browser-extension/VERSION)"
MANIFEST_VERSION="$(/usr/bin/plutil -extract version raw -o - browser-extension/manifest.json)"
PROTOCOL_VERSION="$(sed -n 's/^pub const PROTOCOL_VERSION: u16 = \([0-9][0-9]*\);/\1/p' src/browser_harness/mod.rs)"
CONTRACT_SCHEMA_VERSION="$(/usr/bin/plutil -extract properties.schema_version.const raw -o - protocol/contract.json)"
CONTRACT_PRODUCT_VERSION="$(/usr/bin/plutil -extract properties.product_version.const raw -o - protocol/contract.json)"
CONTRACT_PROTOCOL_VERSION="$(/usr/bin/plutil -extract properties.protocol_version.const raw -o - protocol/contract.json)"
test -n "$PACKAGE_VERSION" && test "$PACKAGE_VERSION" = "$EXT_VERSION" \
  && test "$EXT_VERSION" = "$MANIFEST_VERSION" \
  && test "$PACKAGE_VERSION" = "$CONTRACT_PRODUCT_VERSION" \
  && test "$CONTRACT_SCHEMA_VERSION" = "1" \
  && test "$PROTOCOL_VERSION" = "$CONTRACT_PROTOCOL_VERSION" \
  || { echo "Taceta Link version/protocol mismatch" >&2; exit 1; }
cargo build --release

test -x "$RELEASE_BINARY"
test -x "$HOST_BINARY"
test -f "$ICON_SOURCE"
rm -rf -- "$APP_DIR"
mkdir -p -- "$MACOS_DIR" "$RESOURCES_DIR"
install -m 0755 "$RELEASE_BINARY" "$MACOS_DIR/$APP_NAME"
install -m 0755 "$HOST_BINARY" "$MACOS_DIR/taceta-link-host"
mkdir -p "$RESOURCES_DIR/TacetaLink"
cp -R browser-extension/. "$RESOURCES_DIR/TacetaLink/"
install -m 0644 "$ICON_SOURCE" "$RESOURCES_DIR/Taceta.icns"

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
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
	<key>CFBundleIconFile</key>
	<string>Taceta.icns</string>
	<key>CFBundleName</key>
	<string>Taceta</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$PACKAGE_VERSION</string>
	<key>CFBundleVersion</key>
	<string>$PACKAGE_VERSION</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
</dict>
</plist>
PLIST

printf 'Built %s\n' "$APP_DIR"
