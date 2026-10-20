//! Shared input/output history items for Responses, Conversations, and storage.

mod item_id;

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
#[cfg(feature = "openapi")]
use utoipa::{
    PartialSchema, ToSchema,
    openapi::{Ref, RefOr, schema::Schema},
};

use super::{InputItem, OutputItem};
use crate::utils::common::serialize_to_value;

/// An input or output item retained in request, response, or conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, from = "InputItem")]
pub enum InOutItem {
    Input(InputItem),
    Output(OutputItem),
}

#[cfg(feature = "openapi")]
impl PartialSchema for InOutItem {
    fn schema() -> RefOr<Schema> {
        // Submitted history follows the strict input contract, even when its
        // wire shape also appears in upstream output.
        Ref::from_schema_name("InputItem").into()
    }
}

#[cfg(feature = "openapi")]
impl ToSchema for InOutItem {}

impl PartialEq for InOutItem {
    fn eq(&self, other: &Self) -> bool {
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            return false;
        }
        match (serialize_to_value(self), serialize_to_value(other)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
    }
}

impl From<InputItem> for InOutItem {
    fn from(item: InputItem) -> Self {
        Self::Input(item)
    }
}

impl From<OutputItem> for InOutItem {
    fn from(item: OutputItem) -> Self {
        Self::Output(item)
    }
}

impl InOutItem {
    /// Borrow an input record or project a public output using the existing continuation conversion.
    pub(crate) fn as_input(&self) -> Option<Cow<'_, InputItem>> {
        match self {
            Self::Input(input) => Some(Cow::Borrowed(input)),
            Self::Output(output) => output.to_input_item().map(Cow::Owned),
        }
    }

    pub(crate) fn call_id(&self) -> Option<&str> {
        match self {
            Self::Input(item) => item.call_id(),
            Self::Output(OutputItem::FunctionCall(call)) => Some(&call.call_id),
            Self::Output(OutputItem::ToolSearchCall(call)) => Some(&call.call_id),
            Self::Output(OutputItem::CustomToolCall(call)) => Some(&call.call_id),
            Self::Output(OutputItem::ShellCall(call)) => Some(&call.call_id),
            Self::Output(_) => None,
        }
    }

    /// Converts stored history into input items for continuation processing.
    /// Internal items are removed later by `ResponsesInput::model_input`.
    #[must_use]
    pub fn into_input_items(history: Vec<InOutItem>) -> Vec<InputItem> {
        history
            .into_iter()
            .filter_map(|item| match item {
                Self::Input(item) => Some(item),
                Self::Output(output) => output.to_input_item(),
            })
            .map(|item| match item {
                InputItem::ShellCall(call) => InputItem::FunctionCall(call.into()),
                InputItem::ShellCallOutput(output) => InputItem::FunctionCallOutput(output.into()),
                InputItem::CustomToolCall(call) => InputItem::FunctionCall(call.into()),
                InputItem::CustomToolCallOutput(output) => InputItem::FunctionCallOutput(output.into()),
                item => item,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
