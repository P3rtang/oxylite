#!/usr/bin/env bash
## Oxylite CLI
##
## usage:
## usage:
##   oxylite.sh init [version]     vendor the PGlite bundle → ./assets/pglite
##                                 AND provision the dev environment:
##                                 compose.yaml + .env + postgres up, then
##                                 apply the MIGRATIONS (the CLI is the
##                                 owner — the lib's list from the published
##                                 crate's registry copy + the app's
##                                 migrations/ dir, one NNNN stream; every
##                                 applied row is seeded into _sqlx_migrations
##                                 so sqlx's boot migrator uses the CLI's
##                                 history, checksum-verified)
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

# ---- #47: the CLI owns the environment and the migrations ----
# The consumer environment defaults: container port 5433 (the repo's own
# dev stack keeps 5432; nothing collides), engine via COMPOSE exactly
# like the repo's scripts (podman locally, docker where containerized).
# DB name derives from the project dir (override: OX_DB_NAME).
COMPOSE_ENGINE="${COMPOSE:-podman compose}"
OX_DB_PORT_INIT="${OX_DB_PORT:-5433}"

# Locate the PUBLISHED crate's migrations dir: the registry copy is the
# single source for the lib half — the CLI never ships or caches lib SQL
# itself (the version comes from the consumer's own Cargo.toml req).
lib_migrations_dir() {
    local ver dir
    ver="$(sed -n 's/^oxylite = { version = "\([0-9.]*\)".*/\1/p' Cargo.toml | head -1)"
    [ -n "$ver" ] || ver="$(sed -n 's/^oxylite = "\([0-9.]*\)"$/\1/p' Cargo.toml | head -1)"
    [ -n "$ver" ] || { echo ""; return 0; } # oxylite not a dep yet — provision still proceeds
    local dir
    dir="$(ls -d "$HOME"/.cargo/registry/src/index.crates.io-*/oxylite-"$ver" 2>/dev/null | head -1)"
    [ -n "$dir" ] || { red "oxylite $ver pinned but not in the registry cache — run any cargo build once, then rerun init"; exit 1; }
    printf '%s/migrations' "$dir"
}


