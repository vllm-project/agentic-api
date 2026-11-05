//! Typed state of a persisted Messages checkpoint.

use super::StorageError;

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
