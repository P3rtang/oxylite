-- Server push via pub/sub (roadmap 2.2): every logged op notifies the
-- `oxylite_ops` channel with its seq. ONE integer is the whole wire
-- format of the wake — the bus carries the news, never the rows
-- (per-socket cursors force per-socket pulls).
-- NOTIFY fires at COMMIT: a transaction's seqs arrive together, in seq
-- order, so a burst coalesces to one wake at the max seq. Unused on the
-- client (PGlite has no listener; pg_notify with none is a no-op) — it
-- rides along so both sides apply one identical migration set. The
-- channel literal is the adapter const's twin (oxylite::server::wake);
-- the live pubsub tests pin the pair.
CREATE OR REPLACE FUNCTION oxylite.notify_sync_log() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('oxylite_ops', NEW.seq::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS sync_log_notify ON oxylite.sync_log;
CREATE TRIGGER sync_log_notify
    AFTER INSERT ON oxylite.sync_log
    FOR EACH ROW EXECUTE FUNCTION oxylite.notify_sync_log();
