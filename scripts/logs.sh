#!/usr/bin/env bash
# Tail dev logs. Usage: scripts/logs.sh [server|build-server|e2e]
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

name="${1:-}"
[ -z "$name" ] && { red "usage: scripts/logs.sh [server|build-server|e2e]"; exit 1; }

log_file="$LOG_DIR/$name.log"
[ -f "$log_file" ] || { red "no log file: $log_file"; exit 1; }
tail -f "$log_file"
