//! Conversion between stored item rows and history items.
//!
//! This is the only place item JSON is read or written. `items.data` holds the item exactly as serde writes it, and
//! `items.metadata` holds its [`ItemMetadata`].

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};

use crate::storage::models::Item as StorageDbItem;
use crate::storage::{StorageError, StoreResult};
use crate::types::io::InOutItem;
use crate::utils::common::{utcnow_str, uuid7_str};

/// Item kind (input vs output). Both shapes share item types, so the kind is recorded beside the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Input,
    Output,
}

/// Storage metadata kept in `items.metadata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMetadata {
    pub kind: ItemKind,
}

/// Decode a stored row. A row whose metadata or data does not decode is invalid history; errors never include
/// stored content.
impl TryFrom<&StorageDbItem> for InOutItem {
    type Error = StorageError;

    fn try_from(row: &StorageDbItem) -> Result<Self, Self::Error> {
        let invalid = |_| StorageError::InvalidHistoryItem {
            item_id: row.id.clone(),
        };
        let metadata: ItemMetadata = serde_json::from_str(&row.metadata).map_err(invalid)?;
        match metadata.kind {
            ItemKind::Input => serde_json::from_str(&row.data).map(Self::Input),
            ItemKind::Output => serde_json::from_str(&row.data).map(Self::Output),
        }
        .map_err(invalid)
    }
}

/// Decode a row as the public item returned by the Conversations and input-items APIs.
///
/// # Errors
/// Returns [`StorageError::InvalidHistoryItem`] when the row cannot be decoded.
pub(in crate::storage) fn decode_public(row: &StorageDbItem) -> StoreResult<InOutItem> {
    Ok(InOutItem::try_from(row)?.into_public(row.public_id.clone()))
}

