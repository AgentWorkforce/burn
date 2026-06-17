#!/usr/bin/env bash
#
# Builds BurnOSX.app — the Burn macOS menu bar app — from the Swift package.
# Requires macOS with the Swift toolchain (Xcode or Command Line Tools).
#
set -euo pipefail

TARGET="Burn"        # SwiftPM executable target (internal module + binary name)
APP_NAME="BurnOSX"   # product: .app bundle, Contents/MacOS executable, DMG
CONFIG="release"

cd "$(dirname "$0")"

echo "Building $APP_NAME ($CONFIG)…"
swift build -c "$CONFIG"

BIN_PATH="$(swift build -c "$CONFIG" --show-bin-path)"
APP_DIR="dist/${APP_NAME}.app"

rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$BIN_PATH/$TARGET" "$APP_DIR/Contents/MacOS/$APP_NAME"
cp "App/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "App/AppIcon.icns" "$APP_DIR/Contents/Resources/AppIcon.icns"

# Bundle SwiftPM resource bundles (brand icons) next to the executable so
# Bundle.module can find them.
for bundle in "$BIN_PATH"/*.bundle; do
    [ -e "$bundle" ] && cp -R "$bundle" "$APP_DIR/Contents/Resources/"
done

# Bundle the native `burn` CLI (self-contained Rust binary from this repo's
# relayburn-cli) into Contents/MacOS so spend works with no separate install.
# No name clash: the app executable is `BurnOSX`, the CLI is `burn`. Skipped if
# cargo is unavailable — the app then falls back to a `burn` on PATH.
REPO_ROOT="$(cd ../.. && pwd)"
if command -v cargo >/dev/null 2>&1; then
    echo "Building burn helper (cargo)…"
    ( cd "$REPO_ROOT" && cargo build --release -p relayburn-cli )
    cp "$REPO_ROOT/target/release/burn" "$APP_DIR/Contents/MacOS/burn"
else
    echo "warning: cargo not found — skipping bundled burn helper (spend will"
    echo "         require a 'burn' on PATH at runtime)."
fi

echo "Built $APP_DIR"
echo
echo "Launch it with:  open \"$APP_DIR\""
echo "Install it with: cp -R \"$APP_DIR\" /Applications/"
