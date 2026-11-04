-- Items record their kind in typed `metadata` instead of a hidden key inside `data`, every row carries its public
-- identity, and the unused `reference_id` is removed.
--
-- Databases created before this migration hold data in the earlier format: the gateway recreates the default SQLite
-- database and rejects any other database until it is reset. This migration therefore only runs on an empty
-- database, which is what lets it add NOT NULL columns without a default.
DROP INDEX idx_items_public_identity;
DROP INDEX idx_items_conversation_reference;
ALTER TABLE items DROP COLUMN reference_id;

-- `public_id` was nullable while older rows resolved through `reference_id` or `id`; every row now stores it.
ALTER TABLE items DROP COLUMN public_id;
ALTER TABLE items ADD COLUMN public_id TEXT NOT NULL;
CREATE INDEX idx_items_tenant_public_id ON items (tenant_id, public_id);

ALTER TABLE items ADD COLUMN metadata TEXT NOT NULL;

-- The response exactly as returned, for `GET /v1/responses/{id}`. It has its own column so that continuing a
-- response decodes only the continuation settings in `metadata`, never the full response.
ALTER TABLE responses ADD COLUMN snapshot TEXT;
