//! Storage layer for persistence operations.
//!
//! Row models stay private. Callers use typed store operations instead of decoding database columns.
//!
//! ```compile_fail
//! use agentic_core::storage::models::Response;
//! ```
//!
//! ```compile_fail
//! use agentic_core::storage::ConversationStore;
//! ConversationStore::disabled().pool();
//! ```

// Database URL classification and sanitization
pub mod backend;

// Strong types for storage operations (newtype pattern)
pub mod types;

// Database connection pooling and initialization
pub mod pool;

// Database schema management and migrations
pub mod schema;

// Database schema models (sqlx FromRow types)
mod models;

// Response storage operations
pub mod messages;
pub mod response;

// Conversation storage operations
pub mod conversation;

// Re-export commonly used types for convenience
pub use backend::DatabaseBackend;
pub use conversation::ConversationStore;
pub use pool::{
    DbPool, DbResult, DbTransaction, create_pool, create_pool_with_configs, create_pool_with_schema,
    create_pool_with_schema_and_configs, create_pool_with_schema_and_sqlite_config, create_pool_with_sqlite_config,
};
pub use response::ResponseStore;
pub use schema::{PoolWithSchema, SchemaManager};
pub use types::{
    ConversationData, ConversationSnapshot, ItemKind, ResponseCheckpoint, ResponseData, ResponseMetadata, StorageError,
    StoreResult,
};
pub mod file_search;
pub mod local_files;

#[cfg(test)]
#[path = "../../tests/support/mod.rs"]
mod test_support;

#[cfg(test)]
#[path = "tests/conversation_storage_regressions.rs"]
mod conversation_storage_regressions;
#[cfg(test)]
#[path = "tests/postgres_storage_integration.rs"]
mod postgres_storage_integration;
#[cfg(test)]
#[path = "tests/response_deletion_storage_test.rs"]
mod response_deletion_storage_test;
