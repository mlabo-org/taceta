#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
APP_NAME="Taceta"
APP_DIR="$REPO_ROOT/dist/$APP_NAME.app"
RELEASE_BINARY="$REPO_ROOT/target/release/taceta"
HOST_BINARY="$REPO_ROOT/target/release/taceta-link-host"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
ICON_SOURCE="$REPO_ROOT/assets/taceta-icon.png"

cd -- "$REPO_ROOT"
PACKAGE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
EXT_VERSION="$(tr -d '[:space:]' < browser-extension/VERSION)"
MANIFEST_VERSION="$(node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync("browser-extension/manifest.json", "utf8")).version)')"
PROTOCOL_VERSION="$(sed -n 's/^pub const PROTOCOL_VERSION: u16 = \([0-9][0-9]*\);/\1/p' src/browser_harness/mod.rs)"
CONTRACT_SCHEMA_VERSION="$(node -e 'process.stdout.write(String(JSON.parse(require("fs").readFileSync("protocol/contract.json", "utf8")).properties.schema_version.const))')"
CONTRACT_PRODUCT_VERSION="$(node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync("protocol/contract.json", "utf8")).properties.product_version.const)')"
CONTRACT_PROTOCOL_VERSION="$(node -e 'process.stdout.write(String(JSON.parse(require("fs").readFileSync("protocol/contract.json", "utf8")).properties.protocol_version.const))')"
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

ICONSET_DIR="$APP_DIR/.taceta-icon.iconset"
mkdir -p "$ICONSET_DIR"
render_icon() {
  local size="$1"
  local name="$2"
  sips -z "$size" "$size" "$ICON_SOURCE" --out "$ICONSET_DIR/$name" >/dev/null
}
render_icon 16 icon_16x16.png
render_icon 32 icon_16x16@2x.png
render_icon 32 icon_32x32.png
render_icon 64 icon_32x32@2x.png
render_icon 128 icon_128x128.png
render_icon 256 icon_128x128@2x.png
render_icon 256 icon_256x256.png
render_icon 512 icon_256x256@2x.png
render_icon 512 icon_512x512.png
render_icon 1024 icon_512x512@2x.png
iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/Taceta.icns"
rm -rf -- "$ICONSET_DIR"

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
