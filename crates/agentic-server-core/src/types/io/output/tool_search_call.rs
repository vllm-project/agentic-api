//! The public tool-search call item, client-executed or hosted.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::io::ToolSearchOutputMessage;
use crate::types::io::input::{
    check_tool_search_call_id, deserialize_non_blank_string, deserialize_optional_non_blank_string,
};
use crate::types::io::multi_agent::AgentAttribution;
use crate::types::io::output::FunctionToolCall;
use crate::types::tools::{ResponsesTool, ToolSearchExecution, ToolSearchStatus};
use crate::utils::common::{deserialize_from_str, stable_name_hash};

#[derive(Deserialize)]
struct ToolSearchCallWire {
    #[serde(default)]
    agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    id: String,
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    call_id: Option<String>,
    execution: ToolSearchExecution,
    arguments: Value,
    status: ToolSearchStatus,
}

impl TryFrom<ToolSearchCallWire> for ToolSearchCall {
    type Error = &'static str;

    fn try_from(wire: ToolSearchCallWire) -> Result<Self, Self::Error> {
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

/// A newly emitted public tool-search call, client-executed or hosted.
///
/// Unlike replay input, execution and status have no serde defaults: response
/// translation must populate both fields explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "ToolSearchCallWire")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ToolSearchCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(deserialize_with = "deserialize_non_blank_string")]
    pub id: String,
    /// Set for client execution; `null` for a hosted search.
    #[serde(default, deserialize_with = "deserialize_optional_non_blank_string")]
    pub call_id: Option<String>,
    pub execution: ToolSearchExecution,
    pub arguments: Value,
    pub status: ToolSearchStatus,
}

impl ToolSearchCall {
    /// The public call for a hosted search the model made as `call`.
    pub(crate) fn hosted(call: &FunctionToolCall, status: ToolSearchStatus) -> Self {
        let arguments = match status {
            ToolSearchStatus::Completed => {
                deserialize_from_str::<Value>(&call.arguments).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            // Arguments arrive whole on `output_item.done`, as the Responses API streams them.
            ToolSearchStatus::InProgress | ToolSearchStatus::Incomplete => Value::Object(Map::new()),
        };
        Self {
            agent: call.agent.clone(),
            id: public_tool_search_item_id(&call.id),
            call_id: None,
            execution: ToolSearchExecution::Server,
            arguments,
            status,
        }
    }
}

impl ToolSearchOutputMessage {
    /// The public output of the hosted search the model made as `call`, which loaded `tools`.
    pub(crate) fn hosted(call: &FunctionToolCall, tools: Vec<ResponsesTool>) -> Self {
        let call_item_id = public_tool_search_item_id(&call.id);
        Self {
            agent: call.agent.clone(),
            id: Some(format!("tso_{}", call_item_id.trim_start_matches("tsc_"))),
            call_id: None,
            execution: ToolSearchExecution::Server,
            status: ToolSearchStatus::Completed,
            tools,
        }
    }
}

/// The public `tsc_` item id of the tool-search call the model made with item id `item_id`.
pub(crate) fn public_tool_search_item_id(item_id: &str) -> String {
    if item_id.strip_prefix("tsc_").is_some_and(|suffix| !suffix.is_empty()) {
        return item_id.to_owned();
    }
    if let Some(suffix) = item_id.strip_prefix("fc_").filter(|suffix| !suffix.is_empty()) {
        return format!("tsc_{suffix}");
    }
    let domain_separated = format!("tool_search_item:{item_id}");
    format!("tsc_{:016x}", stable_name_hash(&domain_separated))
}
