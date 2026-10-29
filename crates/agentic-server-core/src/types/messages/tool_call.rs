//! Completed tool-call data shared by the Messages JSON and SSE adapters.

use serde_json::{Map, Value};
use thiserror::Error;

/// Arguments stay arbitrary JSON, but a dispatchable input must be an object.
#[derive(Debug)]
pub(crate) struct MessagesToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: Result<Map<String, Value>, MessagesToolInputError>,
}

#[derive(Debug, Error)]
pub(crate) enum MessagesToolInputError {
    #[error("invalid tool arguments (not a JSON object)")]
    MissingObject,
    #[error("could not parse tool arguments as JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("tool arguments must be a JSON object")]
    NonObject,
}

/// The single parser for reconstructed streamed arguments. Its typed failure
/// travels with the call, even when history retains the legacy empty fallback.
pub(crate) fn parse_input(input: &str) -> Result<Map<String, Value>, MessagesToolInputError> {
    match serde_json::from_str(input).map_err(MessagesToolInputError::InvalidJson)? {
        Value::Object(object) => Ok(object),
        _ => Err(MessagesToolInputError::NonObject),
    }
}
