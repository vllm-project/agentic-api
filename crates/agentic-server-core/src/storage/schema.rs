//! Database schema management and migrations.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::Connection;
use tracing::{debug, info};

use super::backend::{DatabaseBackend, configure_postgres_timeouts};
use super::pool::DbPool;
use crate::config::DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS;

type DbResult<T> = Result<T, sqlx::Error>;

const POSTGRES_SCHEMA_ADVISORY_LOCK: i64 = 7_194_963_546_799_751;
const REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT: i64 = 68;
const REQUIRED_POSTGRES_CONSTRAINT_COUNT: i64 = 20;
const REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT: i64 = 4;
const POSTGRES_INTEGER_WIDENING_SQL: &str = "
    ALTER TABLE conversations
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT;
    ALTER TABLE items
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT,
        ALTER COLUMN seq TYPE BIGINT USING seq::BIGINT;
    ALTER TABLE responses
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT;
";

async fn configure_postgres_migration_timeout(
    connection: &mut sqlx::AnyConnection,
    migration_timeout: Duration,
) -> DbResult<()> {
    configure_postgres_timeouts(connection, migration_timeout, migration_timeout).await
}

async fn widen_postgres_integer_columns(connection: &mut sqlx::AnyConnection) -> DbResult<()> {
    let mut transaction = connection.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(POSTGRES_SCHEMA_ADVISORY_LOCK)
        .execute(&mut *transaction)
        .await?;
    let schema_column_count = postgres_required_schema_column_count(&mut *transaction).await?;
    let constraint_count = postgres_required_constraint_count(&mut *transaction).await?;
    let (integer_column_count, narrow_column_count) = postgres_integer_column_compatibility(&mut *transaction).await?;
    let sequence_index_ready = postgres_sequence_index_ready(&mut *transaction).await?;
    validate_required_postgres_schema(
        schema_column_count,
        constraint_count,
        integer_column_count,
        sequence_index_ready,
    )?;
    if narrow_column_count > 0 {
        sqlx::raw_sql(POSTGRES_INTEGER_WIDENING_SQL)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await
}

async fn postgres_required_schema_column_count<'e, E>(executor: E) -> DbResult<i64>
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
                 ('items', 'created_at', 'integer', 'NO'), \
                 ('items', 'conversation_id', 'text', 'YES'), \
                 ('items', 'seq', 'integer', 'YES'), \
                 ('items', 'tenant_id', 'text', 'YES'), \
                 ('items', 'raw_tokens', 'text', 'YES'), \
                 ('responses', 'id', 'text', 'NO'), \
                 ('responses', 'conversation_id', 'text', 'YES'), \
                 ('responses', 'previous_response_id', 'text', 'YES'), \
                 ('responses', 'history_item_ids', 'text', 'YES'), \
                 ('responses', 'metadata', 'text', 'YES'), \
                 ('responses', 'created_at', 'integer', 'NO'), \
                 ('responses', 'tenant_id', 'text', 'YES'), \
                 ('responses', 'raw_tokens', 'text', 'YES'), \
                 ('file_search_files', 'id', 'text', 'NO'), \
                 ('file_search_files', 'created_at', 'bigint', 'NO'), \
                 ('file_search_files', 'data', 'text', 'NO'), \
                 ('file_search_files', 'content_type', 'text', 'NO'), \
                 ('file_search_files', 'content_base64', 'text', 'NO'), \
                 ('file_search_files', 'expires_at', 'bigint', 'YES'), \
                 ('file_search_files', 'purpose', 'text', 'YES'), \
                 ('file_search_blob_cleanup', 'file_id', 'text', 'NO'), \
                 ('file_search_stores', 'id', 'text', 'NO'), \
                 ('file_search_stores', 'created_at', 'bigint', 'NO'), \
                 ('file_search_stores', 'data', 'text', 'NO'), \
                 ('file_search_stores', 'embedding_identity', 'text', 'NO'), \
                 ('file_search_stores', 'embedding_dimensions', 'bigint', 'NO'), \
                 ('file_search_stores', 'last_active_at', 'bigint', 'YES'), \
                 ('file_search_stores', 'expires_after_days', 'bigint', 'YES'), \
                 ('file_search_stores', 'expires_at', 'bigint', 'YES'), \
                 ('file_search_stores', 'lifecycle_status', 'text', 'NO'), \
                 ('file_search_attachments', 'store_id', 'text', 'NO'), \
                 ('file_search_attachments', 'file_id', 'text', 'NO'), \
                 ('file_search_attachments', 'created_at', 'bigint', 'NO'), \
                 ('file_search_attachments', 'usage_bytes', 'bigint', 'NO'), \
                 ('file_search_attachments', 'storage_bytes', 'bigint', 'NO'), \
                 ('file_search_attachments', 'data', 'text', 'NO'), \
                 ('file_search_attachments', 'status', 'text', 'NO'), \
                 ('file_search_attachments', 'parsed_content', 'text', 'YES'), \
                 ('file_search_attachments', 'generation', 'text', 'NO'), \
                 ('file_search_batches', 'id', 'text', 'NO'), \
                 ('file_search_batches', 'store_id', 'text', 'NO'), \
                 ('file_search_batches', 'created_at', 'bigint', 'NO'), \
                 ('file_search_batches', 'cancelled', 'bigint', 'NO'), \
                 ('file_search_jobs', 'id', 'text', 'NO'), \
                 ('file_search_jobs', 'batch_id', 'text', 'NO'), \
                 ('file_search_jobs', 'store_id', 'text', 'NO'), \
                 ('file_search_jobs', 'file_id', 'text', 'NO'), \
                 ('file_search_jobs', 'generation', 'text', 'NO'), \
                 ('file_search_jobs', 'created_at', 'bigint', 'NO'), \
                 ('file_search_jobs', 'updated_at', 'bigint', 'NO'), \
                 ('file_search_jobs', 'state', 'text', 'NO'), \
                 ('file_search_jobs', 'options', 'text', 'NO'), \
                 ('file_search_jobs', 'identity', 'text', 'NO'), \
                 ('file_search_jobs', 'snapshot', 'text', 'NO'), \
                 ('file_search_jobs', 'claim_token', 'text', 'YES'), \
                 ('file_search_jobs', 'lease_until', 'bigint', 'YES'), \
                 ('file_search_jobs', 'attempts', 'bigint', 'NO'), \
                 ('file_search_chunks', 'store_id', 'text', 'NO'), \
                 ('file_search_chunks', 'file_id', 'text', 'NO'), \
                 ('file_search_chunks', 'chunk_index', 'bigint', 'NO'), \
                 ('file_search_chunks', 'data', 'text', 'NO') \
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

async fn postgres_required_constraint_count<'e, E>(executor: E) -> DbResult<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_scalar(
        "WITH required(table_name, constraint_type, definition) AS ( \
             VALUES \
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
                 ('file_search_batches', 'p', 'PRIMARY KEY (id)'), \
                 ('file_search_batches', 'f', 'FOREIGN KEY (store_id) REFERENCES file_search_stores(id) ON DELETE CASCADE'), \
                 ('file_search_jobs', 'p', 'PRIMARY KEY (id)'), \
                 ('file_search_jobs', 'f', 'FOREIGN KEY (batch_id) REFERENCES file_search_batches(id) ON DELETE CASCADE'), \
                 ('file_search_jobs', 'u', 'UNIQUE (batch_id, file_id)'), \
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

async fn postgres_integer_column_compatibility<'e, E>(executor: E) -> DbResult<(i64, i64)>
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

async fn postgres_sequence_index_ready<'e, E>(executor: E) -> DbResult<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 \
             FROM pg_index index_metadata \
             JOIN pg_class index_relation ON index_relation.oid = index_metadata.indexrelid \
             JOIN pg_class table_relation ON table_relation.oid = index_metadata.indrelid \
             WHERE table_relation.relname = 'items' \
             AND pg_table_is_visible(table_relation.oid) \
             AND index_relation.relname = 'idx_items_conversation_id' \
             AND index_metadata.indisvalid \
             AND index_metadata.indisready \
             AND index_metadata.indisunique \
             AND pg_get_indexdef(index_metadata.indexrelid) LIKE '%(conversation_id, seq)' \
         )",
    )
    .fetch_one(executor)
    .await
}

