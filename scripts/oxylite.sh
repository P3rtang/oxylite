#!/usr/bin/env bash
## Oxylite CLI
##
## usage:
##   oxylite.sh init [version]     vendor the PGlite bundle → ./assets/pglite
##   oxylite.sh migrate up <name>  scaffold the next NNNN_name.sql in the
##                                 app's migrations dir (up only — `add`,
##                                 i.e. up + down pairs, is future work)
##   oxylite.sh serve              build the client once + serve page and
##                                 bundle from the consumer's axum server (./server)
##   oxylite.sh watch [dx args…]   same + dx devserver with hot reload (dx serve)
##
## env:
## - DX (dioxus CLI path)
## - PGLITE_DEST (vendor target, default assets/pglite)
## - PGLITE_VERSION (default 0.5.8, the lib's vendored copy)
## - OXYLITE_MIGRATIONS_DIR (migrations dir for `migrate up`; same env
##   the `migrations!` macro reads — default ./migrations)
## - SERVER_PORT (override; default := the Dioxus.toml proxy's backend
##   port — the consumer's own contract) / PORT (dx devserver, 8080)
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
SERVER_PORT="${SERVER_PORT:-}" # derived from Dioxus.toml per consumer

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

    # The server port is the CONSUMER's contract, not the CLI's default:
    # the Dioxus.toml [[web.proxy]] backend names it (the /pglite mount
    # the boot snippet needs — hello-world :3001, counter :3002). Same
    # env override as the macro reads when the project lies elsewhere.
    SERVER_PORT="${SERVER_PORT:-$(sed -n 's|^backend = "http://127.0.0.1:\([0-9]*\)/pglite"$|\1|p' Dioxus.toml | head -1)}"
    [ -n "$SERVER_PORT" ] || {
        red "Dioxus.toml has no [[web.proxy]] backend naming the server's /pglite port"
        red "(expected form in $PWD: backend = \"http://127.0.0.1:PORT/pglite\")"
        exit 1
    }

    # The consumer's .env is the BUILD-time env source (the user's
    # expectation, mechanized): the checked sqlx macros compile against
    # a live DB and sqlx ≥0.7 no longer auto-loads .env — only a real
    # exported var reaches the macro expansion (a .env placed anywhere —
    # counter root, shared/, repo root — never gets read by the
    # registry-built oxylite). Sourcing it here makes `watch` work with
    # the var written once. The RUNTIME stays env -u: the server binary
    # starts below without DATABASE_URL, so a .env full of build-time
    # values cannot leak into it (the counter-server's own default URL
    # is the runtime contract — the gap-#44-1 env-leak rule).
    if [ -f .env ]; then
        blue "loading .env (build-time env — the server itself runs without it)"
        set -a; . ./.env; set +a
    fi

    # Build failures MUST surface: set -e eats a failed `cargo build -q
    # 2> log` as a silent exit (a watch that died after "building
    # server…" with the 14 DATABASE_URL macro errors sitting in
    # .logs/build-server.log was the report). Both builds tail their
    # log on failure, and the macro case gets the workaround hint —
    # .sqlx shipped in the crate is the real fix (ledger gap #1).
    blue "building client (dx build)…"
    "$DX" build --platform web > "$LOG_DIR/build-client.log" 2>&1 || {
        red "dx build failed — tail of $LOG_DIR/build-client.log:"
        tail -n 20 "$LOG_DIR/build-client.log"
        exit 1
    }

    blue "building server…"
    cargo build -q --manifest-path server/Cargo.toml \
        2> "$LOG_DIR/build-server.log" || {
        red "server build failed — tail of $LOG_DIR/build-server.log:"
        tail -n 20 "$LOG_DIR/build-server.log"
        if grep -q "DATABASE_URL" "$LOG_DIR/build-server.log"; then
            red "hint: the lib's checked sqlx macros compile against a live DB —"
            red "export DATABASE_URL=<a migrated database> (e.g. this app's own) and rerun."
            red ".sqlx shipped inside the oxylite package is the durable fix"
            red "(gap ledger #1); until it lands every consumer needs this env at build time."
        fi
        exit 1
    }

    # The binary is the server package's own name — read, not assumed
    # (the hello-world hardcode launched the WRONG server for a second
    # consumer and then mistook the neighbor's running server for this
    # one, courtesy of the shared default port). Its location follows
    # the workspace shape: a member server builds into the ROOT target
    # (the counter's one-workspace layout), a detached [workspace]
    # server builds into server/target (hello-world's).
    SERVER_BIN="$(sed -n 's/^name = "\(.*\)"$/\1/p' server/Cargo.toml | head -1)"
    [ -n "$SERVER_BIN" ] || { red "no package name in server/Cargo.toml"; exit 1; }
    SERVER_BIN_PATH="$PWD/server/target/debug/$SERVER_BIN"
    [ -x "$SERVER_BIN_PATH" ] || SERVER_BIN_PATH="$PWD/target/debug/$SERVER_BIN"

    if is_running "$SERVER_PORT"; then
        green "server already running on :$SERVER_PORT (pid $(port_pid $SERVER_PORT))"
    else
        blue "starting server…"
        # PORT is passed explicitly: the script's own PORT is the DX
        # devserver port (8080) — leaking it here would bind the server
        # on top of the devserver's port. DATABASE_URL is UNSET
        # explicitly (gap #44-1 workaround class): the env a BUILD needs
        # for sqlx's checked macros (the demo's database) must not
        # become the consumer server's RUNTIME database — an inherited
        # DATABASE_URL pointed the counter's migrator at the demo's
        # offline_notes and it panicked VersionMissing(8) — the demo's
        # version, not in this app's list.
        (setsid env -u DATABASE_URL PORT="$SERVER_PORT" "$SERVER_BIN_PATH" \
            > "$LOG_DIR/server.log" 2>&1 &)
        wait_for_url "http://127.0.0.1:$SERVER_PORT/health" 30 \
            || { red "server failed to start — see $LOG_DIR/server.log"; exit 1; }
    fi

    green "server ready:  http://localhost:$SERVER_PORT  (pglite at /pglite)"
}

