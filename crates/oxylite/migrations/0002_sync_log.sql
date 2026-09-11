-- Server-side change journal: every accepted op gets a row with a
-- monotonic seq that cursors point into. Clients create it too (harmless,
-- unused) so both sides apply one identical migration set.
--
-- The LIB's namespace (#32): every protocol table lives in the `oxylite`
-- schema, on the server and in PGlite alike, so an app's own tables can
-- never collide with the lib's. 0002 bootstraps the schema; the later
-- lib migrations rely on it (they always run after 0002).
CREATE SCHEMA IF NOT EXISTS oxylite;
CREATE TABLE IF NOT EXISTS oxylite.sync_log (
    seq        BIGSERIAL PRIMARY KEY,
    table_name TEXT NOT NULL,
    row_id     UUID NOT NULL,
    payload    JSONB NOT NULL
);
