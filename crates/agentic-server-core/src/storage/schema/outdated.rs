//! Databases holding data in a format this release does not read.
//!
//! Before migrating, the default `SQLite` database in the Agentic API home is dropped and recreated; any other
//! database is rejected until its operator resets it. A database an earlier release migrated but never wrote to
//! migrates normally.

use sqlx::Connection;
use tracing::warn;

use super::DbResult;
use crate::storage::backend::DatabaseBackend;

/// Databases whose latest migration is older than this store data in a format this release does not read.
const MINIMUM_SUPPORTED_MIGRATION: i64 = 8;

/// Whether the database holds rows written by a release older than [`MINIMUM_SUPPORTED_MIGRATION`]. A database
/// without migration history is new, and one with empty persistence tables has nothing to read in the old format.
pub(super) async fn has_outdated_data(connection: &mut sqlx::AnyConnection) -> DbResult<bool> {
    let has_history: bool = match DatabaseBackend::from_connection(connection) {
        DatabaseBackend::Postgres => {
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                .fetch_one(&mut *connection)
                .await?
        }
        DatabaseBackend::Sqlite | DatabaseBackend::Other => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
            )
            .fetch_one(&mut *connection)
            .await?
                > 0
        }
    };
    if !has_history {
        return Ok(false);
    }
    let latest: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
        .fetch_one(&mut *connection)
        .await?;
    if latest.is_none_or(|version| version >= MINIMUM_SUPPORTED_MIGRATION) {
        return Ok(false);
    }
    // Every migration before the minimum creates these three tables.
    let rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM conversations) + (SELECT COUNT(*) FROM items) + (SELECT COUNT(*) FROM responses)",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(rows > 0)
}

pub(super) fn outdated_schema_error() -> sqlx::Error {
    sqlx::Error::Configuration(
        "the database was created by an earlier agentic-api release and its data is not compatible with this \
         release; reset it (drop its tables) or configure a new database"
            .into(),
    )
}

/// Drop every table of an outdated `SQLite` database so migrations recreate it. The tables reference each other, so
/// foreign keys are checked at commit, when none remain; either every table is dropped or none is.
pub(super) async fn drop_outdated_sqlite_tables(connection: &mut sqlx::AnyConnection) -> DbResult<()> {
    warn!("[schema] Recreating the default database: it holds data written by an earlier release.");
    let mut transaction = connection.begin().await?;
    sqlx::raw_sql(
        "PRAGMA defer_foreign_keys = ON; \
         DROP TABLE IF EXISTS responses; \
         DROP TABLE IF EXISTS items; \
         DROP TABLE IF EXISTS conversations; \
         DROP TABLE IF EXISTS _sqlx_migrations;",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{PostgresConfig, SqliteConfig};
    use crate::storage::pool::{DbPool, create_pool};
    use crate::storage::schema::PoolWithSchema;

    /// A database an earlier release created: migrated to just before the minimum, holding a conversation whose
    /// items reference each other and whose latest turn is a response with a child, so dropping it crosses every
    /// foreign key.
    async fn earlier_release_database() -> Arc<DbPool> {
        let pool = create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("create database");
        let mut migrator = sqlx::migrate!("./migrations");
        migrator
            .migrations
            .to_mut()
            .retain(|migration| migration.version < MINIMUM_SUPPORTED_MIGRATION);
        migrator.run(pool.as_ref()).await.expect("apply earlier migrations");
        sqlx::raw_sql(
            "INSERT INTO conversations (id, created_at) VALUES ('conv_earlier', 0); \
             INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES ('item_a', '{}', 0, 'conv_earlier', 0); \
             INSERT INTO items (id, data, created_at, conversation_id, seq, reference_id) \
                 VALUES ('item_b', '{}', 0, 'conv_earlier', 1, 'item_a'); \
             INSERT INTO responses (id, conversation_id, history_item_ids, metadata, created_at) \
                 VALUES ('resp_earlier', 'conv_earlier', '[\"item_a\",\"item_b\"]', '{}', 0); \
             INSERT INTO responses (id, previous_response_id, history_item_ids, metadata, created_at) \
                 VALUES ('resp_child', 'resp_earlier', '[]', '{}', 0); \
             UPDATE conversations SET latest_response_id = 'resp_earlier' WHERE id = 'conv_earlier';",
        )
        .execute(pool.as_ref())
        .await
        .expect("store earlier data");
        pool
    }

    fn schema(pool: &Arc<DbPool>, recreate_outdated_schema: bool) -> PoolWithSchema {
        let sqlite = SqliteConfig {
            recreate_outdated_schema,
            ..SqliteConfig::default()
        };
        PoolWithSchema::configured(Arc::clone(pool), &PostgresConfig::default(), &sqlite)
    }

    async fn conversation_count(pool: &DbPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(pool)
            .await
            .expect("count conversations")
    }

    #[tokio::test]
    async fn outdated_default_database_is_recreated_at_the_latest_schema() {
        let pool = earlier_release_database().await;
        schema(&pool, true)
            .ensure_schema_ready_with_marker(false)
            .await
            .expect("recreate the default database");

        assert_eq!(conversation_count(&pool).await, 0);
        let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool.as_ref())
            .await
            .expect("latest migration");
        assert!(latest >= MINIMUM_SUPPORTED_MIGRATION);
        sqlx::query("SELECT items.metadata, responses.snapshot FROM items, responses LIMIT 0")
            .fetch_all(pool.as_ref())
            .await
            .expect("latest columns exist");
    }

    #[tokio::test]
    async fn outdated_configured_database_is_rejected_and_kept() {
        for supervisor_managed in [false, true] {
            let pool = earlier_release_database().await;
            let error = schema(&pool, false)
                .ensure_schema_ready_with_marker(supervisor_managed)
                .await
                .expect_err("an outdated configured database must not be migrated");

            assert!(
                error.to_string().contains("earlier agentic-api release"),
                "supervisor_managed={supervisor_managed}: {error}"
            );
            assert_eq!(
                conversation_count(&pool).await,
                1,
                "supervisor_managed={supervisor_managed}"
            );
        }
    }

    #[tokio::test]
    async fn empty_earlier_database_migrates_without_recreation() {
        let pool = earlier_release_database().await;
        sqlx::raw_sql(
            "UPDATE conversations SET latest_response_id = NULL; DELETE FROM responses; DELETE FROM items; \
             DELETE FROM conversations;",
        )
        .execute(pool.as_ref())
        .await
        .expect("empty the earlier database");
        schema(&pool, false)
            .ensure_schema_ready_with_marker(false)
            .await
            .expect("migrate an empty earlier database");
        sqlx::query("SELECT items.metadata, responses.snapshot FROM items, responses LIMIT 0")
            .fetch_all(pool.as_ref())
            .await
            .expect("latest columns exist");
    }

    #[tokio::test]
    async fn new_and_current_databases_are_not_outdated() {
        let pool = create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("create database");
        let mut connection = pool.acquire().await.expect("acquire connection");
        assert!(!has_outdated_data(&mut connection).await.expect("inspect new database"));
        drop(connection);
        schema(&pool, false)
            .ensure_schema_ready_with_marker(false)
            .await
            .expect("migrate a new database");
        let mut connection = pool.acquire().await.expect("acquire connection");
        assert!(
            !has_outdated_data(&mut connection)
                .await
                .expect("inspect current database")
        );
    }
}
