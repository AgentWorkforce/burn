#!/usr/bin/env bash
#
# Builds, signs, notarizes, and packages AgentLimit.app into a distributable
# DMG. Runs locally or in CI — all credentials come from the environment.
#
# Mirrors the Pear release flow: Developer ID signing + notarization via an
# App Store Connect API key, producing a stable-named arm64 DMG.
#
# Signing identity (auto-detected from the keychain if unset):
#   SIGNING_IDENTITY     "Developer ID Application: Your Name (TEAMID)"
#
# Version (optional — stamped into Info.plist before building):
#   VERSION              e.g. 2026.6.1
#
# Notarization — App Store Connect API key (preferred, same as Pear)…
#   APPLE_API_KEY        path to the AuthKey_XXXX.p8 file
#   APPLE_API_KEY_ID     the key ID
#   APPLE_API_ISSUER     the issuer UUID
# …or a stored notarytool profile:
#   NOTARY_PROFILE       name from `xcrun notarytool store-credentials`
# …or an Apple ID + app-specific password:
#   APPLE_ID / APPLE_TEAM_ID / APPLE_APP_PASSWORD
#
set -euo pipefail

APP_NAME="AgentLimit"
ARCH="$(uname -m)"            # arm64 on Apple Silicon CI/runners
APP_DIR="dist/${APP_NAME}.app"
DMG="dist/${APP_NAME}-${ARCH}.dmg"
ZIP="dist/${APP_NAME}-notarize.zip"

cd "$(dirname "$0")"

# 0. Stamp the version into Info.plist (transient — never committed).
if [[ -n "${VERSION:-}" ]]; then
    echo "Stamping version ${VERSION}…"
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${VERSION}" App/Info.plist
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${VERSION}" App/Info.plist
fi

# 1. Build the unsigned .app.
./build.sh

# 2. Resolve the signing identity (auto-detect a Developer ID Application cert).
if [[ -z "${SIGNING_IDENTITY:-}" ]]; then
    SIGNING_IDENTITY="$(security find-identity -v -p codesigning \
        | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
fi
: "${SIGNING_IDENTITY:?No 'Developer ID Application' identity found in the keychain}"

# 3. Code-sign with a hardened runtime (required for notarization). Sign nested
#    bundles (the SwiftPM resource bundle) before the outer app.
echo "Signing with: ${SIGNING_IDENTITY}"
while IFS= read -r -d '' bundle; do
    codesign --force --timestamp --options runtime --sign "${SIGNING_IDENTITY}" "$bundle"
done < <(find "${APP_DIR}/Contents/Resources" -maxdepth 1 -name "*.bundle" -print0)

# Sign the bundled burn helper (if present) before the main executable.
if [[ -f "${APP_DIR}/Contents/MacOS/burn" ]]; then
    codesign --force --timestamp --options runtime --sign "${SIGNING_IDENTITY}" "${APP_DIR}/Contents/MacOS/burn"
fi

codesign --force --timestamp --options runtime --sign "${SIGNING_IDENTITY}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
codesign --force --timestamp --options runtime --sign "${SIGNING_IDENTITY}" "${APP_DIR}"
codesign --verify --deep --strict --verbose=2 "${APP_DIR}"

# 4. Notarize. Apple notarizes a zip/dmg; we submit a zip and wait.
echo "Submitting for notarization (this can take a few minutes)…"
/usr/bin/ditto -c -k --keepParent "${APP_DIR}" "${ZIP}"
if [[ -n "${APPLE_API_KEY:-}" ]]; then
    : "${APPLE_API_KEY_ID:?}"; : "${APPLE_API_ISSUER:?}"
    xcrun notarytool submit "${ZIP}" \
        --key "${APPLE_API_KEY}" \
        --key-id "${APPLE_API_KEY_ID}" \
        --issuer "${APPLE_API_ISSUER}" \
        --wait
elif [[ -n "${NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "${ZIP}" --keychain-profile "${NOTARY_PROFILE}" --wait
else
    : "${APPLE_ID:?Set APPLE_API_KEY, NOTARY_PROFILE, or APPLE_ID}"
    : "${APPLE_TEAM_ID:?}"; : "${APPLE_APP_PASSWORD:?}"
    xcrun notarytool submit "${ZIP}" \
        --apple-id "${APPLE_ID}" \
        --team-id "${APPLE_TEAM_ID}" \
        --password "${APPLE_APP_PASSWORD}" \
        --wait
fi
rm -f "${ZIP}"

# 5. Staple the ticket onto the .app so Gatekeeper passes it offline.
xcrun stapler staple "${APP_DIR}"

# 6. Package a DMG (app + /Applications drop target) from the stapled app.
echo "Building ${DMG}…"
rm -f "${DMG}"
STAGING="dist/dmg-staging"
rm -rf "${STAGING}"; mkdir -p "${STAGING}"
cp -R "${APP_DIR}" "${STAGING}/"
ln -s /Applications "${STAGING}/Applications"
hdiutil create -volname "${APP_NAME}" -srcfolder "${STAGING}" -ov -format UDZO "${DMG}"
rm -rf "${STAGING}"

echo
echo "Built ${DMG}"
