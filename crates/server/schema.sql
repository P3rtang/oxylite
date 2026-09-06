CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TABLE IF NOT EXISTS notes (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    title      TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS sync_log (
    seq        BIGSERIAL PRIMARY KEY,
    table_name TEXT NOT NULL,
    row_id     UUID NOT NULL,
    payload    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS sync_log_seq_idx ON sync_log (seq);

CREATE OR REPLACE FUNCTION log_change(
    p_table TEXT, p_id UUID, p_payload JSONB, p_updated TEXT
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO sync_log (table_name, row_id, payload)
    VALUES (p_table, p_id, p_payload);
    PERFORM pg_notify('sync_changes', p_table);
END $$;
