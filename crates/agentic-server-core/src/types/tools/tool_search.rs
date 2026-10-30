//! Tool-search declaration parameters: who runs the search, and how a hosted declaration is echoed.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who runs a tool search: the client, or the gateway ("hosted" search).
///
/// Replayed items without `execution` keep the historical client default; a
/// declaration without `execution` is hosted, matching the Responses API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchExecution {
    #[default]
    Client,
    Server,
}

/// Lifecycle status of a public tool-search call or output item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchStatus {
    InProgress,
    #[default]
    Completed,
    Incomplete,
}

/// Parameters for a tool-search declaration.
///
/// A hosted declaration serializes as the Responses API echoes it:
/// `description` and `parameters` are always present (possibly null) and
/// `execution` is omitted. A client declaration round-trips as sent.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ToolSearchToolParam {
    #[serde(default = "hosted_tool_search_execution")]
    pub execution: ToolSearchExecution,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    /// `async` applies only to function and custom tools; captured so declaration validation can reject it.
    #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub unsupported_async: Option<bool>,
}

const fn hosted_tool_search_execution() -> ToolSearchExecution {
    ToolSearchExecution::Server
}

impl ToolSearchToolParam {
    /// Whether the gateway runs this search.
    #[must_use]
    pub const fn is_hosted(&self) -> bool {
        matches!(self.execution, ToolSearchExecution::Server)
    }
}

impl Serialize for ToolSearchToolParam {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match self.execution {
            ToolSearchExecution::Server => {
                map.serialize_entry("description", &self.description)?;
                map.serialize_entry("parameters", &self.parameters)?;
            }
            ToolSearchExecution::Client => {
                map.serialize_entry("execution", &self.execution)?;
                if let Some(description) = &self.description {
                    map.serialize_entry("description", description)?;
                }
                if let Some(parameters) = &self.parameters {
                    map.serialize_entry("parameters", parameters)?;
                }
            }
        }
        // Declaration validation rejects `async`; keep it if one is serialized before that check.
        if let Some(async_execution) = self.unsupported_async {
            map.serialize_entry("async", &async_execution)?;
        }
        map.end()
    }
}
