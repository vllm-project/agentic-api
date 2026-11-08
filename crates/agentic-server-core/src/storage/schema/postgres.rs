//! PostgreSQL schema compatibility contract: required columns, constraints, and indices.

use super::DbResult;

pub(super) const REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT: i64 = 59;
pub(super) const REQUIRED_POSTGRES_CONSTRAINT_COUNT: i64 = 21;
pub(super) const REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT: i64 = 4;

pub(super) async fn postgres_required_schema_column_count<'e, E>(executor: E) -> DbResult<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_scalar(
        "WITH required(table_name, column_name, data_type, is_nullable) AS ( \
             VALUES \
                 ('conversations', 'id', 'text', 'NO'), \
                 ('conversations', 'created_at', 'integer', 'NO'), \
                 ('conversations', 'tenant_id', 'text', 'YES'), \
                 ('conversations', 'metadata', 'text', 'YES'), \
                 ('conversations', 'latest_response_id', 'text', 'YES'), \
                 ('items', 'id', 'text', 'NO'), \
                 ('items', 'data', 'text', 'NO'), \
                 ('items', 'public_id', 'text', 'NO'), \
                 ('items', 'metadata', 'text', 'NO'), \
                 ('items', 'created_at', 'integer', 'NO'), \
                 ('items', 'conversation_id', 'text', 'YES'), \
                 ('items', 'seq', 'integer', 'YES'), \
                 ('items', 'tenant_id', 'text', 'YES'), \
                 ('items', 'raw_tokens', 'text', 'YES'), \
                 ('responses', 'id', 'text', 'NO'), \
                 ('responses', 'conversation_id', 'text', 'YES'), \
                 ('responses', 'previous_response_id', 'text', 'YES'), \
                 ('responses', 'history_item_ids', 'text', 'YES'), \
                 ('responses', 'snapshot', 'text', 'YES'), \
                 ('responses', 'metadata', 'text', 'YES'), \
                 ('responses', 'created_at', 'integer', 'NO'), \
                 ('responses', 'tenant_id', 'text', 'YES'), \
                 ('responses', 'raw_tokens', 'text', 'YES'), \
                 ('file_search_files', 'id', 'text', 'NO'), \
                 ('file_search_files', 'created_at', 'bigint', 'NO'), \
                 ('file_search_files', 'data', 'text', 'NO'), \
                 ('file_search_files', 'content_type', 'text', 'NO'), \
                 ('file_search_files', 'content_base64', 'text', 'NO'), \
                 ('file_search_blob_cleanup', 'file_id', 'text', 'NO'), \
                 ('file_search_blob_cleanup', 'owner_token', 'text', 'NO'), \
                 ('file_search_blob_cleanup', 'intent', 'text', 'NO'), \
                 ('file_search_blob_cleanup', 'lease_expires_at', 'bigint', 'NO'), \
                 ('file_search_stores', 'id', 'text', 'NO'), \
                 ('file_search_stores', 'created_at', 'bigint', 'NO'), \
                 ('file_search_stores', 'data', 'text', 'NO'), \
                 ('file_search_stores', 'embedding_identity', 'text', 'NO'), \
                 ('file_search_stores', 'embedding_dimensions', 'bigint', 'NO'), \
                 ('file_search_attachments', 'store_id', 'text', 'NO'), \
                 ('file_search_attachments', 'file_id', 'text', 'NO'), \
                 ('file_search_attachments', 'created_at', 'bigint', 'NO'), \
                 ('file_search_attachments', 'usage_bytes', 'bigint', 'NO'), \
                 ('file_search_attachments', 'storage_bytes', 'bigint', 'NO'), \
                 ('file_search_attachments', 'data', 'text', 'NO'), \
                 ('file_search_chunks', 'store_id', 'text', 'NO'), \
                 ('file_search_chunks', 'file_id', 'text', 'NO'), \
                 ('file_search_chunks', 'chunk_index', 'bigint', 'NO'), \
                 ('file_search_chunks', 'data', 'text', 'NO'), \
                 ('file_search_vec_map', 'id', 'integer', 'NO'), \
                 ('file_search_vec_map', 'store_id', 'text', 'NO'), \
                 ('file_search_vec_map', 'file_id', 'text', 'NO'), \
                 ('file_search_vec_map', 'chunk_index', 'bigint', 'NO'), \
                 ('file_search_vec_map', 'created_at', 'bigint', 'NO'), \
                 ('message_threads', 'message_id', 'text', 'NO'), \
                 ('message_threads', 'owner_hash', 'text', 'NO'), \
                 ('message_threads', 'checkpoint', 'text', 'YES'), \
                 ('message_threads', 'state', 'text', 'NO'), \
                 ('message_threads', 'lease_token', 'text', 'YES'), \
                 ('message_threads', 'lease_until', 'bigint', 'NO'), \
                 ('message_threads', 'expires_at', 'bigint', 'NO') \
         ) \
         SELECT COUNT(*) \
         FROM required \
         JOIN information_schema.columns actual \
           ON actual.table_name = required.table_name \
          AND actual.column_name = required.column_name \
          AND actual.is_nullable = required.is_nullable \
          AND (actual.data_type = required.data_type \
               OR (required.data_type = 'integer' AND actual.data_type = 'bigint')) \
         JOIN pg_class table_relation \
           ON table_relation.relname = actual.table_name \
          AND table_relation.relkind IN ('r', 'p') \
          AND pg_table_is_visible(table_relation.oid) \
         JOIN pg_namespace table_namespace \
           ON table_namespace.oid = table_relation.relnamespace \
          AND table_namespace.nspname = actual.table_schema",
    )
    .fetch_one(executor)
    .await
}

