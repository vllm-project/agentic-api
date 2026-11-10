//! Domain type for response storage.

use std::convert::TryFrom;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::super::models::Response as StorageDbResponse;
use super::super::models::conversation::LatestTurn;
use super::errors::StorageError;
use crate::types::agent_tree::StoredTreeSnapshot;
use crate::types::io::{InputItem, ToolChoice};
use crate::types::request_response::ResponsePayload;
use crate::types::tools::ResponsesTool;
use crate::utils::common::serialize_to_string;

/// Response metadata with effective configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponseMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_tree: Option<StoredTreeSnapshot>,
    pub model: String,
    pub previous_response_id: Option<String>,
    pub effective_tools: Option<Vec<ResponsesTool>>,
    /// Public definitions whose deferred availability was resolved by tool search.
    ///
    /// This is separate from `effective_tools` so public `defer_loading` stays
    /// unchanged while compaction may remove the call/output pair that loaded it.
    pub tool_search_loaded_tools: Option<Vec<ResponsesTool>>,
    pub effective_tool_choice: ToolChoice,
    pub effective_instructions: Option<String>,
}

/// What a turn continues, produced by rehydration and extended by persistence: a stored response, a conversation's
/// latest turn, or a session's in-memory parent.
#[derive(Clone, Default, Serialize)]
pub struct ResponseCheckpoint {
    /// The response that produced it; `None` for a conversation before its first turn.
    pub response_id: Option<String>,
    /// The continued turn's metadata.
    pub metadata: ResponseMetadata,
    /// Whether storage holds its history; a transient (`store: false`) session checkpoint has none.
    pub durable: bool,
    /// The stored history's rows, in order, as rehydration read them. A conversation turn's response stores them
    /// before its own items, and a response turn extends its parent's. Counted in a session's retained size, so a
    /// durable fallback's rows share its budget.
    pub history_item_ids: Vec<String>,
    /// The turn answers the continued history's pending call, so it must be stored right after the continued turn:
    /// another turn stored since had to answer the same call. Set by rehydration; storage refuses the turn then.
    #[serde(skip)]
    pub requires_latest: bool,
    /// The history items a session keeps in memory; empty when only storage holds them.
    pub history: Vec<InputItem>,
}

impl fmt::Debug for ResponseCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseCheckpoint")
            .field("response_id", &self.response_id)
            .field("items", &self.history.len())
            .finish_non_exhaustive()
    }
}

impl From<ResponseData> for ResponseCheckpoint {
    fn from(response: ResponseData) -> Self {
        Self {
            response_id: Some(response.response_id),
            metadata: response.metadata,
            durable: true,
            history_item_ids: response.history_item_ids,
            requires_latest: false,
            history: Vec::new(),
        }
    }
}

/// Domain entity for a stored LLM response.
#[derive(Debug, Clone)]
pub struct ResponseData {
    /// Unique response identifier
    pub response_id: String,
    /// Optional conversation this response belongs to
    pub conversation_id: Option<String>,
    /// Optional reference to previous response for chaining
    pub previous_response_id: Option<String>,
    /// Creation timestamp as Unix timestamp in seconds
    pub created_at: i64,
    /// Deserialized history item IDs (vec of item IDs)
    pub history_item_ids: Vec<String>,
    /// Response metadata with effective configuration (fully typed)
    pub metadata: ResponseMetadata,
}

impl TryFrom<StorageDbResponse> for ResponseData {
    type Error = StorageError;

    fn try_from(row: StorageDbResponse) -> Result<Self, Self::Error> {
        // Do not propagate parser diagnostics: enum errors may echo stored secrets.
        let history_item_ids = row
            .history_item_ids_vec()
            .map_err(|_| StorageError::InvalidResponseHistory {
                response_id: row.id.clone(),
            })?;
        let metadata = row
            .metadata
            .as_deref()
            .map(ResponseMetadata::try_from)
            .transpose()
            .map_err(|_| StorageError::InvalidResponseMetadata {
                response_id: row.id.clone(),
            })?
            .unwrap_or_default();

        Ok(Self {
            response_id: row.id,
            conversation_id: row.conversation_id,
            previous_response_id: row.previous_response_id,
            created_at: row.created_at,
            history_item_ids,
            metadata,
        })
    }
}

/// Decode the `responses.metadata` column of the response that committed a conversation's latest turn. `turn` is
/// `None` when that response is missing or belongs to another conversation, which is invalid.
pub(in crate::storage) fn decode_turn_metadata(
    response_id: &str,
    turn: Option<&LatestTurn>,
) -> Result<Option<ResponseMetadata>, StorageError> {
    let invalid_metadata = || StorageError::InvalidResponseMetadata {
        response_id: response_id.to_owned(),
    };
    turn.ok_or_else(invalid_metadata)?
        .metadata
        .as_deref()
        .map(ResponseMetadata::try_from)
        .transpose()
        .map_err(|_| invalid_metadata())
}

