#!/usr/bin/env bash
# Isolated local dev stack for Drive (never touches a live running Drive,
# ~/Drive, ~/.local/share/drive-agent, or port 8096).
#
# Usage:
#   scripts/dev.sh up [--mount] [--web DIR]   Start isolated node + agent
#   scripts/dev.sh down                       Stop only this dev stack's PIDs
#   scripts/dev.sh status                     Show dev node, agent & mount state
#   scripts/dev.sh env                        Print exports for eval "$(scripts/dev.sh env)"
#   scripts/dev.sh cli <drive args...>        Run built `drive` against the dev agent
#   scripts/dev.sh logs [node|agent]          Show recent logs
#   scripts/dev.sh reset                      Stop and wipe the .dev directory
#
# Multiple parallel worktrees/agents can run side-by-side by overriding:
#   DRIVE_DEV_ROOT=/path/to/.dev-other DRIVE_DEV_PORT=18097 scripts/dev.sh up

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="${DRIVE_DEV_ROOT:-$REPO/.dev}"
PORT="${DRIVE_DEV_PORT:-18096}"
SPACE="${DRIVE_DEV_SPACE:-dev}"

NODE_DIR="$ROOT/node"
AGENT_DIR="$ROOT/agent"
CLIENT_DIR="$ROOT/client"
MOUNT_DIR="$ROOT/mount"
SAMPLE_DIR="$ROOT/sample"
RUN_DIR="$ROOT/run"
LOG_DIR="$ROOT/logs"

NODE_PID="$RUN_DIR/node.pid"
AGENT_PID="$RUN_DIR/agent.pid"
NODE_URL="http://127.0.0.1:$PORT"
NODE_WS="ws://127.0.0.1:$PORT/sync"

NODE_BIN="$REPO/target/debug/drive-node"
AGENT_BIN="$REPO/target/debug/drive"

pid_alive() {
    local pf="$1"
    [[ -f "$pf" ]] || return 1
    local pid
    pid="$(<"$pf")"
    [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null
}

is_mounted() {
    local target="$1"
    [[ -d "$target" ]] || return 1
    if command -v mountpoint >/dev/null 2>&1; then
        mountpoint -q "$target"
    else
        grep -qs " $target " /proc/mounts
    fi
}

stop_pid() {
    local name="$1" pf="$2"
    if ! pid_alive "$pf"; then
        rm -f "$pf"
        return 0
    fi
    local pid
    pid="$(<"$pf")"
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 30); do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$pf"
            return 0
        fi
        sleep 0.1
    done
    echo "dev.sh: $name (pid $pid) did not exit on SIGTERM; sending SIGKILL" >&2
    kill -KILL "$pid" 2>/dev/null || true
    rm -f "$pf"
}

cmd_env() {
    printf 'export DRIVE_DEV_ROOT=%q\n' "$ROOT"
    printf 'export DRIVE_DEV_PORT=%q\n' "$PORT"
    printf 'export DRIVE_NODE=%q\n' "$NODE_WS"
    printf 'export DRIVE_AGENT_DIR=%q\n' "$AGENT_DIR"
    printf 'export DRIVE_CLIENT_DIR=%q\n' "$CLIENT_DIR"
}

cmd_status() {
    echo "root:   $ROOT"
    echo "node:   $(pid_alive "$NODE_PID" && echo "running (pid $(<"$NODE_PID"), $NODE_URL)" || echo "stopped")"
    echo "agent:  $(pid_alive "$AGENT_PID" && echo "running (pid $(<"$AGENT_PID"), dir $AGENT_DIR)" || echo "stopped")"
    echo "mount:  $(is_mounted "$MOUNT_DIR" && echo "mounted at $MOUNT_DIR" || echo "not mounted")"
}

cmd_down() {
    stop_pid "agent" "$AGENT_PID"
    if is_mounted "$MOUNT_DIR"; then
        fusermount3 -u "$MOUNT_DIR" 2>/dev/null || fusermount3 -u -z "$MOUNT_DIR" 2>/dev/null || true
    fi
    stop_pid "node" "$NODE_PID"
    echo "dev stack stopped ($ROOT)."
}

