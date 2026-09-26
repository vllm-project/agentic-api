-- A public item may occur in multiple conversation histories.
-- The primary key remains the identity of the individual history row.
ALTER TABLE items ADD COLUMN reference_id TEXT REFERENCES items(id);
CREATE INDEX idx_items_conversation_reference ON items (conversation_id, reference_id);
UPDATE items SET tenant_id = COALESCE(
    (SELECT tenant_id FROM conversations WHERE conversations.id = items.conversation_id),
    'default_tenant'
) WHERE tenant_id IS NULL;
