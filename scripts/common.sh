#!/usr/bin/env bash
# Shared helpers for offline-notes dev scripts.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="$ROOT/.logs"
mkdir -p "$LOG_DIR"

PORT_SERVER=3000

blue()  { printf "\033[1;34m%s\033[0m\n" "$*"; }
green() { printf "\033[1;32m%s\033[0m\n" "$*"; }
red()   { printf "\033[1;31m%s\033[0m\n" "$*"; }

port_pid() {
    fuser "$1/tcp" 2>/dev/null | tr -s ' ' ' '
}

is_running() { # port
    [ -n "$(port_pid "$1")" ]
}

wait_for_url() { # url, timeout_seconds
    local url="$1" timeout="${2:-30}" elapsed=0
    until curl -s -m 2 -o /dev/null "$url"; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$timeout" ]; then
            return 1
        fi
    done
}

wait_for_pg() { # timeout_seconds — Postgres readiness via pg_isready INSIDE
    # the container: curl cannot speak the wire protocol (a postgres:// URL
    # fails instantly on modern curl, which killed serve-server under set -e),
    # and pg_isready on the host is not a given — the container has it.
    local timeout="${1:-30}" elapsed=0
    until (cd "$ROOT" && podman compose exec -T postgres pg_isready -U sync -d offline_notes) >/dev/null 2>&1; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$timeout" ]; then
            return 1
        fi
    done
}
