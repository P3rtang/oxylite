-- pending_ops gains a client-generated batch id: the durable op log may
-- only shrink when the SERVER confirms a batch (Ack echoes the id), never
-- when a socket merely accepted the frame — a socket can accept a send
-- and die before the server reads it, which silently dropped offline
-- writes forever (#33). Rows written before this migration get one uuid
-- each, so a legacy backlog flushes as single-op batches and acks
-- normally. Unused on the server; exists there only so both sides apply
-- one identical migration set (same convention as `meta`).
ALTER TABLE oxylite.pending_ops ADD COLUMN IF NOT EXISTS batch_id uuid;
UPDATE oxylite.pending_ops SET batch_id = gen_random_uuid()
WHERE batch_id IS NULL;
