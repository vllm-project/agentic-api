//! Retained-size accounting for output text annotations.

use crate::types::io::OutputAnnotation;

use super::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize};

impl RetainedSize for OutputAnnotation {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::FileCitation(citation) => {
                RETAINED_CONTAINER_OVERHEAD_BYTES
                    + "file_citation".len()
                    + citation.file_id.len()
                    + citation.filename.len()
                    + 8
            }
            Self::Other(value) => value.retained_bytes(),
        }
    }
}
