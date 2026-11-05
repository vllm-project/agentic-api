//! Typed state of a persisted Messages checkpoint.

use super::StorageError;
use crate::storage::models::message_thread::MessageThread;
use crate::types::messages::thread::MessageThreadCheckpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageThreadState {
    Ready,
    InFlight,
    Consumed,
}

impl TryFrom<String> for MessageThreadState {
    type Error = StorageError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "ready" => Ok(Self::Ready),
            "in_flight" => Ok(Self::InFlight),
            "consumed" => Ok(Self::Consumed),
            _ => Err(StorageError::Validation(
                "invalid stored Messages thread state".to_owned(),
            )),
        }
    }
}

impl TryFrom<MessageThread> for MessageThreadCheckpoint {
    type Error = StorageError;

    fn try_from(row: MessageThread) -> Result<Self, Self::Error> {
        let checkpoint = row
            .checkpoint
            .ok_or_else(|| StorageError::Validation("stored Messages checkpoint is missing".to_owned()))?;
        serde_json::from_str(&checkpoint).map_err(StorageError::Serialization)
    }
}

impl TryFrom<&MessageThreadCheckpoint> for String {
    type Error = StorageError;

    fn try_from(checkpoint: &MessageThreadCheckpoint) -> Result<Self, Self::Error> {
        serde_json::to_string(checkpoint).map_err(StorageError::Serialization)
    }
}
