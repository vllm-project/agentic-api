use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::io::{
    FunctionTool, InputItem, InputMessage, InputMessageContent, OutputItem, ResponseUsage, ResponsesInput, ToolChoice,
};
use super::tools::ResponsesTool;
use crate::utils::common::serialize_to_string;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPayload {
    pub model: String,
    pub input: ResponsesInput,
    pub instructions: Option<String>,
    pub previous_response_id: Option<String>,
    pub conversation_id: Option<String>,
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub stream: bool,
    #[serde(default = "default_true")]
    pub store: bool,
    pub include: Option<Vec<String>>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub truncation: Option<String>,
    pub metadata: Option<Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct UpstreamRequest<'a> {
    pub model: &'a str,
    pub input: &'a ResponsesInput,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<&'a str>,
    /// Normalised tools forwarded to vLLM — always `Vec<FunctionTool>` regardless of
    /// what tool types the client declared. Gateway-managed types (`MCP`, `web_search`, …)
    /// are normalized to function stubs; function tools pass through unchanged.
    /// Skipped when empty so vLLM does not receive an empty array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<FunctionTool>>,
    #[serde(skip_serializing_if = "is_default_tool_choice")]
    pub tool_choice: &'a ToolChoice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Value>,
}

fn is_default_tool_choice(choice: &ToolChoice) -> bool {
    matches!(choice, ToolChoice::Auto)
}

impl RequestPayload {
    /// Construct an `UpstreamRequest` suitable for forwarding to vLLM.
    ///
    /// All tool types are normalised to `Vec<FunctionTool>` via
    /// [`ResponsesTool::to_function_tool`]. Gateway-managed tool types whose handlers
    /// have not yet landed (`MCP`, `web_search`, `file_search`, `code_interpreter`) are skipped
    /// with a warning — vLLM only understands `type: "function"`.
    #[must_use]
    pub fn to_upstream_request(&self, stream: bool) -> UpstreamRequest<'_> {
        let tools: Option<Vec<FunctionTool>> = self
            .tools
            .as_ref()
            .map(|tools| tools.iter().filter_map(ResponsesTool::to_function_tool).collect());
        // Treat an empty normalised list the same as no tools (skip the field entirely).
        let tools = tools.filter(|v| !v.is_empty());

        UpstreamRequest {
            model: &self.model,
            input: &self.input,
            stream,
            instructions: self.instructions.as_deref(),
            tools,
            tool_choice: &self.tool_choice,
            include: self.include.as_ref(),
            temperature: self.temperature,
            top_p: self.top_p,
            max_output_tokens: self.max_output_tokens,
            truncation: self.truncation.as_deref(),
            metadata: self.metadata.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteDetails {
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsePayload {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub model: String,
    pub status: String,
    #[serde(default)]
    pub output: Vec<OutputItem>,
    pub usage: Option<ResponseUsage>,
    pub incomplete_details: Option<IncompleteDetails>,
    pub error: Option<Value>,
    pub previous_response_id: Option<String>,
    pub conversation_id: Option<String>,
    pub instructions: Option<String>,
}

impl ResponsePayload {
    #[must_use]
    pub fn as_responses_chunk(&self) -> String {
        let json_str = serialize_to_string(self).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }
}

impl From<&ResponsesInput> for Vec<InputItem> {
    fn from(input: &ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                role: "user".into(),
                content: InputMessageContent::Text(text.clone()),
            })],
            ResponsesInput::Items(items) => items.clone(),
        }
    }
}
