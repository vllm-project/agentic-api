//! Semantic event identities and lifecycle validation errors.

use super::slot::{ItemIdentity, OutputIndex};
use crate::events::{EventFrame, EventPayload, ValidatedFrame, expected_item_type};
use crate::executor::error::ExecutorError;
use crate::types::io::OutputItem;

pub(super) fn output_item_call_id(item: &OutputItem) -> Option<&str> {
    match item {
        OutputItem::FunctionCall(call) => Some(&call.call_id),
        OutputItem::ToolSearchCall(call) => Some(&call.call_id),
        OutputItem::CustomToolCall(call) => Some(&call.call_id),
        OutputItem::ShellCall(call) => Some(&call.call_id),
        OutputItem::MultiAgentCall(call) => Some(&call.call_id),
        OutputItem::MultiAgentCallOutput(output) => Some(&output.call_id),
        _ => None,
    }
}

pub(super) fn invalid_lifecycle(event_name: &str) -> ExecutorError {
    invalid_stream(format!(
        "upstream stream event '{event_name}' is out of lifecycle order"
    ))
}

pub(super) fn invalid_lifecycle_or_id(event_name: &str) -> ExecutorError {
    invalid_stream(format!(
        "upstream stream event '{event_name}' is out of lifecycle order or changes the response id"
    ))
}

pub(super) fn invalid_stream(message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}

pub(super) fn item_identity<'a>(
    frame: &'a EventFrame,
    validated: Option<&ValidatedFrame<'a>>,
) -> Option<ItemIdentity<'a>> {
    if let Some(item) = validated.and_then(|frame| frame.item.as_ref()) {
        return Some(ItemIdentity {
            index: Some(OutputIndex::new(item.output_index)),
            item_id: (!item.item_id.is_empty()).then_some(item.item_id),
            item_type: item.item_type,
            event_agent: frame.wire.agent.as_ref(),
        });
    }
    let (item_id, item_type) = match &frame.payload {
        EventPayload::OutputItemAdded { item_id, item_type, .. }
        | EventPayload::OutputItemDone { item_id, item_type, .. } => (item_id.as_str(), *item_type),
        payload => {
            let item_type = expected_item_type(frame)?;
            let item_id = match payload {
                EventPayload::AgentMessageContentDone { item_id, .. }
                | EventPayload::MessageContentDone { item_id, .. }
                | EventPayload::TextDelta { item_id, .. }
                | EventPayload::TextDone { item_id, .. }
                | EventPayload::FunctionCallArgsDelta { item_id, .. }
                | EventPayload::FunctionCallArgsDone { item_id, .. }
                | EventPayload::CustomToolCallInputDelta { item_id, .. }
                | EventPayload::CustomToolCallInputDone { item_id, .. }
                | EventPayload::ReasoningTextDelta { item_id, .. }
                | EventPayload::ReasoningTextDone { item_id, .. }
                | EventPayload::ReasoningSummaryTextDelta { item_id, .. }
                | EventPayload::ReasoningSummaryTextDone { item_id, .. } => item_id.as_str(),
                _ => frame
                    .wire
                    .rest
                    .get("item_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            };
            (item_id, item_type)
        }
    };
    Some(ItemIdentity {
        index: frame.output_index().map(OutputIndex::new),
        item_id: (!item_id.is_empty()).then_some(item_id),
        item_type,
        event_agent: frame.wire.agent.as_ref(),
    })
}
