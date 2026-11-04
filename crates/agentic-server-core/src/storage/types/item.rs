//! Conversion between stored item rows and history items.
//!
//! This is the only place item JSON is read or written: rows decode to [`InOutItem`] and items encode to the
//! `(public_id, data)` pair inserted by [`crate::storage::models::item::create_in_tx`].

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::storage::models::Item as StorageDbItem;
use crate::storage::{StorageError, StoreResult};
use crate::types::io::code_interpreter::CodeInterpreterCallOrigin;
use crate::types::io::{InOutItem, InputItem, OutputItem, ReasoningOutput};
use crate::utils::common::uuid7_str;

pub(crate) const STORED_ITEM_KIND_KEY: &str = "_agentic_item_kind";
pub(crate) const STORED_CODE_INTERPRETER_ORIGIN_KEY: &str = "_agentic_code_interpreter_origin";

/// Item kind (input vs output) for storage and retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Input,
    Output,
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
/// The row is parsed once. A marked row decodes as its recorded kind; a legacy unmarked row prefers a known output,
/// then an input, then an unknown output. A malformed known item is an error, not an absent item or a replayable
/// placeholder.
impl TryFrom<&StorageDbItem> for InOutItem {
    type Error = StorageError;

    fn try_from(row: &StorageDbItem) -> Result<Self, Self::Error> {
        let invalid = || StorageError::InvalidHistoryItem {
            item_id: row.id.clone(),
        };
        let mut value: Value = serde_json::from_str(&row.data).map_err(|_| invalid())?;
        let (kind, origin) = take_storage_markers(&mut value);
        decode(&value, kind, origin).ok_or_else(|| {
            warn!(item_id = %row.id, "unrecognized item type in stored data");
            invalid()
        })
    }
}

/// Decode a row as the public item returned by the Conversations and input-items APIs.
///
/// # Errors
/// Returns [`StorageError::InvalidHistoryItem`] when the row cannot be decoded.
pub(in crate::storage) fn decode_public(row: &StorageDbItem) -> StoreResult<InOutItem> {
    Ok(InOutItem::try_from(row)?.into_public(row.public_id().to_owned()))
}

/// Encode an item as its `(public_id, data)` row values, generating a typed public ID when the item has none.
///
/// # Errors
/// Returns [`StorageError::Validation`] for an empty ID, or a serialization error.
pub(in crate::storage) fn encode(item: &InOutItem) -> StoreResult<(String, String)> {
    let id = match item.id() {
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
    Ok((id, String::try_from(item)?))
}

/// Encode the `items.data` column, recording the item kind and a gateway code-interpreter origin.
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
        if let Some(object) = value.as_object_mut() {
            object.insert(STORED_ITEM_KIND_KEY.to_owned(), serde_json::to_value(kind)?);
            if let InOutItem::Output(OutputItem::CodeInterpreterCall(call)) = item
                && call.origin == CodeInterpreterCallOrigin::Gateway
            {
                object.insert(
                    STORED_CODE_INTERPRETER_ORIGIN_KEY.to_owned(),
                    serde_json::to_value(call.origin)?,
                );
            }
        }
        serde_json::to_string(&value).map_err(StorageError::Serialization)
    }
}

/// Remove storage-only keys, returning the recorded item kind and code-interpreter origin.
fn take_storage_markers(value: &mut Value) -> (Option<ItemKind>, CodeInterpreterCallOrigin) {
    let Some(object) = value.as_object_mut() else {
        return (None, CodeInterpreterCallOrigin::default());
    };
    let kind = object
        .remove(STORED_ITEM_KIND_KEY)
        .and_then(|kind| ItemKind::deserialize(kind).ok());
    let origin = object
        .remove(STORED_CODE_INTERPRETER_ORIGIN_KEY)
        .and_then(|origin| CodeInterpreterCallOrigin::deserialize(origin).ok())
        .unwrap_or_default();
    (kind, origin)
}

fn decode(value: &Value, kind: Option<ItemKind>, origin: CodeInterpreterCallOrigin) -> Option<InOutItem> {
    match kind {
        Some(ItemKind::Input) => {
            if let Some(input) = decode_input(value) {
                return Some(InOutItem::Input(input));
            }
        }
        Some(ItemKind::Output) => {
            if let Some(output) = decode_output(value, origin) {
                return Some(InOutItem::Output(output));
            }
        }
        None => {}
    }
    let output = decode_output(value, origin);
    if output.as_ref().is_some_and(|item| !matches!(item, OutputItem::Unknown)) {
        return output.map(InOutItem::Output);
    }
    decode_input(value)
        .map(InOutItem::Input)
        .or_else(|| output.map(InOutItem::Output))
}

