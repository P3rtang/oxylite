#!/usr/bin/env bash
# The committed default database state (scripts/db-default.sql): schema as
# the server's migrations create it, plus a small deterministic seed — so
# every e2e run (and dev session) starts from a known state instead of
# accumulated test junk.
#
#   restore  Recreate schema + seed inside the running postgres. Fast, no
#            restart; used before every e2e run.
#   rebuild  Full regeneration: wipe the volume, boot the server once to
#            apply the migrations, seed, pg_dump the result into
#            scripts/db-default.sql (committed). Run after migration
#            changes — or `./test.sh --fresh`.
#
# The dump is byte-stable: seed rows use fixed UUIDs and timestamps, so
# `rebuild` twice never churns the checked-in file.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

DUMP="$ROOT/scripts/db-default.sql"
PSQL=(podman compose exec -T postgres psql -U sync -d offline_notes)

wait_pg() {
    local elapsed=0
    until podman compose exec -T postgres psql -U sync -d offline_notes -c "SELECT 1" >/dev/null 2>&1; do
        sleep 0.5
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge 120 ]; then
            red "postgres did not come up"
            exit 1
        fi
    done
}

# Seed rows the app can bootstrap from. Fixed ids + timestamps keep the
# pg_dump byte-stable; the id prefix keeps them distinct from any test or
# user data (and lets sync_log rehydrate exactly these rows).
SEED_SQL="INSERT INTO notes (id, title, body, updated_at) VALUES
  ('01980000-0000-7000-8000-000000000001', 'seed: welcome', 'This note ships with the default database state (scripts/db-default.sql).', '2026-01-01T00:00:01.000Z'),
  ('01980000-0000-7000-8000-000000000002', 'seed: offline-first', 'Writes land in local PGlite first, then sync over websocket with LWW.', '2026-01-01T00:00:02.000Z'),
  ('01980000-0000-7000-8000-000000000003', 'seed: multi-tab', 'One engine per browser — subordinate tabs proxy to the leader.', '2026-01-01T00:00:03.000Z');
INSERT INTO oxylite.sync_log (table_name, row_id, payload, updated_at)
  SELECT 'notes', id, jsonb_build_object('id', id, 'title', title, 'body', body, 'updated_at', updated_at), updated_at
  FROM notes WHERE id::text LIKE '01980000-%';"

restore() {
    blue "restoring default database state…"
    podman compose up -d >/dev/null 2>&1
    wait_pg
    # The dump is --clean: drops + recreates each table, so leftover test
    # junk (notes, sync_log, poison rows) cannot survive into the run.
    podman compose exec -T postgres psql -U sync -d offline_notes \
        -v ON_ERROR_STOP=1 -q <"$DUMP"
    green "default state restored"
}

rebuild() {
    blue "rebuilding default state from migrations + seed…"
    # The sqlx macros validate queries at compile time; in live mode that
    # needs a populated DB — which a wiped volume cannot offer (and which
    # earlier failed rebuilds may have left empty). The committed .sqlx
    # cache (pre-commit hook keeps it fresh) makes the build DB-independent.
    if pid="$(port_pid "$PORT_SERVER")" && [ -n "$pid" ]; then
        blue "stopping stale server (pid $pid)…"
        fuser -k "$PORT_SERVER"/tcp >/dev/null 2>&1 || true
        sleep 1
    fi
    SQLX_OFFLINE=true cargo build -q -p server

    podman compose down -v >/dev/null 2>&1
    podman compose up -d >/dev/null 2>&1
    wait_pg

    # The fresh binary applies the embedded migrations on boot; that boot
    # IS the schema source of truth for the dump.
    setsid "$ROOT/target/debug/server" >"$ROOT/.logs/db-default-server.log" 2>&1 &
    if ! wait_for_url "http://localhost:3000/health" 60; then
        red "server failed to boot (see .logs/db-default-server.log)"
        exit 1
    fi
    fuser -k "$PORT_SERVER"/tcp >/dev/null 2>&1 || true
    sleep 1

    podman compose exec -T postgres psql -U sync -d offline_notes \
        -v ON_ERROR_STOP=1 -q <<<"$SEED_SQL"
    # pg_dump, normalized: _sqlx_migrations' installed_on / validation_time
    # are wall-clock side effects of the migration run — pinned so `rebuild`
    # is byte-stable and never churns the committed file.
    podman compose exec -T postgres pg_dump -U sync --clean --if-exists \
        offline_notes | awk -F'\t' -v OFS='\t' '
        /^COPY public\._sqlx_migrations / { inblk = 1; print; next }
        # the COPY terminator is the two-character line "\."
        inblk && $0 == "\\."        { inblk = 0; print; next }
        inblk {
            $3 = "2026-01-01 00:00:00+00"; $6 = "0"; print; next
        }
        { print }' >"$DUMP"
    green "default state written to scripts/db-default.sql"
}

case "${1:-}" in
    restore) restore ;;
    rebuild) rebuild ;;
    *)
        red "usage: db-default.sh [restore|rebuild]"
        exit 1
        ;;
esac