# Apply the merged migration stream: the APP's migrations/ dir AND the
# lib's registry copy, one NNNN-sorted pass — the same merge rule as
# migration_list::migrations_merged (a collision between the two
# namespaces fails HERE, not at boot — the init-version of the #40
# guard). Every applied migration is seeded into _sqlx_migrations with
# the checksum sqlx's own migrator computes (SHA-384 of the file,
# sqlx-core 0.8 Migration::new): boot then verifies and skips the
# CLI-owned history instead of colliding with existing tables.
# TARGET: the APP's database (the DATABASE_URL .env names), NOT the
# postgres bootstrap db — the history must live where the server's
# migrator looks at boot.
apply_migrations() {
    local app_dir="${OXYLITE_MIGRATIONS_DIR:-migrations}" lib_dir
    local app_db="${OX_DB_NAME:-${DATABASE_URL##*/}}"
    if [ -z "$app_db" ]; then
        red "no target database — set DATABASE_URL in .env (init provisions it)"
        exit 1
    fi
    # The history table only exists once a MIGRATOR has run — the CLI is
    # frequently here before any server boot, so create it exactly as
    # sqlx-postgres does (migrate.rs's ensure_migrations_table).
    $COMPOSE_ENGINE exec -T postgres psql -U sync -d "$app_db" -q \
        -v ON_ERROR_STOP=1 -c "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )" || { red "could not ensure _sqlx_migrations"; exit 1; }
    [ -d "$app_dir" ] || { blue "no migrations/ dir yet — the CLI applied none"; return 0; }
    lib_dir="$(lib_migrations_dir 2>/dev/null)" || exit 1
    [ -n "$lib_dir" ] || { blue "oxylite is not a dependency yet — the lib's migrations ride the next init/migrate apply"; return 0; }

    # The #48 merge rule, CLI twin: LIB FIRST (its own numeric-version
    # order), THEN the app's stream (its own order — timestamps from
    # `migrate up`, legacy NNNN accepted). The streams never interleave:
    # a consumer's file name carries no constraint against the lib's.
    # The numeric version remains GLOBAL (the history table's PK) — a
    # collision panics here, both files named, instead of crashing the
    # sqlx boot midway.
    local name f ver prevver=0 prevname=""
    # Numeric global sort across BOTH streams: the lib's integers
    # (0001…) sort before any consumer timestamp (≥1.7e13) by eleven
    # orders of magnitude — so lexicographic-numeric on the union IS
    # the lib-first-then-app rule, without special-casing.
    for name in $( (ls "$lib_dir"/*.sql 2>/dev/null; ls "$app_dir"/*.sql 2>/dev/null) \
                   | sed 's|^.*/||' | LC_ALL=C sort -t_ -k1,1n -u ); do
        case "$name" in
            [0-9]*_*.sql) ;;
            *) red "migration $name has no <version>_<name> prefix — skip (the macro rejects it too)"; continue ;;
        esac
        stem="${name%%_*}"
        ver=$((10#$stem))
        if [ "$ver" -eq "$prevver" ] 2>/dev/null; then
            red "migration version collision: $prevname and $name both claim $ver — rename one"
            exit 1
        fi
        prevver=$ver; prevname=$name
        f="$lib_dir/$name"; [ -f "$f" ] || f="$app_dir/$name"
        desc="${name#*_}"; desc="${desc%.sql}"
        if $COMPOSE_ENGINE exec -T postgres psql -U sync -d "$app_db" -tAc \
            "SELECT 1 FROM _sqlx_migrations WHERE version = $ver" 2>/dev/null | grep -q 1; then
            blue "already applied: $name"
            continue
        fi
        sha="$(shasum -a 384 "$f" | cut -d' ' -f1)"
        # Host paths are meaningless to in-container psql — stdin pipes
        # the file. Apply + seed are ONE transaction: a migration whose
        # history row failed leaves NO half-recorded state behind.
        { cat "$f"; printf '\nINSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (%s, %s, now(), true, decode(%s, %s), 0);\n' \
            "$ver" "$(printf "'%s'" "$desc")" "$(printf "'%s'" "$sha")" "'hex'"; } \
            | $COMPOSE_ENGINE exec -T postgres psql -U sync -d "$app_db" -1 -q \
                -v ON_ERROR_STOP=1 || { red "migration $name failed — nothing half-recorded"; exit 1; }
        green "applied: $name"
    done
}

# Provision the environment of a consuming project: its own compose
# stack (fresh port, own DB) + .env visible to the macro-checked builds
# (the #45 loader reads it), database created. The degradation path
# respects what exists: an .env without a compose.yaml = the consumer
# has their own database (external infra) — nothing scaffolded, and
# the environment step only sources .env; a compose.yaml without .env
# = scaffolded-before-.env (impossible from here, but honest guards).
provision() {
    if [ -f .env ] && [ -f compose.yaml ]; then
        blue "provision: .env + compose.yaml already exist — using them as-is"
        set -a; . ./.env; set +a
        return 0
    fi
    if [ -f .env ]; then
        blue "provision: .env exists (your database, external infra OK) — no compose scaffold"
        set -a; . ./.env; set +a
        return 0
    fi
    local db="${OX_DB_NAME:-$(basename "$PWD" | tr -c 'a-zA-Z0-9_' '_' | sed 's/_*$//')}"
    if [ -f compose.yaml ]; then
        blue "provision: compose.yaml exists; scaffolding .env against its container"
    else
        cat > compose.yaml <<EOF
# provisioned by oxylite.sh init — the consumer's own development
# database (host port ${OX_DB_PORT_INIT}: the repo/ladder stacks keep 5432).
services:
  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: sync
      POSTGRES_PASSWORD: sync
      POSTGRES_DB: postgres
    ports:
      - "${OX_DB_PORT_INIT}:5432"
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U sync"]
      interval: 2s
      timeout: 2s
      retries: 30
volumes:
  pgdata:
EOF
        green "wrote compose.yaml (postgres, host port ${OX_DB_PORT_INIT})"
    fi
    printf 'OX_DB_PORT=%s\nOX_DB_NAME=%s\nDATABASE_URL=postgres://sync:sync@localhost:%s/%s\n' \
        "$OX_DB_PORT_INIT" "$db" "$OX_DB_PORT_INIT" "$db" > .env
    green "wrote .env: DATABASE_URL → postgres://sync:sync@localhost:${OX_DB_PORT_INIT}/$db"
    set -a; . ./.env; set +a
    blue "bringing the stack up (${COMPOSE_ENGINE})…"
    $COMPOSE_ENGINE up -d
    local tries=0
    until $COMPOSE_ENGINE exec -T postgres pg_isready -U sync -q; do
        tries=$((tries + 1)); [ $tries -gt 30 ] && { red "postgres never became ready"; exit 1; }
        sleep 1
    done
    if ! $COMPOSE_ENGINE exec -T postgres psql -U sync -d postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname = '$db'" | grep -q 1; then
        $COMPOSE_ENGINE exec -T postgres psql -U sync -d postgres -c \
            "CREATE DATABASE \"$db\""
    fi
    green "database ready: $db"
}


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
    # the var written once.
    # Runtime rule (#47 amendment): the .env is now the CONSUMER'S OWN
    # declared database (init provisions it), so a .env DATABASE_URL is
    # the server's runtime URL too. What still may NOT leak is a
    # borrowed shell env (the gap-#44-1 rule) — hence: .env present →
    # start with its value; no .env → env -u keeps the protect.
    if [ -f .env ]; then
        blue "loading .env (the consumer's own database — build AND runtime)"
        set -a; . ./.env; set +a
    fi
    if [ -n "${DATABASE_URL:-}" ]; then
        RUNTIME_DB=pass-through
    else
        RUNTIME_DB=stripped
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
            red "hint: the checked sqlx macros compile against a live migrated DB."
            red "run 'oxylite.sh init' first — it provisions the consumer stack"
            red "(compose + .env + database) and applies the migrations, then rerun."
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
        # on top of the devserver's port. The DATABASE_URL rule:
        # consumer-owned .env → pass-through; borrowed shell env (no
        # .env) → stripped (gap #44-1: a build-time borrowed URL must
        # not run the consumer's migrator against a foreign database —
        # VersionMissing panic, the counter's old nose-dive).
        if [ "$RUNTIME_DB" = pass-through ]; then
            (setsid env PORT="$SERVER_PORT" "$SERVER_BIN_PATH" \
                > "$LOG_DIR/server.log" 2>&1 &)
        else
            (setsid env -u DATABASE_URL PORT="$SERVER_PORT" "$SERVER_BIN_PATH" \
                > "$LOG_DIR/server.log" 2>&1 &)
        fi
        wait_for_url "http://127.0.0.1:$SERVER_PORT/health" 30 \
            || { red "server failed to start — see $LOG_DIR/server.log"; exit 1; }
    fi

    green "server ready:  http://localhost:$SERVER_PORT  (pglite at /pglite)"
}

# Scaffold the next up migration in the consumer's migrations dir — the
# standard's day-one motion: `oxylite.sh migrate up <name>` writes a
# TIMESTAMP-stamped file (the #48 scheme: consumers own their clock —
# seconds since epoch-style YYYYMMDDHHMMSS; bumped forward one second
# while the exact stamp is taken, so two scaffolds in a burst stay
# ordered), validates the name against the macro's rule (a bad name
# fails here, not at compile). UP ONLY by ruling: `add` (an up + down
# pair) is reserved for the future omnidirectional story, so the verb
# stays honest about what it creates.
migrate_up() { # name...
    local dir="${OXYLITE_MIGRATIONS_DIR:-migrations}"
    [ $# -ge 1 ] || { red "usage: oxylite.sh migrate up <name-of-migration>"; exit 1; }

    # Normalize: lowercase, whitespace runs → single underscore (the
    # demo's house style). Hyphens stay legal — the macro only cares
    # about the numerically-versioned prefix.
    local name
    name="$(echo "$*" | tr '[:upper:]' '[:lower:]' | tr -s ' _' '_' | sed 's/^_*//; s/_*$//')"
    [ -n "$name" ] || { red "empty migration name"; exit 1; }

    mkdir -p "$dir"
    # Next version: the WALL CLOCK, bumped one second while taken — the
    # consumer's clock owns their project's order (the lib's stream
    # stays integers; the two never interleave, so no range guard is
    # needed — the #48 ruling that dissolved gap #2).
    local next f stem
    next="$(date +%Y%m%d%H%M%S)"
    f="$dir/${next}_${name}.sql"
    while [ -e "$f" ]; do
        next=$((next + 1))
        f="$dir/${next}_${name}.sql"
    done

    [ -e "$f" ] && { red "already exists: $f"; exit 1; }
    cat > "$f" <<EOF
-- ${name} — up migration (applied once, lib stream first then this
-- stream by version order, per the merge rule). Postgres + PGlite
-- compatible SQL; the down half is future work (the omnidirectional
-- \`add\`).
EOF

    green "created $f"
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
        # bump the version arg to upgrade the bundle), then provisions
        # the environment and applies the migrations (the CLI is the
        # migration owner — #47: idempotent; second runs seed nothing).
        vendor "$DEST" "${1:-$VERSION}"
        provision
        apply_migrations
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
