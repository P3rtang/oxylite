#!/usr/bin/env bash
# Build the dioxus web client once into crates/client/dist (served by axum).
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

DX="${DX:-$HOME/.cargo/bin/dx}"
[ -x "$DX" ] || { red "dx not found at $DX (cargo install dioxus-cli)"; exit 1; }

blue "building client (dx build)…"
(cd "$ROOT/crates/client" && "$DX" build --platform web --package client) \
    > "$LOG_DIR/build-client.log" 2>&1

if [ ! -d "$ROOT/crates/client/dist" ]; then
    # dx 0.7 writes to target/dx/<app>/<mode>/web/public — copy it to dist/.
    SRC="$ROOT/target/dx/client/debug/web/public"
    [ -d "$SRC" ] || SRC="$(find "$ROOT/target/dx" -maxdepth 4 -type d -name public 2>/dev/null | head -1)"
    [ -z "${SRC:-}" ] || [ ! -d "$SRC" ] && { red "dx build output not found — see $LOG_DIR/build-client.log"; exit 1; }
    mkdir -p "$ROOT/crates/client/dist"
    cp -r "$SRC"/. "$ROOT/crates/client/dist/"
fi

green "client dist ready: $ROOT/crates/client/dist"