fn validate_required_postgres_schema(
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

fn validate_supervisor_schema(
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

async fn verify_supervisor_managed_postgres_schema(
    pool: &DbPool,
    postgres_migration_timeout: Duration,
) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    if DatabaseBackend::from_connection(&connection) != DatabaseBackend::Postgres {
        return Ok(());
    }

    if let Err(error) = configure_postgres_migration_timeout(&mut connection, postgres_migration_timeout).await {
        let _ = connection.close().await;
        return Err(error);
    }
    let compatibility_result = async {
        let schema_column_count = postgres_required_schema_column_count(&mut *connection).await?;
        let constraint_count = postgres_required_constraint_count(&mut *connection).await?;
        let (integer_column_count, narrow_column_count) =
            postgres_integer_column_compatibility(&mut *connection).await?;
        let sequence_index_ready = postgres_sequence_index_ready(&mut *connection).await?;
        validate_supervisor_schema(
            schema_column_count,
            constraint_count,
            integer_column_count,
            narrow_column_count,
            sequence_index_ready,
        )
    }
    .await;
    let close_result = connection.close().await;
    compatibility_result?;
    close_result
}

async fn apply_postgres_compatibility(
    connection: &mut sqlx::AnyConnection,
    postgres_migration_timeout: Duration,
) -> DbResult<()> {
    configure_postgres_migration_timeout(connection, postgres_migration_timeout).await?;
    widen_postgres_integer_columns(connection).await
}

fn migration_error(error: sqlx::migrate::MigrateError) -> sqlx::Error {
    error.into()
}

pub(crate) async fn pin_postgres_persistence_schema(connection: &mut sqlx::AnyConnection) -> DbResult<()> {
    let populated_search_path_schemas: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT table_namespace.nspname::text \
         FROM pg_class table_relation \
         JOIN pg_namespace table_namespace ON table_namespace.oid = table_relation.relnamespace \
         WHERE table_namespace.nspname = ANY(current_schemas(false)) \
         AND table_relation.relkind IN ('r', 'p', 'v', 'm', 'f') \
         AND table_relation.relname IN ('_sqlx_migrations', 'conversations', 'items', 'responses', \
              'file_search_files', 'file_search_stores', 'file_search_attachments', 'file_search_chunks', 'file_search_blob_cleanup', 'file_search_batches', 'file_search_jobs') \
         ORDER BY table_namespace.nspname::text",
    )
    .fetch_all(&mut *connection)
    .await?;
    let target_schema = match populated_search_path_schemas.as_slice() {
        [] => sqlx::query_scalar::<_, Option<String>>("SELECT current_schema()::text")
            .fetch_one(&mut *connection)
            .await?
            .ok_or_else(|| {
                sqlx::Error::Configuration(
                    "PostgreSQL search_path does not contain an existing schema for migrations".into(),
                )
            })?,
        [schema] => schema.clone(),
        _ => {
            return Err(sqlx::Error::Configuration(
                "PostgreSQL persistence tables or migration history exist in multiple search_path schemas".into(),
            ));
        }
    };
    sqlx::query("SELECT set_config('search_path', quote_ident($1), false)")
        .bind(target_schema)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

pub(crate) async fn verify_persistence_writable(pool: &DbPool) -> DbResult<()> {
    let mut transaction = pool.begin().await?;
    let probe_result = async {
        let suffix = uuid::Uuid::now_v7().simple();
        let conversation_id = format!("conv_readiness_{suffix}");
        let item_id = format!("item_readiness_{suffix}");
        let response_id = format!("resp_readiness_{suffix}");
        let created_at = crate::utils::common::utcnow_str();
        sqlx::query("INSERT INTO conversations (id, created_at) VALUES ($1, $2)")
            .bind(&conversation_id)
            .bind(created_at)
            .execute(&mut *transaction)
            .await?;
        crate::storage::models::conversation::lock_in_tx(&mut transaction, &conversation_id).await?;
        crate::storage::models::item::create_in_tx(
            &mut transaction,
            vec![(item_id.clone(), "{}".to_owned())],
            Some(&conversation_id),
        )
        .await?;
        crate::storage::models::response::create_in_tx(
            &mut transaction,
            &response_id,
            Some(&conversation_id),
            None,
            Some(&format!("[\"{item_id}\"]")),
            Some("{}"),
        )
        .await?;
        crate::storage::models::conversation::set_latest_response_in_tx(
            &mut transaction,
            &conversation_id,
            &response_id,
        )
        .await?;
        Ok(())
    }
    .await;
    match probe_result {
        Ok(()) => transaction.rollback().await,
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

pub(crate) async fn verify_persistence_ready(pool: &DbPool) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    match DatabaseBackend::from_connection(&connection) {
        DatabaseBackend::Postgres => {
            let ready: bool = sqlx::query_scalar(
                "WITH required(table_name, privilege) AS ( \
                     VALUES \
                         ('conversations', 'SELECT'), \
                         ('conversations', 'INSERT'), \
                         ('conversations', 'UPDATE'), \
                         ('items', 'SELECT'), \
                         ('items', 'INSERT'), \
                         ('responses', 'SELECT'), \
                         ('responses', 'INSERT'), \
                         ('file_search_batches', 'SELECT'), \
                         ('file_search_batches', 'INSERT'), \
                         ('file_search_batches', 'UPDATE'), \
                         ('file_search_batches', 'DELETE'), \
                         ('file_search_jobs', 'SELECT'), \
                         ('file_search_jobs', 'INSERT'), \
                         ('file_search_jobs', 'UPDATE'), \
                         ('file_search_jobs', 'DELETE'), \
                         ('file_search_files', 'SELECT'), \
                         ('file_search_files', 'INSERT'), \
                         ('file_search_files', 'UPDATE'), \
                         ('file_search_blob_cleanup', 'SELECT'), \
                         ('file_search_blob_cleanup', 'INSERT'), \
                         ('file_search_blob_cleanup', 'DELETE'), \
                         ('file_search_files', 'DELETE'), \
                         ('file_search_stores', 'SELECT'), \
                         ('file_search_stores', 'INSERT'), \
                         ('file_search_stores', 'UPDATE'), \
                         ('file_search_stores', 'DELETE'), \
                         ('file_search_attachments', 'SELECT'), \
                         ('file_search_attachments', 'INSERT'), \
                         ('file_search_attachments', 'DELETE'), \
                         ('file_search_chunks', 'SELECT'), \
                         ('file_search_attachments', 'UPDATE'), \
                         ('file_search_chunks', 'UPDATE'), \
                         ('file_search_chunks', 'INSERT') \
                 ) \
                 SELECT current_setting('transaction_read_only') = 'off' \
                    AND COUNT(table_relation.oid) = 33 \
                    AND COALESCE(BOOL_AND( \
                        has_table_privilege(current_user, table_relation.oid, required.privilege) \
                    ), false) \
                 FROM required \
                 LEFT JOIN pg_class table_relation \
                   ON table_relation.relname = required.table_name \
                  AND table_relation.relkind IN ('r', 'p') \
                  AND pg_table_is_visible(table_relation.oid)",
            )
            .fetch_one(&mut *connection)
            .await?;
            if !ready {
                return Err(sqlx::Error::Configuration(
                    "PostgreSQL persistence tables are unavailable, read-only, or missing required privileges".into(),
                ));
            }
        }
        DatabaseBackend::Sqlite => {
            let query_only: i64 = sqlx::query_scalar("PRAGMA query_only")
                .fetch_one(&mut *connection)
                .await?;
            if query_only != 0 {
                return Err(sqlx::Error::Configuration("SQLite persistence is read-only".into()));
            }
            for statement in [
                "SELECT id FROM conversations LIMIT 0",
                "SELECT id FROM items LIMIT 0",
                "SELECT id FROM responses LIMIT 0",
                "SELECT id, created_at, data, content_type, content_base64, expires_at, purpose FROM file_search_files LIMIT 0",
                "SELECT file_id FROM file_search_blob_cleanup LIMIT 0",
                "SELECT id, store_id, created_at, cancelled FROM file_search_batches LIMIT 0",
                "SELECT id,batch_id,store_id,file_id,generation,created_at,updated_at,state,options,identity,snapshot,claim_token,lease_until,attempts FROM file_search_jobs LIMIT 0",
                "SELECT id, created_at, data, embedding_identity, embedding_dimensions, last_active_at, expires_after_days, expires_at, lifecycle_status FROM file_search_stores LIMIT 0",
                "SELECT store_id, file_id, created_at, usage_bytes, storage_bytes, data, status, parsed_content, generation FROM file_search_attachments LIMIT 0",
                "SELECT store_id, file_id, chunk_index, data FROM file_search_chunks LIMIT 0",
            ] {
                sqlx::query(statement).execute(&mut *connection).await?;
            }
        }
        DatabaseBackend::Other => {
            sqlx::query("SELECT 1").execute(&mut *connection).await?;
        }
    }
    Ok(())
}

async fn run_embedded_migrations(pool: &DbPool, postgres_migration_timeout: Duration) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    let is_postgres = DatabaseBackend::from_connection(&connection) == DatabaseBackend::Postgres;
    if is_postgres {
        if let Err(error) = configure_postgres_migration_timeout(&mut connection, postgres_migration_timeout).await {
            let _ = connection.close().await;
            return Err(error);
        }
    }

    let migration_result = sqlx::migrate!("./migrations")
        .run(&mut *connection)
        .await
        .map_err(migration_error);
    let postgres_result = if migration_result.is_ok() && is_postgres {
        apply_postgres_compatibility(&mut connection, postgres_migration_timeout).await
    } else {
        Ok(())
    };
    let close_result = if is_postgres {
        connection.close().await
    } else {
        drop(connection);
        Ok(())
    };

    migration_result?;
    postgres_result?;
    close_result
}

fn is_marked_ready() -> bool {
    matches!(
        env::var("AGENTIC_API_SCHEMA_READY").as_deref(),
        Ok("1" | "true" | "t" | "yes" | "y" | "on")
    )
}

/// Database pool with per-pool schema readiness tracking.
///
/// Wraps `DbPool` and adds an `AtomicBool` flag to track schema initialization
/// per pool instance. This eliminates the issue of global state interfering
/// when multiple pools point to different databases.
pub struct PoolWithSchema {
    pool: Arc<DbPool>,
    schema_ready: AtomicBool,
    postgres_migration_timeout: Duration,
}

impl PoolWithSchema {
    /// Creates a new pool with schema tracking.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self::with_postgres_migration_timeout(pool, Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS))
    }

    /// Creates a new pool with schema tracking and a `PostgreSQL` migration timeout.
    #[must_use]
    pub fn with_postgres_migration_timeout(pool: Arc<DbPool>, postgres_migration_timeout: Duration) -> Self {
        Self {
            pool,
            schema_ready: AtomicBool::new(false),
            postgres_migration_timeout,
        }
    }

    /// Returns a reference to the underlying database pool.
    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    /// Ensures database schema is ready by running pending migrations.
    ///
    /// Checks if migrations have already been applied via one of:
    /// 1. Per-pool flag (`schema_ready`)
    /// 2. `AGENTIC_API_SCHEMA_READY` environment variable
    ///
    /// If none of the above, runs all pending migrations from the `migrations/` directory.
    /// Supervisor-managed `PostgreSQL` schemas still verify required compatibility upgrades.
    ///
    /// # Errors
    ///
    /// Returns a [`sqlx::Error`] if migrations fail.
    pub async fn ensure_schema_ready(&self) -> DbResult<()> {
        self.ensure_schema_ready_with_marker(is_marked_ready()).await
    }

    async fn ensure_schema_ready_with_marker(&self, supervisor_managed: bool) -> DbResult<()> {
        if self.schema_ready.load(Ordering::SeqCst) {
            return Ok(());
        }

        if supervisor_managed {
            debug!("[schema] Migrations skipped — marked ready by supervisor.");
            verify_supervisor_managed_postgres_schema(self.pool.as_ref(), self.postgres_migration_timeout).await?;
            verify_persistence_ready(self.pool.as_ref()).await?;
            self.schema_ready.store(true, Ordering::SeqCst);
            return Ok(());
        }

        debug!("[schema] Running migrations...");
        run_embedded_migrations(self.pool.as_ref(), self.postgres_migration_timeout).await?;
        info!("[schema] DB schema ready.");
        self.schema_ready.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Manages database schema initialization and migrations (deprecated).
///
/// This struct is kept for backward compatibility. New code should use
/// [`PoolWithSchema::ensure_schema_ready`] instead.
pub struct SchemaManager<'a> {
    pool: &'a DbPool,
}

impl<'a> SchemaManager<'a> {
    /// Creates a new schema manager for the given database pool (deprecated).
    #[must_use]
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Runs migrations without checking any flag.
    ///
    /// # Errors
    ///
    /// Returns a [`sqlx::Error`] if migrations fail.
    pub async fn run_migrations(&self) -> DbResult<()> {
        debug!("[schema] Running migrations...");
        run_embedded_migrations(
            self.pool,
            Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS),
        )
        .await?;
        info!("[schema] DB schema ready.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_errors_preserve_their_type_and_source() {
        use std::error::Error as _;

        let error = migration_error(sqlx::migrate::MigrateError::VersionMissing(7));
        assert!(matches!(error, sqlx::Error::Migrate(_)));
        assert!(error.source().is_some());
    }

    #[test]
    fn supervisor_schema_validation_requires_columns_constraints_and_sequence_index() {
        assert!(
            validate_supervisor_schema(
                REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT - 1,
                REQUIRED_POSTGRES_CONSTRAINT_COUNT,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                0,
                true,
            )
            .is_err()
        );
        assert!(
            validate_supervisor_schema(
                REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
                REQUIRED_POSTGRES_CONSTRAINT_COUNT,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                0,
                false,
            )
            .is_err()
        );
        assert!(
            validate_supervisor_schema(
                REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
                REQUIRED_POSTGRES_CONSTRAINT_COUNT - 1,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                0,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn test_env_var_pattern() {
        let test_values = vec![
            ("1", true),
            ("true", true),
            ("t", true),
            ("yes", true),
            ("y", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("f", false),
            ("no", false),
            ("n", false),
            ("off", false),
            ("", false),
        ];

        for (val, expected) in test_values {
            let matches = matches!(
                Ok::<&str, String>(val).as_deref(),
                Ok("1" | "true" | "t" | "yes" | "y" | "on")
            );
            assert_eq!(matches, expected, "Mismatch for value '{val}'");
        }
    }

    #[tokio::test]
    async fn file_search_readiness_and_supervisor_marker_require_new_tables() {
        let pool = crate::storage::pool::create_pool(Some("sqlite::memory:"))
            .await
            .unwrap();
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_add_placeholders.sql"),
            include_str!("../../migrations/0003_index_conversation_sequence.sql"),
            include_str!("../../migrations/0004_link_conversation_latest_response.sql"),
        ] {
            sqlx::raw_sql(migration).execute(pool.as_ref()).await.unwrap();
        }
        assert!(
            verify_persistence_ready(pool.as_ref()).await.is_err(),
            "file search tables must be ready before accepting requests"
        );
        let wrapper = PoolWithSchema::new(pool.clone());
        assert!(
            wrapper.ensure_schema_ready_with_marker(true).await.is_err(),
            "a supervisor marker cannot conceal missing file search migrations"
        );
        assert!(!wrapper.schema_ready.load(Ordering::SeqCst));
        sqlx::raw_sql(include_str!("../../migrations/0005_file_search.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        assert!(
            verify_persistence_ready(pool.as_ref()).await.is_err(),
            "expiry migration is required"
        );
        assert!(wrapper.ensure_schema_ready_with_marker(true).await.is_err());
        sqlx::raw_sql(include_str!("../../migrations/0006_file_expiration.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        assert!(
            verify_persistence_ready(pool.as_ref()).await.is_err(),
            "store lifecycle migration is required"
        );
        assert!(wrapper.ensure_schema_ready_with_marker(true).await.is_err());
        sqlx::raw_sql(include_str!("../../migrations/0007_vector_store_lifecycle.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        assert!(
            verify_persistence_ready(pool.as_ref()).await.is_err(),
            "batch migration is required"
        );
        sqlx::raw_sql(include_str!("../../migrations/0008_vector_store_batches.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        verify_persistence_ready(pool.as_ref()).await.unwrap();
        wrapper.ensure_schema_ready_with_marker(true).await.unwrap();
    }

    #[tokio::test]
    async fn test_pool_with_schema_ready() {
        let pool = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool");

        let pool_with_schema = PoolWithSchema::new(pool);

        // First call should run migrations
        let result = pool_with_schema.ensure_schema_ready().await;
        assert!(result.is_ok(), "ensure_schema_ready failed: {result:?}");

        // Flag should now be set
        assert!(pool_with_schema.schema_ready.load(Ordering::SeqCst));

        // Second call should return immediately without doing work
        let result = pool_with_schema.ensure_schema_ready().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_multiple_pools_independent() {
        // Create two in-memory pools
        let pool1 = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool1");

        let pool2 = crate::storage::pool::create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("failed to create pool2");

        let pwc1 = PoolWithSchema::new(pool1);
        let pwc2 = PoolWithSchema::new(pool2);

        // Initialize both
        pwc1.ensure_schema_ready().await.expect("pool1 failed");
        pwc2.ensure_schema_ready().await.expect("pool2 failed");

        // Both should be marked ready independently
        assert!(pwc1.schema_ready.load(Ordering::SeqCst));
        assert!(pwc2.schema_ready.load(Ordering::SeqCst));

        // Subsequent calls should succeed without re-running migrations
        pwc1.ensure_schema_ready().await.expect("pool1 repeat failed");
        pwc2.ensure_schema_ready().await.expect("pool2 repeat failed");
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    async fn postgres_supervisor_requires_file_search_migration() {
        let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
        let pool = crate::storage::pool::create_pool(Some(&database_url)).await.unwrap();
        let schema = format!("file_search_upgrade_{}", uuid::Uuid::now_v7().simple());
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("SELECT set_config('search_path', $1, false)")
            .bind(&schema)
            .execute(&mut *connection)
            .await
            .unwrap();
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_add_placeholders.sql"),
            include_str!("../../migrations/0003_index_conversation_sequence.sql"),
            include_str!("../../migrations/0004_link_conversation_latest_response.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&mut *connection).await.unwrap();
        }
        sqlx::raw_sql(POSTGRES_INTEGER_WIDENING_SQL)
            .execute(&mut *connection)
            .await
            .unwrap();
        let selected_schema = schema.clone();
        let supervisor_pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _| {
                let schema = selected_schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .unwrap();
        let supervisor = PoolWithSchema::new(Arc::new(supervisor_pool));
        assert!(supervisor.ensure_schema_ready_with_marker(true).await.is_err());
        assert!(!supervisor.schema_ready.load(Ordering::SeqCst));
        sqlx::raw_sql(include_str!("../../migrations/0005_file_search.sql"))
            .execute(&mut *connection)
            .await
            .unwrap();
        assert!(
            supervisor.ensure_schema_ready_with_marker(true).await.is_err(),
            "expiry migration is required"
        );
        sqlx::raw_sql(include_str!("../../migrations/0006_file_expiration.sql"))
            .execute(&mut *connection)
            .await
            .unwrap();
        assert!(
            supervisor.ensure_schema_ready_with_marker(true).await.is_err(),
            "store lifecycle migration is required"
        );
        sqlx::raw_sql(include_str!("../../migrations/0007_vector_store_lifecycle.sql"))
            .execute(&mut *connection)
            .await
            .unwrap();
        assert!(
            supervisor.ensure_schema_ready_with_marker(true).await.is_err(),
            "batch migration is required"
        );
        sqlx::raw_sql(include_str!("../../migrations/0008_vector_store_batches.sql"))
            .execute(&mut *connection)
            .await
            .unwrap();
        supervisor.ensure_schema_ready_with_marker(true).await.unwrap();
        supervisor.pool.close().await;
        sqlx::query("SET search_path TO public")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    #[allow(
        clippy::too_many_lines,
        reason = "keeps the complete migration lifecycle in one integration test"
    )]
    async fn postgres_integer_widening_upgrade_preserves_existing_state() {
        let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
        let pool = crate::storage::pool::create_pool(Some(&database_url))
            .await
            .expect("create PostgreSQL upgrade-test pool");
        let schema = format!("upgrade_{}", uuid::Uuid::now_v7().simple());
        let mut connection = pool.acquire().await.expect("acquire PostgreSQL upgrade connection");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *connection)
            .await
            .expect("create isolated upgrade schema");
        sqlx::query(&format!("SET search_path TO {schema}"))
            .execute(&mut *connection)
            .await
            .expect("select isolated upgrade schema");
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_add_placeholders.sql"),
            include_str!("../../migrations/0003_index_conversation_sequence.sql"),
            include_str!("../../migrations/0004_link_conversation_latest_response.sql"),
            include_str!("../../migrations/0005_file_search.sql"),
            include_str!("../../migrations/0006_file_expiration.sql"),
            include_str!("../../migrations/0007_vector_store_lifecycle.sql"),
            include_str!("../../migrations/0008_vector_store_batches.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&mut *connection)
                .await
                .expect("apply portable migration");
        }
        sqlx::query("INSERT INTO conversations (id, created_at, metadata) VALUES ($1, $2, $3)")
            .bind("conv_upgrade")
            .bind(1_704_067_200_i64)
            .bind("{\"source\":\"upgrade\"}")
            .execute(&mut *connection)
            .await
            .expect("seed conversation");
        sqlx::query("INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES ($1, $2, $3, $4, $5)")
            .bind("item_upgrade")
            .bind("{}")
            .bind(1_704_067_200_i64)
            .bind("conv_upgrade")
            .bind(0_i64)
            .execute(&mut *connection)
            .await
            .expect("seed item");
        sqlx::query(
            "INSERT INTO responses \
             (id, conversation_id, history_item_ids, metadata, created_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("resp_upgrade")
        .bind("conv_upgrade")
        .bind("[\"item_upgrade\"]")
        .bind("{\"source\":\"upgrade\"}")
        .bind(1_704_067_200_i64)
        .execute(&mut *connection)
        .await
        .expect("seed response");

        let schema_column_count = postgres_required_schema_column_count(&mut *connection)
            .await
            .expect("inspect pre-upgrade PostgreSQL schema");
        let constraint_count = postgres_required_constraint_count(&mut *connection)
            .await
            .expect("inspect pre-upgrade PostgreSQL constraints");
        let (integer_column_count, narrow_column_count) = postgres_integer_column_compatibility(&mut *connection)
            .await
            .expect("inspect pre-upgrade PostgreSQL columns");
        let sequence_index_ready = postgres_sequence_index_ready(&mut *connection)
            .await
            .expect("inspect pre-upgrade PostgreSQL sequence index");
        assert_eq!(schema_column_count, REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT);
        assert_eq!(constraint_count, REQUIRED_POSTGRES_CONSTRAINT_COUNT);
        assert_eq!(integer_column_count, REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT);
        assert_eq!(narrow_column_count, 4);
        assert!(sequence_index_ready);
        assert!(
            validate_supervisor_schema(
                schema_column_count,
                constraint_count,
                integer_column_count,
                narrow_column_count,
                sequence_index_ready,
            )
            .is_err()
        );

        let supervisor_schema_name = schema.clone();
        let supervisor_pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _metadata| {
                let supervisor_schema_name = supervisor_schema_name.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(supervisor_schema_name)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("create supervisor-managed PostgreSQL pool");
        let supervisor_schema =
            PoolWithSchema::with_postgres_migration_timeout(Arc::new(supervisor_pool), Duration::from_secs(5));
        let supervisor_error = supervisor_schema
            .ensure_schema_ready_with_marker(true)
            .await
            .expect_err("narrow supervisor-managed schema should fail compatibility check");
        assert!(supervisor_error.to_string().contains("BIGINT compatibility upgrade"));
        assert!(!supervisor_schema.schema_ready.load(Ordering::SeqCst));

        apply_postgres_compatibility(&mut connection, Duration::from_secs(5))
            .await
            .expect("widen PostgreSQL integer columns");
        supervisor_schema
            .ensure_schema_ready_with_marker(true)
            .await
            .expect("widened supervisor-managed schema should pass compatibility check");
        assert!(supervisor_schema.schema_ready.load(Ordering::SeqCst));

        let future_timestamp = i64::from(i32::MAX) + 1;
        sqlx::query("UPDATE conversations SET created_at = $1 WHERE id = $2")
            .bind(future_timestamp)
            .bind("conv_upgrade")
            .execute(&mut *connection)
            .await
            .expect("write timestamp beyond PostgreSQL INT4 range");
        let linked_state: (i64, String, String) = sqlx::query_as(
            "SELECT conversations.created_at, items.id, responses.id \
             FROM conversations \
             JOIN items ON items.conversation_id = conversations.id \
             JOIN responses ON responses.conversation_id = conversations.id \
             WHERE conversations.id = $1",
        )
        .bind("conv_upgrade")
        .fetch_one(&mut *connection)
        .await
        .expect("load migrated linked state");
        assert_eq!(
            linked_state,
            (future_timestamp, "item_upgrade".to_owned(), "resp_upgrade".to_owned())
        );
        let bigint_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = $1 AND data_type = 'bigint' \
             AND ((table_name = 'conversations' AND column_name = 'created_at') \
               OR (table_name = 'items' AND column_name IN ('created_at', 'seq')) \
               OR (table_name = 'responses' AND column_name = 'created_at'))",
        )
        .bind(&schema)
        .fetch_one(&mut *connection)
        .await
        .expect("inspect widened PostgreSQL columns");
        assert_eq!(bigint_columns, 4);
        assert!(
            validate_supervisor_schema(
                REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
                REQUIRED_POSTGRES_CONSTRAINT_COUNT,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                4 - bigint_columns,
                true,
            )
            .is_ok()
        );
        let foreign_key_error =
            sqlx::query("INSERT INTO items (id, data, created_at, conversation_id) VALUES ($1, $2, $3, $4)")
                .bind("item_invalid")
                .bind("{}")
                .bind(future_timestamp)
                .bind("conv_missing")
                .execute(&mut *connection)
                .await;
        assert!(foreign_key_error.is_err());

        sqlx::raw_sql(
            "ALTER TABLE items DROP CONSTRAINT items_conversation_id_fkey; \
             ALTER TABLE responses ADD CONSTRAINT responses_conversation_id_duplicate_fkey \
             FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL;",
        )
        .execute(&mut *connection)
        .await
        .expect("replace one required foreign key with a duplicate");
        let invalid_constraint_schema =
            PoolWithSchema::with_postgres_migration_timeout(supervisor_schema.pool.clone(), Duration::from_secs(5));
        let constraint_error = invalid_constraint_schema
            .ensure_schema_ready_with_marker(true)
            .await
            .expect_err("a duplicate foreign key must not compensate for a missing required constraint");
        assert!(
            constraint_error
                .to_string()
                .contains("missing required PostgreSQL tables, columns, constraints, or indexes"),
            "{constraint_error}"
        );

        sqlx::query("SET search_path TO public")
            .execute(&mut *connection)
            .await
            .expect("restore public schema");
        supervisor_schema.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&mut *connection)
            .await
            .expect("drop isolated upgrade schema");
        drop(connection);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    #[allow(
        clippy::too_many_lines,
        reason = "keeps the complete multi-schema migration and runtime connection lifecycle together"
    )]
    async fn postgres_migrations_use_visible_tables_when_current_schema_is_empty() {
        let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
        let pool = crate::storage::pool::create_pool(Some(&database_url))
            .await
            .expect("create PostgreSQL schema-visibility test pool");
        let table_schema = format!("visible_{}", uuid::Uuid::now_v7().simple());
        let empty_schema = format!("empty_{}", uuid::Uuid::now_v7().simple());
        let mut connection = pool.acquire().await.expect("acquire PostgreSQL test connection");

        sqlx::query(&format!("CREATE SCHEMA {table_schema}"))
            .execute(&mut *connection)
            .await
            .expect("create table schema");
        sqlx::query(&format!("CREATE SCHEMA {empty_schema}"))
            .execute(&mut *connection)
            .await
            .expect("create empty schema");
        sqlx::query("SELECT set_config('search_path', $1, false)")
            .bind(&table_schema)
            .execute(&mut *connection)
            .await
            .expect("select table schema");
        sqlx::migrate!("./migrations")
            .run(&mut *connection)
            .await
            .expect("apply portable migrations to table schema");
        sqlx::query("INSERT INTO conversations (id, created_at) VALUES ($1, $2)")
            .bind("conv_visible_schema")
            .bind(1_704_067_200_i64)
            .execute(&mut *connection)
            .await
            .expect("seed visible-schema conversation");

        sqlx::query("SELECT set_config('search_path', $1, false)")
            .bind(format!("{empty_schema},{table_schema}"))
            .execute(&mut *connection)
            .await
            .expect("put empty schema first in search path");
        let current_schema: String = sqlx::query_scalar("SELECT current_schema()::text")
            .fetch_one(&mut *connection)
            .await
            .expect("inspect current schema");
        assert_eq!(current_schema, empty_schema);

        pin_postgres_persistence_schema(&mut connection)
            .await
            .expect("pin visible PostgreSQL migration schema");
        let migration_result = sqlx::migrate!("./migrations").run(&mut *connection).await;
        let compatibility_result = apply_postgres_compatibility(&mut connection, Duration::from_secs(5)).await;
        let bigint_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = $1 AND data_type = 'bigint' \
             AND ((table_name = 'conversations' AND column_name = 'created_at') \
               OR (table_name = 'items' AND column_name IN ('created_at', 'seq')) \
               OR (table_name = 'responses' AND column_name = 'created_at'))",
        )
        .bind(&table_schema)
        .fetch_one(&mut *connection)
        .await
        .expect("inspect visible PostgreSQL columns");
        let shadow_table_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name IN ('conversations', 'items', 'responses')",
        )
        .bind(&empty_schema)
        .fetch_one(&mut *connection)
        .await
        .expect("inspect empty schema for shadow tables");
        let seeded_conversation_count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table_schema}.conversations WHERE id = $1"
        ))
        .bind("conv_visible_schema")
        .fetch_one(&mut *connection)
        .await
        .expect("inspect seeded conversation");
        let query_separator = if database_url.contains('?') { '&' } else { '?' };
        let runtime_database_url =
            format!("{database_url}{query_separator}options=-csearch_path%3D{empty_schema}%2C{table_schema}");
        let runtime_pool = crate::storage::pool::create_pool(Some(&runtime_database_url))
            .await
            .expect("create runtime pool with multi-schema search path");
        let runtime_schema: String = sqlx::query_scalar("SELECT current_schema()::text")
            .fetch_one(runtime_pool.as_ref())
            .await
            .expect("inspect runtime pool schema");
        let runtime_seeded_conversation_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = $1")
                .bind("conv_visible_schema")
                .fetch_one(runtime_pool.as_ref())
                .await
                .expect("load seeded conversation through runtime pool");
        verify_persistence_writable(runtime_pool.as_ref())
            .await
            .expect("run PostgreSQL functional persistence probe");
        verify_persistence_ready(runtime_pool.as_ref())
            .await
            .expect("run PostgreSQL read-only persistence probe");
        runtime_pool.close().await;

        sqlx::query("SET search_path TO public")
            .execute(&mut *connection)
            .await
            .expect("restore public schema");
        sqlx::query(&format!("DROP SCHEMA {empty_schema} CASCADE"))
            .execute(&mut *connection)
            .await
            .expect("drop empty schema");
        sqlx::query(&format!("DROP SCHEMA {table_schema} CASCADE"))
            .execute(&mut *connection)
            .await
            .expect("drop table schema");
        drop(connection);
        pool.close().await;

        migration_result.expect("visible PostgreSQL schema should accept repeated migrations");
        compatibility_result.expect("visible PostgreSQL schema should pass compatibility check");
        assert_eq!(shadow_table_count, 0);
        assert_eq!(seeded_conversation_count, 1);
        assert_eq!(runtime_schema, table_schema);
        assert_eq!(runtime_seeded_conversation_count, 1);
        assert_eq!(bigint_columns, REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT);
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    async fn supervisor_managed_postgres_schema_rejects_missing_tables() {
        let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
        let administration_pool = crate::storage::pool::create_pool(Some(&database_url))
            .await
            .expect("create PostgreSQL administration pool");
        let schema_name = format!("empty_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema_name}"))
            .execute(administration_pool.as_ref())
            .await
            .expect("create isolated empty schema");

        let connection_schema_name = schema_name.clone();
        let supervisor_pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _metadata| {
                let connection_schema_name = connection_schema_name.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(connection_schema_name)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("create supervisor-managed PostgreSQL pool");
        let supervisor_schema =
            PoolWithSchema::with_postgres_migration_timeout(Arc::new(supervisor_pool), Duration::from_secs(5));

        let validation_result = supervisor_schema.ensure_schema_ready_with_marker(true).await;
        supervisor_schema.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema_name} CASCADE"))
            .execute(administration_pool.as_ref())
            .await
            .expect("drop isolated empty schema");
        administration_pool.close().await;

        let error = validation_result.expect_err("empty supervisor-managed schema should fail validation");
        assert!(
            error
                .to_string()
                .contains("missing required PostgreSQL tables, columns, constraints, or indexes"),
            "{error}"
        );
        assert!(!supervisor_schema.schema_ready.load(Ordering::SeqCst));
    }
}
