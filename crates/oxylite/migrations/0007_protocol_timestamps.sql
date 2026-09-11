-- The LWW axis becomes a real timestamp: type-enforced at the storage
-- layer, not just validated in the DTO (review #27 round 3). Postgres
-- rejects anything that isn't a timestamp on INSERT — raw psql seeds
-- can no longer poison LWW/tombstone comparisons with bogus text — and
-- timestamptz (not naive `timestamp`) is deliberate: it stores UTC
-- micros exactly like a UTC naive column would, but CONVERTS offsets on
-- input instead of silently discarding them (naive drops the "+02:00"
-- and keeps the wall time — the silent-corruption class this review is
-- killing). Every value in this protocol is a UTC instant.
--
-- The USING casts fail loud on legacy rows whose text isn't a
-- timestamp: a corrupt store must stop the migration, not flow on.
--
-- The lib's own tables (the app's replicated tables get the same
-- treatment in the APP's migrations — for the notes app that is
-- 0008_notes_timestamptz). Split from the original combined 0007 when
-- the migrations split along the lib/app boundary (#31).
ALTER TABLE sync_log
    ALTER COLUMN updated_at TYPE timestamptz USING updated_at::timestamptz;
ALTER TABLE tombstones
    ALTER COLUMN deleted_at TYPE timestamptz USING deleted_at::timestamptz;
