-- Membership snapshots survive detach and source deletion. Store deletion removes its batch history.
ALTER TABLE file_search_attachments ADD COLUMN generation TEXT NOT NULL DEFAULT '';
CREATE TABLE file_search_batches (
 id TEXT PRIMARY KEY, store_id TEXT NOT NULL REFERENCES file_search_stores(id) ON DELETE CASCADE,
 created_at BIGINT NOT NULL, cancelled BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE file_search_jobs (
 id TEXT PRIMARY KEY, batch_id TEXT NOT NULL REFERENCES file_search_batches(id) ON DELETE CASCADE,
 store_id TEXT NOT NULL, file_id TEXT NOT NULL, generation TEXT NOT NULL,
 created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, state TEXT NOT NULL,
 options TEXT NOT NULL, identity TEXT NOT NULL, snapshot TEXT NOT NULL,
 claim_token TEXT, lease_until BIGINT, attempts BIGINT NOT NULL DEFAULT 0,
 UNIQUE(batch_id, file_id)
);
CREATE INDEX file_search_job_claims ON file_search_jobs(state, lease_until, created_at, id);
CREATE INDEX file_search_job_members ON file_search_jobs(batch_id, created_at, file_id);
