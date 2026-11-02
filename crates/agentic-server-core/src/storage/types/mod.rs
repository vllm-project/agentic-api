//! Domain types for storage operations.

pub mod background;
pub mod conversation;
pub mod errors;
pub mod item;
pub mod response;

pub use background::{BackgroundStatus, UnknownBackgroundStatus};
pub use conversation::{ConversationData, ConversationSnapshot, ConversationVersion};
pub use errors::{StorageError, StoreResult};
pub use item::ItemKind;
pub use response::{ResponseData, ResponseMetadata};
