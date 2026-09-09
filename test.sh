#!/usr/bin/env bash
# One entry point for all testing. Consistent by construction:
#   ./test.sh             # same as --full
#   ./test.sh --lint      # rust build + fmt --check + clippy + unit tests
#   ./test.sh --e2e       # fresh client dist + playwright suite
#   ./test.sh --full      # lint + e2e
#   --fresh               # (with e2e/full) rebuild the default state from
#                         # migrations + seed, refreshing the committed dump
#
# Everything runs from a clean state: --e2e kills any server on :3000 and
# rebuilds the dist via dx (the stale-wasm class of bug lives in forgetting
# that step). Format drift fails with a hint instead of auto-fixing —
# fixing formatting is a dev action, not a test action.
set -euo pipefail
source "$(dirname "$0")/scripts/common.sh"

mode=""
fresh=false

for arg in "$@"; do
    case "$arg" in
        --lint) mode="${mode:+$mode }lint" ;;
        --e2e) mode="${mode:+$mode }e2e" ;;
        --full) mode="lint e2e" ;;
        --fresh) fresh=true ;;
        *)
            red "unknown flag: $arg"
            echo "usage: ./test.sh [--lint] [--e2e] [--full] [--fresh]"
            exit 1
            ;;
    esac
done

# No flags = full.
[ -z "$mode" ] && mode="lint e2e"

lint() {
    blue "cargo fmt --check…"
    if ! cargo fmt --check 2>"$LOG_DIR/test-fmt.log"; then
        red "formatting drift — fix with: cargo fmt (see $LOG_DIR/test-fmt.log)"
        exit 1
    fi

    blue "cargo build --workspace…"
    cargo build --workspace

    blue "clippy (host)…"
    cargo clippy --workspace --all-targets -- -D warnings

    blue "clippy (wasm: sync + client)…"
    cargo clippy -q -p sync --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -q -p client --target wasm32-unknown-unknown -- -D warnings

    blue "cargo test --workspace…"
    cargo test --workspace
    green "lint: clean"
}

e2e() {
    # A running server would be reused by playwright's webServer check and
    # might be a stale binary — consistency means restarting it. It also
    # must not hold connections while the default state is restored.
    if pid="$(port_pid "$PORT_SERVER")" && [ -n "$pid" ]; then
        blue "stopping stale server (pid $pid)…"
        kill "$pid" 2>/dev/null || true
        sleep 1
    fi

    if $fresh; then
        # Wipe the volume, re-apply migrations, re-seed, refresh the
        # committed dump — the honest baseline after migration changes.
        "$ROOT/scripts/db-default.sh" rebuild
    else
        # Every run starts from the committed default state: deterministic
        # and free of accumulated test junk (restore is fast, no restart).
        "$ROOT/scripts/db-default.sh" restore
    fi

    # Dist freshness is owned by the serve steps, not here: the webServer
    # command and serve-server.sh both build before starting, so a reused
    # server always postdates its dist build.
    blue "playwright suite…"
    "$ROOT/scripts/e2e.sh"
    green "e2e: passed"
}

for m in $mode; do
    case "$m" in
        lint) lint ;;
        e2e) e2e ;;
    esac
done

green "test.sh: all requested modules passed"
