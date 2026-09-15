#!/usr/bin/env bash
## Oxylite CLI
##
## usage:
##   oxylite.sh init [version]     vendor the PGlite bundle → ./assets/pglite
##   oxylite.sh serve              build the client once + serve page and
##                                 bundle from the consumer's axum server (./server)
##   oxylite.sh watch [dx args…]   same + dx devserver with hot reload (dx serve)
##
## env:
## - DX (dioxus CLI path)
## - PGLITE_DEST (vendor target, default assets/pglite)
## - PGLITE_VERSION (default 0.5.8, the lib's vendored copy)
## - SERVER_PORT (consumer server, default 3001) / PORT (dx devserver, 8080)
#
# init mirrors the lib repo's scripts/vendor-pglite.sh (same npm source,
# same file set — the bundle the lib's boot snippet dynamic-imports).
# serve/watch mirror its scripts/dev.sh shape: the consumer's axum server
# (examples/hello-world/server in miniature, the demo's crates/server in
# full) mounts assets/pglite at /pglite/ and serves the built page — a
# bare dx devserver does not serve raw asset files, so `watch` only adds
# dx's devserver on top, with the Dioxus.toml proxy pointing /pglite at
# the consumer server.
set -euo pipefail

# Standalone CLI — no sourcing repo helpers: the script gets installed
# via symlink (e.g. ~/.local/bin/oxylite.sh), where dirname "$0" is NOT
# the repo. The handful of common.sh helpers it needs are inlined
# verbatim instead.
blue()  { printf "\033[1;34m%s\033[0m\n" "$*"; }
green() { printf "\033[1;32m%s\033[0m\n" "$*"; }
red()   { printf "\033[1;31m%s\033[0m\n" "$*"; }

port_pid() { # port
    fuser "$1/tcp" 2>/dev/null | tr -s ' ' ' '
}

is_running() {
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

DX="${DX:-$HOME/.cargo/bin/dx}"
VERSION="${PGLITE_VERSION:-0.5.8}"
DEST="${PGLITE_DEST:-assets/pglite}"
PORT="${PORT:-8080}"
SERVER_PORT="${SERVER_PORT:-3001}"

# Logs live in the CONSUMING project (the repo's .logs convention moved
# with the CLI — the consumer owns its own logs; no basename prefixing,
# one consumer per directory).
LOG_DIR="$PWD/.logs"
mkdir -p "$LOG_DIR"

usage() { grep '^##' "$0" | sed 's/^## \?//'; }

vendor() {
    local dest="$1" version="$2"
    # Global, not local: the EXIT trap outlives this function (a local
    # would be unbound by then under set -u).
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    blue "downloading @electric-sql/pglite@$version …"
    curl -sL "https://registry.npmjs.org/@electric-sql/pglite/-/pglite-$version.tgz" \
        -o "$tmp/pglite.tgz"
    tar -xzf "$tmp/pglite.tgz" -C "$tmp"

    mkdir -p "$dest"
    # The PGlite class lives in index.js + chunks; pglite.wasm/.data are
    # resolved relative to the importing module URL, so the whole set
    # must sit side by side. The ESM files carry `//# sourceMappingURL=`
    # annotations — devtools follows them on every boot, so the maps
    # vendor too (missing maps = a 404 line per module in the server's
    # log; loaded only when devtools is open).
    cp "$tmp/package/dist/index.js" "$dest/"
    cp "$tmp"/package/dist/chunk-*.js "$dest/"
    cp "$tmp/package/dist/index.js.map" "$dest/"
    cp "$tmp"/package/dist/chunk-*.js.map "$dest/"
    cp "$tmp/package/dist/pglite.wasm" "$tmp/package/dist/pglite.data" \
        "$tmp/package/dist/initdb.wasm" "$dest/"
    cp "$tmp/package/LICENSE" "$dest/LICENSE"
    # The marker `serve`/`watch` read: a missing or mismatched version
    # re-runs init instead of serving a bundle that may not match.
    printf '%s\n' "$version" > "$dest/.version"

    green "vendored pglite@$version into $dest:"
}

# The vendored bundle must exist before anything serves the page: the
# boot snippet's dynamic import hits /pglite/ as soon as the browser
# loads. Auto-init (mechanized, so no human remembers the step) unless
# the marker says the right version is already there.
ensure_vendored() {
    [ -f "$DEST/index.js" ] && [ "$(cat "$DEST/.version" 2>/dev/null || echo)" = "$VERSION" ] && return 0
    vendor "$DEST" "$VERSION"
}

[ -x "$DX" ] || { red "dx not found at $DX (cargo install dioxus-cli)"; exit 1; }

# The serving preamble serve/watch share — the demo's serve-server.sh in
# miniature. The consumer's axum server must exist (it is the /pglite/
# mount the boot snippet needs); the client dist is built FIRST so the
# server never serves a stale page (the demo's dist-freshness rule —
# "a running server always postdates its dist build"); the server runs
# detached and idempotent, exactly like serve-server.sh.
prep_consumer() {
    [ -f Dioxus.toml ] || { red "no Dioxus.toml in $PWD — run inside the consuming project"; exit 1; }
    [ -d server ] || {
        red "no server/ in $PWD — the consumer's axum server mounting"
        red "assets/pglite at /pglite/ (see examples/hello-world/server)"
        exit 1
    }
    ensure_vendored

    blue "building client (dx build)…"
    "$DX" build --platform web > "$LOG_DIR/build-client.log" 2>&1

    blue "building server…"
    cargo build -q --manifest-path server/Cargo.toml \
        2> "$LOG_DIR/build-server.log"

    if is_running "$SERVER_PORT"; then
        green "server already running on :$SERVER_PORT (pid $(port_pid $SERVER_PORT))"
    else
        blue "starting server…"
        # PORT is passed explicitly: the script's own PORT is the DX
        # devserver port (8080) — leaking it here would bind the server
        # on top of the devserver's port.
        (setsid env PORT="$SERVER_PORT" "$PWD/server/target/debug/hello-world-server" \
            > "$LOG_DIR/server.log" 2>&1 &)
        wait_for_url "http://127.0.0.1:$SERVER_PORT/health" 30 \
            || { red "server failed to start — see $LOG_DIR/server.log"; exit 1; }
    fi

    green "server ready:  http://localhost:$SERVER_PORT  (pglite at /pglite)"
}

# The command word is not a dx argument — shift it off so the passthrough
# args (`$@`) are exactly what serve/watch forward to the CLI.
cmd="${1:-}"
[ $# -gt 0 ] && shift

case "$cmd" in
    init)
        # Explicit init: always re-vendors (the re-run is the point —
        # bump the version arg to upgrade the bundle).
        vendor "$DEST" "${1:-$VERSION}"
        ;;
    serve)
        [ $# -eq 0 ] || { red "serve takes no args (watch carries the dx passthrough)"; exit 1; }
        prep_consumer
        green "open http://localhost:$SERVER_PORT"
        ;;
    watch)
        prep_consumer
        blue "starting dx dev server (hot reload, :$PORT — /pglite proxies to :$SERVER_PORT)…"
        exec "$DX" serve --platform web "$@" --port $PORT
        ;;
    -h|--help|help|'') usage ;;
    *) red "unknown command: $cmd"; echo; usage; exit 1 ;;
esac
