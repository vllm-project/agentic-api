//! Content resolved from the Files API for one model-facing request.
//!
//! `FileHandler::resolve` reads the referenced files and builds a [`ResolvedFiles`] map; only
//! [`super::ResponsesInput::normalized_model_input`] uses it, to replace a client's `file_id`
//! with inline content in the model copy. Stored history and `enriched_request.input` keep the
//! client's reference.
//!
//! Stored files do not carry a client-visible content type, so both the text rules and the
//! image data URL use the uploaded filename, as `OpenAI` does.

use std::collections::HashMap;

use crate::types::file_search::FileObject;

/// Files resolved for one request, keyed by the client's `file_id`.
///
/// The value is the stored object, whose filename decides the file kind, and its bytes.
pub type ResolvedFiles = HashMap<String, (FileObject, Vec<u8>)>;
