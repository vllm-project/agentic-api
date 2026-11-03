//! Domain types for storage operations.

pub mod conversation;
pub mod errors;
pub mod item;
pub mod response;

pub use conversation::{ConversationData, ConversationSnapshot};
pub use errors::{StorageError, StoreResult};
pub use item::{ItemKind, ItemMetadata};
pub use response::{ResponseData, ResponseMetadata};
