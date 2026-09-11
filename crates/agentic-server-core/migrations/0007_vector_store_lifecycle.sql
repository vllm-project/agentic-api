-- Lifecycle columns allow visibility and filtered pagination before row limits.
-- Legacy stores remain permanent; their creation time seeds first activity.
ALTER TABLE file_search_stores ADD COLUMN last_active_at BIGINT;
ALTER TABLE file_search_stores ADD COLUMN expires_after_days BIGINT;
ALTER TABLE file_search_stores ADD COLUMN expires_at BIGINT;
ALTER TABLE file_search_stores ADD COLUMN lifecycle_status TEXT NOT NULL DEFAULT 'completed';
UPDATE file_search_stores SET last_active_at = created_at;
CREATE INDEX file_search_store_expiration ON file_search_stores(lifecycle_status, expires_at, id);

-- Old successful attachments can recover parsed text from their original upload.
ALTER TABLE file_search_attachments ADD COLUMN status TEXT NOT NULL DEFAULT 'completed';
ALTER TABLE file_search_attachments ADD COLUMN parsed_content TEXT;
CREATE INDEX file_search_attachment_status ON file_search_attachments(store_id, status, created_at, file_id);
