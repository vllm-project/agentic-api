-- Legacy rows deliberately retain unknown provenance (NULL). Never infer an origin
-- from public reasoning fields or backfill provider identity from response metadata.
ALTER TABLE items ADD COLUMN reasoning_provenance TEXT;
