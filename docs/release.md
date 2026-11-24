# Releases

Drive releases are CalVer tags, `vYY.M.MICRO` (`v26.11.0`, `v26.11.1`,
`v26.12.0`), on a commit of `main`. **The tag is the release:** there are
no release branches, PRs or commits, and the version in `Cargo.toml` stays
`0.1.0` on `main`. The release build writes the tag's version into its own
checkout.

**Nothing runs CI automatically.** Pushes and pull requests start no
workflow: the checks are local while developing. The one workflow,
[`.github/workflows/release.yml`](../.github/workflows/release.yml), runs
when a tag is pushed or when someone asks for it.

## Cutting a release

```sh
just release-plan            # the latest tag, the next version, the notes so far
just release --dry-run       # what it would tag, without tagging
just release                 # tag origin/main as the next version and push: this ships
just release <sha>           # a given commit of main instead
just release-notes v26.11.0  # a tag's notes
```

`just release` refuses these:
- A commit not on `origin/main`.
- A commit that is already released.
- A commit older than the latest release.

The next version is the current month's next `MICRO`: `MICRO` counts up
within a month and restarts at `0` in a new one. The release notes come
from `main`'s first-parent commit titles since the previous tag (one
conventional commit per PR), grouped into Features, Fixes, Performance and
Other.

## What the tag runs

1. **Checks**, in parallel on THEBATTLESHIP's `thebattleship` runners: the
   tests (`just ci-tests`), clippy and the release script's self-test
   (`just ci-lint`), and the browser build (`just ci-wasm`).
2. **Binaries**, only if every check passed. Each build stamps the tag's
   version into its checkout, checks that the tag is on `main` and matches,
   and runs `scripts/dist.sh`:
   - `x86_64-unknown-linux-gnu` on a `thebattleship-release` runner;
   - `aarch64-apple-darwin` and `x86_64-apple-darwin` on airlock (`airlock`
     runner), with the agent built without the FUSE mount.
3. **Nix packages** for Linux, built on THEBATTLESHIP from
   `github:FastTrackStudios/drive/<tag>` (`#drive`, `#drive-node`) exactly as
   a machine evaluates them, unstamped, and pushed to
   `fasttrackstudio.cachix.org`. The Macs install the release's macOS
   tarballs through Nix instead: a package in the fleet config that fetches
   the tarball, pinned to its SHA-256. airlock's Nix store has no room for
   Drive's build.
4. **GitHub Release**: the tag's notes, every tarball, and one `SHA256SUMS`.

Machines run the release from Nix, pinned to its tag (the fleet's Drive
modules). Development builds stay in dev checkouts.

If any check fails, the tag gets no Release. Fix forward on `main` and
release the next version. A tag is never moved, deleted or reused.

## Why it is fast

Every THEBATTLESHIP runner has kache as its `RUSTC_WRAPPER`. The jobs build
inside the repo's dev shell (`devenv shell`), whose `rustc` is the same Nix
store path as a local `devenv shell` on this machine. So kache serves every
crate already compiled here, by a local build or an earlier run, and a
release mostly compiles nothing.

The checks build the tag's commit **unstamped**, exactly as a local build
of that commit, so they hit the cache too. Keep that true:
- Keep rustflags in `.cargo/config.toml`. A `RUSTFLAGS` in the workflow
  changes every crate's cache key.
- Move the toolchain pins only with devenv-fleet.

Each build kind (tests, lint, wasm, dist) keeps its own target dir under
the repo's `$CI_CACHE_DIR` on the runner.

## Checks without a release

```sh
just ci                     # locally: everything the tag runs first
just ci-remote my-branch    # the same on THEBATTLESHIP, for a pushed branch; publishes nothing
```

The runners' job gate (`~/.flake/hosts/THEBATTLESHIP/github-runners.nix`)
accepts only these for this public repo:
- a `v*` tag push;
- a push to `main` or `release/v*` (this workflow has no push trigger, so
  only tags start it);
- a `workflow_dispatch`, which needs write access.

It refuses everything else, forks included.
