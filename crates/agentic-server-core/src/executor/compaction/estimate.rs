use std::borrow::Borrow;

use crate::types::io::input::model_items;
use crate::types::io::{
    CodeInterpreterCallOutput, InputContent, InputFileContent, InputItem, InputMessageContent, ReasoningOutput,
    ReasoningStatus, ResponsesInput, ToolCallOutput, ToolOutputContent,
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
        let text_tokens = self.text_bytes / ESTIMATED_BYTES_PER_TOKEN
            + u64::from(!self.text_bytes.is_multiple_of(ESTIMATED_BYTES_PER_TOKEN));
        self.fixed_tokens.saturating_add(text_tokens)
    }

    fn add_message_content(&mut self, content: &InputMessageContent) {
        match content {
            InputMessageContent::Text(text) => self.add_text(text),
            InputMessageContent::Parts(parts) => {
                for part in parts {
                    match part {
                        InputContent::InputText(text)
                        | InputContent::OutputText(text)
                        | InputContent::ReasoningText(text) => {
                            self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                            self.add_text(&text.text);
                        }
                        InputContent::Refusal(refusal) => {
                            self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                            self.add_text(&refusal.refusal);
                        }
                        InputContent::InputImage(_) => self.add_tokens(ESTIMATED_IMAGE_TOKENS),
                        InputContent::InputFile(file) => self.add_file_content(file),
                        InputContent::Unknown(_) => self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS),
                    }
                }
            }
        }
    }

    fn add_file_content(&mut self, file: &InputFileContent) {
        self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
        self.add_optional_text(file.file_data.as_deref());
        self.add_optional_text(file.file_id.as_deref());
        self.add_optional_text(file.file_url.as_deref());
        self.add_optional_text(file.filename.as_deref());
        self.add_optional_text(file.detail.as_deref());
    }

    fn add_tool_definition(&mut self, tool: &ResponsesTool) {
        // Serialize only declarations, preserving nested schemas and extra fields without
        // copying image-bearing input. An unrepresentable declaration must not undercount.
        match serialize_to_value(tool) {
            Ok(value) => self.add_json_value(&value),
            Err(_) => self.add_tokens(u64::MAX),
        }
    }

    fn add_tool_call_output(&mut self, output: &ToolCallOutput) {
        match output {
            ToolCallOutput::Text(text) => self.add_text(text),
            ToolCallOutput::Content(parts) => {
                for part in parts {
                    match part {
                        ToolOutputContent::InputText(text) => {
                            self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                            self.add_text(&text.text);
                        }
                        ToolOutputContent::InputImage(_) => self.add_tokens(ESTIMATED_IMAGE_TOKENS),
                        ToolOutputContent::InputFile(file) => self.add_file_content(file),
                    }
                }
            }
        }
    }

    fn add_item(&mut self, item: &InputItem) {
        if !item.is_model_visible() {
            return;
        }
        self.add_tokens(ESTIMATED_ITEM_OVERHEAD_TOKENS);

        match item {
            InputItem::Message(message) => {
                self.add_optional_text(message.id.as_deref());
                self.add_text(&message.role);
                self.add_message_content(&message.content);
            }
            InputItem::FunctionCall(call) => {
                self.add_optional_text(call.id.as_deref());
                self.add_text(&call.call_id);
                self.add_text(&call.name);
                self.add_optional_text(call.namespace.as_deref());
                self.add_text(&call.arguments);
            }
            InputItem::FunctionCallOutput(output) => {
                self.add_text(&output.call_id);
                self.add_tool_call_output(&output.output);
            }
            InputItem::CodeInterpreterCall(call) => {
                self.add_text(&call.id);
                self.add_text(&call.container_id);
                self.add_text(&call.code);
                if let Some(outputs) = &call.outputs {
                    for output in outputs {
                        match output {
                            CodeInterpreterCallOutput::Logs { logs } => self.add_text(logs),
                            CodeInterpreterCallOutput::Image { .. } => self.add_tokens(ESTIMATED_IMAGE_TOKENS),
                        }
                    }
                }
            }
            InputItem::ToolSearchCall(call) => {
                self.add_text(&call.id);
                self.add_text(&call.call_id);
                self.add_json_value(&call.arguments);
            }
            InputItem::ToolSearchOutput(output) => {
                self.add_text(&output.call_id);
                for tool in &output.tools {
                    self.add_tool_definition(tool);
                }
            }
            InputItem::CustomToolCall(call) => {
                self.add_text(&call.id);
                self.add_text(&call.call_id);
                self.add_text(&call.name);
                self.add_text(&call.input);
            }
            InputItem::CustomToolCallOutput(output) => {
                self.add_text(&output.call_id);
                self.add_optional_text(output.name.as_deref());
                self.add_tool_call_output(&output.output);
            }
            InputItem::ShellCall(_) | InputItem::ShellCallOutput(_) => {
                // Shell items carry textual commands and outputs, without image payloads.
                match serialize_to_value(item) {
                    Ok(value) => self.add_json_value(&value),
                    Err(_) => self.add_tokens(u64::MAX),
                }
            }
            InputItem::Reasoning(reasoning) => self.add_reasoning(reasoning),
            InputItem::WebSearchCall(call) => {
                self.add_text(&call.id);
                if let Ok(action) = serialize_to_value(&call.action) {
                    self.add_json_value(&action);
                }
            }
            InputItem::McpCall(call) => {
                self.add_text(&call.id);
                self.add_text(&call.server_label);
                self.add_text(&call.name);
                self.add_text(&call.arguments);
                self.add_optional_text(call.output.as_deref());
            }
            InputItem::Compaction(compaction) => {
                // `model_input` presents the checkpoint as one assistant output-text message.
                self.add_text("assistant");
                self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
                self.add_text(&compaction.encrypted_content);
            }
            InputItem::MultiAgentCall(_)
            | InputItem::MultiAgentCallOutput(_)
            | InputItem::AgentMessage(_)
            | InputItem::McpListTools(_)
            | InputItem::CompactionTrigger => {}
        }
    }

    fn add_reasoning(&mut self, reasoning: &ReasoningOutput) {
        self.add_text(&reasoning.id);
        self.add_optional_text(reasoning.status.map(ReasoningStatus::as_str));
        for content in &reasoning.content {
            self.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
            self.add_text(&content.text);
        }
        for summary in &reasoning.summary {
            // Match the previous JSON estimate: one object, two strings,
            // and the literal field names, without reconstructing JSON.
            self.add_tokens(3 * ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS);
            self.add_text("type");
            self.add_text("summary_text");
            self.add_text("text");
            self.add_text(&summary.text);
        }
        if let Some(encrypted_content) = &reasoning.encrypted_content {
            self.add_tokens(ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS);
            self.add_text(encrypted_content.as_str());
        }
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
        ResponsesInput::Items(_) => InputTokenEstimate::from_items(input.input_items().collect()).total_tokens(),
    }
}

pub(crate) fn estimate_history_tokens(items: &[InputItem]) -> u64 {
    InputTokenEstimate::from_items(items.iter().collect()).total_tokens()
}

impl InputTokenEstimate {
    fn from_items<T: Borrow<InputItem>>(items: Vec<T>) -> Self {
        let mut estimate = Self::default();
        estimate.add_tokens(ESTIMATED_INPUT_OVERHEAD_TOKENS);
        for item in model_items(items) {
            estimate.add_item(item.borrow());
        }
        estimate
    }
}
