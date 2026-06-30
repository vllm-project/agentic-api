use serde::{Deserialize, Serialize};

use super::output::ReasoningOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTextContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputImageContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub image_url: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputContent {
    #[serde(rename = "input_text")]
    Text(InputTextContent),
    #[serde(rename = "input_image")]
    Image(InputImageContent),
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
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionToolResultMessage),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningOutput),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<InputItem>),
}