# Scaffold the next up migration in the consumer's migrations dir — the
# standard's day-one motion: `oxylite.sh migrate up <name>` computes the
# next free NNNN, validates the stem against the macro's rule (so a bad
# name fails here, not at compile), and writes the file. UP ONLY by
# ruling: `add` (an up + down pair) is reserved for the future
# omnidirectional story, so the verb stays honest about what it creates.
migrate_up() { # name...
    local dir="${OXYLITE_MIGRATIONS_DIR:-migrations}"
    [ $# -ge 1 ] || { red "usage: oxylite.sh migrate up <name-of-migration>"; exit 1; }

    # Normalize: lowercase, whitespace runs → single underscore (the
    # demo's house style: 0001_notes, 0008_notes_timestamptz). Hyphens
    # stay legal — the macro only cares about the NNNN_ prefix.
    local name
    name="$(echo "$*" | tr '[:upper:]' '[:lower:]' | tr -s ' _' '_' | sed 's/^_*//; s/_*$//')"
    [ -n "$name" ] || { red "empty migration name"; exit 1; }

    mkdir -p "$dir"
    # Next free NNNN: max stem version + 1 (down files share their up's
    # number — they cannot skew the max). Five digits are REJECTED by
    # the macro on purpose ("10000" sorts before "9999"), so hitting
    # 10000 is a hard stop, not a wrap.
    local next=1 f stem
    for f in "$dir"/*.sql; do
        [ -e "$f" ] || continue
        stem="${f##*/}"; stem="${stem%%_*}"
        [[ "$stem" =~ ^[0-9]{4}$ ]] || continue
        [ $((10#$stem)) -ge "$next" ] && next=$((10#$stem + 1))
    done
    [ "$next" -le 9999 ] || {
        red "migrations dir exhausted — next number would be $next and the convention caps at 9999"
        exit 1
    }

    local file fname stem rest
    file="$(printf '%s/%04d_%s.sql' "$dir" "$next" "$name")"
    fname="${file##*/}"
    stem="${fname%%_*}"
    rest="${fname#*_}"

    # Same check the macro compiles with — validate here so the error
    # carries the file's intent, not a compile error's location.
    if [ ${#stem} -ne 4 ] || [ -z "$rest" ]; then
        red "normalized name does not satisfy NNNN_name: $file"
        exit 1
    fi

    [ -e "$file" ] && { red "already exists: $file"; exit 1; }
    cat > "$file" <<EOF
-- ${name} — up migration (applied once, lexicographic order == applied
-- order on both engines). Postgres + PGlite compatible SQL; the down
-- half is future work (the omnidirectional \`add\`).
EOF

    green "created $file"
    blue "write the SQL; the next build embeds it (a one-line build.rs with"
    blue 'cargo:rerun-if-changed=migrations keeps new files picked up)'
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
    migrate)
        # One verb today (`up`); the namespace is reserved — down/status
        # ride the future omnidirectional story, not ad-hoc scripts.
        verb="${1:-}"
        [ $# -gt 0 ] && shift
        case "$verb" in
            up) migrate_up "$@" ;;
            '') red "migrate needs a verb: up (down/status are future work)"; exit 1 ;;
            *) red "migrate $verb is not implemented (up is; add → up + down is future work)"; exit 1 ;;
        esac
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
