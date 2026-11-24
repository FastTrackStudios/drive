#!/usr/bin/env bash
# Try the File Provider extension without the whole app: a bundle whose
# executable is the domain registrar, with the extension in PlugIns,
# signed, launched once through LaunchServices (the step that makes the
# system take its extension into account) and asked to register Drive.
# Finder then shows "Drive" under Locations, served by the agent that is
# running (its socket in the app group's container).
#
#   KEYCHAIN_PW=… bash macos/try-fileprovider.sh [sync|list|clear|signal|ping]
#
# A harness, not the product: it has no UI and is meant to be thrown away
# (`clear` unregisters).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/target/fileprovider}"
APP="$OUT_DIR/Drive.app"
KEYCHAIN="${KEYCHAIN:-fts-build.keychain}"

bash "$HERE/build-fileprovider.sh"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/PlugIns"
cp "$OUT_DIR/DriveFileProviderDomains" "$APP/Contents/MacOS/Drive"
cp -R "$OUT_DIR/DriveFileProvider.appex" "$APP/Contents/PlugIns/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>Drive</string>
    <key>CFBundleIdentifier</key><string>app.fasttrackstudio.drive</string>
    <key>CFBundleName</key><string>Drive</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
</dict>
</plist>
PLIST

SIGN_ID="${SIGN_ID:-$(security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}"
# Inside out: signing a nested bundle after the outer one breaks it.
codesign --force --timestamp --options runtime --keychain "$KEYCHAIN" \
    --entitlements "$HERE/FileProvider/DriveFileProvider.entitlements" \
    --sign "$SIGN_ID" "$APP/Contents/PlugIns/DriveFileProvider.appex"
codesign --force --timestamp --options runtime --keychain "$KEYCHAIN" \
    --entitlements "$HERE/Drive.entitlements" --sign "$SIGN_ID" "$APP"
codesign --verify --deep --strict "$APP"

# Listed (pluginkit) is not usable: the system takes an app's extensions
# into account once the app is launched through LaunchServices.
pluginkit -a "$APP/Contents/PlugIns/DriveFileProvider.appex"
open -a "$APP" --args list || true
sleep 3
"$APP/Contents/MacOS/Drive" "${1:-sync}"
echo "harness: $APP  (\"$APP/Contents/MacOS/Drive\" list|clear|signal|ping)"
