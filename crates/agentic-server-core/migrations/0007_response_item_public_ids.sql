-- Response history needs a separate row for each occurrence of a public item ID.
ALTER TABLE items ADD COLUMN public_id TEXT;
