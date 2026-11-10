//! Executor retention through compaction: async calls and their last outputs survive summaries.
//! Model visibility is separate; `types::io::input` owns the model's compaction window.

use std::borrow::Borrow;
use std::collections::HashSet;

use crate::types::io::InputItem;
use crate::types::io::input::{async_records, open_async_call_ids, retained_async_call};

/// What a compaction must keep from the items it summarizes, selected once from borrowed histories.
///
/// A summary cannot stand in for an async call whose output may still arrive, so every open call
/// stays in the model input after the summary ([`Self::open_calls`]). Compaction also replaces this
/// request's own items in storage: late outputs and answered async calls go before the checkpoint;
/// its own open async calls go after it. A call stored earlier keeps its place and stays model-visible through the compaction
/// window; storing it again would duplicate its item and `call_id`.
pub(crate) struct CompactionCarryOver {
    pub(super) open_calls: Vec<InputItem>,
    pub(crate) before: Vec<InputItem>,
    pub(crate) after: Vec<InputItem>,
}

impl CompactionCarryOver {
    pub(crate) fn new(
        compacted_items: impl Iterator<Item = impl Borrow<InputItem>>,
        own_items: impl Iterator<Item = impl Borrow<InputItem>>,
    ) -> Self {
        let compacted_items: Vec<_> = compacted_items.collect();
        let open = open_async_call_ids(&compacted_items);
        // Only tool records can be carried over; do not copy ordinary messages.
        let own_items: Vec<_> = own_items
            .filter(|item| item.borrow().call_id().is_some() || item.borrow().output_call_id().is_some())
            .map(|item| item.borrow().clone())
            .collect();
        let own_calls: HashSet<_> = own_items.iter().filter_map(InputItem::call_id).collect();
        // Reuse the retained-history rule for answered async calls and their last outputs.
        let mut records = async_records(&own_items);
        for (item, record) in own_items.iter().zip(&mut records) {
            let late = item
                .output_call_id()
                .is_some_and(|call_id| !own_calls.contains(call_id));
            *record = late || (*record && !retained_async_call(item, &open));
        }
        let mut carry = Self {
            open_calls: Self::open_calls_in(&compacted_items, &open),
            before: Vec::new(),
            after: Vec::new(),
        };
        for (item, record) in own_items.into_iter().zip(records) {
            if record {
                carry.before.push(item);
            } else if retained_async_call(&item, &open) {
                carry.after.push(item);
            }
        }
        carry
    }

    /// The open async calls in `compacted_items`, which stay in the model input after the summary.
    pub(crate) fn open_calls(compacted_items: impl Iterator<Item = impl Borrow<InputItem>>) -> Vec<InputItem> {
        let compacted_items: Vec<_> = compacted_items.collect();
        Self::open_calls_in(&compacted_items, &open_async_call_ids(&compacted_items))
    }

    fn open_calls_in(compacted_items: &[impl Borrow<InputItem>], open: &HashSet<&str>) -> Vec<InputItem> {
        compacted_items
            .iter()
            .map(Borrow::borrow)
            .filter(|item| retained_async_call(item, open))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::input::retain_window;
    use crate::types::io::{CompactionItem, InOutItem, InputMessage, InputMessageContent};

    fn user_message() -> InputItem {
        InputItem::Message(InputMessage {
            role: "user".to_owned(),
            content: InputMessageContent::Text("poll".to_owned()),
            ..Default::default()
        })
    }

    #[test]
    fn owned_carry_over_keeps_open_calls_in_their_public_form() {
        let call: InOutItem = serde_json::from_value(serde_json::json!({
            "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
            "name": "poll", "input": "status", "async": true
        }))
        .unwrap();
        let history = [call];
        let carry = CompactionCarryOver::new(
            history.iter().filter_map(InOutItem::as_input),
            history.iter().filter_map(InOutItem::as_input),
        );
        assert_eq!(
            serde_json::to_value(&carry.open_calls).unwrap(),
            serde_json::to_value(&history).unwrap(),
        );
        assert!(carry.before.is_empty());
        assert_eq!(
            serde_json::to_value(carry.after).unwrap(),
            serde_json::to_value(&history).unwrap(),
        );
    }

    /// The request's own answered async call is stored once, before the compaction item.
    #[test]
    fn own_answered_async_records_are_stored_once() {
        let item = |value: serde_json::Value| -> InputItem { serde_json::from_value(value).unwrap() };
        let call = item(
            serde_json::json!({"type": "function_call", "id": "fc_a", "call_id": "call_a",
            "name": "poll", "arguments": "{}", "status": "completed", "async": true}),
        );
        let first = item(serde_json::json!({"type": "function_call_output", "call_id": "call_a", "output": "1"}));
        let last = item(serde_json::json!({"type": "function_call_output", "call_id": "call_a", "output": "2"}));
        let own = [user_message(), call, first, last];
        let ids = |items: &[InputItem]| -> Vec<String> {
            items
                .iter()
                .map(|item| {
                    serde_json::to_value(item).unwrap()["output"]
                        .as_str()
                        .unwrap_or("call")
                        .to_owned()
                })
                .collect()
        };

        let carry = CompactionCarryOver::new(own.iter(), own.iter());
        assert_eq!(ids(&carry.before), ["call", "2"], "the call and its last output");
        assert!(carry.after.is_empty());
    }

    #[test]
    fn history_replacement_drops_discovery_and_keeps_async_records_in_order() {
        let history: Vec<InputItem> = serde_json::from_value(serde_json::json!([
            {"type": "message", "role": "assistant", "content": "old answer"},
            {"type": "message", "role": "user", "id": "msg_kept", "status": "completed", "content": "poll"},
            {"type": "mcp_list_tools", "id": "mcp_1", "server_label": "counter", "tools": []},
            {"type": "function_call", "call_id": "async", "name": "poll", "arguments": "{}", "async": true},
            {"type": "function_call_output", "call_id": "async", "output": "first"},
            {"type": "function_call_output", "call_id": "async", "output": "last"},
            {"type": "custom_tool_call", "call_id": "open", "name": "poll", "input": "status", "async": true},
            {"type": "function_call", "call_id": "sync", "name": "poll", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "sync", "output": "done"}
        ]))
        .unwrap();
        let compaction = CompactionItem {
            agent: None,
            id: Some("cmp_1".into()),
            encrypted_content: "summary".into(),
        };
        let carry = CompactionCarryOver::new(history.iter(), history.iter());
        let checkpoint = InputItem::Compaction(compaction);
        assert_eq!(
            serde_json::to_value(carry.before).unwrap(),
            serde_json::to_value([&history[3], &history[5]]).unwrap(),
            "answered records precede the checkpoint",
        );
        assert_eq!(
            serde_json::to_value(carry.after).unwrap(),
            serde_json::to_value([&history[6]]).unwrap(),
            "open calls follow the checkpoint",
        );

        let expected_window =
            serde_json::to_value([&history[1], &history[3], &history[5], &history[6], &checkpoint]).unwrap();
        let mut stored_history = history;
        stored_history.push(checkpoint);
        assert_eq!(
            serde_json::to_value(retain_window(stored_history)).unwrap(),
            expected_window,
            "session retention preserves the records' existing order",
        );
    }
}
