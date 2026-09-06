#!/usr/bin/env bash
# Stop the running dev server (Postgres is left up).
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

for port in "$PORT_SERVER"; do
    if pid="$(port_pid "$port")"; [ -n "$pid" ]; then
        blue "stopping port $port (pid $pid)"
        kill "$pid" 2>/dev/null || true
    fi
done
sleep 1
green "stopped"
