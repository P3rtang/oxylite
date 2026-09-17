-- counters — up migration (applied once, lexicographic order == applied
-- order on both engines). Postgres + PGlite compatible SQL; the down
-- half is future work (the omnidirectional `add`).
CREATE TABLE IF NOT EXISTS counters (
    id    UUID PRIMARY KEY,
    name  TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0
);
