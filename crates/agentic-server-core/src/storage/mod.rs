//! Storage layer for persistence operations.

// Strong types for storage operations (newtype pattern)
pub mod types;

// Database connection pooling and initialization
pub mod pool;

// Database schema management and migrations
pub mod schema;

// Database schema models (sqlx FromRow types)
pub mod models;

// Response storage operations
pub mod response;

// Conversation storage operations
pub mod conversation;

// Re-export commonly used types for convenience
pub use conversation::ConversationStore;
pub use models::Conversation as DbConversation;
pub use models::Item;
pub use models::Response as DbResponse;
pub use pool::{DbPool, DbResult, DbTransaction, create_pool, create_pool_with_schema};
pub use response::ResponseStore;
pub use schema::{PoolWithSchema, SchemaManager};
pub use types::{ConversationData, InOutItem, ItemKind, ResponseData, ResponseMetadata, StorageError, StoreResult};
