//! Database schema management and migrations.

mod outdated;
mod postgres;
mod readiness;
use outdated::recreate_or_reject_outdated;
use postgres::{
    postgres_integer_column_compatibility, postgres_required_constraint_count, postgres_required_schema_column_count,
    postgres_sequence_index_ready, validate_required_postgres_schema, validate_supervisor_schema,
};
pub(crate) use readiness::{verify_persistence_ready, verify_persistence_writable};

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::Connection;
use tracing::{debug, info};

use super::backend::{DatabaseBackend, configure_postgres_timeouts};
use super::pool::DbPool;
use crate::config::{DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS, PostgresConfig, SqliteConfig};

type DbResult<T> = Result<T, sqlx::Error>;

const POSTGRES_SCHEMA_ADVISORY_LOCK: i64 = 7_194_963_546_799_751;
const POSTGRES_INTEGER_WIDENING_SQL: &str = "
    ALTER TABLE conversations
        ALTER COLUMN created_at TYPE BIGINT USING created_at::BIGINT,
        ALTER COLUMN revision TYPE BIGINT USING revision::BIGINT;
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
              'file_search_files', 'file_search_blob_cleanup', 'file_search_stores', \
              'file_search_attachments', 'file_search_chunks', 'message_threads') \
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

async fn run_embedded_migrations(pool: &DbPool, postgres_migration_timeout: Duration) -> DbResult<()> {
    let mut connection = pool.acquire().await?;
    let is_postgres = DatabaseBackend::from_connection(&connection) == DatabaseBackend::Postgres;
    if is_postgres
        && let Err(error) = configure_postgres_migration_timeout(&mut connection, postgres_migration_timeout).await
    {
        let _ = connection.close().await;
        return Err(error);
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
    recreate_outdated_sqlite: bool,
}

impl PoolWithSchema {
    /// Creates a new pool with schema tracking and default database settings.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self::configured(pool, &PostgresConfig::default(), &SqliteConfig::default())
    }

    /// Creates a new pool with schema tracking and the given database settings.
    #[must_use]
    pub fn configured(pool: Arc<DbPool>, postgres: &PostgresConfig, sqlite: &SqliteConfig) -> Self {
        Self {
            pool,
            schema_ready: AtomicBool::new(false),
            postgres_migration_timeout: postgres.migration_timeout,
            recreate_outdated_sqlite: sqlite.recreate_outdated_schema,
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

        let recreate_outdated = self.recreate_outdated_sqlite && !supervisor_managed;
        recreate_or_reject_outdated(&mut *self.pool.acquire().await?, recreate_outdated).await?;
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
        recreate_or_reject_outdated(&mut *self.pool.acquire().await?, false).await?;
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
    use super::postgres::{
        REQUIRED_POSTGRES_CONSTRAINT_COUNT, REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
        REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
    };
    use super::*;

    fn supervisor_pool_with_schema(pool: Arc<DbPool>) -> PoolWithSchema {
        let postgres = PostgresConfig {
            migration_timeout: Duration::from_secs(5),
            ..PostgresConfig::default()
        };
        PoolWithSchema::configured(pool, &postgres, &SqliteConfig::default())
    }

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
            include_str!("../../migrations/0005_conversation_revision.sql"),
            include_str!("../../migrations/0006_item_references.sql"),
            include_str!("../../migrations/0007_response_item_public_ids.sql"),
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
        sqlx::raw_sql(include_str!("../../migrations/0008_file_search.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        assert!(verify_persistence_ready(pool.as_ref()).await.is_err());
        sqlx::raw_sql(include_str!("../../migrations/0009_typed_item_metadata.sql"))
            .execute(pool.as_ref())
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../migrations/0010_message_threads.sql"))
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
            include_str!("../../migrations/0005_conversation_revision.sql"),
            include_str!("../../migrations/0006_item_references.sql"),
            include_str!("../../migrations/0007_response_item_public_ids.sql"),
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
        for migration in [
            include_str!("../../migrations/0008_file_search.sql"),
            include_str!("../../migrations/0009_typed_item_metadata.sql"),
            include_str!("../../migrations/0010_message_threads.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&mut *connection).await.unwrap();
        }
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
            include_str!("../../migrations/0005_conversation_revision.sql"),
            include_str!("../../migrations/0006_item_references.sql"),
            include_str!("../../migrations/0007_response_item_public_ids.sql"),
            include_str!("../../migrations/0008_file_search.sql"),
            include_str!("../../migrations/0009_typed_item_metadata.sql"),
            include_str!("../../migrations/0010_message_threads.sql"),
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
        sqlx::query(
            "INSERT INTO items (id, public_id, metadata, data, created_at, conversation_id, seq) \
             VALUES ($1, $1, $2, $3, $4, $5, $6)",
        )
        .bind("item_upgrade")
        .bind(r#"{"kind":"input"}"#)
        .bind("{}")
        .bind(1_704_067_200_i64)
        .bind("conv_upgrade")
        .bind(0_i64)
        .execute(&mut *connection)
        .await
        .expect("seed item");
        sqlx::query("INSERT INTO responses (id, history_item_ids, metadata, created_at) VALUES ($1, $2, $3, $4)")
            .bind("resp_upgrade")
            .bind("[\"item_upgrade\"]")
            .bind("{\"source\":\"upgrade\"}")
            .bind(1_704_067_200_i64)
            .execute(&mut *connection)
            .await
            .expect("seed response");
        sqlx::query("UPDATE conversations SET latest_response_id = $1 WHERE id = $2")
            .bind("resp_upgrade")
            .bind("conv_upgrade")
            .execute(&mut *connection)
            .await
            .expect("link the conversation's latest response");

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
        let supervisor_schema = supervisor_pool_with_schema(Arc::new(supervisor_pool));
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
             JOIN responses ON responses.id = conversations.latest_response_id \
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
             AND ((table_name = 'conversations' AND column_name IN ('created_at', 'revision')) \
               OR (table_name = 'items' AND column_name IN ('created_at', 'seq')) \
               OR (table_name = 'responses' AND column_name = 'created_at'))",
        )
        .bind(&schema)
        .fetch_one(&mut *connection)
        .await
        .expect("inspect widened PostgreSQL columns");
        assert_eq!(bigint_columns, REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT);
        assert!(
            validate_supervisor_schema(
                REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
                REQUIRED_POSTGRES_CONSTRAINT_COUNT,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT - bigint_columns,
                true,
            )
            .is_ok()
        );
        let foreign_key_error = sqlx::query(
            "INSERT INTO items (id, public_id, metadata, data, created_at, conversation_id) \
                 VALUES ($1, $1, $2, $3, $4, $5)",
        )
        .bind("item_invalid")
        .bind(r#"{"kind":"input"}"#)
        .bind("{}")
        .bind(future_timestamp)
        .bind("conv_missing")
        .execute(&mut *connection)
        .await;
        assert!(foreign_key_error.is_err());

        sqlx::raw_sql(
            "ALTER TABLE items DROP CONSTRAINT items_conversation_id_fkey; \
             ALTER TABLE responses ADD CONSTRAINT responses_previous_response_id_duplicate_fkey \
             FOREIGN KEY (previous_response_id) REFERENCES responses(id) ON DELETE SET NULL;",
        )
        .execute(&mut *connection)
        .await
        .expect("replace one required foreign key with a duplicate");
        let invalid_constraint_schema = supervisor_pool_with_schema(supervisor_schema.pool.clone());
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
             AND ((table_name = 'conversations' AND column_name IN ('created_at', 'revision')) \
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
        let supervisor_schema = supervisor_pool_with_schema(Arc::new(supervisor_pool));

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

#[cfg(test)]
mod messages_thread_schema_tests {
    use super::postgres::{
        REQUIRED_POSTGRES_CONSTRAINT_COUNT, REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
        REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
    };
    use super::*;
    use crate::storage::create_pool_with_schema;

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    async fn postgres_messages_table_requires_each_constraint_and_lease_column() {
        let url = env::var("TEST_POSTGRES_URL").unwrap();
        let pool = create_pool_with_schema(Some(&url)).await.unwrap();
        let constraints: Vec<String> = sqlx::query_scalar(
            "SELECT conname::text FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
             WHERE t.relname = 'message_threads' AND pg_table_is_visible(t.oid)",
        )
        .fetch_all(pool.as_ref())
        .await
        .unwrap();
        assert_eq!(constraints.len(), 4);
        for constraint in constraints {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(&format!("ALTER TABLE message_threads DROP CONSTRAINT \"{constraint}\""))
                .execute(&mut *tx)
                .await
                .unwrap();
            let count = postgres_required_constraint_count(&mut *tx).await.unwrap();
            assert_eq!(count, REQUIRED_POSTGRES_CONSTRAINT_COUNT - 1);
            assert!(
                validate_required_postgres_schema(
                    REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT,
                    count,
                    REQUIRED_POSTGRES_INTEGER_COLUMN_COUNT,
                    true
                )
                .is_err()
            );
            tx.rollback().await.unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE message_threads DROP COLUMN lease_until")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            postgres_required_schema_column_count(&mut *tx).await.unwrap(),
            REQUIRED_POSTGRES_SCHEMA_COLUMN_COUNT - 1
        );
        tx.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
    async fn postgres_schema_pin_recognizes_a_messages_only_schema() {
        let url = env::var("TEST_POSTGRES_URL").unwrap();
        let pool = create_pool_with_schema(Some(&url)).await.unwrap();
        let first = format!("empty_{}", uuid::Uuid::now_v7().simple());
        let second = format!("messages_{}", uuid::Uuid::now_v7().simple());
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(&format!("CREATE SCHEMA {first}; CREATE SCHEMA {second}; CREATE TABLE {second}.message_threads (id TEXT); SET LOCAL search_path TO {first}, {second};"))
            .execute(&mut *tx).await.unwrap();
        pin_postgres_persistence_schema(&mut tx).await.unwrap();
        let schema: String = sqlx::query_scalar("SELECT current_schema()::text")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(schema, second);
        tx.rollback().await.unwrap();
        pool.close().await;
    }
}
