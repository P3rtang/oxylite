-- Tombstones: the guard against stale writes resurrecting hard-deleted
-- rows. The upsert guard `ON CONFLICT ... WHERE updated_at` cannot fire on
-- an absent row (no conflict = plain INSERT), so without this table an
-- offline client pushing a pre-delete edit would resurrect the row
-- everywhere. Lib-owned and generic — one row per deleted (table, id);
-- upserts check it before applying, delete ops populate it, a strictly
-- newer write clears it (resurrection). Retained unboundedly: stale
-- pushes bypass cursors, so cursor math cannot prune these — acceptable,
-- they are tiny (no row data).
CREATE TABLE IF NOT EXISTS tombstones (
    table_name TEXT NOT NULL,
    id         UUID NOT NULL,
    deleted_at TEXT NOT NULL,
    PRIMARY KEY (table_name, id)
);

-- A delete op's payload is null (that is the delete marker), so its
-- updated_at cannot ride the payload like an upsert's does. sync_log
-- gets its own column; pull replays it per event. Backfill from the
-- payloads already logged, then drop the add-column default.
ALTER TABLE sync_log ADD COLUMN IF NOT EXISTS updated_at TEXT NOT NULL DEFAULT '';
UPDATE sync_log SET updated_at = COALESCE(payload->>'updated_at', '') WHERE updated_at = '';
ALTER TABLE sync_log ALTER COLUMN updated_at DROP DEFAULT;
