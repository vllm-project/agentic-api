//! Generated file IDs and upload cancellation errors shared by providers.

use super::FileSearchError;

/// Validates the generated key used by every file-byte provider.
///
/// # Errors
/// Returns invalid-request when the ID is not a canonical `file-UUID` key.
pub fn validate_file_id(id: &str) -> Result<(), FileSearchError> {
    let valid = id
        .strip_prefix("file-")
        .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok().map(|uuid| (suffix, uuid)))
        .is_some_and(|(suffix, uuid)| uuid.hyphenated().to_string() == suffix);
    if !valid {
        return Err(FileSearchError::InvalidRequest(
            "file ID must be a generated file-UUID key".into(),
        ));
    }
    Ok(())
}

pub(crate) fn cancelled_upload_error() -> FileSearchError {
    FileSearchError::Unavailable("File upload was cancelled".into())
}
