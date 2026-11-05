//! Databases holding data in a format this release does not read.
//!
//! Before migrating, the default `SQLite` database in the Agentic API home is dropped and recreated; any other
//! database is rejected until its operator resets it. A database an earlier release migrated but never wrote to
//! migrates normally.

use std::fmt::Write;

use sqlx::Connection;
use tracing::warn;

use super::DbResult;
use crate::storage::backend::DatabaseBackend;

/// Databases whose latest migration is older than this store data in a format this release does not read.
const MINIMUM_SUPPORTED_MIGRATION: i64 = 9;

/// Recreate the database when it holds outdated data and `recreate_sqlite` allows it (`SQLite` only); otherwise
/// reject it, leaving its data untouched.
pub(super) async fn recreate_or_reject_outdated(
    connection: &mut sqlx::AnyConnection,
    recreate_sqlite: bool,
) -> DbResult<()> {
    if !has_outdated_data(connection).await? {
        return Ok(());
    }
    if recreate_sqlite && DatabaseBackend::from_connection(connection) == DatabaseBackend::Sqlite {
        return drop_outdated_sqlite_tables(connection).await;
    }
    Err(sqlx::Error::Configuration(
        "the database was created by an earlier agentic-api release and its data is not compatible with this \
         release; reset it (drop its tables) or configure a new database"
            .into(),
    ))
}

/// Whether the database holds rows written by a release older than [`MINIMUM_SUPPORTED_MIGRATION`]. A database
/// without migration history is new, and one with empty persistence tables has nothing to read in the old format.
async fn has_outdated_data(connection: &mut sqlx::AnyConnection) -> DbResult<bool> {
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

/// Drop every table of an outdated `SQLite` database so migrations recreate it. The tables reference each other, so
/// foreign keys are checked at commit, when none remain; either every table is dropped or none is.
async fn drop_outdated_sqlite_tables(connection: &mut sqlx::AnyConnection) -> DbResult<()> {
    warn!("[schema] Recreating the default database: it holds data written by an earlier release.");
    let mut transaction = connection.begin().await?;
    // Newest first, so a table is dropped before the tables its foreign keys reference.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid DESC",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let mut sql = String::from("PRAGMA defer_foreign_keys = ON;");
    for table in &tables {
        // Table names come from `sqlite_master`; quoting keeps any name a single identifier.
        let _ = write!(sql, " DROP TABLE \"{}\";", table.replace('"', "\"\""));
    }
    sqlx::raw_sql(&sql).execute(&mut *transaction).await?;
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

    /// SHA-384 checksums of the migrations released on `main`. A database records each applied migration's checksum
    /// and refuses to start when the file differs, so a released migration must never change; add a new one instead.
    const RELEASED_MIGRATIONS: [(i64, &str); 9] = [
        (
            1,
            "2772d573aa4a5ea9d4de483e959f72569bda372aa8962895fa48627a9e314a2d64ffc4b82329013bf717e63915a418fa",
        ),
        (
            2,
            "12f48463e3dec09338fbc26dc233477768e9dacbc67fe9aefb67280e8a937a0289e29dab6a2ea68ecb7301a7b5c16667",
        ),
        (
            3,
            "1f427292f94fad3d1af813e533df6c88273b4a42ac66f87cb67dc471454855e3c4d38854aec37e8e742a36831873c95a",
        ),
        (
            4,
            "378f9056d06b745f77c0f2f4100fe0485fdddba78cc30ac4320e51dd4620439c674ffcbf90e4b07349f09594ee6ae038",
        ),
        (
            5,
            "bccfd170c895412d7bc8bd89a0ab911a2147353fc1ffab45acdda5bcfcafbd7f7e7699e218b706104360b73de4277139",
        ),
        (
            6,
            "9103036e4a06a5cbffa0c83ae98c1dc2faae3e7601d62d9f7487917f6e3aa7de879a4a2f0f60d0163f83481aeb7c3095",
        ),
        (
            7,
            "8a400ac46c06cf69bd36ad8e0ea9864025cd6816065394b3b288de2e033e40ca19b2d2f516884f1d06d7d73445822d44",
        ),
        (
            8,
            "17fa35c1de51bc33e6691d43dbecc3222725bbef65f04d28f563a0db6cf1e192f8c97ba163fd4948f90bc84cfc521b5b",
        ),
        (
            9,
            "f51466c819176147ff190fe513f54ef44cb3002c6fbb4fdce8933325411ff943b6071298bf062a754f8f52c6517f54ce",
        ),
    ];

    #[test]
    fn released_migrations_are_unchanged() {
        let migrator = sqlx::migrate!("./migrations");
        for (version, expected) in RELEASED_MIGRATIONS {
            let migration = migrator
                .iter()
                .find(|migration| migration.version == version)
                .unwrap_or_else(|| panic!("released migration {version} is missing"));
            let checksum = migration.checksum.iter().fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
            assert_eq!(checksum, expected, "released migration {version} was edited");
        }
    }

    #[tokio::test]
    async fn database_of_the_latest_release_migrates_and_keeps_its_data() {
        let pool = create_pool(Some("sqlite://?mode=memory"))
            .await
            .expect("create database");
        let mut migrator = sqlx::migrate!("./migrations");
        let latest_release = RELEASED_MIGRATIONS[RELEASED_MIGRATIONS.len() - 1].0;
        migrator
            .migrations
            .to_mut()
            .retain(|migration| migration.version <= latest_release);
        migrator
            .run(pool.as_ref())
            .await
            .expect("apply the released migrations");
        sqlx::query("INSERT INTO conversations (id, created_at) VALUES ('conv_released', 0)")
            .execute(pool.as_ref())
            .await
            .expect("store a conversation");

        schema(&pool, false)
            .ensure_schema_ready_with_marker(false)
            .await
            .expect("migrate a database of the latest release");

        assert_eq!(conversation_count(&pool).await, 1);
        assert!(
            sqlx::query("SELECT revision FROM conversations")
                .fetch_all(pool.as_ref())
                .await
                .is_err(),
            "the conversation revision is dropped"
        );
    }
}
