//! What a compaction keeps from the items it summarizes: open async calls for the model input,
//! and the request's own items that storage needs again.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use crate::types::io::InOutItem;
use crate::types::io::input::{async_records, open_async_call_ids, retained_async_call};
use crate::types::io::{InputItem, OutputItem};

/// What a compaction must keep from the items it summarizes, selected once from borrowed histories.
///
/// A summary cannot stand in for an async call whose output may still arrive, so every open call
/// stays in the model input after the summary ([`Self::open_calls`]). Compaction also replaces this
/// request's own items in storage, so some of them are stored again, as the request supplied them
/// ([`Self::stored`]): outputs that answer calls made before this request; the request's own
/// answered async calls with the last output for each; and its own async calls that are still
/// open. A call stored earlier keeps its place and stays model-visible through the compaction
/// window; storing it again would duplicate its item and `call_id`.
pub(crate) struct CompactionCarryOver {
    pub(super) open_calls: Vec<InputItem>,
    pub(crate) stored: StoredCarryOver,
}

impl CompactionCarryOver {
    /// `suffix` holds the items a multi-agent compaction leaves after its window; a call or output
    /// there is kept in place, not stored twice. Single-agent compaction passes an empty suffix.
    pub(crate) fn new(
        compacted_items: impl Iterator<Item = impl Borrow<InputItem>>,
        own_items: impl Iterator<Item = impl Borrow<InputItem>>,
        suffix: &[InputItem],
    ) -> Self {
        let compacted_items: Vec<_> = compacted_items.collect();
        let open = open_async_call_ids(&compacted_items);
        // Only tool records can be carried over; do not copy ordinary messages.
        let own_items: Vec<_> = own_items
            .filter(|item| item.borrow().call_id().is_some() || item.borrow().output_call_id().is_some())
            .map(|item| item.borrow().clone())
            .collect();
        let own_calls: HashSet<_> = own_items.iter().filter_map(InputItem::call_id).collect();
        let kept_in_suffix: HashSet<_> = suffix.iter().filter_map(InputItem::output_call_id).collect();
        let calls_in_suffix: HashSet<_> = suffix.iter().filter_map(InputItem::call_id).collect();
        // Reuse the retained-history rule for answered async calls and their last outputs.
        let mut records = async_records(&own_items);
        for (item, record) in own_items.iter().zip(&mut records) {
            let late = item
                .output_call_id()
                .is_some_and(|call_id| !own_calls.contains(call_id) && !kept_in_suffix.contains(call_id));
            let in_suffix = item.call_id().is_some_and(|call_id| calls_in_suffix.contains(call_id))
                || item
                    .output_call_id()
                    .is_some_and(|call_id| kept_in_suffix.contains(call_id));
            *record = late || (*record && !retained_async_call(item, &open) && !in_suffix);
        }
        let mut stored = StoredCarryOver {
            answered: Vec::new(),
            own_open_calls: Vec::new(),
        };
        for (item, record) in own_items.into_iter().zip(records) {
            if record {
                stored.answered.push(item);
            } else if retained_async_call(&item, &open) {
                stored.own_open_calls.push(item);
            }
        }
        Self {
            open_calls: compacted_items
                .iter()
                .map(Borrow::borrow)
                .filter(|item| retained_async_call(item, &open))
                .cloned()
                .collect(),
            stored,
        }
    }
}

/// The request's own items a compaction stores again (see [`CompactionCarryOver::stored`]).
pub(crate) struct StoredCarryOver {
    /// Late outputs and the request's own answered async records, in request order.
    answered: Vec<InputItem>,
    own_open_calls: Vec<InputItem>,
}

impl StoredCarryOver {
    /// The stored window: `compacted`, which ends with its compaction item (and, for multi-agent
    /// compaction, MCP discovery records), with the answered records before that item, so an
    /// answered call and its output leave the window together, and the request's own open calls
    /// after it.
    pub(super) fn insert_into(self, mut compacted: Vec<InputItem>) -> Vec<InputItem> {
        let at = compacted
            .iter()
            .rposition(|item| matches!(item, InputItem::Compaction(_)))
            .unwrap_or(compacted.len());
        compacted.splice(at..at, self.answered);
        compacted.extend(self.own_open_calls);
        compacted
    }

    pub(crate) fn into_inputs(self) -> Vec<InputItem> {
        let mut items = self.answered;
        items.extend(self.own_open_calls);
        items
    }
}

/// The public form of the calls in `call_ids` from a stored history, by `call_id`. Continuation
/// lowers custom calls for the model; the stored items keep the form the client saw.
pub(super) fn public_calls(history: Vec<InOutItem>, call_ids: &HashSet<&str>) -> HashMap<String, InputItem> {
    history
        .into_iter()
        .filter_map(|item| {
            let public = match item {
                InOutItem::Input(item) => item,
                InOutItem::Output(OutputItem::CustomToolCall(call)) => InputItem::CustomToolCall(call),
                InOutItem::Output(output) => output.to_input_item()?,
            };
            let call_id = public
                .call_id()
                .filter(|call_id| call_ids.contains(call_id))?
                .to_owned();
            Some((call_id, public))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{InputMessage, InputMessageContent};

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
            &[],
        );
        assert_eq!(
            serde_json::to_value(&carry.open_calls).unwrap(),
            serde_json::to_value(&history).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(carry.stored.into_inputs()).unwrap(),
            serde_json::to_value(&history).unwrap(),
        );
    }

    /// The request's own answered async call is stored once: before the compaction item, or left in
    /// place when a multi-agent compaction keeps it in the suffix.
    #[test]
    fn own_answered_async_records_are_stored_once() {
        let item = |value: serde_json::Value| -> InputItem { serde_json::from_value(value).unwrap() };
        let call = item(
            serde_json::json!({"type": "function_call", "id": "fc_a", "call_id": "call_a",
            "name": "poll", "arguments": "{}", "status": "completed", "async": true}),
        );
        let first = item(serde_json::json!({"type": "function_call_output", "call_id": "call_a", "output": "1"}));
        let last = item(serde_json::json!({"type": "function_call_output", "call_id": "call_a", "output": "2"}));
        let own = [user_message(), call.clone(), first, last.clone()];
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

        let stored = CompactionCarryOver::new(own.iter(), own.iter(), &[])
            .stored
            .into_inputs();
        assert_eq!(ids(&stored), ["call", "2"], "the call and its last output");

        let suffix = [call, last];
        let stored = CompactionCarryOver::new(own[..1].iter(), own.iter(), &suffix)
            .stored
            .into_inputs();
        assert!(stored.is_empty(), "the suffix keeps them: {stored:?}");
    }
}
