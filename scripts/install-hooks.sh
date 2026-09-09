#!/usr/bin/env bash
# Activate the repo's git hooks (scripts/hooks/) for this clone, via
# core.hooksPath. Idempotent; run once after cloning.
set -euo pipefail
source "$(dirname "$0")/common.sh"

if [ -n "$(git -C "$ROOT" config core.hooksPath)" ] \
    && [ "$(git -C "$ROOT" config core.hooksPath)" != "scripts/hooks" ]; then
    red "refusing to overwrite existing core.hooksPath: $(git -C "$ROOT" config core.hooksPath)"
    exit 1
fi

git -C "$ROOT" config core.hooksPath scripts/hooks
green "hooks installed: $(git -C "$ROOT" config core.hooksPath)/ (see scripts/hooks/)"
