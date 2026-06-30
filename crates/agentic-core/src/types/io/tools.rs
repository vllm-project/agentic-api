use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionTool {
    #[serde(rename = "type")]
    pub type_: String,
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
    pub strict: Option<bool>,
}

pub type ResponsesTool = FunctionTool;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    #[serde(rename = "function")]
    Function {
        name: String,
    },
}

/// Returns the effective tool list, preferring `request_tools` when explicitly
/// set by the caller, otherwise falling back to the stored configuration.
#[inline]
pub(crate) fn resolve_tools(
    request_tools: Option<&[ResponsesTool]>,
    stored_tools: Option<&[ResponsesTool]>,
    tools_explicitly_set: bool,
) -> Option<Vec<ResponsesTool>> {
    if tools_explicitly_set {
        request_tools
    } else {
        stored_tools
    }
    .map(<[_]>::to_vec)
}

/// Returns the effective tool choice using the same precedence as [`resolve_tools`].
#[inline]
pub(crate) fn resolve_tool_choice(
    request_choice: &ToolChoice,
    stored_choice: &ToolChoice,
    explicitly_set: bool,
) -> ToolChoice {
    if explicitly_set { request_choice } else { stored_choice }.clone()
}
