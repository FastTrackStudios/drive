# Agent Guidelines for Drive

## Never disrupt the live Drive instance

The host machine may be running a live Drive node (`127.0.0.1:8096`), live
`drive` agent (`~/.local/share/drive-agent`), live FUSE mount (`~/Drive`), and
live `kache` (`127.0.0.1:9778`) used by other applications.

- **Never** run `pkill drive`, `pkill drive-node`, `killall`, or restart user
  systemd/launchd Drive services.
- **Never** unmount `~/Drive` or run `fusermount3 -u` against any path outside
  your own `.dev/` or `mktemp -d` directory.
- **Never** invoke the system `drive` CLI without `--dir <dev-or-temp-dir>` or
  `DRIVE_AGENT_DIR=<dev-or-temp-dir>`.

## Development & testing workflow

1. **Prefer `cargo test` first** (all workspace tests use `tempfile::tempdir()`
   and ephemeral `127.0.0.1:0` listeners):
   - Targeted: `cargo test -p <crate>`
   - Full check: `cargo test --workspace` and `cargo clippy --workspace --all-targets`
2. **When a live node, agent, CLI, MCP, or FUSE mount is needed**, use the
   isolated dev stack documented in `docs/dev.md`:
   - `scripts/dev.sh up` (or `scripts/dev.sh up --mount` for an isolated FUSE
     mount at `.dev/mount`)
   - For parallel worktrees/agents, set unique `DRIVE_DEV_ROOT` and
     `DRIVE_DEV_PORT` (e.g. `18097`, `18098`, ...).
   - Always tear down with `scripts/dev.sh down` (PID-file scoped) when done.
3. **Commits:** Commit locally on your branch; do not push to remote unless asked.
