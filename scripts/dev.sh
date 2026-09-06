#!/usr/bin/env bash
# Dev loop: axum sync server on :3000 + `dx serve` dev server with hot
# reload. The dev server proxies /sync and /pglite to :3000 (see
# crates/client/Dioxus.toml), so open http://localhost:8081 while iterating.
# Port 8081 because 8080 is taken by a system nginx on this machine.
set -euo pipefail
source "$(dirname "$0")/common.sh"

DX="${DX:-$HOME/.cargo/bin/dx}"
[ -x "$DX" ] || { red "dx not found at $DX (cargo install dioxus-cli)"; exit 1; }

./scripts/serve-server.sh

blue "starting dx dev server (hot reload)…"
exec "$DX" serve --platform web --package client --open false --port 8081