pub(super) async fn postgres_required_constraint_count<'e, E>(executor: E) -> DbResult<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_scalar(
        "WITH required(table_name, constraint_type, definition) AS ( \
             VALUES \
                 ('file_search_vec_map', 'p', 'PRIMARY KEY (id)'), \
                 ('file_search_vec_map', 'u', 'UNIQUE (store_id, file_id, chunk_index)'), \
                 ('message_threads', 'p', 'PRIMARY KEY (message_id)'), \
                 ('message_threads', 'c', 'CHECK ((state = ANY (ARRAY[''ready''::text, ''in_flight''::text, ''consumed''::text])))'), \
                 ('message_threads', 'c', 'CHECK ((((state = ''consumed''::text) AND (checkpoint IS NULL)) OR ((state <> ''consumed''::text) AND (checkpoint IS NOT NULL))))'), \
                 ('message_threads', 'c', 'CHECK ((((state = ''in_flight''::text) AND (lease_token IS NOT NULL)) OR ((state <> ''in_flight''::text) AND (lease_token IS NULL))))'), \
                 ('conversations', 'p', 'PRIMARY KEY (id)'), \
                 ('items', 'p', 'PRIMARY KEY (id)'), \
                 ('responses', 'p', 'PRIMARY KEY (id)'), \
                 ('items', 'f', \
                  'FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE'), \
                 ('responses', 'f', \
                  'FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL'), \
                 ('responses', 'f', \
                  'FOREIGN KEY (previous_response_id) REFERENCES responses(id) ON DELETE SET NULL'), \
                 ('conversations', 'f', \
                  'FOREIGN KEY (latest_response_id) REFERENCES responses(id) ON DELETE SET NULL'), \
                 ('file_search_files', 'p', 'PRIMARY KEY (id)'), \
                 ('file_search_blob_cleanup', 'p', 'PRIMARY KEY (file_id)'), \
                 ('file_search_stores', 'p', 'PRIMARY KEY (id)'), \
                 ('file_search_attachments', 'p', 'PRIMARY KEY (store_id, file_id)'), \
                 ('file_search_chunks', 'p', 'PRIMARY KEY (store_id, file_id, chunk_index)'), \
                 ('file_search_attachments', 'f', 'FOREIGN KEY (store_id) REFERENCES file_search_stores(id) ON DELETE CASCADE'), \
                 ('file_search_attachments', 'f', 'FOREIGN KEY (file_id) REFERENCES file_search_files(id) ON DELETE CASCADE'), \
                 ('file_search_chunks', 'f', 'FOREIGN KEY (store_id, file_id) REFERENCES file_search_attachments(store_id, file_id) ON DELETE CASCADE') \
         ) \
         SELECT COUNT(*) \
         FROM required \
         WHERE EXISTS ( \
             SELECT 1 \
             FROM pg_constraint constraint_metadata \
             JOIN pg_class table_relation ON table_relation.oid = constraint_metadata.conrelid \
             WHERE table_relation.relname = required.table_name \
             AND pg_table_is_visible(table_relation.oid) \
             AND constraint_metadata.contype::text = required.constraint_type \
             AND pg_get_constraintdef(constraint_metadata.oid) = required.definition \
         )",
    )
    .fetch_one(executor)
    .await
}

pub(super) async fn postgres_integer_column_compatibility<'e, E>(executor: E) -> DbResult<(i64, i64)>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE actual.data_type <> 'bigint') \
         FROM information_schema.columns actual \
         JOIN pg_class table_relation \
           ON table_relation.relname = actual.table_name \
          AND table_relation.relkind IN ('r', 'p') \
          AND pg_table_is_visible(table_relation.oid) \
         JOIN pg_namespace table_namespace \
           ON table_namespace.oid = table_relation.relnamespace \
          AND table_namespace.nspname = actual.table_schema \
         WHERE (actual.table_name = 'conversations' AND actual.column_name = 'created_at') \
            OR (actual.table_name = 'items' AND actual.column_name IN ('created_at', 'seq')) \
            OR (actual.table_name = 'responses' AND actual.column_name = 'created_at')",
    )
    .fetch_one(executor)
    .await
}

pub(super) fn validate_required_postgres_schema(
    schema_column_count: i64,
    constraint_count: i64,
    integer_column_count: i64,
    sequence_index_ready: bool,
) -> DbResult<()> {
    if schema_column_count == REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT
        && constraint_count == REQUIRED_POSTGRES_CONSTRAINT_COUNT
        && integer_column_count == REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT
        && sequence_index_ready
    {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(
        "database schema is missing required PostgreSQL tables, columns, constraints, or indexes".into(),
    ))
}

pub(super) fn validate_supervisor_schema(
    schema_column_count: i64,
    constraint_count: i64,
    integer_column_count: i64,
    narrow_column_count: i64,
    sequence_index_ready: bool,
) -> DbResult<()> {
    validate_required_postgres_schema(
        schema_column_count,
        constraint_count,
        integer_column_count,
        sequence_index_ready,
    )?;
    if narrow_column_count == 0 {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(
        "supervisor-managed PostgreSQL schema requires BIGINT compatibility upgrade; \
         apply the documented ALTER TABLE statements before setting AGENTIC_API_SCHEMA_READY"
            .into(),
    ))
}
