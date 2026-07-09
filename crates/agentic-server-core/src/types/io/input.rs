use serde::{Deserialize, Serialize};

use super::output::{FunctionToolCall, ReasoningOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTextContent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputImageContent {
    pub image_url: Option<String>,
    pub detail: Option<String>,
}

/// Content item inside a message input.
///
/// Uses an internally-tagged enum — serde consumes `"type"` for the variant
/// discriminant so the inner structs must NOT redeclare a `type_` field.
/// `output_text` and `reasoning_text` reuse `InputTextContent` since they
/// carry only a `text` field; they are preserved so vLLM sees the full history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    InputText(InputTextContent),
    InputImage(InputImageContent),
    /// Assistant output text in rehydrated history.
    OutputText(InputTextContent),
    /// Reasoning step text in rehydrated history.
    ReasoningText(InputTextContent),
    /// Any other content type — drop silently.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub content: InputMessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    Text(String),
    Parts(Vec<InputContent>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionToolResultMessage {
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputItem {
    #[serde(rename = "message")]
    Message(InputMessage),
    /// The model's tool invocation — appears in rehydrated history so vLLM sees
    /// the full call/output pair across turns.
    #[serde(rename = "function_call")]
    FunctionCall(FunctionToolCall),
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionToolResultMessage),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningOutput),
    #[serde(other)]
    Unknown,
}

impl InputItem {
    #[must_use]
    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<InputItem>),
}
