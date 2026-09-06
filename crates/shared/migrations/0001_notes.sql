-- Replicated table: exists on both the server (Postgres) and every client
-- (PGlite). Rows are created offline, so ids are client-generated UUIDv7:
-- time-ordered, which keeps the primary-key index local and lets row order
-- reflect creation. LWW conflict resolution compares updated_at as canonical
-- ISO-8601 text (fixed format, from Date.prototype.toISOString).
CREATE TABLE IF NOT EXISTS notes (
    id         UUID PRIMARY KEY,
    title      TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);

