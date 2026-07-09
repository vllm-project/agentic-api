//! Domain types for storage operations.

pub mod conversation;
pub mod errors;
pub mod item;
pub mod response;

pub use conversation::ConversationData;
pub use errors::{StorageError, StoreResult};
pub use item::{InOutItem, ItemKind};
pub use response::{ResponseData, ResponseMetadata};