/// Decode the `responses.metadata` column.
impl TryFrom<&str> for ResponseMetadata {
    type Error = StorageError;

    fn try_from(json: &str) -> Result<Self, Self::Error> {
        Ok(serde_json::from_str(json)?)
    }
}

impl TryFrom<&ResponseMetadata> for String {
    type Error = StorageError;

    fn try_from(metadata: &ResponseMetadata) -> Result<Self, Self::Error> {
        let mut persisted = metadata.clone();
        if let Some(tools) = persisted.effective_tools.as_mut() {
            for tool in tools {
                tool.sanitize_for_persistence();
            }
        }
        if let Some(tools) = persisted.tool_search_loaded_tools.as_mut() {
            for tool in tools {
                tool.sanitize_for_persistence();
            }
        }
        serialize_to_string(&persisted).map_err(StorageError::Serialization)
    }
}

/// Encode the `responses.snapshot` column: the response exactly as returned, without request-scoped MCP secrets.
impl TryFrom<&ResponsePayload> for String {
    type Error = StorageError;

    fn try_from(snapshot: &ResponsePayload) -> Result<Self, Self::Error> {
        let mut persisted = snapshot.clone();
        if let Some(tools) = persisted.tools.as_mut() {
            for tool in tools {
                tool.sanitize_for_persistence();
            }
        }
        serialize_to_string(&persisted).map_err(StorageError::Serialization)
    }
}

/// The number of output items in a `responses.snapshot` column, counted without decoding them.
pub(in crate::storage) fn snapshot_output_count(json: &str) -> Result<usize, StorageError> {
    #[derive(Deserialize)]
    struct SnapshotOutput {
        #[serde(default)]
        output: Vec<serde::de::IgnoredAny>,
    }
    Ok(serde_json::from_str::<SnapshotOutput>(json)?.output.len())
}

/// Decode the `responses.snapshot` column.
impl TryFrom<&str> for ResponsePayload {
    type Error = StorageError;

