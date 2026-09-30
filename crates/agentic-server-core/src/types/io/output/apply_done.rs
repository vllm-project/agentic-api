use super::{
    CompactionItem, CustomToolCall, FunctionToolCall, McpCall, McpListTools, ReasoningOutput, ReasoningTextContent,
    ToolSearchCall,
};
use crate::events::EventPayload;
use crate::utils::common::deserialize_from_value_opt;
use serde_json::Value;

/// Applies a `*Done` event payload onto an in-flight output item.
///
/// `buffer` holds accumulated delta text/arguments when an output type needs
/// fallback reconstruction. Implementations clear it when the done payload is
/// authoritative.
pub trait ApplyDone {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String);
}

impl ApplyDone for ReasoningOutput {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        match payload {
            EventPayload::ReasoningTextDone {
                text, content_index, ..
            } => {
                buffer.clear();
                if !text.is_empty() {
                    insert_at_part_index(&mut self.content, *content_index, ReasoningTextContent::new(text));
                }
            }
            EventPayload::ReasoningSummaryTextDone {
                text, summary_index, ..
            } => {
                buffer.clear();
                if !text.is_empty() {
                    insert_at_part_index(
                        &mut self.summary,
                        *summary_index,
                        serde_json::json!({"type": "summary_text", "text": text}),
                    );
                }
            }
            EventPayload::OutputItemDone { item, .. } => {
                let Some(raw_item) = item.as_object() else {
                    return;
                };
                let Ok(mut completed) = Self::try_from(payload) else {
                    return;
                };

                if !raw_item.contains_key("content") {
                    completed.content = std::mem::take(&mut self.content);
                }
                if !raw_item.contains_key("summary") {
                    completed.summary = std::mem::take(&mut self.summary);
                }
                *self = completed;
            }
            _ => {}
        }
    }
}

fn insert_at_part_index<T>(parts: &mut Vec<T>, part_index: u32, part: T) {
    // Part indexes address a contiguous wire array. Clamp malformed sparse
    // indexes instead of manufacturing placeholder parts that never arrived.
    let index = usize::try_from(part_index).unwrap_or(usize::MAX).min(parts.len());
    parts.insert(index, part);
}

impl ApplyDone for FunctionToolCall {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        match payload {
            EventPayload::FunctionCallArgsDone {
                arguments,
                call_id,
                name,
                ..
            } => {
                self.arguments = if arguments.is_empty() {
                    std::mem::take(buffer)
                } else {
                    buffer.clear();
                    arguments.clone()
                };
                if let Some(cid) = call_id.as_deref().filter(|s| !s.is_empty()) {
                    cid.clone_into(&mut self.call_id);
                }
                if !name.is_empty() {
                    name.clone_into(&mut self.name);
                }
            }
            EventPayload::OutputItemDone { item, .. } => {
                let Some(mut call) = deserialize_from_value_opt::<Self>(item.clone()) else {
                    return;
                };
                if item.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
                    call.id.clone_from(&self.id);
                }
                if call.call_id.is_empty() {
                    call.call_id.clone_from(&self.call_id);
                }
                if call.name.is_empty() {
                    call.name.clone_from(&self.name);
                }
                if call.namespace.is_none() {
                    call.namespace.clone_from(&self.namespace);
                }
                if call.arguments.is_empty() {
                    call.arguments = if self.arguments.is_empty() {
                        std::mem::take(buffer)
                    } else {
                        std::mem::take(&mut self.arguments)
                    };
                } else {
                    buffer.clear();
                }
                *self = call;
            }
            _ => {}
        }
    }
}

impl ApplyDone for ToolSearchCall {
    fn apply_done(&mut self, payload: &EventPayload, _buffer: &mut String) {
        let EventPayload::OutputItemDone { item, .. } = payload else {
            return;
        };
        if let Some(call) = deserialize_from_value_opt(item.clone()) {
            *self = call;
        }
    }
}

impl ApplyDone for CustomToolCall {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        match payload {
            EventPayload::CustomToolCallInputDone { input, .. } => {
                self.input = if input.is_empty() {
                    std::mem::take(buffer)
                } else {
                    buffer.clear();
                    input.clone()
                };
            }
            EventPayload::OutputItemDone { item, .. } => {
                let Some(mut call) = deserialize_from_value_opt::<Self>(item.clone()) else {
                    return;
                };
                if call.input.is_empty() {
                    call.input = if self.input.is_empty() {
                        std::mem::take(buffer)
                    } else {
                        std::mem::take(&mut self.input)
                    };
                } else {
                    buffer.clear();
                }
                *self = call;
            }
            _ => {}
        }
    }
}

impl ApplyDone for McpCall {
    fn apply_done(&mut self, payload: &EventPayload, _buffer: &mut String) {
        let EventPayload::OutputItemDone { item, .. } = payload else {
            return;
        };
        if let Some(call) = deserialize_from_value_opt(item.clone()) {
            *self = call;
        }
    }
}

impl ApplyDone for McpListTools {
    fn apply_done(&mut self, payload: &EventPayload, _buffer: &mut String) {
        let EventPayload::OutputItemDone { item, .. } = payload else {
            return;
        };
        if let Some(list_tools) = deserialize_from_value_opt(item.clone()) {
            *self = list_tools;
        }
    }
}

impl ApplyDone for CompactionItem {
    fn apply_done(&mut self, payload: &EventPayload, _buffer: &mut String) {
        let EventPayload::OutputItemDone { item, .. } = payload else {
            return;
        };
        let Some(mut compaction) = deserialize_from_value_opt::<Self>(item.clone()) else {
            return;
        };
        if compaction.id.as_deref().is_none_or(str::is_empty) {
            compaction.id.clone_from(&self.id);
        }
        *self = compaction;
    }
}