/// Typed decoding, projecting reasoning rows written before typed reasoning.
fn decode_input(value: &Value) -> Option<InputItem> {
    InputItem::deserialize(value)
        .ok()
        .or_else(|| ReasoningOutput::from_legacy_value(value).map(InputItem::Reasoning))
}

fn decode_output(value: &Value, origin: CodeInterpreterCallOrigin) -> Option<OutputItem> {
    let mut output = OutputItem::deserialize(value)
        .ok()
        .or_else(|| ReasoningOutput::from_legacy_value(value).map(OutputItem::Reasoning))?;
    if let OutputItem::CodeInterpreterCall(call) = &mut output {
        call.origin = origin;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{FunctionToolCall, InputMessage, InputMessageContent, ReasoningTextContent};

    fn row(id: &str, data: String) -> StorageDbItem {
        StorageDbItem {
            id: id.to_owned(),
            reference_id: None,
            public_id: None,
            data,
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
            tenant_id: None,
        }
    }

    fn stored_row(item: &InOutItem) -> StorageDbItem {
        row("item_row", String::try_from(item).expect("encode item"))
    }

    fn stored(value: Value) -> InOutItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn encoded_item_records_its_kind() {
        let item = InOutItem::Input(InputItem::Message(InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Text("test".to_owned()),
            ..Default::default()
        }));
        let (id, data) = encode(&item).expect("encode item");
        assert!(id.starts_with("msg_"));
        let value: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(value[STORED_ITEM_KIND_KEY], "input");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "test");
    }

    #[test]
    fn encoding_keeps_supplied_ids_and_rejects_empty_ids() {
        let (id, _) = encode(&stored(
            json!({"type":"message", "id":"msg_keep", "role":"user", "content":"x"}),
        ))
        .unwrap();
        assert_eq!(id, "msg_keep");
        let error = encode(&stored(
            json!({"type":"message", "id":"", "role":"user", "content":"x"}),
        ))
        .unwrap_err();
        assert!(error.is_validation());
    }

    #[test]
    fn test_item_kind_serialization() {
        assert_eq!(serde_json::to_string(&ItemKind::Input).unwrap(), "\"input\"");
        assert_eq!(serde_json::to_string(&ItemKind::Output).unwrap(), "\"output\"");
    }

    #[test]
    fn complete_reasoning_round_trip_strips_storage_marker() {
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
        let row = stored_row(&InOutItem::Output(OutputItem::Reasoning(reasoning)));
        assert!(row.data.contains(STORED_ITEM_KIND_KEY));

        let Ok(InOutItem::Output(OutputItem::Reasoning(reasoning))) = InOutItem::try_from(&row) else {
            panic!("expected stored reasoning output");
        };
        assert_eq!(reasoning.id, "rs_1");
        assert_eq!(reasoning.content.len(), 2);
        assert_eq!(reasoning.summary[0].text, "concise summary");
        assert_eq!(
            reasoning
                .encrypted_content
                .as_ref()
                .map(crate::types::OpaqueReasoning::as_str),
            Some("opaque")
        );
        assert_eq!(reasoning.status, Some(crate::types::ReasoningStatus::Completed));

        let reconstructed = serde_json::to_value(OutputItem::Reasoning(reasoning)).expect("reasoning value");
        assert!(reconstructed.get(STORED_ITEM_KIND_KEY).is_none());
    }

    #[test]
    fn legacy_output_message_decodes_as_output_before_unknown_input() {
        let row = row(
            "item_message",
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "hello", "annotations": []}]
            })
            .to_string(),
        );

        let item = InOutItem::try_from(&row).expect("stored item");
        assert!(matches!(item, InOutItem::Output(OutputItem::Message(_))));
    }

    #[test]
    fn namespaced_function_call_decodes_without_storage_marker() {
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

        let restored = InOutItem::try_from(&stored_row(&call)).expect("stored item");
        let value = serde_json::to_value(&restored).expect("item value");

        assert_eq!(restored, call);
        assert_eq!(value["namespace"], "mcp__shell");
        assert!(value.get(STORED_ITEM_KIND_KEY).is_none());
    }

    #[test]
    fn shell_call_round_trips_through_storage() {
        let call = InOutItem::Output(
            serde_json::from_value(json!({
                "type": "shell_call",
                "id": "sh_1",
                "call_id": "call_shell",
                "action": {"commands": ["pwd"], "timeout_ms": 1_000, "max_output_length": 4_096},
                "status": "completed"
            }))
            .expect("shell output item"),
        );

        let restored = InOutItem::try_from(&stored_row(&call)).expect("stored shell item");

        assert!(matches!(restored, InOutItem::Output(OutputItem::ShellCall(_))));
        assert_eq!(restored, call);
    }

    #[test]
    fn unknown_output_round_trips_as_unknown() {
        let restored = InOutItem::try_from(&stored_row(&InOutItem::Output(OutputItem::Unknown))).expect("stored item");
        assert!(matches!(restored, InOutItem::Output(OutputItem::Unknown)));
    }

    #[test]
    fn malformed_row_is_an_error() {
        let error = InOutItem::try_from(&row("item_bad", "not json".to_owned())).unwrap_err();
        assert!(matches!(error, StorageError::InvalidHistoryItem { item_id } if item_id == "item_bad"));
    }

    #[test]
    fn submitted_gateway_calls_remain_inputs_after_storage_round_trip() {
        for wire in [
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"weather"}}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"weather", "name":"forecast", "arguments":"{}", "output":"sunny"}),
        ] {
            let input = stored(wire);
            let restored = InOutItem::try_from(&stored_row(&input)).unwrap();
            assert!(matches!(restored, InOutItem::Input(_)));
            assert_eq!(restored, input);
        }
    }

    #[test]
    fn public_decoding_uses_the_row_public_identity() {
        let mut row = stored_row(&stored(json!({"type":"message", "role":"user", "content":"hello"})));
        row.public_id = Some("msg_public".to_owned());
        let public = decode_public(&row).unwrap();
        assert_eq!(public.id(), Some("msg_public"));
    }

    #[test]
    fn supplied_ids_are_validated_by_item_type_without_changing_call_ids() {
        let cases = [
            json!({"type":"message", "id":"msg_1", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"tool_search_call", "id":"tsc_1", "call_id":"call_1", "arguments":{}}),
            json!({"type":"function_call_output", "id":"fc_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"custom_tool_call_output", "id":"ctco_1", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "id":"sho_1", "call_id":"call_1", "output":[]}),
            json!({"type":"tool_search_output", "id":"tso_1", "call_id":"call_1", "tools":[]}),
            json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1", "name":"f", "input":"x"}),
            json!({"type":"shell_call", "id":"sh_1", "call_id":"call_1", "action":{"commands":["pwd"]}}),
            json!({"type":"reasoning", "id":"rs_1", "summary":[]}),
            json!({"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"s", "tools":[]}),
            json!({"type":"mcp_call", "id":"mcp_1", "server_label":"s", "name":"f", "arguments":"{}"}),
            json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"test"}}),
            json!({"type":"compaction", "id":"cmp_1", "encrypted_content":"opaque"}),
        ];
        for mut value in cases {
            let item = stored(value.clone());
            item.validate_client_id("items[0].id".into())
                .expect("matching public ID prefix");
            value["id"] = json!("wrong_1");
            let item = stored(value);
            let error = item.validate_client_id("items[0].id".into()).unwrap_err();
            assert!(error.is_validation());
            assert!(matches!(error, StorageError::InvalidItemId { param, .. } if param == "items[0].id"));
        }
    }

    #[test]
    fn optional_ids_are_not_required() {
        for value in [
            json!({"type":"message", "role":"user", "content":"hello"}),
            json!({"type":"function_call", "call_id":"call_1", "name":"f", "arguments":"{}"}),
            json!({"type":"function_call_output", "call_id":"call_1", "output":"done"}),
            json!({"type":"shell_call_output", "call_id":"call_1", "output":[]}),
        ] {
            stored(value).validate_client_id("items[0].id".into()).unwrap();
        }
    }

    #[test]
    fn invalid_id_reports_its_batch_index_and_rejects_empty_ids() {
        for id in ["", "item_wrongprefix"] {
            let items = [
                stored(json!({"type":"message", "role":"user", "content":"valid"})),
                stored(json!({"type":"message", "id":id, "role":"user", "content":"invalid"})),
            ];
            let error = items
                .iter()
                .enumerate()
                .try_for_each(|(index, item)| item.validate_client_id(format!("items[{index}].id")))
                .unwrap_err();
            assert!(matches!(&error, StorageError::InvalidItemId { param, .. } if param == "items[1].id"));
            assert_eq!(
                error.to_string(),
                format!("Invalid 'items[1].id': '{id}'. Expected an ID that begins with 'msg'.")
            );
        }
    }
}
