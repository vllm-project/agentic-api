//! Prepared SQL values, including provenance outside the public item JSON.

use crate::storage::{InOutItem, StorageError};
use crate::utils::common::serialize_to_string;

pub(crate) struct InsertItem {
    pub id: String,
    pub data: String,
    pub reasoning_provenance: Option<String>,
}

impl InsertItem {
    pub(crate) fn from_item(id: String, item: &InOutItem) -> Result<Self, StorageError> {
        Ok(Self {
            id,
            data: String::try_from(item)?,
            reasoning_provenance: item.reasoning_provenance().map(serialize_to_string).transpose()?,
        })
    }

    /// Raw rows used only by migration/readiness checks and storage unit tests.
    pub(crate) fn unmarked(id: String, data: String) -> Self {
        Self {
            id,
            data,
            reasoning_provenance: None,
        }
    }
}
