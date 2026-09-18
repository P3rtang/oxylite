-- counters gains its LWW axis: tombstoned deletes and racing writes
-- need a timestamp to lose against (an LWW-less table cannot delete
-- safely across clients — batch order decides, and a delete op replayed
-- after a newer bump would resurrect nothing but also never tombstone).
-- Default now() backfills the rows the earlier migration created.
-- FIRST APP FILE AFTER THE LIB'S RESERVED RANGE (0002-0011): the CLI's
-- `migrate up` scaffolds against the app dir only and would have picked
-- 0002 — a collision the lib's merge would only panic on at boot. Gap,
-- recorded 2026-09-15: the scaffolder should know the lib's range.
ALTER TABLE counters
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
