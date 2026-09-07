#!/usr/bin/env bash
# Run the Playwright e2e suite (bun + playwright).
# Starts the full stack via the playwright webServers, then runs the specs.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BUN="${BUN:-$HOME/.bun/bin/bun}"
[ -x "$BUN" ] || { red "bun not found at $BUN"; exit 1; }

cd "$ROOT/e2e"

blue "starting playwright (it manages postgres + server itself)…"
# The dist build is owned by the webServer command (build step for the e2e
# stack); a reused server always postdates its own dist build.
# `bun run` uses the pinned local @playwright/test; `bun x playwright`
# re-resolves and can pull a version-mismatched standalone package.
"$BUN" run test "$@"
