use crate::types::event::MessageStatus;
use crate::types::io::{
    InOutItem, McpCall, OutputItem, OutputMessage, OutputMessageContent, ShellCallOutputMessage, ShellCallStatus,
};
use crate::types::tools::ResponsesTool;
use crate::utils::common::stable_name_hash;
use serde_json::Value;

use super::{
    CustomToolCallOutputMessage, FunctionToolResultMessage, InputContent, InputFunctionToolCall, InputItem,
    InputMessage, InputMessageContent, InputMessageType, InputTextContent, InputValidationError, ResponsesInput,
};
use std::borrow::Cow;
use std::collections::HashMap;

/// The existing discovery naming rule, also used when projecting returned MCP history.
pub(crate) fn internal_mcp_tool_name(
    server_label: &str,
    tool_name: &str,
    used: &mut HashMap<String, (String, String)>,
) -> String {
    const MAX_NAME_BYTES: usize = 64;
    let identity = (server_label.to_owned(), tool_name.to_owned());
    let base: String = format!("mcp__{server_label}__{tool_name}")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if base.len() <= MAX_NAME_BYTES && used.get(&base).is_none_or(|existing| existing == &identity) {
        used.insert(base.clone(), identity);
        return base;
    }
    let mut attempt = 0_u32;
    loop {
        let source = if attempt == 0 {
            format!("{server_label}:{tool_name}")
        } else {
            format!("{server_label}:{tool_name}:{attempt}")
        };
        let suffix = format!("__{:010x}", stable_name_hash(&source) & 0xff_ffff_ffff);
        let prefix_len = MAX_NAME_BYTES.saturating_sub(suffix.len());
        let candidate = format!("{}{}", &base[..base.len().min(prefix_len)], suffix);
        if used.get(&candidate).is_none_or(|existing| existing == &identity) {
            used.insert(candidate.clone(), identity);
            return candidate;
        }
        attempt = attempt.saturating_add(1);
    }
}

impl From<ShellCallOutputMessage> for FunctionToolResultMessage {
    fn from(output: ShellCallOutputMessage) -> Self {
        Self {
            id: output.id,
            status: match output.status {
                Some(ShellCallStatus::Completed) => Some(MessageStatus::Completed),
                Some(ShellCallStatus::InProgress) => Some(MessageStatus::InProgress),
                Some(ShellCallStatus::Incomplete) | None => None,
            },
            call_id: output.call_id,
            // Command outputs contain only JSON-compatible values and string map keys.
            output: serde_json::to_string(&output.output)
                .expect("shell outputs serialize to JSON")
                .into(),
        }
    }
}

impl From<CustomToolCallOutputMessage> for FunctionToolResultMessage {
    fn from(output: CustomToolCallOutputMessage) -> Self {
        Self {
            id: output.id,
            status: output.status,
            call_id: output.call_id,
            output: output.output,
        }
    }
}

impl From<OutputMessageContent> for InputContent {
    fn from(part: OutputMessageContent) -> Self {
        match part {
            OutputMessageContent::InputText(part) => Self::InputText(part),
            OutputMessageContent::OutputText(part) => {
                let mut text = InputTextContent::new(part.text);
                if let Some(annotations) = part.annotations {
                    text.extra.insert("annotations".into(), Value::Array(annotations));
                }
                // These fixed structs contain only JSON-compatible values and string keys.
                text.extra.insert(
                    "logprobs".into(),
                    serde_json::to_value(part.logprobs).expect("text log probabilities serialize to JSON"),
                );
                Self::OutputText(text)
            }
        }
    }
}

impl From<OutputMessage> for InputMessage {
    fn from(msg: OutputMessage) -> Self {
        let parts = msg.content.into_iter().map(InputContent::from).collect();
        Self {
            id: Some(msg.id),
            role: msg.role,
            phase: msg.phase,
            agent: msg.agent,
            status: Some(msg.status),
            content: InputMessageContent::Parts(parts),
            kind: InputMessageType::Message,
        }
    }
}

