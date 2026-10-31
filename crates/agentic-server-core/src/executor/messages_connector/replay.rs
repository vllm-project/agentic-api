//! Typed connector history lowering and mixed-call continuation validation.
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::input::internal_mcp_tool_name;
use crate::types::messages::GatewayToolMap;
use crate::types::messages::mcp::PendingMcpCall;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ConnectorHistoryBlock {
    McpToolUse {
        id: String,
        name: String,
        server_name: String,
        input: Map<String, Value>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    McpToolResult {
        tool_use_id: String,
        #[serde(default)]
        is_error: bool,
        content: Value,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UpstreamConnectorBlock {
    ToolUse {
        id: String,
        name: String,
        input: Map<String, Value>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        is_error: bool,
        content: Value,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
}

pub(in crate::executor) fn normalize_replayed_mcp(raw: &mut Value, map: &GatewayToolMap) -> ExecutorResult<()> {
    let available_tools = raw["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<HashSet<_>>();
    let Some(messages) = raw["messages"].as_array_mut() else {
        return Ok(());
    };
    let mut normalized = Vec::with_capacity(messages.len());
    let mut names = map
        .mcp_names()
        .map(|(name, server, tool)| (name.to_owned(), (server.to_owned(), tool.to_owned())))
        .collect();
    for mut message in std::mem::take(messages) {
        let Some(blocks) = message["content"].as_array_mut() else {
            normalized.push(message);
            continue;
        };
        let blocks = std::mem::take(blocks);
        if blocks.is_empty() {
            normalized.push(message);
            continue;
        }
        let original_role = message["role"].as_str().unwrap_or_default().to_owned();
        let mut segment_role = original_role.as_str();
        let mut segment = Vec::new();
        for mut block in blocks {
            if block["type"] == "tool_search_tool_result"
                && let Some(references) = block["content"]["tool_references"].as_array_mut()
            {
                // Hosted search references must resolve to current definitions.
                // Removing a stale reference does not remove the historical call or output.
                references.retain(|reference| {
                    reference["type"] != "tool_reference"
                        || reference["tool_name"]
                            .as_str()
                            .is_some_and(|name| available_tools.contains(name))
                });
            }
            // Preserve the chronology: assistant calls, user outputs, then the
            // assistant's answer or next calls. Do not move the answer before its outputs.
            let is_mcp_result = normalize_replayed_mcp_block(&mut block, map, &mut names)?;
            let role = if is_mcp_result { "user" } else { original_role.as_str() };
            if role != segment_role && !segment.is_empty() {
                normalized.push(replay_segment(&message, segment_role, std::mem::take(&mut segment)));
            }
            segment_role = role;
            segment.push(block);
        }
        normalized.push(replay_segment(&message, segment_role, segment));
    }
    *messages = normalized;
    Ok(())
}

fn replay_segment(message: &Value, role: &str, blocks: Vec<Value>) -> Value {
    let mut segment = message.clone();
    segment["role"] = Value::String(role.to_owned());
    segment["content"] = Value::Array(blocks);
    segment
}

fn normalize_replayed_mcp_block(
    block: &mut Value,
    map: &GatewayToolMap,
    names: &mut HashMap<String, (String, String)>,
) -> ExecutorResult<bool> {
    if !matches!(block["type"].as_str(), Some("mcp_tool_use" | "mcp_tool_result")) {
        return Ok(false);
    }
    let parsed = ConnectorHistoryBlock::deserialize(&*block).map_err(ExecutorError::JsonError)?;
    let (lowered, result) = match parsed {
        ConnectorHistoryBlock::McpToolUse {
            id,
            name,
            server_name,
            input,
            extra,
        } => {
            if id.is_empty() || name.is_empty() || server_name.is_empty() {
                return Err(ExecutorError::InvalidRequest(
                    "replayed MCP call requires id, server_name and name".to_owned(),
                ));
            }
            // Replaying an old call does not grant execution permission.
            let internal = map
                .mcp_internal_name(&server_name, &name)
                .map_or_else(|| internal_mcp_tool_name(&server_name, &name, names), str::to_owned);
            (
                UpstreamConnectorBlock::ToolUse {
                    id,
                    name: internal,
                    input,
                    extra,
                },
                false,
            )
        }
        ConnectorHistoryBlock::McpToolResult {
            tool_use_id,
            is_error,
            content,
            extra,
        } => (
            UpstreamConnectorBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
                extra,
            },
            true,
        ),
    };
    *block = serde_json::to_value(lowered).map_err(ExecutorError::JsonError)?;
    Ok(result)
}

/// Recognize an unresolved mixed turn before replay normalization loses MCP identity.
/// Historical completed calls never become execution grants.
pub(in crate::executor) fn pending_mcp_calls(raw: &Value, map: &GatewayToolMap) -> ExecutorResult<Vec<PendingMcpCall>> {
    let invalid = || {
        ExecutorError::InvalidRequest("invalid pending MCP continuation: provide all matching client tool_result blocks immediately after the mixed assistant turn".to_owned())
    };
    let Some(messages) = raw["messages"].as_array() else {
        return Ok(Vec::new());
    };
    let mut pending = HashMap::new();
    let mut seen = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("mcp_tool_use") => {
                    let call = PendingMcpCall::deserialize(block).map_err(|_| invalid())?;
                    if message["role"] != "assistant"
                        || call.id.is_empty()
                        || call.name.is_empty()
                        || call.server_name.is_empty()
                        || !seen.insert(call.id.clone())
                    {
                        return Err(invalid());
                    }
                    pending.insert(call.id.clone(), (index, call));
                }
                Some("mcp_tool_result") => {
                    if let Some(id) = block["tool_use_id"].as_str() {
                        pending.remove(id);
                    }
                }
                _ => {}
            }
        }
    }
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let assistant_index = messages.len().checked_sub(2).ok_or_else(invalid)?;
    if pending.values().any(|(index, _)| *index != assistant_index)
        || messages[assistant_index]["role"] != "assistant"
        || messages[assistant_index + 1]["role"] != "user"
    {
        return Err(invalid());
    }
    let assistant = messages[assistant_index]["content"].as_array().ok_or_else(invalid)?;
    let mut clients = HashSet::new();
    for block in assistant.iter().filter(|block| block["type"] == "tool_use") {
        let name = block["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(invalid)?;
        if !map.is_gateway_owned(name) {
            let id = block["id"].as_str().filter(|id| !id.is_empty()).ok_or_else(invalid)?;
            if seen.contains(id) || !clients.insert(id) {
                return Err(invalid());
            }
        }
    }
    let results = messages[assistant_index + 1]["content"]
        .as_array()
        .ok_or_else(invalid)?;
    if clients.is_empty() || results.len() != clients.len() {
        return Err(invalid());
    }
    for result in results {
        if result["type"] != "tool_result" || !result["tool_use_id"].as_str().is_some_and(|id| clients.remove(id)) {
            return Err(invalid());
        }
    }
    // Preserve the assistant's call order rather than HashMap iteration order.
    Ok(assistant
        .iter()
        .filter_map(|block| {
            block["id"]
                .as_str()
                .and_then(|id| pending.remove(id))
                .map(|(_, call)| call)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_replay_preserves_extensions_and_does_not_grant_execution() {
        let mut raw = json!({"messages":[{"role":"assistant", "provider_message":true, "content":[
            {"type":"mcp_tool_use", "id":"call", "server_name":"s", "name":"echo",
                "input":{"nested":[1,true]}, "cache_control":{"type":"ephemeral"}},
            {"type":"mcp_tool_result", "tool_use_id":"call", "is_error":false,
                "content":[{"type":"text","text":"ok","provider_text":true}], "provider_result":42}
        ]}]});
        let map = GatewayToolMap::default();
        normalize_replayed_mcp(&mut raw, &map).unwrap();
        assert_eq!(raw["messages"][0]["role"], "assistant");
        assert_eq!(raw["messages"][1]["role"], "user");
        let call = &raw["messages"][0]["content"][0];
        assert_eq!(call["type"], "tool_use");
        assert_eq!(call["input"], json!({"nested":[1,true]}));
        assert_eq!(call["cache_control"], json!({"type":"ephemeral"}));
        assert!(!map.is_gateway_owned(call["name"].as_str().unwrap()));
        let result = &raw["messages"][1]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["provider_result"], 42);
        assert_eq!(result["content"][0]["provider_text"], true);
        assert_eq!(raw["messages"][1]["provider_message"], true);
    }
}