cmd_up() {
    local want_mount=0
    local web_dir=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --mount) want_mount=1; shift ;;
            --web) web_dir="${2:?--web requires a directory}"; shift 2 ;;
            *) echo "unknown option: $1" >&2; exit 2 ;;
        esac
    done

    mkdir -p "$NODE_DIR" "$AGENT_DIR" "$CLIENT_DIR" "$SAMPLE_DIR" "$RUN_DIR" "$LOG_DIR"

    # Build binaries from this checkout (never use a globally installed drive).
    # `drive-agent` enables `mount` by default.
    (cd "$REPO" && cargo build -p drive-node -p drive-agent)

    # Start isolated drive-node.
    if ! pid_alive "$NODE_PID"; then
        if curl -fsS "$NODE_URL/health" >/dev/null 2>&1; then
            echo "dev.sh: port $PORT is already serving something else; pick another DRIVE_DEV_PORT" >&2
            exit 1
        fi
        local node_args=(
            --listen "127.0.0.1:$PORT"
            --data "$NODE_DIR"
            --org "$SPACE"
            --iroh-local
        )
        if [[ -z "$web_dir" && -d "$REPO/apps/drive/dist" ]]; then
            web_dir="$REPO/apps/drive/dist"
        fi
        if [[ -n "$web_dir" ]]; then
            node_args+=(--app "/=$web_dir")
        fi
        nohup "$NODE_BIN" "${node_args[@]}" >"$LOG_DIR/node.log" 2>&1 &
        echo $! >"$NODE_PID"

        local ready=0
        for _ in $(seq 1 50); do
            if curl -fsS "$NODE_URL/health" >/dev/null 2>&1; then
                ready=1
                break
            fi
            if ! pid_alive "$NODE_PID"; then
                echo "dev.sh: drive-node exited early; see $LOG_DIR/node.log" >&2
                tail -n 30 "$LOG_DIR/node.log" >&2 || true
                exit 1
            fi
            sleep 0.1
        done
        if [[ "$ready" -ne 1 ]]; then
            echo "dev.sh: timed out waiting for drive-node on $NODE_URL" >&2
            exit 1
        fi
    fi

    # First-time agent init + disposable sample location.
    if [[ ! -f "$AGENT_DIR/config.json" ]]; then
        printf 'Hello from the isolated Drive dev environment.\n' >"$SAMPLE_DIR/README.txt"
        "$AGENT_BIN" --dir "$AGENT_DIR" init --node "$NODE_WS" --space "$SPACE" --name "Dev"
        "$AGENT_BIN" --dir "$AGENT_DIR" location add sample "$SAMPLE_DIR"
    fi

    # Start isolated drive-agent (serve, or mount at .dev/mount).
    if ! pid_alive "$AGENT_PID"; then
        if [[ "$want_mount" -eq 1 ]]; then
            mkdir -p "$MOUNT_DIR"
            # On NixOS inside `devenv shell`, pkgs.fuse3 can shadow the setuid wrapper.
            if [[ -z "${FUSERMOUNT_PATH:-}" && -x /run/wrappers/bin/fusermount3 ]]; then
                export FUSERMOUNT_PATH=/run/wrappers/bin/fusermount3
            fi
            nohup "$AGENT_BIN" --dir "$AGENT_DIR" mount "$MOUNT_DIR" >"$LOG_DIR/agent.log" 2>&1 &
        else
            nohup "$AGENT_BIN" --dir "$AGENT_DIR" serve >"$LOG_DIR/agent.log" 2>&1 &
        fi
        echo $! >"$AGENT_PID"
        if [[ "$want_mount" -eq 1 ]]; then
            local mounted=0
            for _ in $(seq 1 50); do
                if is_mounted "$MOUNT_DIR"; then
                    mounted=1
                    break
                fi
                if ! pid_alive "$AGENT_PID"; then
                    echo "dev.sh: drive-agent exited early; see $LOG_DIR/agent.log" >&2
                    tail -n 30 "$LOG_DIR/agent.log" >&2 || true
                    exit 1
                fi
                sleep 0.1
            done
            if [[ "$mounted" -ne 1 ]]; then
                echo "dev.sh: timed out waiting for FUSE mount at $MOUNT_DIR; see $LOG_DIR/agent.log" >&2
                tail -n 30 "$LOG_DIR/agent.log" >&2 || true
                exit 1
            fi
        else
            sleep 0.4
            if ! pid_alive "$AGENT_PID"; then
                echo "dev.sh: drive-agent exited early; see $LOG_DIR/agent.log" >&2
                tail -n 30 "$LOG_DIR/agent.log" >&2 || true
                exit 1
            fi
        fi
    fi

    cmd_status
    echo
    echo "Use this dev stack in your shell:"
    echo "  eval \"\$(scripts/dev.sh env)\""
}

cmd_cli() {
    if [[ ! -x "$AGENT_BIN" ]]; then
        (cd "$REPO" && cargo build -p drive-agent)
    fi
    exec env DRIVE_AGENT_DIR="$AGENT_DIR" DRIVE_NODE="$NODE_WS" "$AGENT_BIN" --dir "$AGENT_DIR" "$@"
}

cmd_logs() {
    local which="${1:-all}"
    case "$which" in
        node) tail -n 50 "$LOG_DIR/node.log" ;;
        agent) tail -n 50 "$LOG_DIR/agent.log" ;;
        all)
            echo "=== node.log ==="
            tail -n 30 "$LOG_DIR/node.log" 2>/dev/null || true
            echo "=== agent.log ==="
            tail -n 30 "$LOG_DIR/agent.log" 2>/dev/null || true
            ;;
        *) echo "usage: scripts/dev.sh logs [node|agent|all]" >&2; exit 2 ;;
    esac
}

cmd_reset() {
    cmd_down
    if [[ -z "$ROOT" || "$ROOT" == "/" || "$ROOT" == "$HOME" ]]; then
        echo "dev.sh: refusing to remove unsafe ROOT='$ROOT'" >&2
        exit 1
    fi
    rm -rf "$ROOT"
    echo "removed $ROOT."
}

case "${1:-help}" in
    up) shift; cmd_up "$@" ;;
    down) cmd_down ;;
    status) cmd_status ;;
    env) cmd_env ;;
    cli) shift; cmd_cli "$@" ;;
    logs) shift; cmd_logs "$@" ;;
    reset) cmd_reset ;;
    *)
        sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
esac
