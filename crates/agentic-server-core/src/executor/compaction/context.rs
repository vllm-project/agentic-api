//! Identify whether resolved input contains context worth compacting.

use crate::types::io::{InputContent, InputItem, InputMessageContent};

fn value_has_content(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(text) => !text.trim().is_empty(),
        serde_json::Value::Array(values) => values.iter().any(value_has_content),
        serde_json::Value::Object(values) => values.values().any(value_has_content),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}

pub(super) fn item_has_meaningful_context(item: &InputItem) -> bool {
    match item {
        InputItem::Message(message) => match &message.content {
            InputMessageContent::Text(text) => !text.trim().is_empty(),
            InputMessageContent::Parts(parts) => parts.iter().any(|part| match part {
                InputContent::InputText(text) | InputContent::OutputText(text) | InputContent::ReasoningText(text) => {
                    !text.text.trim().is_empty()
                }
                // An image is context whether it is inline or a file reference.
                InputContent::InputImage(image) => [image.image_url.as_deref(), image.file_id.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|reference| !reference.trim().is_empty()),
                InputContent::Refusal(refusal) => !refusal.refusal.trim().is_empty(),
                // Message files and unmodeled parts are rejected during typed input validation.
                InputContent::InputFile(_) | InputContent::Unknown(_) => false,
            }),
        },
        InputItem::FunctionCall(call) => !call.name.trim().is_empty() || !call.arguments.trim().is_empty(),
        InputItem::FunctionCallOutput(output) => output.output.has_content(),
        InputItem::ToolSearchCall(call) => !call.call_id.trim().is_empty() || value_has_content(&call.arguments),
        InputItem::ToolSearchOutput(output) => !output.call_id.trim().is_empty() || !output.tools.is_empty(),
        InputItem::CustomToolCall(call) => !call.name.trim().is_empty() || !call.input.trim().is_empty(),
        InputItem::CustomToolCallOutput(output) => output.output.has_content(),
        InputItem::ShellCall(call) => !call.action.commands.is_empty(),
        InputItem::ShellCallOutput(output) => !output.output.is_empty(),
        InputItem::Reasoning(reasoning) => {
            reasoning.content.iter().any(|content| !content.text.trim().is_empty())
                || reasoning.summary.iter().any(value_has_content)
                || reasoning.encrypted_content.as_ref().is_some_and(value_has_content)
        }
        InputItem::Compaction(compaction) => !compaction.encrypted_content.trim().is_empty(),
        InputItem::MultiAgentCall(_)
        | InputItem::MultiAgentCallOutput(_)
        | InputItem::AgentMessage(_)
        | InputItem::McpListTools(_)
        | InputItem::CompactionTrigger
        | InputItem::Unknown => false,
    }
}
