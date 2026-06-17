#!/usr/bin/env bash
#
# Regenerates App/AppIcon.icns from the flame artwork. Run after changing
# scripts/generate-icon.swift. Requires macOS (swift, sips, iconutil).
#
set -euo pipefail
cd "$(dirname "$0")/.."

MASTER="$(mktemp -d)/icon_1024.png"
ICONSET="$(mktemp -d)/AppIcon.iconset"

swift scripts/generate-icon.swift "$MASTER"
mkdir -p "$ICONSET"

# name:size pairs for a macOS .icns
for entry in \
    "icon_16x16:16" "icon_16x16@2x:32" \
    "icon_32x32:32" "icon_32x32@2x:64" \
    "icon_128x128:128" "icon_128x128@2x:256" \
    "icon_256x256:256" "icon_256x256@2x:512" \
    "icon_512x512:512" "icon_512x512@2x:1024"; do
    name="${entry%%:*}"; px="${entry##*:}"
    sips -z "$px" "$px" "$MASTER" --out "$ICONSET/$name.png" >/dev/null
done

iconutil -c icns "$ICONSET" -o App/AppIcon.icns
echo "Wrote App/AppIcon.icns"
