//! Database schema management and migrations.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, info};

use super::pool::DbPool;

type DbResult<T> = Result<T, sqlx::Error>;

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
}

impl PoolWithSchema {
    /// Creates a new pool with schema tracking.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self {
            pool,
            schema_ready: AtomicBool::new(false),
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
    ///
    /// # Errors
    ///
    /// Returns a [`sqlx::Error`] if migrations fail.
    pub async fn ensure_schema_ready(&self) -> DbResult<()> {
        if self.schema_ready.load(Ordering::SeqCst) {
            return Ok(());
        }

        if is_marked_ready() {
            debug!("[schema] DDL skipped — marked ready by supervisor.");
            self.schema_ready.store(true, Ordering::SeqCst);
            return Ok(());
        }

        debug!("[schema] Running migrations...");
        sqlx::migrate!("./migrations")
            .run(self.pool.as_ref())
            .await
            .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
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
        sqlx::migrate!("./migrations")
            .run(self.pool)
            .await
            .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
        info!("[schema] DB schema ready.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
