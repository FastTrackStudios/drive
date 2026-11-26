# Drive's recipes. `just ci` is the full check the release build runs;
# nothing runs it on a push or a PR (docs/release.md).

default:
    @just --list

# The full check: tests, rustfmt, clippy, the browser build. What a release tag runs first.
ci: ci-tests ci-lint ci-wasm

ci-tests:
    cargo test --workspace --locked

ci-lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    bash scripts/release.sh self-test

ci-wasm:
    cargo clippy --locked -p drive-app --target wasm32-unknown-unknown --features web --all-targets -- -D warnings
    cargo clippy --locked -p drive-client -p drive-dioxus --target wasm32-unknown-unknown --all-targets -- -D warnings

# The release binaries (drive, drive-node) for TARGET (default: this machine), packed into OUT.
dist out="dist" target="":
    bash scripts/dist.sh "{{target}}" "{{out}}"

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
