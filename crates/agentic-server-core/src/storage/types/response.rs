//! Domain type for response storage.

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::models::Response as StorageDbResponse;
use super::errors::StorageError;
use crate::types::agent_tree::StoredTreeSnapshot;
use crate::types::io::ToolChoice;
use crate::types::io::reasoning::upgrade_legacy_reasoning;
use crate::types::request_response::ResponsePayload;
use crate::types::tools::ResponsesTool;
use crate::utils::common::serialize_to_string;

/// Response metadata with effective configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponseMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_tree: Option<StoredTreeSnapshot>,
    /// Exact terminal Responses payload, absent for legacy and non-Responses records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_snapshot: Option<Box<ResponsePayload>>,
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
        let metadata = decode_metadata(&row)
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

/// Decode stored metadata; every reader of stored [`ResponseMetadata`] goes through here.
///
/// Snapshots and agent trees stored before typed reasoning can hold reasoning items
/// in the earlier shape; project those and decode again. Anything the projection
/// can't read still fails closed.
pub(in crate::storage) fn decode_metadata(
    row: &StorageDbResponse,
) -> Result<Option<ResponseMetadata>, serde_json::Error> {
    row.metadata_as::<ResponseMetadata>().or_else(|error| {
        let Some(mut metadata) = row.metadata_as::<Value>()? else {
            return Err(error);
        };
        if let Some(output) = metadata
            .pointer_mut("/response_snapshot/output")
            .and_then(Value::as_array_mut)
        {
            upgrade_legacy_reasoning(output);
        }
        if let Some(agents) = metadata
            .pointer_mut("/multi_agent_tree/agents")
            .and_then(Value::as_array_mut)
        {
            for agent in agents {
                if let Some(history) = agent.get_mut("history").and_then(Value::as_array_mut) {
                    upgrade_legacy_reasoning(history);
                }
            }
        }
        ResponseMetadata::deserialize(metadata).map(Some)
    })
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
        if let Some(snapshot) = persisted.response_snapshot.as_mut() {
            if let Some(tools) = snapshot.tools.as_mut() {
                for tool in tools {
                    tool.sanitize_for_persistence();
                }
            }
        }
        serialize_to_string(&persisted).map_err(StorageError::Serialization)
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
    fn legacy_conversation_id_snapshot_serializes_with_canonical_conversation() {
        let metadata = serde_json::json!({
            "model": "test-model",
            "effective_tool_choice": "auto",
            "response_snapshot": {
                "id": "resp_legacy",
                "object": "response",
                "created_at": 1_704_067_200,
                "model": "test-model",
                "status": "completed",
                "output": [],
                "conversation_id": "conv_legacy"
            }
        });
        let row = StorageDbResponse {
            id: "resp_legacy".to_owned(),
            conversation_id: Some("conv_legacy".to_owned()),
            previous_response_id: None,
            history_item_ids: Some("[]".to_owned()),
            metadata: Some(metadata.to_string()),
            created_at: 1_704_067_200,
        };

        let stored = ResponseData::try_from(row).expect("decode pre-rename stored metadata");
        let snapshot = stored.metadata.response_snapshot.expect("stored response snapshot");
        assert_eq!(snapshot.conversation.as_deref(), Some("conv_legacy"));
        assert!(snapshot.standard_fields.store, "legacy response snapshots were stored");
        let public = serde_json::to_value(snapshot).expect("serialize retrieved response");
        assert_eq!(public["conversation"], serde_json::json!({"id": "conv_legacy"}));
        assert!(public.get("conversation_id").is_none());
    }

    #[test]
    fn test_response_metadata_serialization() {
        let metadata = ResponseMetadata {
            multi_agent_tree: None,
            model: "gpt-4".to_string(),
            previous_response_id: Some("resp_1".to_string()),
            effective_tools: None,
            tool_search_loaded_tools: None,
            response_snapshot: None,
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
            response_snapshot: Some(Box::new(snapshot)),
            ..ResponseMetadata::default()
        };

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
            metadata: Some(r#"{"model":"gpt-3.5","effective_tool_choice":"auto"}"#.to_string()),
            created_at: 1_704_067_200,
        };

        let response = ResponseData::try_from(db_row).expect("valid stored response");
        assert_eq!(response.history_item_ids.len(), 3);
        assert_eq!(response.history_item_ids[0], "item_1");
        assert_eq!(response.history_item_ids[2], "item_3");
        assert_eq!(response.previous_response_id, Some("resp_prev".to_string()));
    }

    /// Agent histories in a stored tree get the same projection as snapshots and rows.
    #[test]
    fn legacy_reasoning_in_stored_agent_history_is_projected_or_fails_closed() {
        use crate::types::agent::{AgentIdentity, AgentTurnId};
        use crate::types::agent_tree::{AgentState, StoredAgent};
        use crate::types::io::{InputItem, MultiAgentConfig, ReasoningOutput};

        let metadata = ResponseMetadata {
            multi_agent_tree: Some(StoredTreeSnapshot {
                version: 1,
                config: MultiAgentConfig {
                    enabled: true,
                    max_concurrent_subagents: None,
                },
                agents: vec![StoredAgent {
                    identity: AgentIdentity::root(),
                    parent: None,
                    turn: AgentTurnId::new(),
                    state: AgentState::Idle,
                    mailbox: Vec::new(),
                    history: vec![InputItem::Reasoning(ReasoningOutput::new("rs_tree"))],
                    loaded_tools: Vec::new(),
                    last_task: String::new(),
                    final_answer: None,
                    rounds: 0,
                    wait: None,
                }],
                client_calls: Vec::new(),
            }),
            ..ResponseMetadata::default()
        };
        let stored = |legacy: Value| {
            let mut metadata: Value = serde_json::from_str(&String::try_from(&metadata).unwrap()).unwrap();
            metadata["multi_agent_tree"]["agents"][0]["history"][0]
                .as_object_mut()
                .unwrap()
                .extend(legacy.as_object().unwrap().clone());
            ResponseData::try_from(StorageDbResponse {
                id: "resp_tree".into(),
                conversation_id: None,
                previous_response_id: None,
                history_item_ids: None,
                metadata: Some(metadata.to_string()),
                created_at: 0,
            })
        };

        let projected = stored(serde_json::json!({
            "content": [{"type": "unexpected_provider_type", "text": "tree plaintext"}],
            "encrypted_content": {"ciphertext": "untyped state"},
            "status": "failed"
        }))
        .unwrap();
        let tree = projected.metadata.multi_agent_tree.unwrap();
        let InputItem::Reasoning(reasoning) = &tree.agents[0].history[0] else {
            panic!("expected reasoning history");
        };
        assert_eq!(reasoning.content[0].text, "tree plaintext");
        assert!(reasoning.encrypted_content.is_none() && reasoning.status.is_none());

        let error = stored(serde_json::json!({"content": [{"text": "part without a type"}]})).unwrap_err();
        assert!(matches!(error, StorageError::InvalidResponseMetadata { .. }));
    }
}
