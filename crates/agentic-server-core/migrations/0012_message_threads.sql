CREATE TABLE IF NOT EXISTS message_threads (
    message_id  TEXT PRIMARY KEY,
    owner_hash  TEXT NOT NULL,
    checkpoint  TEXT,
    state       TEXT NOT NULL CHECK (state IN ('ready', 'in_flight', 'consumed')),
    lease_token TEXT,
    lease_until BIGINT NOT NULL DEFAULT 0,
    expires_at  BIGINT NOT NULL,
    CHECK ((state = 'consumed' AND checkpoint IS NULL) OR (state <> 'consumed' AND checkpoint IS NOT NULL)),
    CHECK ((state = 'in_flight' AND lease_token IS NOT NULL) OR (state <> 'in_flight' AND lease_token IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_message_threads_expiry ON message_threads (expires_at);
