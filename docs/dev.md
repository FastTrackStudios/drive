# Developing & Testing Drive Safely

This machine may already have a live Drive instance running (`drive-node` on
`127.0.0.1:8096`, `drive` agent with state in `~/.local/share/drive-agent`,
FUSE mount at `~/Drive`, and `kache` on `127.0.0.1:9778`). **Never restart,
stop, unmount, or mutate the live instance while developing.**

Every component of Drive already supports running a completely isolated second
instance side-by-side.

## 1. Default loop: `cargo test` (in-process, no daemons)

Most development and almost all automated/agent verification belongs in
`cargo test`. Every integration test in the workspace uses `tempfile::tempdir()`
and ephemeral `127.0.0.1:0` sockets, so it never collides with the live Drive
or with parallel worktrees:

```sh
# Fast targeted iteration
cargo test -p drive-node
cargo test -p drive-agent

# Full verification before committing
cargo test --workspace
cargo clippy --workspace --all-targets
```

## 2. Isolated live dev stack (`scripts/dev.sh`)

When you need a real running node, agent, CLI, MCP server, web UI, native app,
or FUSE mount, use [scripts/dev.sh](../scripts/dev.sh). It keeps everything
under `.dev/` (gitignored) on port `18096`:

| Resource | Live Drive | Dev stack (`scripts/dev.sh`) |
|---|---|---|
| Node HTTP / WS | `127.0.0.1:8096` | `127.0.0.1:18096` (`DRIVE_DEV_PORT`) |
| Node data | `./data` or system service | `.dev/node` |
| Node auth / iroh | prod issuer & relays | local loopback only, `--iroh-local` |
| Agent state (`config.json`, `app.sock`, `cache/`) | `~/.local/share/drive-agent` | `.dev/agent` (`DRIVE_AGENT_DIR`) |
| FUSE mount | `~/Drive` | `.dev/mount` (only with `--mount`) |
| Sample location | real folders | `.dev/sample` |
| Native client cache & store | `~/.local/share/drive` | `.dev/client` (`DRIVE_CLIENT_DIR`) |
| `kache` S3 port | `127.0.0.1:9778` | off by default (`drive kache port 19778` if testing) |

### Common commands

```sh
# Start isolated node + agent (serve mode, no FUSE mount)
scripts/dev.sh up

# Or start with an isolated FUSE mount at .dev/mount (leaves ~/Drive alone)
scripts/dev.sh up --mount

# Point your current shell's `drive` CLI, `drive-local`, and native apps at dev
eval "$(scripts/dev.sh env)"

# Run the locally built `drive` binary against the dev agent directly
scripts/dev.sh cli locations
scripts/dev.sh cli search README
scripts/dev.sh cli mcp

# Run the native Drive app against the dev node & isolated client cache
eval "$(scripts/dev.sh env)"
cargo run -p drive-app --features native

# Check status, inspect logs, tear down ONLY the dev PIDs, or wipe .dev
scripts/dev.sh status
scripts/dev.sh logs
scripts/dev.sh down
scripts/dev.sh reset
```

### Parallel worktrees / multiple agents

Override `DRIVE_DEV_ROOT` and `DRIVE_DEV_PORT` so two branches or agents can
each run their own stack simultaneously:

```sh
DRIVE_DEV_ROOT="$PWD/.dev-feat" DRIVE_DEV_PORT=18097 scripts/dev.sh up --mount
```

## 3. Why a second FUSE mount works safely

- `drive mount <MOUNTPOINT>` creates a separate FUSE session for `<MOUNTPOINT>`
  and only calls `fusermount3 -u` on that exact path.
- `drive-agent`'s volume discovery (`crates/drive-agent/src/places.rs`) skips
  `fuse.drive*` filesystems, so neither the live agent nor the dev agent indexes
  or exposes each other's mounts.
- `drive-agent`'s local app socket (`app.sock`) and loopback port file
  (`app.port`) live inside `--dir` / `DRIVE_AGENT_DIR`, so `drive-local` clients
  in a dev shell talk exclusively to the dev agent.
- On NixOS, `scripts/dev.sh up --mount` automatically exports
  `FUSERMOUNT_PATH=/run/wrappers/bin/fusermount3` so `devenv`'s unprivileged
  `pkgs.fuse3` does not shadow the setuid wrapper. Inside sandboxed agent
  shells with `NoNewPrivs: 1` (where setuid `fusermount3` is blocked by the
  kernel), use default `scripts/dev.sh up` (`serve` mode) and unit/integration
  tests (`cargo test`).
