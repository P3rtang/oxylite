#!/usr/bin/env bash
# Run the Playwright e2e suite (bun + playwright).
# Starts the full stack via the playwright webServers, then runs the specs.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BUN="${BUN:-$HOME/.bun/bin/bun}"
[ -x "$BUN" ] || { red "bun not found at $BUN"; exit 1; }

cd "$ROOT/e2e"

blue "building client dist (served by the sync server)…"
DX="${DX:-$HOME/.cargo/bin/dx}" "$ROOT/scripts/build-client.sh"

blue "starting playwright (it manages postgres + server itself)…"
"$BUN" x playwright test "$@"
