-- Nullable expiry preserves existing uploads, including legacy inline rows.
ALTER TABLE file_search_files ADD COLUMN expires_at BIGINT;
ALTER TABLE file_search_files ADD COLUMN purpose TEXT;
CREATE INDEX file_search_files_expiration ON file_search_files(expires_at);
CREATE TABLE file_search_blob_cleanup (file_id TEXT PRIMARY KEY);
