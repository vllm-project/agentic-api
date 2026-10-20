-- Response history needs a separate row for each occurrence of a public item ID.
-- The primary key remains the identity of the individual history row.
ALTER TABLE items ADD COLUMN public_id TEXT;

-- Item lookups resolve public identity with COALESCE(public_id, reference_id, id). Index the exact
-- expression so a conversation insert finds its source row for one tenant instead of scanning every
-- item, and so rows written before this column still resolve through reference_id or id.
CREATE INDEX idx_items_public_identity ON items (tenant_id, (COALESCE(public_id, reference_id, id)));
