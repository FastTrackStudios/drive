# Drive's recipes. `just ci` is the full check the release build runs;
# nothing runs it on a push or a PR (docs/release.md).

default:
    @just --list

# The full check: tests, clippy, the browser build. What a release tag runs first.
ci: ci-tests ci-lint ci-wasm

ci-tests:
    cargo test --workspace --locked

ci-lint:
    cargo clippy --workspace --all-targets --locked
    bash scripts/release.sh self-test

ci-wasm:
    cargo check --locked -p drive-app --target wasm32-unknown-unknown --features web
    cargo check --locked -p drive-client -p drive-dioxus --target wasm32-unknown-unknown

# The release binaries (drive, drive-node) for this machine, packed into OUT.
dist out="dist":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --locked -p drive-agent -p drive-node
    target="${CARGO_TARGET_DIR:-target}/release"
    version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' Cargo.toml | head -1)"
    triple="$(rustc -vV | sed -n 's/^host: //p')"
    name="drive-$version-$triple"
    mkdir -p "{{out}}/$name"
    cp "$target/drive" "$target/drive-node" README.md "{{out}}/$name/"
    tar -C "{{out}}" -czf "{{out}}/$name.tar.gz" "$name"
    rm -rf "{{out}}/$name"
    (cd "{{out}}" && sha256sum ./*.tar.gz > SHA256SUMS)
    ls -l "{{out}}"

# Latest release, what main holds since, the next version and its notes.
release-plan:
    bash scripts/release.sh plan

# Tag origin/main (or SHA) as the next version and push it: THIS SHIPS. --dry-run, --yes.
release *args:
    bash scripts/release.sh release {{args}}

# A tag's notes (default: the draft for origin/main).
release-notes tag="":
    bash scripts/release.sh notes {{tag}}

# Run the release checks on THEBATTLESHIP for a branch, without releasing.
ci-remote ref="main":
    gh workflow run release.yml --ref {{ref}}
