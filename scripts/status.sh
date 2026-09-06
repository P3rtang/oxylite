#!/usr/bin/env bash
# Print status of the stack.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

postgres() {
    (podman ps --format "{{.Names}} {{.Status}}" 2>/dev/null | grep postgres | head -1) \
        || echo "not running"
}

printf "%-14s %s\n" "postgres:"  "$(podman ps -q 2>/dev/null | grep -q . \
    && psql "postgres://sync:sync@localhost:5432/offline_notes" -tAc 'select 1' >/dev/null 2>&1 \
        && echo "up" || echo "not running")"

printf "%-14s " "server (3000):"
if is_running "$PORT_SERVER"; then
    echo "running (pid $(port_pid $PORT_SERVER))"
else
    echo "stopped"
fi

printf "%-14s " "client dist:"
[ -f "$ROOT/crates/client/dist/index.html" ] \
    && echo "built" || echo "missing (run scripts/build-client.sh)"
