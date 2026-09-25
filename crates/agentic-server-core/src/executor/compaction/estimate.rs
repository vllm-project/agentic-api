use crate::types::io::input::model_items;
use crate::types::io::{
    InputContent, InputFileContent, InputItem, InputMessageContent, ResponsesInput, ToolCallOutput, ToolOutputContent,
};
use crate::types::tools::ResponsesTool;
use crate::utils::common::serialize_to_value;

const ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const ESTIMATED_INPUT_OVERHEAD_TOKENS: u64 = 1;
const ESTIMATED_ITEM_OVERHEAD_TOKENS: u64 = 12;
const ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS: u64 = 7;
const ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS: u64 = 1;

/// Fixed model-agnostic allowance for one image, including its content-part framing.
///
/// Actual vision-token usage depends on the model, processor, image dimensions, and detail
/// setting. A conservative fixed budget avoids treating images as free without mistaking their
/// base64 transport encoding for model-visible text.
pub(super) const ESTIMATED_IMAGE_TOKENS: u64 = 1_024;

#[derive(Default)]
struct InputTokenEstimate {
    text_bytes: u64,
    fixed_tokens: u64,
}

impl InputTokenEstimate {
    fn add_text(&mut self, text: &str) {
        let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        self.text_bytes = self.text_bytes.saturating_add(bytes);
    }

    fn add_optional_text(&mut self, text: Option<&str>) {
        if let Some(text) = text {
            self.add_text(text);
        }
    }

    const fn add_tokens(&mut self, tokens: u64) {
        self.fixed_tokens = self.fixed_tokens.saturating_add(tokens);
    }

    fn add_json_value(&mut self, value: &serde_json::Value) {
        self.add_tokens(ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS);
        match value {
            serde_json::Value::String(text) => self.add_text(text),
            serde_json::Value::Number(number) => self.add_text(&number.to_string()),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.add_json_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    self.add_text(key);
                    self.add_json_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) => {}
        }
    }

    fn total_tokens(self) -> u64 {
        let text_tokens =
            self.text_bytes / ESTIMATED_BYTES_PER_TOKEN + u64::from(self.text_bytes % ESTIMATED_BYTES_PER_TOKEN != 0);
        self.fixed_tokens.saturating_add(text_tokens)
    }
}

fn add_message_content(estimate: &mut InputTokenEstimate, content: &InputMessageContent) {
    match content {
        InputMessageContent::Text(text) => estimate.add_text(text),
        InputMessageContent::Parts(parts) => {
            for part in parts {
                match part {
                    InputContent::InputText(text)
                    | InputContent::OutputText(text)
                    | InputContent::ReasoningText(text) => {
                        estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                        estimate.add_text(&text.text);
                    }
                    InputContent::Refusal(refusal) => {
                        estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                        estimate.add_text(&refusal.refusal);
                    }
                    InputContent::InputImage(_) => estimate.add_tokens(ESTIMATED_IMAGE_TOKENS),
                    InputContent::InputFile(file) => add_file_content(estimate, file),
                    InputContent::Unknown(_) => estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS),
                }
            }
        }
    }
}

fn add_file_content(estimate: &mut InputTokenEstimate, file: &InputFileContent) {
    estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
    estimate.add_optional_text(file.file_data.as_deref());
    estimate.add_optional_text(file.file_id.as_deref());
    estimate.add_optional_text(file.file_url.as_deref());
    estimate.add_optional_text(file.filename.as_deref());
    estimate.add_optional_text(file.detail.as_deref());
}

fn add_tool_definition(estimate: &mut InputTokenEstimate, tool: &ResponsesTool) {
    // Serialize only declarations, preserving nested schemas and extra fields without
    // copying image-bearing input. An unrepresentable declaration must not undercount.
    match serialize_to_value(tool) {
        Ok(value) => estimate.add_json_value(&value),
        Err(_) => estimate.add_tokens(u64::MAX),
    }
}

fn add_tool_call_output(estimate: &mut InputTokenEstimate, output: &ToolCallOutput) {
    match output {
        ToolCallOutput::Text(text) => estimate.add_text(text),
        ToolCallOutput::Content(parts) => {
            for part in parts {
                match part {
                    ToolOutputContent::InputText(text) => {
                        estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                        estimate.add_text(&text.text);
                    }
                    ToolOutputContent::InputImage(_) => estimate.add_tokens(ESTIMATED_IMAGE_TOKENS),
                    ToolOutputContent::InputFile(file) => add_file_content(estimate, file),
                }
            }
        }
    }
}