impl ResponsesInput {
    /// Project a returned MCP result using its current binding, or retained discovery name.
    fn mcp_call_input(&self, call: McpCall, tools: &[ResponsesTool]) -> [InputItem; 2] {
        let mut used = HashMap::new();
        let mut resolved = tools.iter().find_map(|tool| match tool {
            ResponsesTool::Mcp(param) if param.server_label == call.server_label => param
                .discovered_tools
                .iter()
                .find(|tool| tool.tool_name == call.name)
                .map(|tool| tool.internal_name.clone()),
            _ => None,
        });
        if resolved.is_none() {
            let listing = match self {
                Self::Items(items) => items.iter().rev().find_map(|item| match item {
                    InOutItem::Input(InputItem::McpListTools(listing))
                    | InOutItem::Output(OutputItem::McpListTools(listing))
                        if listing.server_label == call.server_label && listing.error.is_none() =>
                    {
                        Some(listing)
                    }
                    _ => None,
                }),
                Self::Text(_) => None,
            };
            if let Some(listing) = listing {
                for tool in &listing.tools {
                    let name = internal_mcp_tool_name(&call.server_label, &tool.name, &mut used);
                    if tool.name == call.name {
                        resolved = Some(name);
                        break;
                    }
                }
            }
        }
        let name = resolved.unwrap_or_else(|| internal_mcp_tool_name(&call.server_label, &call.name, &mut used));
        let output = call.output.unwrap_or_else(|| match &call.error {
            // The typed error contains only JSON-compatible data.
            Some(error) => serde_json::json!({"error": error}).to_string(),
            None => String::new(),
        });
        [
            InputItem::FunctionCall(InputFunctionToolCall {
                id: None,
                agent: call.agent,
                call_id: call.id.clone(),
                name,
                namespace: None,
                arguments: call.arguments,
                status: Some(MessageStatus::Completed),
                async_execution: false,
            }),
            InputItem::FunctionCallOutput(FunctionToolResultMessage {
                id: None,
                status: Some(MessageStatus::Completed),
                call_id: call.id,
                output: output.into(),
            }),
        ]
    }
    /// Append a history item, preserving shorthand text as the first user message.
    pub(crate) fn push(&mut self, item: impl Into<InOutItem>) {
        match self {
            Self::Items(items) => items.push(item.into()),
            Self::Text(_) => {
                let mut items = std::mem::take(self).into_canonical_items();
                items.push(item.into());
                *self = Self::Items(items);
            }
        }
    }

    /// Move request input into canonical items without normalizing tool types,
    /// removing items, or applying model-specific content defaults.
    /// Text becomes one user message, including when the text is empty.
    /// Submitted tool results default to completed before persistence and model
    /// lowering. This describes delivery of the result, even if its payload
    /// reports an error; explicit statuses and call execution states are retained.
    #[must_use]
    pub fn into_canonical_items(self) -> Vec<InOutItem> {
        match self {
            Self::Items(mut items) => {
                for item in &mut items {
                    match item {
                        InOutItem::Input(InputItem::FunctionCallOutput(output)) => {
                            output.status.get_or_insert(MessageStatus::Completed);
                        }
                        InOutItem::Input(InputItem::CustomToolCallOutput(output)) => {
                            output.status.get_or_insert(MessageStatus::Completed);
                        }
                        InOutItem::Input(InputItem::ShellCallOutput(output)) => {
                            output.status.get_or_insert(ShellCallStatus::Completed);
                        }
                        _ => {}
                    }
                }
                items
            }
            // Reuse the existing user-message conversion only for text. Its
            // item-array branch also lowers custom/shell items for inference.
            text @ Self::Text(_) => Vec::<InputItem>::from(text).into_iter().map(InOutItem::Input).collect(),
        }
    }

