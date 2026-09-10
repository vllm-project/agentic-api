-- Portable durable files and exact retrieval. Raw bytes use base64 TEXT because
-- SQLx Any does not expose a common SQLite BLOB / PostgreSQL BYTEA bind type.
CREATE TABLE file_search_files (
    id TEXT PRIMARY KEY,
    created_at BIGINT NOT NULL,
    data TEXT NOT NULL,
    content_type TEXT NOT NULL,
    content_base64 TEXT NOT NULL
);

CREATE TABLE file_search_stores (
    id TEXT PRIMARY KEY,
    created_at BIGINT NOT NULL,
    data TEXT NOT NULL,
    embedding_identity TEXT NOT NULL,
    embedding_dimensions BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE file_search_attachments (
    store_id TEXT NOT NULL REFERENCES file_search_stores(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL REFERENCES file_search_files(id) ON DELETE CASCADE,
    created_at BIGINT NOT NULL,
    usage_bytes BIGINT NOT NULL,
    storage_bytes BIGINT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (store_id, file_id)
);

CREATE TABLE file_search_chunks (
    store_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    chunk_index BIGINT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (store_id, file_id, chunk_index),
    FOREIGN KEY (store_id, file_id) REFERENCES file_search_attachments(store_id, file_id) ON DELETE CASCADE
);

CREATE INDEX file_search_attachment_file ON file_search_attachments(file_id);
