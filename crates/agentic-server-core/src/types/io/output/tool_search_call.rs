//! The public tool-search call item, client-executed or hosted.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::io::input::{
    check_tool_search_call_id, deserialize_non_blank_string, deserialize_optional_non_blank_string,
};
use crate::types::io::multi_agent::AgentAttribution;
use crate::types::tools::{ToolSearchExecution, ToolSearchStatus};

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
