//! Prepared SQL values, including provenance outside the public item JSON.

use crate::storage::StorageError;
use crate::types::io::InOutItem;
use crate::utils::common::uuid7_str;

/// An item ready to be written: its public ID, stored JSON, and server-owned provenance.
#[derive(Debug, Clone)]
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
            reasoning_provenance: None,
        })
    }

    /// Prepare new items, generating an ID with the item type's prefix where none was supplied.
    ///
    /// # Errors
    /// Returns a validation error for an empty supplied ID, or a serialization error.
    pub(crate) fn prepare_all(items: Vec<InOutItem>) -> Result<Vec<Self>, StorageError> {
        items
            .into_iter()
            .map(|item| {
                let prefix = match &item {
                    InOutItem::Input(input) => input.id_prefix(),
                    InOutItem::Output(output) => output.id_prefix(),
                };
                if item.id() == Some("") {
                    return Err(StorageError::Validation("item id cannot be an empty string".to_owned()));
                }
                let id = item
                    .id()
                    .map_or_else(|| uuid7_str(prefix.unwrap_or("item_")), str::to_owned);
                Self::from_item(id, &item)
            })
            .collect()
    }

    /// Raw rows used only by readiness checks and storage unit tests.
    pub(crate) fn unmarked(id: String, data: String) -> Self {
        Self {
            id,
            data,
            reasoning_provenance: None,
        }
    }
}
