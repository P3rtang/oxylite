-- Client-side durable op log: local writes that could not be delivered
-- while offline. The rows-to-push persist here (per browser, in PGlite) so
-- a reload — or the leader tab dying with a subordinate's write in flight
-- — cannot lose them; the connect flush drains the table and deletes the
-- rows the socket accepted. Unused on the server; exists there only so
-- both sides apply one identical migration set (same convention as `meta`).
CREATE TABLE IF NOT EXISTS oxylite.pending_ops (
    seq BIGSERIAL PRIMARY KEY,
    op  TEXT NOT NULL
);
