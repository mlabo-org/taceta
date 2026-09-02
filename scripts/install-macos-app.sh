#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
APP_NAME="Taceta.app"
SOURCE_APP="$REPO_ROOT/dist/$APP_NAME"
INSTALL_DIR="${HOME:?HOME is required}/Applications"
BUILD_FIRST=true

usage() {
  cat <<'USAGE'
Usage: ./scripts/install-macos-app.sh [--no-build] [--install-dir DIRECTORY]

Builds Taceta and installs the app bundle for the current user.

Options:
  --no-build               Install the existing dist/Taceta.app.
  --install-dir DIRECTORY  Destination directory (default: $HOME/Applications).
  -h, --help               Show this help.
USAGE
}

while (($# > 0)); do
  case "$1" in
    --no-build)
      BUILD_FIRST=false
      shift
      ;;
    --install-dir)
      (($# >= 2)) || { echo "--install-dir requires a directory" >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$BUILD_FIRST" == true ]]; then
  "$SCRIPT_DIR/build-macos-app.sh"
fi

[[ -d "$SOURCE_APP" ]] || {
  echo "Missing $SOURCE_APP; run ./scripts/build-macos-app.sh first." >&2
  exit 1
}

mkdir -p -- "$INSTALL_DIR"
INSTALL_DIR="$(cd -- "$INSTALL_DIR" && pwd -P)"
TARGET_APP="$INSTALL_DIR/$APP_NAME"
STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/taceta-install.XXXXXX")"
STAGED_APP="$STAGING_DIR/$APP_NAME"
PREVIOUS_APP="$STAGING_DIR/Taceta.previous.app"

cleanup() {
  if [[ -d "$PREVIOUS_APP" && ! -e "$TARGET_APP" ]]; then
    mv -- "$PREVIOUS_APP" "$TARGET_APP" || true
  fi
  rm -rf -- "$STAGING_DIR"
}
trap cleanup EXIT

/usr/bin/ditto "$SOURCE_APP" "$STAGED_APP"
[[ -x "$STAGED_APP/Contents/MacOS/Taceta" ]] || {
  echo "Staged Taceta app is incomplete." >&2
  exit 1
}

if [[ -e "$TARGET_APP" ]]; then
  [[ -d "$TARGET_APP" && "$TARGET_APP" == */Taceta.app ]] || {
    echo "Refusing to replace unexpected target: $TARGET_APP" >&2
    exit 1
  }
  mv -- "$TARGET_APP" "$PREVIOUS_APP"
fi

if ! mv -- "$STAGED_APP" "$TARGET_APP"; then
  if [[ -d "$PREVIOUS_APP" && ! -e "$TARGET_APP" ]]; then
    mv -- "$PREVIOUS_APP" "$TARGET_APP"
  fi
  echo "Taceta installation failed; the previous app was restored when possible." >&2
  exit 1
fi

printf 'Installed %s\n' "$TARGET_APP"
