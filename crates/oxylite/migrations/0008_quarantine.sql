-- Quarantine (#35, roadmap 3.3): ops the CURRENT schema cannot apply.
-- The poison floor's strength is unchanged — the batch commits, the
-- sender is never wedged, the error surfaces — but the op lands HERE
-- instead of sync_log: a payload no compatible client can apply is not
-- replayable history, and logging it into the log made every cold
-- client re-parse the poison on every boot. The log is the APPLYABLE
-- history; this is the inspectable, prunable record of what wasn't.
-- Unused on the client; exists there only so both sides apply one
-- identical migration set (same convention as `meta`).
CREATE TABLE IF NOT EXISTS oxylite.quarantine (
    seq        BIGSERIAL PRIMARY KEY,
    table_name TEXT NOT NULL,
    row_id     UUID NOT NULL,
    payload    JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    error      TEXT NOT NULL,
    logged_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Server-arrival time for age-based pruning (roadmap 3.4): the op's
-- updated_at is the CLIENT's clock — pruning on it would be wrong.
-- The backfill stamps existing rows with the migration instant (they
-- look fresh; harmless for the demo log, correct from the next
-- --fresh rebuild on). Pruning itself is mechanism-free: prune_sync_log
-- is the operation; scheduling waits for an event bus / cron task
-- system (reviewer decision, #35).
ALTER TABLE oxylite.sync_log
    ADD COLUMN IF NOT EXISTS logged_at TIMESTAMPTZ NOT NULL DEFAULT now();
