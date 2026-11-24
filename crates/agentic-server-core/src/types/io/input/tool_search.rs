//! Tool-search input items and the `call_id` rule they share: a client search pairs its call and output by
//! `call_id`, while a hosted search has none.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{deserialize_non_blank_string, deserialize_optional_non_blank_string};
use crate::types::io::multi_agent::AgentAttribution;
use crate::types::io::output::ToolSearchCall;
use crate::types::tools::{ResponsesTool, ToolSearchExecution, ToolSearchStatus};

/// Checks the call-id rule shared by every tool-search item: a client search
/// pairs its call and output by `call_id`, while a hosted search has none.
pub(in crate::types::io) fn check_tool_search_call_id(
    execution: ToolSearchExecution,
    call_id: Option<&str>,
) -> Result<(), &'static str> {
    match (execution, call_id) {
        (ToolSearchExecution::Client, Some(_)) | (ToolSearchExecution::Server, None) => Ok(()),
        (ToolSearchExecution::Client, None) => Err("a client tool_search item requires a call_id"),
        (ToolSearchExecution::Server, Some(_)) => Err("a hosted tool_search item must have a null call_id"),
    }
}

#[derive(Deserialize)]
struct InputToolSearchCallWire {
    #[serde(default)]
    agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    id: String,
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    call_id: Option<String>,
    #[serde(default)]
    execution: ToolSearchExecution,
    arguments: Value,
    #[serde(default)]
    status: ToolSearchStatus,
}

impl TryFrom<InputToolSearchCallWire> for InputToolSearchCall {
    type Error = &'static str;

    fn try_from(wire: InputToolSearchCallWire) -> Result<Self, Self::Error> {
        check_tool_search_call_id(wire.execution, wire.call_id.as_deref())?;
        Ok(Self {
            agent: wire.agent,
            id: wire.id,
            call_id: wire.call_id,
            execution: wire.execution,
            arguments: wire.arguments,
            status: wire.status,
        })
    }
}

/// A public model-generated tool-search call replayed as Responses input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "InputToolSearchCallWire")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub id: String,
    /// Required for client execution; `null` for a hosted search.
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    pub call_id: Option<String>,
    #[serde(default)]
    pub execution: ToolSearchExecution,
    pub arguments: Value,
    #[serde(default)]
    pub status: ToolSearchStatus,
}

impl TryFrom<&ToolSearchCall> for InputToolSearchCall {
    type Error = ToolSearchStatus;

    fn try_from(call: &ToolSearchCall) -> Result<Self, Self::Error> {
        if call.status != ToolSearchStatus::Completed {
            return Err(call.status);
        }
        Ok(Self {
            agent: call.agent.clone(),
            id: call.id.clone(),
            call_id: call.call_id.clone(),
            execution: call.execution,
            arguments: call.arguments.clone(),
            status: ToolSearchStatus::Completed,
        })
    }
}

#[derive(Deserialize)]
struct ToolSearchOutputWire {
    #[serde(default)]
    agent: Option<AgentAttribution>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    call_id: Option<String>,
    #[serde(default)]
    execution: ToolSearchExecution,
    #[serde(default)]
    status: ToolSearchStatus,
    tools: Vec<ResponsesTool>,
}

impl TryFrom<ToolSearchOutputWire> for ToolSearchOutputMessage {
    type Error = &'static str;

    fn try_from(wire: ToolSearchOutputWire) -> Result<Self, Self::Error> {
        check_tool_search_call_id(wire.execution, wire.call_id.as_deref())?;
        Ok(Self {
            agent: wire.agent,
            id: wire.id,
            call_id: wire.call_id,
            execution: wire.execution,
            status: wire.status,
            tools: wire.tools,
        })
    }
}

/// Declarations resolving a public tool-search call: returned by the client,
/// or produced by the gateway for a hosted search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "ToolSearchOutputWire")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ToolSearchOutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Required for client execution; `null` for a hosted search.
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    pub call_id: Option<String>,
    #[serde(default)]
    pub execution: ToolSearchExecution,
    #[serde(default)]
    pub status: ToolSearchStatus,
    pub tools: Vec<ResponsesTool>,
}
