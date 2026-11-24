#!/usr/bin/env bash
# Drive.app, the desktop app for a release: apps/drive (feature `desktop`,
# in the menu bar) bundled by dx, with the File Provider extension in
# PlugIns and its registrar beside the app's program, signed with the
# Developer ID, zipped as OUT/Drive-<version>-<target>.zip.
#
#   bash macos/bundle-app.sh [TARGET] [OUT]     (default: this Mac's, ./dist)
#
# Env:
#   DX           dx matching the dioxus crate (default: `dx` on PATH)
#   KEYCHAIN, KEYCHAIN_PW, SIGN_ID   as in build-fileprovider.sh
#   NOTARIZE     1 (default): notarize and staple, with the App Store
#                Connect key in ~/.appstoreconnect/config.env; 0 signs only
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
target="${1:-$(uname -m | sed 's/arm64/aarch64/')-apple-darwin}"
out="$(mkdir -p "${2:-dist}" && cd "${2:-dist}" && pwd)"
DX="${DX:-dx}"
KEYCHAIN="${KEYCHAIN:-fts-build.keychain}"
work="${CARGO_TARGET_DIR:-$ROOT/target}/drive-app-bundle/$target"

[ "$(uname -s)" = Darwin ] || { echo "Drive.app is built on a Mac" >&2; exit 1; }

# dx writes the asset!() table into the binary; only the version of the
# dioxus crate in Cargo.lock can.
want="$(awk '/^name = "dioxus"$/{getline; gsub(/version = |"/, ""); print; exit}' "$ROOT/Cargo.lock")"
have="$("$DX" --version | awk '{print $2}')"
[ "$want" = "$have" ] || { echo "dx $have, but the dioxus crate is $want" >&2; exit 1; }

version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' "$ROOT/Cargo.toml" | head -1)"

echo "── the File Provider extension and its registrar ($target) ─────"
TARGETS="$target" OUT_DIR="$work/fileprovider" bash "$HERE/build-fileprovider.sh"

echo "── the app (dx bundle) ─────────────────────────────────────────"
rm -rf "$work/bundle"
rustup target add "$target" >/dev/null 2>&1 || true
(cd "$ROOT/apps/drive" && "$DX" bundle --release --platform macos --renderer native \
    --no-default-features --features desktop --target "$target" \
    --package-types macos --out-dir "$work/bundle")
app="$(find "$work/bundle" -maxdepth 3 -name '*.app' -type d | head -1)"
[ -n "$app" ] || { echo "dx bundle made no .app in $work/bundle" >&2; exit 1; }

echo "── assembling Drive.app ────────────────────────────────────────"
final="$work/Drive.app"
rm -rf "$final"
cp -R "$app" "$final"
plist="$final/Contents/Info.plist"
# The program desktop.rs runs is Contents/MacOS/DriveFileProviderDomains.
cp "$work/fileprovider/DriveFileProviderDomains" "$final/Contents/MacOS/"
mkdir -p "$final/Contents/PlugIns"
cp -R "$work/fileprovider/DriveFileProvider.appex" "$final/Contents/PlugIns/"
# In the menu bar, not the Dock; the release's version.
plutil -replace LSUIElement -bool YES "$plist"
plutil -replace CFBundleIdentifier -string app.fasttrackstudio.drive "$plist"
plutil -replace CFBundleShortVersionString -string "$version" "$plist"
plutil -replace CFBundleVersion -string "$version" "$plist"

echo "── signing (inside out) ────────────────────────────────────────"
[ -n "${KEYCHAIN_PW:-}" ] && security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
SIGN_ID="${SIGN_ID:-$(security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}"
[ -n "$SIGN_ID" ] || { echo "no Developer ID identity in $KEYCHAIN" >&2; exit 1; }
sign() { codesign --force --timestamp --options runtime --keychain "$KEYCHAIN" --sign "$SIGN_ID" "$@"; }
sign --entitlements "$HERE/FileProvider/DriveFileProvider.entitlements" \
    "$final/Contents/PlugIns/DriveFileProvider.appex"
sign --entitlements "$HERE/Drive.entitlements" "$final/Contents/MacOS/DriveFileProviderDomains"
sign --entitlements "$HERE/Drive.entitlements" "$final"
codesign --verify --deep --strict "$final"

zip="$out/Drive-$version-$target.zip"
rm -f "$zip"
ditto -c -k --keepParent "$final" "$zip"

if [ "${NOTARIZE:-1}" = 1 ]; then
    echo "── notarizing ──────────────────────────────────────────────────"
    # The App Store Connect API key Studio's releases use.
    # shellcheck disable=SC1091
    [ -f "$HOME/.appstoreconnect/config.env" ] && source "$HOME/.appstoreconnect/config.env"
    : "${ASC_KEY_ID:?ASC_KEY_ID missing (~/.appstoreconnect/config.env); NOTARIZE=0 signs only}"
    : "${ASC_ISSUER_ID:?ASC_ISSUER_ID missing}"
    : "${ASC_KEY_PATH:?ASC_KEY_PATH missing}"
    xcrun notarytool submit "$zip" --key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" \
        --issuer "$ASC_ISSUER_ID" --wait --timeout 30m
    xcrun stapler staple "$final"
    xcrun stapler validate "$final"
    # Again, with the ticket stapled in.
    rm -f "$zip"
    ditto -c -k --keepParent "$final" "$zip"
fi
echo "built $zip"
