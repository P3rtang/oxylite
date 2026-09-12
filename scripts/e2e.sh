#!/usr/bin/env bash
# Run the Playwright e2e suite (bun + playwright).
# Starts the full stack via the playwright webServers, then runs the specs.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

BUN="${BUN:-$HOME/.bun/bin/bun}"
[ -x "$BUN" ] || { red "bun not found at $BUN"; exit 1; }

cd "$ROOT/e2e"

blue "starting playwright (it manages postgres + server itself)…"
# `bun run` uses the pinned local @playwright/test; `bun x playwright`
# re-resolves and can pull a version-mismatched standalone package.
#
# Two passes: the main suite keeps full parallelism (3 workers); specs
# tagged @isolated in their title need the shared infra EXCLUSIVELY
# (they stop/restart the shared Postgres) and run last, single-worker.
# User args apply to both passes; empty passes pass (a targeted run of
# one spec must not fail because the other pass finds nothing).
"$BUN" run test --pass-with-no-tests --grep-invert "@isolated" "$@"
"$BUN" run test --pass-with-no-tests --grep "@isolated" --workers=1 "$@"