    /// Normalize tool items only in the model-facing copy of canonical history.
    /// Returned MCP records become function-call/result pairs using the same
    /// discovery naming rule, while their public history remains unchanged.
    ///
    /// Message files become `input_text` parts in place through
    /// `InputTextContent::try_from(&InputFileContent)`; the canonical history
    /// keeps the client's `input_file`.
    ///
    /// # Errors
    /// Returns a file that cannot be converted to text. Positions refer to the
    /// model-facing copy; [`Self::validate`] reports the same failure with the
    /// request's positions and runs first on typed paths.
    pub(crate) fn normalized_model_input(
        &self,
        tools: &[ResponsesTool],
    ) -> Result<Cow<'_, Self>, InputValidationError> {
        let input = self.model_input();
        let has_tools_to_normalize = matches!(&*input, Self::Items(items) if items.iter().any(|item| matches!(item,
            InOutItem::Input(InputItem::ShellCall(_) | InputItem::ShellCallOutput(_)
                | InputItem::CustomToolCall(_) | InputItem::CustomToolCallOutput(_) | InputItem::McpCall(_))
        )));
        let has_parts_to_normalize = matches!(&*input, Self::Items(items) if items.iter().any(|item| matches!(item,
            InOutItem::Input(InputItem::Message(message)) if matches!(&message.content,
                InputMessageContent::Parts(parts) if parts.iter().any(|part| matches!(part,
                    InputContent::InputImage(image) if image.detail.is_none())
                    || matches!(part, InputContent::InputFile(_)))))));
        if !has_tools_to_normalize && !has_parts_to_normalize {
            return Ok(input);
        }
        let mut model = if has_tools_to_normalize {
            let items = Vec::<InputItem>::from(input.into_owned());
            let mut normalized = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    InputItem::McpCall(call) => normalized.extend(self.mcp_call_input(call, tools)),
                    other => normalized.push(other),
                }
            }
            Self::from(normalized)
        } else {
            input.into_owned()
        };
        if let Self::Items(items) = &mut model {
            for (item_index, item) in items.iter_mut().enumerate() {
                if let InOutItem::Input(InputItem::Message(message)) = item
                    && let InputMessageContent::Parts(parts) = &mut message.content
                {
                    for (part_index, part) in parts.iter_mut().enumerate() {
                        match part {
                            InputContent::InputImage(image) => {
                                image.detail.get_or_insert_with(|| "auto".to_owned());
                            }
                            InputContent::InputFile(file) => {
                                let text = InputTextContent::try_from(&*file).map_err(|reason| {
                                    InputValidationError::unsupported_file(item_index, part_index, reason)
                                })?;
                                *part = InputContent::InputText(text);
                            }
                            InputContent::InputText(_)
                            | InputContent::OutputText(_)
                            | InputContent::Refusal(_)
                            | InputContent::ReasoningText(_)
                            | InputContent::Unknown(_) => {}
                        }
                    }
                }
            }
        }
        Ok(Cow::Owned(model))
    }
}

impl From<Vec<InputItem>> for ResponsesInput {
    fn from(items: Vec<InputItem>) -> Self {
        Self::Items(items.into_iter().map(InOutItem::Input).collect())
    }
}

impl From<Vec<InOutItem>> for ResponsesInput {
    fn from(items: Vec<InOutItem>) -> Self {
        Self::Items(items)
    }
}

impl From<&ResponsesInput> for Vec<InputItem> {
    fn from(input: &ResponsesInput) -> Self {
        Self::from(input.clone())
    }
}

