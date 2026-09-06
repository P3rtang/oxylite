-- Server-side bulk-load snapshots, one row per synced table. A client that
-- is far behind (fresh IndexedDB, or offline for ages) loads one of these
-- instead of replaying the whole sync_log row by row. `seq` is the sync_log
-- position the snapshot reflects; created_at bounds how stale it may be.
CREATE TABLE snapshots (
    table_name TEXT PRIMARY KEY,
    seq BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    data JSONB NOT NULL
);
