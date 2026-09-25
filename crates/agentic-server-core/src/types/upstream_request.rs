//! Upstream request contract shared by the default and reserved replay adapters.

use serde::Serialize;

use super::io::{FunctionTool, ToolChoice};
use super::request_response::{ReasoningConfig, ResponseTextConfig};
use super::upstream_input::UpstreamInput;

#[derive(Debug, Serialize)]
pub struct UpstreamRequest<'a> {
    pub model: &'a str,
    pub input: UpstreamInput<'a>,
    pub stream: bool,
    /// Upstream storage policy, independent of gateway persistence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<&'a str>,
    /// Function-like declarations are normalized to ordinary function tools.
    /// Skipped when empty so vLLM does not receive an empty array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<UpstreamTool>>,
    #[serde(
        skip_serializing_if = "is_absent_or_default_tool_choice",
        serialize_with = "serialize_upstream_tool_choice"
    )]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<&'a ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a ResponseTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_eos: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<&'a str>,
    // Existing metadata contract, moved unchanged from request_response.rs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_salt: Option<&'a str>,
}

/// Gateway and client tool declarations are converted to function tools before
/// entering this upstream-only payload.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum UpstreamTool {
    Function(FunctionTool),
}

// Serde requires a reference to the field's concrete Option type.
#[allow(clippy::ref_option)]
fn is_absent_or_default_tool_choice(choice: &Option<ToolChoice>) -> bool {
    choice.as_ref().is_none_or(|choice| matches!(choice, ToolChoice::Auto))
}

#[allow(clippy::ref_option)]
fn serialize_upstream_tool_choice<S>(choice: &Option<ToolChoice>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    choice
        .as_ref()
        .map(ToolChoice::normalized_for_upstream)
        .serialize(serializer)
}
