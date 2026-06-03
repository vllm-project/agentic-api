ALTER TABLE conversations ADD COLUMN tenant_id TEXT;
ALTER TABLE conversations ADD COLUMN metadata TEXT;

ALTER TABLE items ADD COLUMN tenant_id TEXT;
ALTER TABLE items ADD COLUMN raw_tokens TEXT;

ALTER TABLE responses ADD COLUMN tenant_id TEXT;
ALTER TABLE responses ADD COLUMN raw_tokens TEXT;

CREATE INDEX IF NOT EXISTS idx_conversations_tenant_id ON conversations (tenant_id);
CREATE INDEX IF NOT EXISTS idx_items_tenant_id ON items (tenant_id);
CREATE INDEX IF NOT EXISTS idx_responses_tenant_id ON responses (tenant_id);
