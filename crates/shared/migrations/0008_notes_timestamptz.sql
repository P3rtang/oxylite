-- The LWW axis becomes a real timestamp: type-enforced at the storage
-- layer, not just validated in the DTO (review #27 round 3). Postgres
-- rejects anything that isn't a timestamp on INSERT — raw psql seeds
-- can no longer poison LWW comparisons with bogus text — and
-- timestamptz (not naive `timestamp`) is deliberate: it stores UTC
-- micros exactly like a UTC naive column would, but CONVERTS offsets on
-- input instead of silently discarding them (naive drops the "+02:00"
-- and keeps the wall time — the silent-corruption class this review is
-- killing). Every value in this protocol is a UTC instant.
--
-- The USING cast fails loud on legacy rows whose text isn't a
-- timestamp: a corrupt store must stop the migration, not flow on.
--
-- The app's replicated table (the lib's own protocol tables get the
-- same treatment in the lib's migrations — 0007_protocol_timestamps).
-- Split from the original combined 0007 when the migrations split
-- along the lib/app boundary (#31).
ALTER TABLE notes
    ALTER COLUMN updated_at TYPE timestamptz USING updated_at::timestamptz;
