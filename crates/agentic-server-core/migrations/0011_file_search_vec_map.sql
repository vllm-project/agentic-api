-- Portable metadata for the optional sqlite-vec index. SQLite assigns the
-- integer primary key used as each vec0 rowid; PostgreSQL retains the schema
-- contract even though the sqlite-vec provider cannot run there.
CREATE TABLE IF NOT EXISTS file_search_vec_map (
    id INTEGER PRIMARY KEY,
    store_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    chunk_index BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE (store_id, file_id, chunk_index)
);

CREATE INDEX IF NOT EXISTS file_search_vec_map_file ON file_search_vec_map(file_id);