impl From<ResponsesInput> for Vec<InputItem> {
    fn from(input: ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                role: "user".into(),
                content: InputMessageContent::Text(text),
                ..Default::default()
            })],
            ResponsesInput::Items(items) => InOutItem::into_input_items(items),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::OutputItem;
    use crate::utils::common::{deserialize_from_value, serialize_to_value};
    use serde_json::json;

    #[test]
    fn canonical_results_default_to_completed_even_when_their_payload_reports_failure() {
        for wire in [
            json!({"type":"function_call_output", "call_id":"call_1", "output":"error: lookup failed"}),
            json!({"type":"custom_tool_call_output", "call_id":"call_1", "output":"error: lookup failed"}),
            json!({"type":"shell_call_output", "call_id":"call_1", "output":[{"stdout":"", "stderr":"failed", "outcome":{"type":"exit", "exit_code":1}}]}),
        ] {
            let item: InOutItem = serde_json::from_value(wire.clone()).unwrap();
            let items = ResponsesInput::Items(vec![item]).into_canonical_items();
            let public = serde_json::to_value(&items[0]).unwrap();
            assert_eq!(public["status"], "completed");
            assert_eq!(public["output"], wire["output"]);
        }
    }

    #[test]
    fn canonical_items_preserve_custom_shell_and_unknown_items() {
        let mut items: Vec<InOutItem> = deserialize_from_value(json!([
            {"type":"custom_tool_call", "id":"ctc_notes", "call_id":"notes", "name":"record_notes", "input":"note"},
            {"type":"custom_tool_call_output", "call_id":"notes", "output":"recorded", "status":"in_progress"},
            {"type":"shell_call", "id":"sh_test", "call_id":"shell", "action":{
                "commands":["pwd"], "timeout_ms":1000, "max_output_length":1024
            }},
            {"type":"shell_call_output", "call_id":"shell", "status":"incomplete", "output":[{
                "stdout":"/tmp", "stderr":"", "outcome":{"type":"exit", "exit_code":0}
            }]}
        ]))
        .unwrap();
        items.push(InOutItem::Output(OutputItem::Unknown));
        let input = ResponsesInput::from(items);
        let expected = serialize_to_value(&input).unwrap();
        let items = input.into_canonical_items();
        assert!(matches!(&items[0], InOutItem::Input(InputItem::CustomToolCall(_))));
        assert!(matches!(
            &items[1],
            InOutItem::Input(InputItem::CustomToolCallOutput(_))
        ));
        assert!(matches!(&items[2], InOutItem::Input(InputItem::ShellCall(_))));
        assert!(matches!(&items[3], InOutItem::Input(InputItem::ShellCallOutput(_))));
        assert!(matches!(&items[4], InOutItem::Output(OutputItem::Unknown)));
        assert_eq!(serialize_to_value(&ResponsesInput::from(items)).unwrap(), expected);
    }

    #[test]
    fn canonical_text_preserves_empty_and_unicode_content() {
        for text in ["", "é🙂"] {
            let items = ResponsesInput::Text(text.to_owned()).into_canonical_items();
            let [InOutItem::Input(InputItem::Message(message))] = items.as_slice() else {
                panic!("one message")
            };
            assert_eq!(message.role, "user");
            assert!(matches!(&message.content, InputMessageContent::Text(content) if content == text));
        }
        assert!(ResponsesInput::Items(Vec::new()).into_canonical_items().is_empty());
    }

    #[test]
    fn canonical_items_keep_media_defaults_separate_from_model_normalization() {
        let input: ResponsesInput = deserialize_from_value(json!([
            {"type":"custom_tool_call_output", "call_id":"notes", "output":"recorded", "status":"in_progress"},
            {"role":"user", "content":[{"type":"input_image", "image_url":"data:image/png;base64,test"}]}
        ]))
        .unwrap();
        let canonical = input.clone().into_canonical_items();
        assert!(matches!(
            &canonical[0],
            InOutItem::Input(InputItem::CustomToolCallOutput(_))
        ));
        let InOutItem::Input(InputItem::Message(message)) = &canonical[1] else {
            panic!("message")
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("parts")
        };
        assert!(matches!(&parts[0], InputContent::InputImage(image) if image.detail.is_none()));

        let model = input.normalized_model_input(&[]).unwrap();
        let ResponsesInput::Items(model_items) = &*model else {
            panic!("model items")
        };
        assert!(matches!(
            &model_items[0],
            InOutItem::Input(InputItem::FunctionCallOutput(_))
        ));
        let InOutItem::Input(InputItem::Message(message)) = &model_items[1] else {
            panic!("message")
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("parts")
        };
        assert!(matches!(&parts[0], InputContent::InputImage(image) if image.detail.as_deref() == Some("auto")));
        assert_eq!(
            serialize_to_value(&input).unwrap(),
            serialize_to_value(&ResponsesInput::from(canonical)).unwrap()
        );
    }

    #[test]
    fn inline_text_files_are_inlined_only_in_the_model_copy() {
        let file = json!({"type":"input_file", "file_data":"data:text/plain;base64,aGk=", "filename":"notes.txt"});
        let input: ResponsesInput = deserialize_from_value(json!([
            {"role":"user", "content":[{"type":"input_text", "text":"before"}, file.clone(),
                {"type":"input_text", "text":"after"}]}
        ]))
        .unwrap();
        let model = input.normalized_model_input(&[]).unwrap();
        assert!(matches!(model, Cow::Owned(_)));
        assert_eq!(
            serialize_to_value(&*model).unwrap()[0]["content"],
            json!([{"type":"input_text", "text":"before"}, {"type":"input_text", "text":"File: notes.txt\nhi"},
                {"type":"input_text", "text":"after"}])
        );
        let canonical = serialize_to_value(&ResponsesInput::from(input.clone().into_canonical_items())).unwrap();
        assert_eq!(canonical[0]["content"][1], file);

        let plain: ResponsesInput = deserialize_from_value(json!([
            {"role":"user", "content":[{"type":"input_text", "text":"hello"}]}
        ]))
        .unwrap();
        assert!(matches!(plain.normalized_model_input(&[]).unwrap(), Cow::Borrowed(_)));
    }

    #[test]
    fn unconvertible_files_fail_normalization_instead_of_reaching_the_model() {
        let input: ResponsesInput = deserialize_from_value(json!([
            {"role":"user", "content":[{"type":"input_text", "text":"read"},
                {"type":"input_file", "file_data":"data:application/pdf;base64,JVBERi0=", "filename":"a.pdf"}]}
        ]))
        .unwrap();
        let error = input.normalized_model_input(&[]).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("input[0].content[1]: input_file content is not supported"),
            "{error}"
        );
    }
}
