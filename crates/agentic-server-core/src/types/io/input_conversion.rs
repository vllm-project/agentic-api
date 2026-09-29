//! Convert Responses input to the model-facing item sequence.

use super::{InputItem, InputMessage, InputMessageContent, ResponsesInput};

impl From<&ResponsesInput> for Vec<InputItem> {
    fn from(input: &ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                id: None,
                role: "user".into(),
                status: None,
                content: InputMessageContent::Text(text.clone()),
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
                id: None,
                role: "user".into(),
                status: None,
                content: InputMessageContent::Text(text),
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
