//! Stored file content returned to request hydration.

/// A stored file's bytes and the metadata needed to inline them in an upstream request.
#[derive(Clone, Debug)]
pub struct FileContent {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}
