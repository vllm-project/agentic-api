//! Responses WebSocket injection payloads. See the beta WebSocket event reference.
use super::client_calls::ClientToolOutput;
use super::io::{
    CustomToolCallOutputMessage, FunctionToolResultMessage, ShellCallOutputMessage, ToolCallOutput,
    ToolSearchOutputMessage,
};
use serde::{Deserialize, Serialize};

/// Client-output wire shapes observed in the multi-agent WebSocket references.
/// Shell and discovery have live acceptance evidence; custom also appears in
/// returned late-injection input. Ownership and kind validation remain in core.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum InjectionInput {
    #[serde(rename = "function_call_output")]
    Function(InjectionFunctionOutput),
    #[serde(rename = "shell_call_output")]
    Shell(ShellCallOutputMessage),
    #[serde(rename = "custom_tool_call_output")]
    Custom(CustomToolCallOutputMessage),
    #[serde(rename = "tool_search_output")]
    ToolSearch(ToolSearchOutputMessage),
}

impl From<InjectionInput> for ClientToolOutput {
    fn from(input: InjectionInput) -> Self {
        match input {
            InjectionInput::Function(output) => Self::Function(output.into()),
            InjectionInput::Shell(output) => Self::Shell(output),
            InjectionInput::Custom(output) => Self::Custom(output),
            InjectionInput::ToolSearch(output) => Self::ToolSearch(output),
        }
    }
}

/// Reject unmodeled fields rather than silently dropping returned continuation input.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InjectionFunctionOutput {
    pub call_id: String,
    pub output: ToolCallOutput,
}

impl From<InjectionFunctionOutput> for FunctionToolResultMessage {
    fn from(output: InjectionFunctionOutput) -> Self {
        Self {
            call_id: output.call_id,
            output: output.output,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponseInjectRequest {
    pub response_id: String,
    pub input: Vec<InjectionInput>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionFailureCode {
    ResponseNotFound,
    ResponseAlreadyCompleted,
    InvalidInput,
}

#[derive(Debug, Serialize)]
pub struct InjectionFailure {
    pub code: InjectionFailureCode,
    pub message: String,
}

/// Sequence and lane are assigned by delivery, never by the caller.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum InjectionEvent {
    #[serde(rename = "response.inject.created")]
    Created { response_id: String },
    #[serde(rename = "response.inject.failed")]
    Failed {
        response_id: String,
        input: Vec<InjectionInput>,
        error: InjectionFailure,
    },
}