/// Encode an item as a new row: a fresh row ID, its public ID (generated with the item's prefix when it has none),
/// its data and its metadata. `create_in_tx` assigns conversation membership, sequence and tenant.
///
/// # Errors
/// Returns [`StorageError::Validation`] for an empty ID, or a serialization error.
pub(in crate::storage) fn encode(item: &InOutItem) -> StoreResult<StorageDbItem> {
    let public_id = match item.id() {
        Some("") => return Err(StorageError::Validation("item id cannot be an empty string".to_owned())),
        Some(id) => id.to_owned(),
        None => {
            let prefix = match item {
                InOutItem::Input(input) => input.id_prefix(),
                InOutItem::Output(output) => output.id_prefix(),
            };
            uuid7_str(prefix.unwrap_or("item_"))
        }
    };
    let (data, kind) = match item {
        InOutItem::Input(input) => (serde_json::to_string(input)?, ItemKind::Input),
        InOutItem::Output(output) => (serde_json::to_string(output)?, ItemKind::Output),
    };
    Ok(StorageDbItem {
        id: uuid7_str("item_"),
        public_id,
        metadata: serde_json::to_string(&ItemMetadata { kind })?,
        data,
        created_at: utcnow_str(),
        conversation_id: None,
        seq: None,
        tenant_id: None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{
        FunctionToolCall, InputItem, InputMessage, InputMessageContent, OutputItem, ReasoningOutput,
        ReasoningTextContent,
    };

    /// The row `create_in_tx` stores for an item.
    fn stored_row(item: &InOutItem) -> StorageDbItem {
        StorageDbItem {
            id: "item_row".to_owned(),
            ..encode(item).expect("encode item")
        }
    }

    fn stored(value: Value) -> InOutItem {
        serde_json::from_value(value).unwrap()
    }

    fn round_trip(item: &InOutItem) -> InOutItem {
        InOutItem::try_from(&stored_row(item)).expect("decode stored item")
    }

    #[test]
    fn encoding_records_the_kind_and_plain_item_data() {
        let item = InOutItem::Input(InputItem::Message(InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Text("test".to_owned()),
            ..Default::default()
        }));
        let row = encode(&item).expect("encode item");
        assert!(row.id.starts_with("item_"));
        assert!(row.public_id.starts_with("msg_"));
        assert_eq!(
            serde_json::from_str::<Value>(&row.metadata).unwrap(),
            json!({"kind": "input"})
        );
        let data: Value = serde_json::from_str(&row.data).unwrap();
        assert_eq!(data, json!({"type": "message", "role": "user", "content": "test"}));
    }

    #[test]
    fn encoding_keeps_supplied_ids_and_rejects_empty_ids() {
        let row = encode(&stored(
            json!({"type":"message", "id":"msg_keep", "role":"user", "content":"x"}),
        ))
        .unwrap();
        assert_eq!(row.public_id, "msg_keep");
        let error = encode(&stored(
            json!({"type":"message", "id":"", "role":"user", "content":"x"}),
        ))
        .unwrap_err();
        assert!(error.is_validation());
    }

    #[test]
    fn reasoning_round_trips() {
        let mut reasoning = ReasoningOutput::new("rs_1");
        reasoning.content.extend([
            ReasoningTextContent::new("first thought"),
            ReasoningTextContent::new("second thought"),
        ]);
        reasoning
            .summary
            .push(crate::types::ReasoningSummaryContent::new("concise summary"));
        reasoning.encrypted_content = Some(crate::types::OpaqueReasoning::try_from("opaque".to_owned()).unwrap());
        reasoning.status = Some(crate::types::ReasoningStatus::Completed);
        let item = InOutItem::Output(OutputItem::Reasoning(reasoning));
        assert_eq!(round_trip(&item), item);
    }

    #[test]
    fn output_items_keep_their_kind() {
        let call = InOutItem::Output(OutputItem::FunctionCall(FunctionToolCall {
            async_execution: false,
            agent: None,
            id: "fc_1".to_owned(),
            call_id: "call_1".to_owned(),
            name: "run".to_owned(),
            namespace: Some("mcp__shell".to_owned()),
            arguments: "{\"cmd\":\"pwd\"}".to_owned(),
            status: MessageStatus::Completed,
        }));
        let shell = InOutItem::Output(
            serde_json::from_value(json!({
                "type": "shell_call",
                "id": "sh_1",
                "call_id": "call_shell",
                "action": {"commands": ["pwd"], "timeout_ms": 1_000, "max_output_length": 4_096},
                "status": "completed"
            }))
            .expect("shell output item"),
        );
        for item in [call, shell, InOutItem::Output(OutputItem::Unknown)] {
            assert_eq!(round_trip(&item), item);
        }
    }

    #[test]
    fn submitted_gateway_calls_remain_inputs() {
        for wire in [
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"weather"}}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"weather", "name":"forecast", "arguments":"{}", "output":"sunny"}),
        ] {
            let input = stored(wire);
            assert!(matches!(input, InOutItem::Input(_)));
            assert_eq!(round_trip(&input), input);
        }
    }

    #[test]
    fn rows_with_malformed_metadata_or_data_are_invalid() {
        let mut row = stored_row(&stored(json!({"type":"message", "role":"user", "content":"x"})));
        row.metadata = r#"{"kind":"unknown"}"#.to_owned();
        assert!(matches!(
            InOutItem::try_from(&row),
            Err(StorageError::InvalidHistoryItem { .. })
        ));
        row.metadata = r#"{"kind":"input"}"#.to_owned();
        row.data = "not json".to_owned();
        assert!(matches!(
            InOutItem::try_from(&row),
            Err(StorageError::InvalidHistoryItem { item_id }) if item_id == "item_row"
        ));
    }

    #[test]
    fn public_decoding_uses_the_row_public_identity() {
        let mut row = stored_row(&stored(json!({"type":"message", "role":"user", "content":"hello"})));
        row.public_id = "msg_public".to_owned();
        assert_eq!(decode_public(&row).unwrap().id(), Some("msg_public"));
    }
}
