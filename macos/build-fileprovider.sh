#!/usr/bin/env bash
# Build Drive's macOS File Provider extension (docs/fileprovider.md): the
# Rust bridge as a universal static library, the Swift extension (.appex)
# and the domain registrar, signed. No Xcode project: three swiftc calls
# (as Task's does: an .xcodeproj is a generated file nobody can review).
#
#   bash macos/build-fileprovider.sh
#
# Env (all optional):
#   OUT_DIR      where it lands            (default: target/fileprovider)
#   SIGN_ID      codesign identity         (default: the keychain's
#                                           "Developer ID Application")
#   KEYCHAIN     keychain holding it       (default: fts-build.keychain)
#   KEYCHAIN_PW  its password: needed from SSH, where codesign otherwise
#                fails with errSecInternalComponent
#   TARGETS      rust targets              (default: this Mac's arch)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SRC="$HERE/FileProvider"
OUT_DIR="${OUT_DIR:-$ROOT/target/fileprovider}"
KEYCHAIN="${KEYCHAIN:-fts-build.keychain}"
TARGETS="${TARGETS:-$(uname -m | sed 's/arm64/aarch64/')-apple-darwin}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}"

[ "$(uname -s)" = Darwin ] || { echo "this builds a macOS extension: run it on a Mac" >&2; exit 1; }

echo "── the Rust bridge ─────────────────────────────────────────────"
LIBS=()
for t in $TARGETS; do
    rustup target add "$t" >/dev/null 2>&1 || true
    (cd "$ROOT" && cargo build --release -p drive-fileprovider --target "$t")
    LIBS+=("${CARGO_TARGET_DIR:-$ROOT/target}/$t/release/libdrive_fileprovider.a")
done
mkdir -p "$OUT_DIR"
LIB="$OUT_DIR/libdrive_fileprovider.a"
if [ "${#LIBS[@]}" -gt 1 ]; then lipo -create "${LIBS[@]}" -output "$LIB"; else cp "${LIBS[0]}" "$LIB"; fi

# The Swift is compiled for the targets' architecture (one target: the
# Mac Pro's x86_64 is built on airlock, an arm64 Mac); with several, lipo
# made the library universal and the Swift is this Mac's.
set -- $TARGETS
if [ "$#" -eq 1 ]; then arch="${1%%-*}"; else arch="$(uname -m)"; fi
arch="${arch/aarch64/arm64}"
SWIFT=(
    -swift-version 5
    -target "$arch-apple-macos$MACOSX_DEPLOYMENT_TARGET"
    -import-objc-header "$ROOT/crates/drive-fileprovider/include/drive_fileprovider.h"
    -framework FileProvider -framework Foundation
    -Xlinker -force_load -Xlinker "$LIB"
)

echo "── the extension ───────────────────────────────────────────────"
APPEX="$OUT_DIR/DriveFileProvider.appex"
rm -rf "$APPEX"
# Resources is empty and still needed: a signature seals it.
mkdir -p "$APPEX/Contents/MacOS" "$APPEX/Contents/Resources"
cp "$SRC/Info.plist" "$APPEX/Contents/Info.plist"
# An appex has no main: the system calls NSExtensionMain.
swiftc "${SWIFT[@]}" -module-name DriveFileProvider -emit-executable \
    -Xlinker -e -Xlinker _NSExtensionMain \
    -o "$APPEX/Contents/MacOS/DriveFileProvider" \
    "$SRC/Bridge.swift" "$SRC/Item.swift" "$SRC/Enumerator.swift" "$SRC/FileProviderExtension.swift"

echo "── the domain registrar ────────────────────────────────────────"
swiftc "${SWIFT[@]}" -module-name DriveFileProviderDomains -emit-executable \
    -o "$OUT_DIR/DriveFileProviderDomains" \
    "$SRC/Bridge.swift" "$SRC/Item.swift" "$SRC/Domains.swift"

echo "── signing ─────────────────────────────────────────────────────"
plutil -lint "$SRC/DriveFileProvider.entitlements" "$HERE/Drive.entitlements" >/dev/null
[ -n "${KEYCHAIN_PW:-}" ] && security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
SIGN_ID="${SIGN_ID:-$(security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}"
if [ -z "$SIGN_ID" ]; then
    echo "   no Developer ID identity in $KEYCHAIN: ad-hoc only (macOS will not load it)"
    codesign --force --sign - --entitlements "$SRC/DriveFileProvider.entitlements" "$APPEX"
else
    codesign --force --timestamp --options runtime --keychain "$KEYCHAIN" \
        --entitlements "$SRC/DriveFileProvider.entitlements" --sign "$SIGN_ID" "$APPEX"
    echo "   signed with $SIGN_ID"
fi
echo "built $APPEX and $OUT_DIR/DriveFileProviderDomains"
