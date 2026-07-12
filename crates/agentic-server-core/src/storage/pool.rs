//! Database connection pooling and initialization.

use std::{sync::Arc, time::Duration};

use sqlx::AnyConnection;
use sqlx::any::AnyPoolOptions;

use crate::config::SqliteConfig;

const SQLITE_MEMORY_MAX_CONNECTIONS: u32 = 1;
const DEFAULT_MAX_CONNECTIONS: u32 = 10;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const SQLITE_WAL_MAX_ATTEMPTS: u32 = 5;
const SQLITE_WAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const SQLITE_BUSY: i32 = 5;
const SQLITE_LOCKED: i32 = 6;

/// Generic database pool type supporting `SQLite`, `PostgreSQL`, and `MySQL`.
pub type DbPool = sqlx::Pool<sqlx::Any>;

/// Database transaction type for multi-statement operations.
pub type DbTransaction<'a> = sqlx::Transaction<'a, sqlx::Any>;

/// Convenience type alias for database operation results.
///
/// All database queries return `DbResult<T>` which is `Result<T, sqlx::Error>`.
pub type DbResult<T> = Result<T, sqlx::Error>;

/// Prepares database URL with appropriate parameters.
///
/// For `SQLite` connections, adds `?mode=rwc` if not already present.
/// This enables write mode (`rwc` = read-write-create) for file-based databases.
///
/// For other database types (`PostgreSQL`, `MySQL`), returns URL as-is.
/// Defaults to `sqlite://./agentic_api.db` if no URL is provided.
fn prepare_db_url(url: Option<&str>) -> String {
    let url = url.unwrap_or("sqlite://./agentic_api.db");
    if !url.starts_with("sqlite") || has_query_param(url, "mode") {
        url.to_string()
    } else {
        append_query_param(url, "mode=rwc")
    }
}

fn has_query_param(url: &str, param: &str) -> bool {
    query_param_value(url, param).is_some()
}

fn query_param_value<'a>(url: &'a str, param: &str) -> Option<&'a str> {
    let (_, query) = url.split_once('?')?;
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    query.split('&').find_map(|part| {
        let (key, value) = part.split_once('=').map_or((part, ""), |(key, value)| (key, value));
        (key == param).then_some(value)
    })
}

fn append_query_param(url: &str, param: &str) -> String {
    let (base, fragment) = url
        .split_once('#')
        .map_or((url, None), |(base, fragment)| (base, Some(fragment)));
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut prepared = format!("{base}{separator}{param}");
    if let Some(fragment) = fragment {
        prepared.push('#');
        prepared.push_str(fragment);
    }
    prepared
}

fn sqlite_is_memory_url(url: &str) -> bool {
    query_param_value(url, "mode").is_some_and(|mode| mode.eq_ignore_ascii_case("memory")) || url.contains(":memory:")
}

fn sqlite_should_enable_wal(url: &str) -> bool {
    url.starts_with("sqlite")
        && !sqlite_is_memory_url(url)
        && !query_param_value(url, "mode").is_some_and(|mode| mode.eq_ignore_ascii_case("ro"))
}

fn sqlite_max_connections(url: &str, config: SqliteConfig) -> u32 {
    if sqlite_is_memory_url(url) {
        SQLITE_MEMORY_MAX_CONNECTIONS
    } else {
        config.max_connections
    }
}

async fn configure_sqlite_connection(conn: &mut AnyConnection, config: SqliteConfig) -> DbResult<()> {
    sqlx::query(&format!("PRAGMA busy_timeout = {SQLITE_BUSY_TIMEOUT_MS}"))
        .execute(&mut *conn)
        .await?;
    sqlx::query(&format!(
        "PRAGMA journal_size_limit = {}",
        config.journal_size_limit_bytes
    ))
    .execute(&mut *conn)
    .await?;
    sqlx::query(&format!("PRAGMA temp_store = {}", config.temp_store.as_pragma_value()))
        .execute(&mut *conn)
        .await?;
    sqlx::query(&format!("PRAGMA mmap_size = {}", config.mmap_size_bytes))
        .execute(&mut *conn)
        .await?;
    sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
    sqlx::query("PRAGMA synchronous = NORMAL").execute(&mut *conn).await?;

    Ok(())
}

