use super::{InputContent, InputItem, InputMessage, InputMessageContent, ResponsesInput};
use std::borrow::Cow;

impl ResponsesInput {
    /// Move request input into canonical items without normalizing tool types,
    /// removing items, or applying model-specific content defaults.
    /// Text becomes one user message, including when the text is empty.
    #[must_use]
    pub fn into_canonical_items(self) -> Vec<InputItem> {
        match self {
            Self::Items(items) => items,
            // Reuse the existing user-message conversion only for text. Its
            // item-array branch also lowers custom/shell items for inference.
            text @ Self::Text(_) => Vec::from(text),
        }
    }

    /// Normalize client tool items only in the model-facing copy of canonical history.
    pub(crate) fn normalized_model_input(&self) -> Cow<'_, Self> {
        let input = self.model_input();
        let has_client_tools = matches!(&*input, Self::Items(items) if items.iter().any(|item| matches!(item,
            InputItem::ShellCall(_) | InputItem::ShellCallOutput(_)
                | InputItem::CustomToolCall(_) | InputItem::CustomToolCallOutput(_)
        )));
        let has_image_without_detail = matches!(&*input, Self::Items(items) if items.iter().any(|item| matches!(item,
            InputItem::Message(message) if matches!(&message.content,
                InputMessageContent::Parts(parts) if parts.iter().any(|part| matches!(part,
                    InputContent::InputImage(image) if image.detail.is_none()))))));
        if !has_client_tools && !has_image_without_detail {
            return input;
        }
        let mut model = if has_client_tools {
            Self::Items(Vec::from(input.into_owned()))
        } else {
            input.into_owned()
        };
        if let Self::Items(items) = &mut model {
            for item in items {
                if let InputItem::Message(message) = item
                    && let InputMessageContent::Parts(parts) = &mut message.content
                {
                    for part in parts {
                        if let InputContent::InputImage(image) = part {
                            image.detail.get_or_insert_with(|| "auto".to_owned());
                        }
                    }
                }
            }
        }
        Cow::Owned(model)
    }
}

impl From<&ResponsesInput> for Vec<InputItem> {
    fn from(input: &ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                role: "user".into(),
                content: InputMessageContent::Text(text.clone()),
                ..Default::default()
            })],
            ResponsesInput::Items(items) => items
                .iter()
                .filter_map(|item| match item {
                    InputItem::Unknown => None,
                    InputItem::ShellCall(call) => Some(InputItem::FunctionCall(call.clone().into())),
                    InputItem::ShellCallOutput(output) => Some(InputItem::FunctionCallOutput(output.clone().into())),
                    InputItem::CustomToolCall(call) => Some(InputItem::FunctionCall(call.clone().into())),
                    InputItem::CustomToolCallOutput(output) => {
                        Some(InputItem::FunctionCallOutput(output.clone().into()))
                    }
                    item => Some(item.clone()),
                })
                .collect(),
        }
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
            ResponsesInput::Items(items) => items
                .into_iter()
                .filter_map(|item| match item {
                    InputItem::Unknown => None,
                    InputItem::ShellCall(call) => Some(InputItem::FunctionCall(call.into())),
                    InputItem::ShellCallOutput(output) => Some(InputItem::FunctionCallOutput(output.into())),
                    InputItem::CustomToolCall(call) => Some(InputItem::FunctionCall(call.into())),
                    InputItem::CustomToolCallOutput(output) => Some(InputItem::FunctionCallOutput(output.into())),
                    item => Some(item),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::common::{deserialize_from_value, serialize_to_value};
    use serde_json::json;

    #[test]
    fn canonical_items_preserve_custom_shell_and_unknown_items() {
        let input: ResponsesInput = deserialize_from_value(json!([
            {"type":"custom_tool_call", "id":"ctc_notes", "call_id":"notes", "name":"record_notes", "input":"note"},
            {"type":"custom_tool_call_output", "call_id":"notes", "output":"recorded"},
            {"type":"shell_call", "id":"sh_test", "call_id":"shell", "action":{
                "commands":["pwd"], "timeout_ms":1000, "max_output_length":1024
            }},
            {"type":"shell_call_output", "call_id":"shell", "output":[{
                "stdout":"/tmp", "stderr":"", "outcome":{"type":"exit", "exit_code":0}
            }]},
            {"type":"unrecognized"}
        ]))
        .unwrap();
        let expected = serialize_to_value(&input).unwrap();
        let items = input.into_canonical_items();
        assert!(matches!(&items[0], InputItem::CustomToolCall(_)));
        assert!(matches!(&items[1], InputItem::CustomToolCallOutput(_)));
        assert!(matches!(&items[2], InputItem::ShellCall(_)));
        assert!(matches!(&items[3], InputItem::ShellCallOutput(_)));
        assert!(matches!(&items[4], InputItem::Unknown));
        assert_eq!(serialize_to_value(&ResponsesInput::Items(items)).unwrap(), expected);
    }

    #[test]
    fn canonical_text_preserves_empty_and_unicode_content() {
        for text in ["", "é🙂"] {
            let items = ResponsesInput::Text(text.to_owned()).into_canonical_items();
            let [InputItem::Message(message)] = items.as_slice() else {
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
            {"type":"custom_tool_call_output", "call_id":"notes", "output":"recorded"},
            {"role":"user", "content":[{"type":"input_image", "image_url":"data:image/png;base64,test"}]}
        ]))
        .unwrap();
        let canonical = input.clone().into_canonical_items();
        assert!(matches!(&canonical[0], InputItem::CustomToolCallOutput(_)));
        let InputItem::Message(message) = &canonical[1] else {
            panic!("message")
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("parts")
        };
        assert!(matches!(&parts[0], InputContent::InputImage(image) if image.detail.is_none()));

        let model = input.normalized_model_input();
        let ResponsesInput::Items(model_items) = &*model else {
            panic!("model items")
        };
        assert!(matches!(&model_items[0], InputItem::FunctionCallOutput(_)));
        let InputItem::Message(message) = &model_items[1] else {
            panic!("message")
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            panic!("parts")
        };
        assert!(matches!(&parts[0], InputContent::InputImage(image) if image.detail.as_deref() == Some("auto")));
        assert_eq!(
            serialize_to_value(&input).unwrap(),
            serialize_to_value(&ResponsesInput::Items(canonical)).unwrap()
        );
    }
}
