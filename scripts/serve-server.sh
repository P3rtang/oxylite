#!/usr/bin/env bash
# Start the sync server (Postgres via podman + axum/tokio/sqlx server).
# Runs detached; follow with: scripts/logs.sh server
set -euo pipefail
source "$(dirname "$0")/common.sh"

if is_running "$PORT_SERVER"; then
    green "server already running on :$PORT_SERVER (pid $(port_pid $PORT_SERVER))"
    exit 0
fi

blue "starting Postgres (${COMPOSE:-podman compose})…"
(cd "$ROOT" && ${COMPOSE:-podman compose} up -d) >/dev/null 2>&1 || red "${COMPOSE:-podman compose} failed — check .logs/server.log"

wait_for_pg 30 || { red "postgres not ready after 30s — check .logs/server.log"; exit 1; }

blue "building server…"
cargo build -q -p server 2>"$LOG_DIR/build-server.log"

# Every serve path builds the dist first: a running server always
# postdates its dist build, so reuse is never stale.
blue "building client dist…"
DX="${DX:-$HOME/.cargo/bin/dx}" "$ROOT/scripts/build-client.sh"

blue "starting sync server…"
(setsid "$ROOT/target/debug/server" > "$LOG_DIR/server.log" 2>&1 &)

if wait_for_url "http://localhost:$PORT_SERVER/health" 30; then
    green "server ready:  http://localhost:$PORT_SERVER  (ws sync at /sync)"
    green "pglite assets: http://localhost:$PORT_SERVER/pglite/index.js"
else
    red "server failed to start — see $LOG_DIR/server.log"
    tail -5 "$LOG_DIR/server.log" 2>/dev/null
    exit 1
fi