async fn enable_sqlite_wal(pool: &DbPool) -> DbResult<()> {
    enable_sqlite_wal_with_retry(pool, SQLITE_WAL_MAX_ATTEMPTS, SQLITE_WAL_RETRY_DELAY).await
}

async fn enable_sqlite_wal_with_retry(pool: &DbPool, max_attempts: u32, retry_delay: Duration) -> DbResult<()> {
    debug_assert!(max_attempts > 0);

    for attempt in 1..=max_attempts {
        match sqlx::query("PRAGMA journal_mode = WAL").execute(pool).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < max_attempts && sqlite_is_busy_or_locked(&error) => {
                tokio::time::sleep(retry_delay).await;
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("WAL retry loop always returns on success or final failure")
}

fn sqlite_is_busy_or_locked(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database_error) = error else {
        return false;
    };
    let Some(code) = database_error.code().and_then(|code| code.parse::<i32>().ok()) else {
        return false;
    };

    matches!(code & 0xff, SQLITE_BUSY | SQLITE_LOCKED)
}

/// Creates a connection pool for the database.
///
/// Initializes a connection pool with sensible defaults:
/// - Max connections: 4 for file-backed `SQLite`, 1 for in-memory `SQLite`, 10 for other databases
/// - Driver auto-detection: supports `SQLite`, `PostgreSQL`, `MySQL`
/// - `SQLite` file mode: read-write-create for file-based databases
///
/// The pool is wrapped in `Arc` for thread-safe sharing across async tasks.
///
/// # Arguments
///
/// * `db_url` - Optional database connection URL. Defaults to `sqlite://./agentic_api.db` if `None`.
///   Examples: `sqlite://data.db`, `postgresql://user:pass@host/db`
///
/// # Errors
///
/// Returns [`sqlx::Error`] if:
/// - Connection URL is invalid
/// - Database server is unreachable
/// - Connection limit is exceeded
/// - Authentication fails
///
pub async fn create_pool(db_url: Option<&str>) -> DbResult<Arc<DbPool>> {
    create_pool_with_sqlite_config(db_url, SqliteConfig::default()).await
}

/// Creates a connection pool for the database with explicit `SQLite` tuning.
///
/// # Errors
///
/// Returns [`sqlx::Error`] if pool creation or connection initialization fails.
pub async fn create_pool_with_sqlite_config(
    db_url: Option<&str>,
    sqlite_config: SqliteConfig,
) -> DbResult<Arc<DbPool>> {
    // Install default drivers for auto-detection
    sqlx::any::install_default_drivers();

    // Prepare URL with database-specific parameters
    let url = prepare_db_url(db_url);

    let max_connections = if url.starts_with("sqlite") {
        sqlite_max_connections(&url, sqlite_config)
    } else {
        DEFAULT_MAX_CONNECTIONS
    };
    let mut options = AnyPoolOptions::new().max_connections(max_connections);
    if url.starts_with("sqlite") {
        options = options.after_connect(move |conn, _meta| Box::pin(configure_sqlite_connection(conn, sqlite_config)));
    }
    let pool = options.connect(&url).await?;
    if sqlite_should_enable_wal(&url) {
        enable_sqlite_wal(&pool).await?;
    }

    // Wrap in Arc for thread-safe sharing across async tasks
    Ok(Arc::new(pool))
}

/// Creates a connection pool and initializes the database schema.
///
/// Combines [`create_pool`] with schema initialization using [`PoolWithSchema`].
/// Each pool has its own per-pool schema readiness flag.
/// # Arguments
///
/// * `db_url` - Database connection URL
///
/// # Errors
///
/// Returns error if pool creation or schema initialization fails.
pub async fn create_pool_with_schema(db_url: Option<&str>) -> DbResult<Arc<DbPool>> {
    create_pool_with_schema_and_sqlite_config(db_url, SqliteConfig::default()).await
}

/// Creates a connection pool with explicit `SQLite` tuning and initializes the database schema.
///
/// # Errors
///
/// Returns error if pool creation or schema initialization fails.
pub async fn create_pool_with_schema_and_sqlite_config(
    db_url: Option<&str>,
    sqlite_config: SqliteConfig,
) -> DbResult<Arc<DbPool>> {
    use crate::storage::PoolWithSchema;

    let pool = create_pool_with_sqlite_config(db_url, sqlite_config).await?;
    let pool_with_schema = PoolWithSchema::new(pool);
    pool_with_schema.ensure_schema_ready().await?;

    Ok(pool_with_schema.pool().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES, DEFAULT_SQLITE_MAX_CONNECTIONS, DEFAULT_SQLITE_MMAP_SIZE_BYTES,
        SqliteTempStore,
    };
    use sqlx::Connection;

    #[test]
    fn test_prepare_sqlite_url_without_params() {
        let url = "sqlite://test.db";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "sqlite://test.db?mode=rwc");
    }

    #[test]
    fn test_prepare_sqlite_url_with_params() {
        let url = "sqlite://test.db?cache=shared";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "sqlite://test.db?cache=shared&mode=rwc");
    }

    #[test]
    fn test_prepare_sqlite_url_with_fragment() {
        let url = "sqlite://test.db?cache=shared#frag";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "sqlite://test.db?cache=shared&mode=rwc#frag");
    }

    #[test]
    fn test_prepare_sqlite_url_with_existing_mode() {
        let url = "sqlite://test.db?mode=ro";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "sqlite://test.db?mode=ro");
    }

    #[test]
    fn test_prepare_sqlite_memory_url_keeps_memory_mode() {
        let url = "sqlite://?mode=memory";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "sqlite://?mode=memory");
    }

    #[test]
    fn test_sqlite_wal_enabled_for_writable_file_urls_only() {
        assert!(sqlite_should_enable_wal("sqlite://test.db?mode=rwc"));
        assert!(sqlite_should_enable_wal("sqlite://test.db?mode=rw"));
        assert!(!sqlite_should_enable_wal("sqlite://test.db?mode=ro"));
        assert!(!sqlite_should_enable_wal("sqlite://?mode=memory"));
        assert!(!sqlite_should_enable_wal("sqlite::memory:"));
    }

    #[test]
    fn test_sqlite_max_connections_for_file_and_memory_urls() {
        let config = SqliteConfig {
            max_connections: 6,
            ..SqliteConfig::default()
        };

        assert_eq!(sqlite_max_connections("sqlite://test.db?mode=rwc", config), 6);
        assert_eq!(
            sqlite_max_connections("sqlite://?mode=memory", config),
            SQLITE_MEMORY_MAX_CONNECTIONS
        );
        assert_eq!(
            sqlite_max_connections("sqlite::memory:", config),
            SQLITE_MEMORY_MAX_CONNECTIONS
        );
    }

    #[test]
    fn test_prepare_postgres_url() {
        let url = "postgresql://user:pass@localhost/db";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "postgresql://user:pass@localhost/db");
    }

    #[test]
    fn test_prepare_mysql_url() {
        let url = "mysql://user:pass@localhost/db";
        let prepared = prepare_db_url(Some(url));
        assert_eq!(prepared, "mysql://user:pass@localhost/db");
    }

    #[test]
    fn test_prepare_default_sqlite_url() {
        let prepared = prepare_db_url(None);
        assert_eq!(prepared, "sqlite://./agentic_api.db?mode=rwc");
    }

    #[tokio::test]
    async fn test_sqlite_connection_pragmas_are_configured() {
        let db_path = std::env::temp_dir().join(format!("pragma_{}.db", uuid::Uuid::now_v7()));
        let db_url = format!("sqlite://{}", db_path.display());
        let pool = create_pool(Some(&db_url)).await.expect("failed to create pool");

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(pool.as_ref())
            .await
            .expect("journal_mode query failed");
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(pool.as_ref())
            .await
            .expect("busy_timeout query failed");
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(pool.as_ref())
            .await
            .expect("foreign_keys query failed");
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(pool.as_ref())
            .await
            .expect("synchronous query failed");
        let journal_size_limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
            .fetch_one(pool.as_ref())
            .await
            .expect("journal_size_limit query failed");
        let temp_store: i64 = sqlx::query_scalar("PRAGMA temp_store")
            .fetch_one(pool.as_ref())
            .await
            .expect("temp_store query failed");
        let mmap_size: i64 = sqlx::query_scalar("PRAGMA mmap_size")
            .fetch_one(pool.as_ref())
            .await
            .expect("mmap_size query failed");

        assert_eq!(journal_mode.to_lowercase(), "wal");
        assert_eq!(pool.options().get_max_connections(), DEFAULT_SQLITE_MAX_CONNECTIONS);
        assert_eq!(
            busy_timeout,
            i64::try_from(SQLITE_BUSY_TIMEOUT_MS).expect("default fits in i64")
        );
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 1);
        assert_eq!(
            journal_size_limit,
            i64::try_from(DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES).expect("default fits in i64")
        );
        assert_eq!(temp_store, 2);
        assert_eq!(
            mmap_size,
            i64::try_from(DEFAULT_SQLITE_MMAP_SIZE_BYTES).expect("default fits in i64")
        );
    }

    #[tokio::test]
    async fn test_sqlite_connection_pragmas_use_explicit_config() {
        let db_path = std::env::temp_dir().join(format!("pragma_custom_{}.db", uuid::Uuid::now_v7()));
        let db_url = format!("sqlite://{}", db_path.display());
        let config = SqliteConfig {
            max_connections: 3,
            journal_size_limit_bytes: 131_072,
            temp_store: SqliteTempStore::File,
            mmap_size_bytes: 1_048_576,
        };
        let pool = create_pool_with_sqlite_config(Some(&db_url), config)
            .await
            .expect("failed to create pool");

        let journal_size_limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
            .fetch_one(pool.as_ref())
            .await
            .expect("journal_size_limit query failed");
        let temp_store: i64 = sqlx::query_scalar("PRAGMA temp_store")
            .fetch_one(pool.as_ref())
            .await
            .expect("temp_store query failed");
        let mmap_size: i64 = sqlx::query_scalar("PRAGMA mmap_size")
            .fetch_one(pool.as_ref())
            .await
            .expect("mmap_size query failed");

        assert_eq!(pool.options().get_max_connections(), 3);
        assert_eq!(journal_size_limit, 131_072);
        assert_eq!(temp_store, 1);
        assert_eq!(mmap_size, 1_048_576);
    }

    #[tokio::test]
    async fn test_sqlite_wal_preflight_retries_locked_database() {
        sqlx::any::install_default_drivers();

        let db_path = std::env::temp_dir().join(format!("wal_retry_{}.db", uuid::Uuid::now_v7()));
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let mut lock_conn = AnyConnection::connect(&db_url)
            .await
            .expect("failed to open lock connection");
        sqlx::query("CREATE TABLE locked_write (id INTEGER PRIMARY KEY)")
            .execute(&mut lock_conn)
            .await
            .expect("failed to create test table");
        sqlx::query("BEGIN EXCLUSIVE")
            .execute(&mut lock_conn)
            .await
            .expect("failed to acquire exclusive lock");

        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("PRAGMA busy_timeout = 20").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&db_url)
            .await
            .expect("failed to create test pool");

        let wal_pool = pool.clone();
        let wal_task = tokio::spawn(async move {
            enable_sqlite_wal_with_retry(&wal_pool, SQLITE_WAL_MAX_ATTEMPTS, Duration::from_millis(50)).await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        sqlx::query("ROLLBACK")
            .execute(&mut lock_conn)
            .await
            .expect("failed to release exclusive lock");

        wal_task
            .await
            .expect("WAL preflight task panicked")
            .expect("WAL preflight should retry after SQLITE_BUSY");

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("journal_mode query failed");
        assert_eq!(journal_mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn test_sqlite_memory_pool_stays_single_connection() {
        let config = SqliteConfig {
            max_connections: 3,
            ..SqliteConfig::default()
        };
        let pool = create_pool_with_sqlite_config(Some("sqlite://?mode=memory"), config)
            .await
            .expect("failed to create pool");

        assert_eq!(pool.options().get_max_connections(), SQLITE_MEMORY_MAX_CONNECTIONS);
    }
}
