-- Server-side change journal: every accepted op gets a row with a
-- monotonic seq that cursors point into. Clients create it too (harmless,
-- unused) so both sides apply one identical migration set.
CREATE TABLE IF NOT EXISTS sync_log (
    seq        BIGSERIAL PRIMARY KEY,
    table_name TEXT NOT NULL,
    row_id     UUID NOT NULL,
    payload    JSONB NOT NULL
);
