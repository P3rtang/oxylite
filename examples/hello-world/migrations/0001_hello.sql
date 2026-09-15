-- the example's first OWN migration — the standard's proof: this file is
-- the whole job (embed via oxylite::migrations!, applied by the merged
-- boot, listed on the page from the apply-once tracker).
CREATE TABLE IF NOT EXISTS hello_boots (
    id       INTEGER PRIMARY KEY,
    seen_at  TEXT NOT NULL
);