    fn try_from(json: &str) -> Result<Self, Self::Error> {
        Ok(serde_json::from_str(json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_data_from_db_response() {
        let db_row = StorageDbResponse {
            id: "resp_123".to_string(),
            conversation_id: Some("conv_456".to_string()),
            previous_response_id: None,
            history_item_ids: Some(r#"["item_1"]"#.to_string()),
            snapshot: None,
            metadata: Some(
                r#"{"model":"gpt-4","previous_response_id":null,"effective_tools":null,"effective_tool_choice":"auto","effective_instructions":null}"#
                    .to_string(),
            ),
            created_at: 1_704_067_200,
        };

        let response = ResponseData::try_from(db_row).expect("valid stored response");
        assert_eq!(response.response_id, "resp_123");
        assert_eq!(response.conversation_id, Some("conv_456".to_string()));
        assert_eq!(response.created_at, 1_704_067_200);
        assert_eq!(response.history_item_ids, vec!["item_1".to_string()]);
        assert_eq!(response.metadata.model, "gpt-4");
    }

    #[test]
    fn test_response_data_from_db_response_optional_fields() {
        let db_row = StorageDbResponse {
            id: "resp_789".to_string(),
            conversation_id: None,
            previous_response_id: None,
            history_item_ids: None,
            snapshot: None,
            metadata: None,
            created_at: 1_704_067_200,
        };

        let response = ResponseData::try_from(db_row).expect("legacy optional fields");
        assert_eq!(response.response_id, "resp_789");
        assert!(response.conversation_id.is_none());
        assert!(response.history_item_ids.is_empty());
        assert_eq!(response.metadata.model, "");
    }

    #[test]
    fn test_response_metadata_serialization() {
        let metadata = ResponseMetadata {
            multi_agent_tree: None,
            model: "gpt-4".to_string(),
            previous_response_id: Some("resp_1".to_string()),
            effective_tools: None,
            tool_search_loaded_tools: None,
            effective_tool_choice: ToolChoice::Auto,
            effective_instructions: Some("be helpful".to_string()),
        };

        let json_str = String::try_from(&metadata).expect("serialization failed");
        assert!(json_str.contains("gpt-4"));
        assert!(json_str.contains("resp_1"));
        assert!(json_str.contains("be helpful"));
    }

    #[test]
    fn code_interpreter_openai_declaration_round_trips_through_metadata_serialization() {
        let declaration = serde_json::json!({"type": "code_interpreter", "container": {"type": "auto"}});
        let tool = serde_json::from_value(declaration.clone()).expect("OpenAI tool declaration");
        let metadata = ResponseMetadata {
            effective_tools: Some(vec![tool]),
            ..ResponseMetadata::default()
        };

        let serialized = String::try_from(&metadata).expect("stored metadata");
        let stored: ResponseMetadata = serde_json::from_str(&serialized).expect("rehydrated metadata");
        assert_eq!(
            serde_json::to_value(stored.effective_tools).expect("stored tools"),
            serde_json::json!([declaration])
        );
    }

    #[test]
    fn test_response_metadata_serialization_removes_request_scoped_mcp_state() {
        let mut tool = serde_json::from_value(serde_json::json!({
            "type": "mcp",
            "server_label": "counter",
            "server_url": "https://mcp.example.com/mcp",
            "headers": {"X-API-Key": "secret"},
            "authorization": "bearer-secret",
            "require_approval": "never"
        }))
        .expect("valid MCP tool");
        let ResponsesTool::Mcp(param) = &mut tool else {
            panic!("expected MCP tool");
        };
        param
            .discovered_tools
            .push(crate::types::tools::McpDiscoveredToolParam {
                server_label: "counter".to_owned(),
                tool_name: "increment".to_owned(),
                internal_name: "mcp__counter__increment".to_owned(),
                tool: serde_json::from_value(serde_json::json!({
                    "name": "increment",
                    "inputSchema": {"type": "object"}
                }))
                .expect("discovered MCP tool"),
            });
        let snapshot = ResponsePayload {
            id: "resp_snapshot".into(),
            object: "response".into(),
            created_at: 123,
            model: "test-model".into(),
            status: "completed".into(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation: None,
            instructions: None,
            max_tool_calls: None,
            service_tier: None,
            tools: Some(vec![tool.clone()]),
            tool_choice: None,
            standard_fields: crate::types::request_response::StandardResponseFields::default(),
        };
        let metadata = ResponseMetadata {
            multi_agent_tree: None,
            effective_tools: Some(vec![tool]),
            tool_search_loaded_tools: None,
            ..ResponseMetadata::default()
        };
        let stored_snapshot = String::try_from(&snapshot).expect("snapshot serialization failed");
        assert!(!stored_snapshot.contains("secret"));
        let stored_snapshot = ResponsePayload::try_from(stored_snapshot.as_str()).expect("stored snapshot");
        let Some([ResponsesTool::Mcp(snapshot_tool)]) = stored_snapshot.tools.as_deref() else {
            panic!("expected stored MCP tool");
        };
        assert!(snapshot_tool.headers.is_none());
        assert!(snapshot_tool.authorization.is_none());

        let serialized = String::try_from(&metadata).expect("serialization failed");
        assert!(!serialized.contains("secret"));
        let serialized_value: serde_json::Value =
            serde_json::from_str(&serialized).expect("serialized response metadata");
        assert!(
            serialized_value["effective_tools"][0]
                .get("_agentic_discovered_tools")
                .is_none()
        );

        let persisted: ResponseMetadata = serde_json::from_str(&serialized).expect("persisted metadata");
        let tools = persisted.effective_tools.expect("persisted tools");
        let ResponsesTool::Mcp(tool) = &tools[0] else {
            panic!("expected MCP tool");
        };

        assert!(tool.headers.is_none());
        assert!(tool.authorization.is_none());
    }

    #[test]
    fn test_response_metadata_default() {
        let metadata = ResponseMetadata::default();
        assert_eq!(metadata.model, "");
        assert!(metadata.previous_response_id.is_none());
        assert!(metadata.effective_tools.is_none());
        assert!(metadata.tool_search_loaded_tools.is_none());
        assert!(metadata.effective_instructions.is_none());
    }

    #[test]
    fn test_response_data_multiple_history_items() {
        let db_row = StorageDbResponse {
            id: "resp_multi".to_string(),
            conversation_id: Some("conv_1".to_string()),
            previous_response_id: Some("resp_prev".to_string()),
            history_item_ids: Some(r#"["item_1","item_2","item_3"]"#.to_string()),
            snapshot: None,
            metadata: Some(r#"{"model":"gpt-3.5","effective_tool_choice":"auto"}"#.to_string()),
            created_at: 1_704_067_200,
        };

        let response = ResponseData::try_from(db_row).expect("valid stored response");
        assert_eq!(response.history_item_ids.len(), 3);
        assert_eq!(response.history_item_ids[0], "item_1");
        assert_eq!(response.history_item_ids[2], "item_3");
        assert_eq!(response.previous_response_id, Some("resp_prev".to_string()));
    }
}
