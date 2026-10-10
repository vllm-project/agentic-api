use super::{InputContent, InputItem, InputMessage, InputMessageContent, ResponsesInput};
use std::borrow::Cow;

impl ResponsesInput {
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
