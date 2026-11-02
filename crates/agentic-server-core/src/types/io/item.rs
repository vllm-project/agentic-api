//! Shared input/output history items for Responses, Conversations, and storage.

mod item_id;

use std::borrow::Cow;
use std::str::FromStr;

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
#[cfg(feature = "openapi")]
use utoipa::{
    PartialSchema, ToSchema,
    openapi::{Ref, RefOr, schema::Schema},
};

use super::{InputContent, InputItem, InputMessageContent, InputTextContent, InputToolSearchCall, OutputItem};
use crate::types::event::MessageStatus;
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
    /// Assign the stored public identity and expand shorthand message content.
    /// This consumes the returned copy; retained model history is not modified.
    #[must_use]
    pub(crate) fn into_public(mut self, id: String) -> Self {
        self.set_id(id);
        if let Self::Input(InputItem::Message(message)) = &mut self {
            message.status.get_or_insert(MessageStatus::Completed);
            if let InputMessageContent::Text(text) = &mut message.content {
                message.content = InputMessageContent::Parts(vec![InputContent::InputText(InputTextContent::new(
                    std::mem::take(text),
                ))]);
            }
        }
        self
    }

    /// The stored record a client replays as input. The continuation conversion leaves out a hosted search item, which
    /// the executor records as history itself; a replayed one is input the upstream projection lowers, so it stays an
    /// input record.
    #[must_use]
    pub(crate) fn into_replayed_input(self) -> Self {
        match self {
            Self::Output(OutputItem::ToolSearchOutput(output)) => Self::Input(InputItem::ToolSearchOutput(output)),
            Self::Output(OutputItem::ToolSearchCall(call)) => InputToolSearchCall::try_from(&call)
                .map_or(Self::Output(OutputItem::ToolSearchCall(call)), |call| {
                    Self::Input(InputItem::ToolSearchCall(call))
                }),
            item @ (Self::Input(_) | Self::Output(_)) => item,
        }
    }

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
            Self::Output(OutputItem::ToolSearchCall(call)) => call.call_id.as_deref(),
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

/// Response for listing items with pagination.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ListItemsResponse {
    /// Object type, always "list".
    pub object: String,

    /// Items in this page.
    pub data: Vec<InOutItem>,

    /// Whether there are more items available.
    pub has_more: bool,

    /// ID of the first item in this page.
    pub first_id: Option<String>,

    /// ID of the last item in this page.
    pub last_id: Option<String>,
}

/// Ordering for item pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ItemOrder {
    Asc,
    #[default]
    Desc,
}

/// Page size for item listings, validated to `0..=100`.
/// Conversation listings additionally require a nonzero limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct ItemLimit(u8);

impl ItemLimit {
    /// The number of items a page holds.
    #[must_use]
    pub fn get(self) -> usize {
        usize::from(self.0)
    }

    /// One extra item determines whether another page follows.
    #[must_use]
    pub fn probe(self) -> i64 {
        i64::from(self.0) + 1
    }
}

impl Default for ItemLimit {
    fn default() -> Self {
        Self(20)
    }
}

impl TryFrom<i64> for ItemLimit {
    type Error = InvalidItemLimit;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            return Err(InvalidItemLimit::BelowMinimum);
        }
        if value > 100 {
            return Err(InvalidItemLimit::AboveMaximum);
        }
        u8::try_from(value)
            .map(Self)
            .map_err(|_| InvalidItemLimit::AboveMaximum)
    }
}

impl From<ItemLimit> for i64 {
    fn from(limit: ItemLimit) -> Self {
        Self::from(limit.0)
    }
}

/// A page size outside `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidItemLimit {
    #[error("limit must be at least 0")]
    BelowMinimum,
    #[error("limit must be at most 100")]
    AboveMaximum,
}

impl InvalidItemLimit {
    /// Machine-readable code for an invalid page size.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BelowMinimum => "integer_below_min_value",
            Self::AboveMaximum => "integer_above_max_value",
        }
    }
}

/// Validated pagination for item listings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemPage<'a> {
    pub limit: ItemLimit,
    /// Public ID of the last item on the previous page.
    pub after: Option<&'a str>,
    pub order: ItemOrder,
}

/// Extra item fields a listing can be asked to include, as `OpenAI` defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ItemInclude {
    #[serde(rename = "file_search_call.results")]
    FileSearchCallResults,
    #[serde(rename = "web_search_call.results")]
    WebSearchCallResults,
    #[serde(rename = "web_search_call.action.sources")]
    WebSearchCallActionSources,
    #[serde(rename = "message.input_image.image_url")]
    MessageInputImageUrl,
    #[serde(rename = "computer_call_output.output.image_url")]
    ComputerCallOutputImageUrl,
    #[serde(rename = "code_interpreter_call.outputs")]
    CodeInterpreterCallOutputs,
    #[serde(rename = "reasoning.encrypted_content")]
    ReasoningEncryptedContent,
    #[serde(rename = "message.output_text.logprobs")]
    MessageOutputTextLogprobs,
}

impl FromStr for ItemInclude {
    type Err = serde::de::value::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(value.into_deserializer())
    }
}

/// Response for successful deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeletedResponse {
    /// ID of the deleted resource.
    pub id: String,

    /// Object type.
    pub object: String,

    /// Whether the deletion was successful.
    pub deleted: bool,
}

impl ListItemsResponse {
    /// Create a new list response.
    #[must_use]
    pub fn new(data: Vec<InOutItem>, has_more: bool) -> Self {
        let first_id = data.first().and_then(InOutItem::id).map(str::to_owned);
        let last_id = data.last().and_then(InOutItem::id).map(str::to_owned);

        Self {
            object: "list".to_string(),
            data,
            has_more,
            first_id,
            last_id,
        }
    }
}

impl DeletedResponse {
    /// Create a deleted response resource.
    #[must_use]
    pub fn response(id: String) -> Self {
        Self {
            id,
            object: "response.deleted".to_string(),
            deleted: true,
        }
    }

    /// Create a deleted conversation response.
    #[must_use]
    pub fn conversation(id: String) -> Self {
        Self {
            id,
            object: "conversation.deleted".to_string(),
            deleted: true,
        }
    }

    /// Create a deleted item response.
    #[must_use]
    pub fn item(id: String) -> Self {
        Self {
            id,
            object: "conversation.item.deleted".to_string(),
            deleted: true,
        }
    }
}

#[cfg(test)]
mod tests;
