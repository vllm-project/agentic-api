//! Messages connector discovery, upstream declaration normalization, and replay lowering.
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_request::normalize_native_server_tools;
use crate::types::messages::request::MessagesToolDeclarations;
use crate::types::messages::{GatewayToolMap, ToolParam};
use crate::utils::common::serialize_to_value;
use serde::Deserialize;
use serde_json::Value;

/// Normalize connector declarations and replay for token counting with the same discovery path.
///
/// # Errors
/// Returns connector validation and registry construction failures.
pub async fn prepare_messages_count_tokens(
    body: &[u8],
    map: &GatewayToolMap,
    executors: &mut crate::tool::GatewayExecutors,
) -> ExecutorResult<Option<Vec<u8>>> {
    let raw: Value = match serde_json::from_slice(body) {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    if !has_mcp_state(&raw) {
        return Ok(None);
    }
    let declarations = MessagesToolDeclarations::deserialize(&raw).map_err(ExecutorError::JsonError)?;
    let declared_tools = declarations.tools.as_deref().unwrap_or_default();
    let mut tools = crate::tool::registry_tools(declarations.tools.as_ref(), map);
    tools.extend(crate::tool::mcp::messages::connector_tools(
        declarations.mcp_servers.as_deref().unwrap_or_default(),
        declared_tools,
    )?);
    let registry = crate::tool::ToolRegistry::build_with_handlers(&mut tools, executors).await?;
    validate_connector_discovery(&registry)?;
    let mut raw = raw;
    normalize_native_server_tools(&mut raw)?;
    normalize_connector(&mut raw, &tools, &mut map.clone())?;
    Ok(Some(serde_json::to_vec(&raw).map_err(ExecutorError::JsonError)?))
}

/// Routing probe only; declarations and history are validated by the typed request and shared normalization.
pub(super) fn has_mcp_state(raw: &Value) -> bool {
    raw.get("mcp_servers").is_some()
        || raw["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["type"] == "mcp_toolset"))
        || raw["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| matches!(block["type"].as_str(), Some("mcp_tool_use" | "mcp_tool_result")))
                })
            })
        })
}

pub(super) fn validate_connector_discovery(registry: &crate::tool::ToolRegistry) -> ExecutorResult<()> {
    if let Some(failed) = registry.mcp_list_tool_items().find(|item| item.error.is_some()) {
        // Remote errors can include credentials; expose only the declared server identity.
        return Err(crate::tool::ToolError::Execution(format!(
            "MCP connector discovery failed for server '{}'",
            failed.server_label
        ))
        .into());
    }
    Ok(())
}

