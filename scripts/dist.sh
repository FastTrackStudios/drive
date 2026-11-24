#!/usr/bin/env bash
# The release binaries for one target, packed: `drive` (the agent) and
# `drive-node`, plus the README, as OUT/drive-<version>-<target>.tar.gz.
#
#   bash scripts/dist.sh [TARGET] [OUT]     (default: this machine's, ./dist)
#
# On macOS the agent is built without the FUSE mount (it needs macFUSE;
# Finder gets the Drive tree through the File Provider extension instead,
# docs/fileprovider.md). SHA256SUMS is written over every tarball in OUT.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
out="${2:-dist}"
features=()
case "$target" in
    *-apple-darwin) features=(--no-default-features) ;;
esac
rustup target add "$target" >/dev/null 2>&1 || true
cargo build --release --locked --target "$target" -p drive-node
cargo build --release --locked --target "$target" -p drive-agent "${features[@]}"
bin="${CARGO_TARGET_DIR:-target}/$target/release"
version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' Cargo.toml | head -1)"
name="drive-$version-$target"
mkdir -p "$out/$name"
cp "$bin/drive" "$bin/drive-node" README.md "$out/$name/"
tar -C "$out" -czf "$out/$name.tar.gz" "$name"
rm -rf "${out:?}/$name"
(cd "$out" && if command -v sha256sum >/dev/null; then sha256sum ./*.tar.gz; else shasum -a 256 ./*.tar.gz; fi > SHA256SUMS)
ls -l "$out"
