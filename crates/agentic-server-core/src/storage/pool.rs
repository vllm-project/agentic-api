//! Database connection pooling and initialization.

use std::sync::Arc;

use sqlx::any::AnyPoolOptions;

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
    if url.starts_with("sqlite") && !url.contains('?') {
        format!("{url}?mode=rwc")
    } else {
        url.to_string()
    }
}

/// Creates a connection pool for the database.
///
/// Initializes a connection pool with sensible defaults:
/// - Max connections: 10 (configurable via [`AnyPoolOptions`])
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
    // Install default drivers for auto-detection
    sqlx::any::install_default_drivers();

    // Prepare URL with database-specific parameters
    let url = prepare_db_url(db_url);

    // SQLite only allows one writer at a time; a single connection in the pool
    // serializes writes at the pool level (queue).
    // For other databases, 10 connections is a conservative default.
    let max_connections = if url.starts_with("sqlite") { 1 } else { 10 };
    let pool = AnyPoolOptions::new()
        .max_connections(max_connections)
        .connect(&url)
        .await?;

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
    use crate::storage::PoolWithSchema;

    let pool = create_pool(db_url).await?;
    let pool_with_schema = PoolWithSchema::new(pool);
    pool_with_schema.ensure_schema_ready().await?;

    Ok(pool_with_schema.pool().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(prepared, "sqlite://test.db?cache=shared");
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
}
