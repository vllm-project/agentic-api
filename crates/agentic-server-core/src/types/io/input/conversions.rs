use crate::types::event::MessageStatus;
use crate::types::io::{
    InOutItem, McpCall, OutputItem, OutputMessage, OutputMessageContent, ReasoningTextContent, ShellCallOutputMessage,
    ShellCallStatus,
};
use crate::types::tools::ResponsesTool;
use crate::utils::common::stable_name_hash;
use serde_json::Value;

use super::{
    CustomToolCallOutputMessage, FunctionToolResultMessage, InputContent, InputFunctionToolCall, InputItem,
    InputMessage, InputMessageContent, InputMessageType, InputTextContent, InputValidationError, ResponsesInput,
};
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
            // Reuse the existing user-message conversion for text.
            text @ Self::Text(_) => Vec::<InputItem>::from(text).into_iter().map(InOutItem::Input).collect(),
        }
    }

    /// The model-facing copy of canonical history; the public history is unchanged. Every tool reaches the model as a
    /// function tool (MCP, web search and code interpreter records as a call and its output, MCP with the discovery
    /// naming rule), message files become `input_text` parts and images without a detail get `auto`. Reasoning keeps
    /// only its plaintext, joined into one part: vLLM's generic conversion reads the first part and its Harmony
    /// conversion joins them all, and neither can use a summary or provider-encrypted state, so a summary-only item
    /// is dropped.
    ///
    /// # Errors
    /// Returns a file that cannot be converted to text, or a reasoning item with only encrypted state. Positions refer
    /// to the model-facing copy; [`Self::validate`] reports a file failure with the request's positions and runs
    /// first on typed paths.
    pub(crate) fn normalized_model_input(&self, tools: &[ResponsesTool]) -> Result<Self, InputValidationError> {
        let input = self.model_input().into_owned();
        let Self::Items(_) = input else {
            return Ok(input);
        };
        let mut model = Vec::new();
        for item in Vec::<InputItem>::from(input) {
            match item {
                InputItem::McpCall(call) => model.extend(self.mcp_call_input(call, tools)),
                InputItem::WebSearchCall(call) => model.extend([
                    InputItem::FunctionCall((&call).into()),
                    InputItem::FunctionCallOutput((&call).into()),
                ]),
                InputItem::CodeInterpreterCall(call) => model.extend([
                    InputItem::FunctionCall((&call).into()),
                    InputItem::FunctionCallOutput((&call).into()),
                ]),
                InputItem::CustomToolCall(call) => model.push(InputItem::FunctionCall(call.into())),
                InputItem::CustomToolCallOutput(output) => model.push(InputItem::FunctionCallOutput(output.into())),
                InputItem::ShellCall(call) => model.push(InputItem::FunctionCall(call.into())),
                InputItem::ShellCallOutput(output) => model.push(InputItem::FunctionCallOutput(output.into())),
                InputItem::Reasoning(mut reasoning) => {
                    if reasoning.content.iter().all(|content| content.text.is_empty()) {
                        if reasoning.encrypted_content.is_some() {
                            return Err(InputValidationError::encrypted_reasoning(model.len()));
                        }
                        continue;
                    }
                    let text = reasoning.content.iter().map(|content| content.text.as_str());
                    reasoning.content = vec![ReasoningTextContent::new(text.collect::<Vec<_>>().join("\n"))];
                    reasoning.summary.clear();
                    reasoning.encrypted_content = None;
                    model.push(InputItem::Reasoning(reasoning));
                }
                InputItem::Message(mut message) => {
                    if let InputMessageContent::Parts(parts) = &mut message.content {
                        for (part_index, part) in parts.iter_mut().enumerate() {
                            match part {
                                InputContent::InputImage(image) => {
                                    image.detail.get_or_insert_with(|| "auto".to_owned());
                                }
                                InputContent::InputFile(file) => {
                                    let text = InputTextContent::try_from(&*file).map_err(|reason| {
                                        InputValidationError::unsupported_file(model.len(), part_index, reason)
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
                    model.push(InputItem::Message(message));
                }
                other => model.push(other),
            }
        }
        Ok(Self::from(model))
    }
}

/// Appends history items, as [`ResponsesInput::push`] does.
impl Extend<InputItem> for ResponsesInput {
    fn extend<I: IntoIterator<Item = InputItem>>(&mut self, items: I) {
        for item in items {
            self.push(item);
        }
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
        let ResponsesInput::Items(model_items) = &model else {
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
        assert_eq!(
            serialize_to_value(&model).unwrap()[0]["content"],
            json!([{"type":"input_text", "text":"before"}, {"type":"input_text", "text":"File: notes.txt\nhi"},
                {"type":"input_text", "text":"after"}])
        );
        let canonical = serialize_to_value(&ResponsesInput::from(input.clone().into_canonical_items())).unwrap();
        assert_eq!(canonical[0]["content"][1], file);
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

    fn reasoning_item(content: &[&str], encrypted_content: Option<&str>) -> InputItem {
        InputItem::Reasoning(crate::types::io::ReasoningOutput {
            agent: None,
            id: "rs_prior".to_owned(),
            content: content.iter().map(|text| ReasoningTextContent::new(*text)).collect(),
            summary: vec![crate::types::ReasoningSummaryContent::new("public summary")],
            encrypted_content: encrypted_content
                .map(|text| crate::types::OpaqueReasoning::try_from(text.to_owned()).unwrap()),
            status: Some(crate::types::ReasoningStatus::Completed),
        })
    }

    #[test]
    fn plaintext_reasoning_is_normalized_for_both_vllm_paths() {
        let input = ResponsesInput::from(vec![reasoning_item(
            &["first continuation part", "second continuation part"],
            Some("opaque-provider-state"),
        )]);

        let model = input
            .normalized_model_input(&[])
            .expect("plaintext reasoning is replayable");

        assert!(
            matches!(&input, ResponsesInput::Items(items) if items.len() == 1),
            "public history is unchanged"
        );
        let ResponsesInput::Items(items) = model else {
            panic!("expected structured input");
        };
        let InOutItem::Input(InputItem::Reasoning(reasoning)) = &items[0] else {
            panic!("expected reasoning item");
        };
        assert_eq!(reasoning.id, "rs_prior");
        assert_eq!(reasoning.content.len(), 1);
        assert_eq!(
            reasoning.content[0].text,
            "first continuation part\nsecond continuation part"
        );
        assert!(reasoning.summary.is_empty());
        assert_eq!(reasoning.status, Some(crate::types::ReasoningStatus::Completed));
        assert_eq!(reasoning.encrypted_content, None);
    }

    #[test]
    fn encrypted_reasoning_requires_nonempty_plaintext_content() {
        for content in [Vec::new(), vec![""], vec!["", ""]] {
            let input = ResponsesInput::from(vec![reasoning_item(&content, Some("opaque-provider-state"))]);

            let error = input
                .normalized_model_input(&[])
                .expect_err("encrypted-only reasoning must not reach vLLM");

            assert!(
                error
                    .to_string()
                    .contains("encrypted state without plaintext reasoning content")
            );
            assert!(!error.to_string().contains("opaque-provider-state"));
        }
    }

    #[test]
    fn plaintext_reasoning_with_null_encrypted_state_is_normalized_without_summary() {
        let input = ResponsesInput::from(vec![reasoning_item(&["plaintext continuation"], None)]);

        let model = input
            .normalized_model_input(&[])
            .expect("null encrypted state is valid");

        let ResponsesInput::Items(items) = model else {
            panic!("expected structured input");
        };
        let InOutItem::Input(InputItem::Reasoning(reasoning)) = &items[0] else {
            panic!("expected reasoning item");
        };
        assert_eq!(
            reasoning.content[0].type_,
            crate::types::ReasoningTextKind::ReasoningText
        );
        assert_eq!(reasoning.content[0].text, "plaintext continuation");
        assert!(reasoning.summary.is_empty());
        assert_eq!(reasoning.encrypted_content, None);
    }

    #[test]
    fn summary_only_reasoning_without_opaque_state_is_removed_from_vllm_copy() {
        for input in [
            r#"[{"type":"reasoning","summary":[{"type":"summary_text","text":"public summary"}]}]"#,
            r#"[{"type":"reasoning","summary":[{"type":"summary_text","text":"public summary"}],"encrypted_content":null}]"#,
        ] {
            let input: ResponsesInput = serde_json::from_str(input).unwrap();

            let model = input
                .normalized_model_input(&[])
                .expect("summary-only reasoning has no usable vLLM state");

            let ResponsesInput::Items(items) = model else {
                panic!("expected structured input");
            };
            assert!(
                items.is_empty(),
                "a reasoning summary must never be promoted to reasoning text"
            );
        }
    }
}
