//! Domain types for conversation items.

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::StorageError;
use crate::storage::models::Item as StorageDbItem;
use crate::types::io::code_interpreter::CodeInterpreterCallOrigin;
use crate::types::io::{InOutItem, OutputItem};

pub(crate) const STORED_ITEM_KIND_KEY: &str = "_agentic_item_kind";
pub(crate) const STORED_CODE_INTERPRETER_ORIGIN_KEY: &str = "_agentic_code_interpreter_origin";

/// Item kind (input vs output) for storage and retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Input,
    Output,
}

impl ItemKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    pub(crate) fn from_stored_str(value: &str) -> Option<Self> {
        match value {
            "input" => Some(Self::Input),
            "output" => Some(Self::Output),
            _ => None,
        }
    }
}

impl InOutItem {
    /// Validate a client-supplied public identity before item creation.
    /// Persistence itself also accepts provider-assigned IDs with different prefixes.
    pub(crate) fn validate_client_id(&self, param: String) -> Result<(), StorageError> {
        let (id, prefix) = match self {
            Self::Input(item) => (item.id(), item.id_prefix()),
            Self::Output(item) => (item.id(), item.id_prefix()),
        };
        if let (Some(id), Some(prefix)) = (id, prefix)
            && !id.starts_with(prefix)
        {
            return Err(StorageError::InvalidItemId {
                param,
                id: id.to_owned(),
                prefix: prefix.trim_end_matches('_'),
            });
        }
        Ok(())
    }
}

/// Decode retained history without silently omitting invalid records.
///
/// Legacy unmarked items retain the existing input/output discrimination policy.
/// A malformed known item is an error, not an absent item or a replayable placeholder.
impl TryFrom<&StorageDbItem> for InOutItem {
    type Error = StorageError;

    fn try_from(row: &StorageDbItem) -> Result<Self, Self::Error> {
        row.as_inout().ok_or_else(|| StorageError::InvalidHistoryItem {
            item_id: row.id.clone(),
        })
    }
}

impl TryFrom<&InOutItem> for String {
    type Error = StorageError;

    fn try_from(item: &InOutItem) -> Result<Self, Self::Error> {
        let (mut value, kind) = match item {
            InOutItem::Input(input) => (
                serde_json::to_value(input).map_err(StorageError::Serialization)?,
                ItemKind::Input,
            ),
            InOutItem::Output(output) => (
                serde_json::to_value(output).map_err(StorageError::Serialization)?,
                ItemKind::Output,
            ),
        };

        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                STORED_ITEM_KIND_KEY.to_string(),
                Value::String(kind.as_str().to_string()),
            );
            if matches!(
                item,
                InOutItem::Output(OutputItem::CodeInterpreterCall(call))
                    if call.origin == CodeInterpreterCallOrigin::Gateway
            ) {
                obj.insert(
                    STORED_CODE_INTERPRETER_ORIGIN_KEY.to_owned(),
                    Value::String("gateway".to_owned()),
                );
            }
        }

        serde_json::to_string(&value).map_err(StorageError::Serialization)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{InputItem, InputMessage, InputMessageContent};

    #[test]
    fn test_inout_item_to_string() {
        let input = InputItem::Message(InputMessage {
            role: "user".to_string(),
            content: InputMessageContent::Text("test".to_string()),
            ..Default::default()
        });
        let item = InOutItem::Input(input);
        let json = String::try_from(&item).expect("serialization failed");
        assert!(json.contains(STORED_ITEM_KIND_KEY));
        assert!(json.contains("input"));
        assert!(json.contains("user"));
        assert!(json.contains("test"));
    }

    #[test]
    fn test_item_kind_serialization() {
        let kind = ItemKind::Input;
        let json = serde_json::to_string(&kind).expect("serialization failed");
        assert_eq!(json, "\"input\"");

        let kind2 = ItemKind::Output;
        let json2 = serde_json::to_string(&kind2).expect("serialization failed");
        assert_eq!(json2, "\"output\"");
    }
}
