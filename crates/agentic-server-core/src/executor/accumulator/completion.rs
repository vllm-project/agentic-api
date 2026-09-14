//! Typed authoritative completion merging for active accumulator items.

use crate::events::EventPayload;
use crate::types::io::output::McpListTools;
use crate::types::io::{
    CompactionItem, CustomToolCall, FunctionToolCall, McpCall, OutputMessage, ReasoningOutput, ShellCall,
    ToolSearchCall, WebSearchCall,
};
use crate::utils::uuid7_str;

/// Merge an already parsed completion with retained per-item state.
///
/// Context is specific to the item: a delta buffer, the original payload for
/// field-presence checks, or the resolved item ID. Identity and lifecycle
/// validation remain in `SlotMap`; this trait performs no parsing or routing.
pub(super) trait MergeDone<Context = (), Completed = Self> {
    fn merge_done(&mut self, done: &Completed, context: Context);
}

impl MergeDone<&mut String> for OutputMessage {
    fn merge_done(&mut self, done: &Self, text: &mut String) {
        self.clone_from(done);
        text.clear();
    }
}

impl MergeDone<&EventPayload> for ReasoningOutput {
    fn merge_done(&mut self, done: &Self, payload: &EventPayload) {
        let mut done = done.clone();
        let raw = match payload {
            EventPayload::OutputItemDone { item, .. } => item.as_object(),
            _ => None,
        };
        if raw.is_some_and(|raw| !raw.contains_key("content")) {
            done.content.clone_from(&self.content);
        }
        if raw.is_some_and(|raw| !raw.contains_key("summary")) {
            done.summary.clone_from(&self.summary);
        }
        if done.id.is_empty() {
            done.id.clone_from(&self.id);
        }
        *self = done;
    }
}

impl MergeDone<&mut String> for FunctionToolCall {
    fn merge_done(&mut self, done: &Self, arguments: &mut String) {
        let mut done = done.clone();
        if done.id.is_empty() {
            done.id.clone_from(&self.id);
        }
        if done.call_id.is_empty() {
            done.call_id.clone_from(&self.call_id);
        }
        if done.name.is_empty() {
            done.name.clone_from(&self.name);
        }
        if done.namespace.is_none() {
            done.namespace.clone_from(&self.namespace);
        }
        if done.arguments.is_empty() {
            done.arguments = if self.arguments.is_empty() {
                std::mem::take(arguments)
            } else {
                self.arguments.clone()
            };
        } else {
            arguments.clear();
        }
        *self = done;
    }
}

impl MergeDone<&mut String> for CustomToolCall {
    fn merge_done(&mut self, done: &Self, input: &mut String) {
        let mut done = done.clone();
        if done.id.is_empty() {
            done.id.clone_from(&self.id);
        }
        if done.call_id.is_empty() {
            done.call_id.clone_from(&self.call_id);
        }
        if done.name.is_empty() {
            done.name.clone_from(&self.name);
        }
        if done.input.is_empty() {
            done.input = if self.input.is_empty() {
                std::mem::take(input)
            } else {
                self.input.clone()
            };
        } else {
            input.clear();
        }
        *self = done;
    }
}

impl MergeDone<&str, WebSearchCall> for Option<WebSearchCall> {
    fn merge_done(&mut self, done: &WebSearchCall, item_id: &str) {
        let mut done = done.clone();
        if done.id.is_empty() {
            done.id = if item_id.is_empty() {
                uuid7_str("ws_")
            } else {
                item_id.to_owned()
            };
        }
        *self = Some(done);
    }
}

impl MergeDone for CompactionItem {
    fn merge_done(&mut self, done: &Self, _context: ()) {
        let mut done = done.clone();
        if done.id.as_deref().is_none_or(str::is_empty) {
            done.id.clone_from(&self.id);
        }
        *self = done;
    }
}

impl MergeDone for ShellCall {
    fn merge_done(&mut self, done: &Self, (): ()) {
        self.clone_from(done);
    }
}

impl MergeDone for ToolSearchCall {
    fn merge_done(&mut self, done: &Self, (): ()) {
        self.clone_from(done);
    }
}

impl MergeDone for McpCall {
    fn merge_done(&mut self, done: &Self, (): ()) {
        self.clone_from(done);
    }
}

impl MergeDone for McpListTools {
    fn merge_done(&mut self, done: &Self, (): ()) {
        self.clone_from(done);
    }
}
