#!/usr/bin/env bash
#
# Builds AgentLimit.app — a macOS menu bar app — from the Swift package.
# Requires macOS with the Swift toolchain (Xcode or Command Line Tools).
#
set -euo pipefail

APP_NAME="AgentLimit"
CONFIG="release"

cd "$(dirname "$0")"

echo "Building $APP_NAME ($CONFIG)…"
swift build -c "$CONFIG"

BIN_PATH="$(swift build -c "$CONFIG" --show-bin-path)"
APP_DIR="dist/${APP_NAME}.app"

rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$BIN_PATH/$APP_NAME" "$APP_DIR/Contents/MacOS/$APP_NAME"
cp "App/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "App/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"

# Bundle SwiftPM resource bundles (brand icons) next to the executable so
# Bundle.module can find them.
for bundle in "$BIN_PATH"/*.bundle; do
    [ -e "$bundle" ] && cp -R "$bundle" "$APP_DIR/Contents/Resources/"
done

echo "Built $APP_DIR"
echo
echo "Launch it with:  open \"$APP_DIR\""
echo "Install it with: cp -R \"$APP_DIR\" /Applications/"
