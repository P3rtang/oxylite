-- Client-side sync state: the cursor lives in the client's own PGlite so
-- pulls resume after reloads. Unused on the server.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
