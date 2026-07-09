CREATE TABLE IF NOT EXISTS conversations (
    id          TEXT PRIMARY KEY,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS items (
    id              TEXT PRIMARY KEY,
    data            TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    conversation_id TEXT REFERENCES conversations(id) ON DELETE CASCADE,
    seq             INTEGER
);

CREATE INDEX IF NOT EXISTS idx_items_conversation_id ON items (conversation_id);
CREATE INDEX IF NOT EXISTS idx_items_created_at ON items (created_at);

CREATE TABLE IF NOT EXISTS responses (
    id                   TEXT PRIMARY KEY,
    conversation_id      TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    previous_response_id TEXT REFERENCES responses(id) ON DELETE SET NULL,
    history_item_ids     TEXT,
    metadata             TEXT,
    created_at           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_responses_conversation_id ON responses (conversation_id);
CREATE INDEX IF NOT EXISTS idx_responses_previous_response_id ON responses (previous_response_id);
CREATE INDEX IF NOT EXISTS idx_responses_created_at ON responses (created_at);
