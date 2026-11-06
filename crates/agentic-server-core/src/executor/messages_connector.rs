//! Messages connector discovery, upstream declaration normalization, and replay lowering.
mod replay;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::messages_request::normalize_native_server_tools;
use crate::tool::mcp::messages::connector_tools;
use crate::tool::{GatewayExecutors, ToolDeclaration, ToolError, ToolRegistry, registry_tools};
use crate::types::messages::request::MessagesToolDeclarations;
use crate::types::messages::tools::MessagesToolFields;
use crate::types::messages::{GatewayToolMap, ToolParam};
use crate::utils::common::serialize_to_value;
pub(super) use replay::{normalize_replayed_mcp, pending_mcp_calls};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Normalize connector declarations and MCP replay for token counting with the same discovery path.
/// Native server-tool blocks must already have been replayed by the caller.
///
/// # Errors
/// Returns connector validation and registry construction failures.
pub async fn prepare_messages_count_tokens(
    body: &[u8],
    map: &GatewayToolMap,
    executors: &mut GatewayExecutors,
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
    let mut tools = registry_tools(declarations.tools.as_ref(), map);
    tools.extend(connector_tools(
        declarations.mcp_servers.as_deref().unwrap_or_default(),
        declared_tools,
    )?);
    let registry = ToolRegistry::build_with_handlers(&mut tools, executors).await?;
    validate_connector_discovery(&registry)?;
    let mut raw = raw;
    normalize_native_server_tools(&mut raw)?;
    let mut gateway_tools = map.clone();
    normalize_connector(&mut raw, &tools, &mut gateway_tools)?;
    normalize_replayed_mcp(&mut raw, &gateway_tools)?;
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

pub(super) fn validate_connector_discovery(registry: &ToolRegistry) -> ExecutorResult<()> {
    if let Some(failed) = registry.mcp_list_tool_items().find(|item| item.error.is_some()) {
        // Remote errors can include credentials; expose only the declared server identity.
        return Err(ToolError::Execution(format!(
            "MCP connector discovery failed for server '{}'",
            failed.server_label
        ))
        .into());
    }
    Ok(())
}

/// Expand connector declarations without changing history. Native server-tool
/// replay must retain each call/result pair in its original assistant message;
/// MCP history is lowered only after that replay has completed.
pub(super) fn normalize_connector(
    raw: &mut Value,
    tools: &[ToolDeclaration],
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
    let declarations = upstream_tools
        .iter()
        .map(|wire| ToolParam::deserialize(wire).map(|tool| (tool, wire.clone())))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ExecutorError::JsonError)?;
    for (tool, _) in &declarations {
        if tool.is_hosted_tool_search() {
            let deferred = tool.fields().and_then(|fields| fields.extra.get("defer_loading"));
            if deferred
                .map(|value| Option::<bool>::deserialize(value).map_err(ExecutorError::JsonError))
                .transpose()?
                .flatten()
                .unwrap_or(false)
            {
                return Err(ExecutorError::InvalidRequest(
                    "the hosted tool search tool cannot be deferred".to_owned(),
                ));
            }
            has_hosted_search = true;
        }
    }
    let mut names = declarations
        .iter()
        .filter_map(|(tool, _)| tool.name().map(str::to_owned))
        .collect::<HashSet<_>>();
    let mut expanded = Vec::new();
    for (declaration, wire) in declarations {
        let ToolParam::McpToolset(declaration) = declaration else {
            expanded.push(wire);
            continue;
        };
        let param = tools
            .iter()
            .find_map(|tool| match tool {
                ToolDeclaration::Mcp(param) if declaration.mcp_server_name == param.server_label => Some(param),
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
            let mut extra = HashMap::new();
            if deferred {
                extra.insert("defer_loading".to_owned(), Value::Bool(true));
            }
            let tool = ToolParam::Function(MessagesToolFields {
                name: discovered.internal_name.clone(),
                description: discovered.tool.description.as_deref().map(str::to_owned),
                input_schema: Some(Value::Object(discovered.tool.input_schema.as_ref().clone())),
                extra,
            });
            expanded.push(serialize_to_value(&tool).map_err(ExecutorError::JsonError)?);
        }
        if let Some(cache_control) = declaration.cache_control {
            let last = expanded
                .get_mut(first..)
                .and_then(|tools| tools.last_mut())
                .ok_or_else(|| {
                    ExecutorError::InvalidRequest(
                        "cache_control on an empty MCP toolset has no upstream cache boundary".to_owned(),
                    )
                })?;
            last["cache_control"] = cache_control;
        }
    }
    *upstream_tools = expanded;
    Ok(())
}