pub(super) fn normalize_connector(
    raw: &mut Value,
    tools: &[crate::tool::ToolDeclaration],
    map: &mut GatewayToolMap,
) -> ExecutorResult<()> {
    if !has_mcp_state(raw) {
        return Ok(());
    }
    if let Some(body) = raw.as_object_mut() {
        body.remove("mcp_servers");
        let tools = body.entry("tools").or_insert_with(|| Value::Array(Vec::new()));
        if tools.is_null() {
            *tools = Value::Array(Vec::new());
        }
    }
    let upstream_tools = raw["tools"]
        .as_array_mut()
        .ok_or_else(|| ExecutorError::InvalidRequest("MCP connector requires tools".to_owned()))?;
    let mut has_hosted_search = false;
    for tool in upstream_tools.iter() {
        let tool = ToolParam::deserialize(tool).map_err(ExecutorError::JsonError)?;
        if tool.is_hosted_tool_search() {
            if tool.defer_loading == Some(true) {
                return Err(ExecutorError::InvalidRequest(
                    "the hosted tool search tool cannot be deferred".to_owned(),
                ));
            }
            has_hosted_search = true;
        }
    }
    let mut names = upstream_tools
        .iter()
        .filter(|tool| tool["type"] != "mcp_toolset")
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<std::collections::HashSet<_>>();
    let mut expanded = Vec::new();
    for declaration in std::mem::take(upstream_tools) {
        if declaration["type"] != "mcp_toolset" {
            expanded.push(declaration);
            continue;
        }
        let param = tools
            .iter()
            .find_map(|tool| match tool {
                crate::tool::ToolDeclaration::Mcp(param)
                    if declaration["mcp_server_name"].as_str() == Some(param.server_label.as_str()) =>
                {
                    Some(param)
                }
                _ => None,
            })
            .ok_or_else(|| ExecutorError::InvalidRequest("MCP toolset has no discovered declaration".to_owned()))?;
        // One toolset breakpoint belongs at the end of its enabled catalog,
        // keeping surrounding client tools and other toolsets in their original order.
        let first = expanded.len();
        for discovered in &param.discovered_tools {
            if !names.insert(discovered.internal_name.clone()) {
                return Err(ExecutorError::InvalidRequest(
                    "discovered MCP name conflicts with a declared Messages tool".to_owned(),
                ));
            }
            map.insert_mcp(
                discovered.internal_name.clone(),
                discovered.server_label.clone(),
                discovered.tool_name.clone(),
            );
            let deferred = param
                .messages_config
                .as_ref()
                .is_some_and(|c| c.effective(&discovered.tool_name).1);
            if deferred && !has_hosted_search {
                return Err(ExecutorError::InvalidRequest(
                    "deferred MCP tools require an upstream-hosted tool search declaration".to_owned(),
                ));
            }
            let tool = ToolParam {
                name: discovered.internal_name.clone(),
                description: discovered.tool.description.as_deref().map(str::to_owned),
                input_schema: Some(Value::Object(discovered.tool.input_schema.as_ref().clone())),
                type_: None,
                mcp_server_name: None,
                default_config: None,
                configs: None,
                defer_loading: deferred.then_some(true),
                extra: std::collections::HashMap::default(),
            };
            expanded.push(serialize_to_value(&tool).map_err(ExecutorError::JsonError)?);
        }
        if let Some(cache_control) = declaration.get("cache_control") {
            if expanded.len() == first {
                return Err(ExecutorError::InvalidRequest(
                    "cache_control on an empty MCP toolset has no upstream cache boundary".to_owned(),
                ));
            }
            if let Some(last) = expanded.last_mut() {
                last["cache_control"] = cache_control.clone();
            }
        }
    }
    *upstream_tools = expanded;
    normalize_replayed_mcp(raw, map)
}

fn normalize_replayed_mcp(raw: &mut Value, map: &GatewayToolMap) -> ExecutorResult<()> {
    let available_tools = raw["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<std::collections::HashSet<_>>();
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
    names: &mut std::collections::HashMap<String, (String, String)>,
) -> ExecutorResult<bool> {
    match block["type"].as_str() {
        Some("mcp_tool_use") => {
            let server = block["server_name"].as_str().filter(|name| !name.is_empty());
            let tool = block["name"].as_str().filter(|name| !name.is_empty());
            let (Some(server), Some(tool)) = (server, tool) else {
                return Err(ExecutorError::InvalidRequest(
                    "replayed MCP call requires server_name and name".to_owned(),
                ));
            };
            // History is not an execution grant. Reuse the shared name allocator
            // even if a tool was disabled, removed, or its server is no longer declared.
            let name = map.mcp_internal_name(server, tool).map_or_else(
                || crate::tool::mcp::handler::internal_mcp_tool_name(server, tool, names),
                str::to_owned,
            );
            block["name"] = Value::String(name);
            block["type"] = Value::String("tool_use".to_owned());
            if let Some(block) = block.as_object_mut() {
                block.remove("server_name");
            }
            Ok(false)
        }
        Some("mcp_tool_result") => {
            block["type"] = Value::String("tool_result".to_owned());
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Recognize an unresolved mixed turn before replay normalization loses MCP identity.
/// Historical completed calls never become execution grants.
pub(super) fn pending_mcp_calls(
    raw: &Value,
    map: &GatewayToolMap,
) -> ExecutorResult<Vec<crate::types::messages::mcp::PendingMcpCall>> {
    use crate::types::messages::mcp::PendingMcpCall;
    use std::collections::{HashMap, HashSet};

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
