#!/usr/bin/env bash
# Start the full stack (Postgres + sync server + built client) and open it
# in the browser at http://localhost:3000.
#
# Console output from the app is tagged: [boot], [sync], [pglite] — open
# Chrome DevTools (F12) to watch them.
set -euo pipefail
source "$(dirname "$0")/common.sh"

./scripts/serve-server.sh

blue "building client dist (always — incremental dx build is cheap)…"
DX="${DX:-$HOME/.cargo/bin/dx}" "$ROOT/scripts/build-client.sh"

green ""
green "open http://localhost:$PORT_SERVER in your browser"
green "watch [boot]/[sync]/[pglite] logs in DevTools console (F12)"
green ""

URL="http://localhost:$PORT_SERVER"
if command -v xdg-open >/dev/null 2>&1; then
    (setsid xdg-open "$URL" >/dev/null 2>&1 &) || true
fi