fn add_input_item(estimate: &mut InputTokenEstimate, item: &InputItem) {
    if !item.is_model_visible() {
        return;
    }
    estimate.add_tokens(ESTIMATED_ITEM_OVERHEAD_TOKENS);

    match item {
        InputItem::Message(message) => {
            estimate.add_optional_text(message.id.as_deref());
            estimate.add_text(&message.role);
            add_message_content(estimate, &message.content);
        }
        InputItem::FunctionCall(call) => {
            estimate.add_optional_text(call.id.as_deref());
            estimate.add_text(&call.call_id);
            estimate.add_text(&call.name);
            estimate.add_optional_text(call.namespace.as_deref());
            estimate.add_text(&call.arguments);
        }
        InputItem::FunctionCallOutput(output) => {
            estimate.add_text(&output.call_id);
            add_tool_call_output(estimate, &output.output);
        }
        InputItem::ToolSearchCall(call) => {
            estimate.add_text(&call.id);
            estimate.add_text(&call.call_id);
            estimate.add_json_value(&call.arguments);
        }
        InputItem::ToolSearchOutput(output) => {
            estimate.add_text(&output.call_id);
            for tool in &output.tools {
                add_tool_definition(estimate, tool);
            }
        }
        InputItem::CustomToolCall(call) => {
            estimate.add_text(&call.id);
            estimate.add_text(&call.call_id);
            estimate.add_text(&call.name);
            estimate.add_text(&call.input);
        }
        InputItem::CustomToolCallOutput(output) => {
            estimate.add_text(&output.call_id);
            estimate.add_optional_text(output.name.as_deref());
            add_tool_call_output(estimate, &output.output);
        }
        InputItem::ShellCall(_) | InputItem::ShellCallOutput(_) => {
            // Shell items carry textual commands and outputs, without image payloads.
            match serialize_to_value(item) {
                Ok(value) => estimate.add_json_value(&value),
                Err(_) => estimate.add_tokens(u64::MAX),
            }
        }
        InputItem::Reasoning(reasoning) => {
            estimate.add_text(&reasoning.id);
            estimate.add_optional_text(reasoning.status.as_deref());
            for content in &reasoning.content {
                estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                estimate.add_text(&content.text);
            }
            for summary in &reasoning.summary {
                estimate.add_json_value(summary);
            }
            if let Some(encrypted_content) = &reasoning.encrypted_content {
                estimate.add_json_value(encrypted_content);
            }
        }
        InputItem::Compaction(compaction) => {
            // `model_input` presents the checkpoint as one assistant output-text message.
            estimate.add_text("assistant");
            estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
            estimate.add_text(&compaction.encrypted_content);
        }
        InputItem::MultiAgentCall(_)
        | InputItem::MultiAgentCallOutput(_)
        | InputItem::AgentMessage(_)
        | InputItem::Unknown
        | InputItem::McpListTools(_)
        | InputItem::CompactionTrigger => {}
    }
}

/// Estimate the current model-facing context size without requiring a model-specific tokenizer.
///
/// Textual fields are aggregated at four UTF-8 bytes per token, with fixed allowances for
/// Responses framing. Images receive [`ESTIMATED_IMAGE_TOKENS`] each; their URLs and inline bytes
/// are deliberately excluded because vision-token usage is unrelated to base64 transport size.
#[must_use]
pub(crate) fn estimate_input_tokens(input: &ResponsesInput) -> u64 {
    match input {
        ResponsesInput::Text(text) => {
            let mut estimate = InputTokenEstimate::default();
            estimate.add_tokens(ESTIMATED_INPUT_OVERHEAD_TOKENS);
            estimate.add_text(text);
            estimate.total_tokens()
        }
        ResponsesInput::Items(items) => estimate_history_tokens(items),
    }
}

pub(crate) fn estimate_history_tokens(items: &[InputItem]) -> u64 {
    let mut estimate = InputTokenEstimate::default();
    estimate.add_tokens(ESTIMATED_INPUT_OVERHEAD_TOKENS);
    for item in model_items(items) {
        add_input_item(&mut estimate, item);
    }
    estimate.total_tokens()
}
