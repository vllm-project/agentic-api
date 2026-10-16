-- Response history needs a separate row for each occurrence of a public item ID.
-- The primary key remains the identity of the individual history row.
ALTER TABLE items ADD COLUMN public_id TEXT;
CREATE INDEX idx_items_public_id ON items (public_id);
