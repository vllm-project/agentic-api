use super::{InputItem, InputMessage, InputMessageContent, ResponsesInput};
use std::borrow::Cow;

impl ResponsesInput {
    /// Normalize client tool items only in the model-facing copy of canonical history.
    pub(crate) fn normalized_model_input(&self) -> Cow<'_, Self> {
        let input = self.model_input();
        if matches!(&*input, Self::Items(items) if items.iter().any(|item| matches!(item,
            InputItem::ShellCall(_) | InputItem::ShellCallOutput(_)
                | InputItem::CustomToolCall(_) | InputItem::CustomToolCallOutput(_)
        ))) {
            Cow::Owned(Self::Items(Vec::from(input.into_owned())))
        } else {
            input
        }
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
