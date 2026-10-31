-- Find a response's history by walking previous_response_id instead of copying the full ordered
-- item ID list into every response.
--
-- Stop-the-world: stop every replica before migrating. Existing responses get explicit history lists;
-- their history_item_ids lists are moved into response_history_refs by the startup step in
-- storage/schema/chain_history.rs (JSON array expansion differs between SQLite and PostgreSQL).
-- Converted and new rows hold the non-array marker '{}' there, so an older binary fails closed.

UPDATE conversations SET tenant_id = 'default_tenant' WHERE tenant_id IS NULL;
UPDATE responses SET tenant_id = 'default_tenant' WHERE tenant_id IS NULL;
UPDATE items SET tenant_id = 'default_tenant' WHERE tenant_id IS NULL;
UPDATE items SET public_id = COALESCE(reference_id, id) WHERE public_id IS NULL;

-- 0 chain: the previous response's history, then this response's own items.
-- 1 list: the ordered response_history_refs rows.
-- 2 conversation: the conversation's entries as of conversation_revision.
ALTER TABLE responses ADD COLUMN history_base BIGINT NOT NULL DEFAULT 1;
ALTER TABLE responses ADD COLUMN conversation_revision BIGINT;

-- The response that stored the item and its position there; NULL for items added through the
-- Conversations items API and for items stored before this migration.
ALTER TABLE items ADD COLUMN response_id TEXT REFERENCES responses(id) ON DELETE SET NULL;
ALTER TABLE items ADD COLUMN position BIGINT;
-- Server-owned origin of a reasoning item, outside its public JSON. NULL means unknown.
ALTER TABLE items ADD COLUMN reasoning_provenance TEXT;
CREATE UNIQUE INDEX idx_items_response_position ON items (response_id, position);
CREATE INDEX idx_items_tenant_public_id ON items (tenant_id, public_id);

-- Ordered history of a response whose history is an explicit list.
CREATE TABLE response_history_refs (
    response_id TEXT NOT NULL REFERENCES responses(id) ON DELETE CASCADE,
    position    BIGINT NOT NULL,
    item_id     TEXT NOT NULL REFERENCES items(id),
    PRIMARY KEY (response_id, position)
);

CREATE INDEX idx_response_history_refs_item ON response_history_refs (item_id);

ALTER TABLE conversations ADD COLUMN next_seq BIGINT NOT NULL DEFAULT 0;
-- Set when the conversation is deleted while stored responses still read its earlier states.
ALTER TABLE conversations ADD COLUMN deleted_at BIGINT;

-- A conversation's item list. Deleting an item records the revision that removed it, so the
-- conversation as of any earlier turn stays readable.
CREATE TABLE conversation_entries (
    conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    seq              BIGINT NOT NULL,
    item_id          TEXT NOT NULL REFERENCES items(id),
    public_id        TEXT NOT NULL,
    added_revision   BIGINT NOT NULL,
    removed_revision BIGINT,
    PRIMARY KEY (conversation_id, seq)
);

CREATE INDEX idx_conversation_entries_public_id ON conversation_entries (conversation_id, public_id);
CREATE INDEX idx_conversation_entries_item ON conversation_entries (item_id);

INSERT INTO conversation_entries (conversation_id, seq, item_id, public_id, added_revision)
SELECT conversation_id, seq, id, public_id, 0
FROM items
WHERE conversation_id IS NOT NULL AND seq IS NOT NULL;

UPDATE conversations SET next_seq = COALESCE(
    (SELECT MAX(seq) + 1 FROM items WHERE items.conversation_id = conversations.id),
    0
);
