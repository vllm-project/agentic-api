//! What a compaction keeps from the items it summarizes: open async calls for the model input,
//! and the request's own items that storage needs again.

use std::collections::{HashMap, HashSet};

use crate::storage::InOutItem;
use crate::types::io::input::{async_records, open_async_call_ids, retained_async_call};
use crate::types::io::{InputItem, OutputItem};

/// What a compaction must keep from the items it summarizes, selected lazily from borrowed items.
///
/// A summary cannot stand in for an async call whose output may still arrive, so every open call
/// stays in the model input after the summary ([`Self::open_calls`]). Compaction also replaces this
/// request's own items in storage, so some of them are stored again, as the request supplied them
/// ([`Self::stored`]): outputs that answer calls made before this request; the request's own
/// answered async calls with the last output for each; and its own async calls that are still
/// open. A call stored earlier keeps its place and stays model-visible through the compaction
/// window; storing it again would duplicate its item and `call_id`.
pub(super) struct CompactionCarryOver<'a> {
    compacted_items: &'a [InputItem],
    own_items: &'a [InputItem],
    open: HashSet<&'a str>,
    own_calls: HashSet<&'a str>,
    kept_in_suffix: HashSet<&'a str>,
    calls_in_suffix: HashSet<&'a str>,
}

impl<'a> CompactionCarryOver<'a> {
    /// `suffix` holds the items a multi-agent compaction leaves after its window; a call or output
    /// there is kept in place, not stored twice. Single-agent compaction passes an empty suffix.
    pub(super) fn new(compacted_items: &'a [InputItem], own_items: &'a [InputItem], suffix: &'a [InputItem]) -> Self {
        Self {
            compacted_items,
            own_items,
            open: open_async_call_ids(compacted_items),
            own_calls: own_items.iter().filter_map(InputItem::call_id).collect(),
            kept_in_suffix: suffix.iter().filter_map(InputItem::output_call_id).collect(),
            calls_in_suffix: suffix.iter().filter_map(InputItem::call_id).collect(),
        }
    }

    /// Every async call still open in the compacted items.
    pub(super) fn open_calls(&self) -> impl Iterator<Item = &'a InputItem> + '_ {
        self.compacted_items
            .iter()
            .filter(|item| retained_async_call(item, &self.open))
    }

    /// The request's own items to store again: its late outputs, its own answered async records,
    /// and its own open async calls.
    pub(super) fn stored(&self) -> StoredCarryOver {
        let late = |item: &InputItem| {
            item.output_call_id()
                .is_some_and(|call_id| !self.own_calls.contains(call_id) && !self.kept_in_suffix.contains(call_id))
        };
        // The request's own answered async calls, and the last output for each, as retained
        // histories keep them (`async_records`), so a further output still finds its call.
        let records = async_records(self.own_items);
        let in_suffix = |item: &InputItem| {
            item.call_id()
                .is_some_and(|call_id| self.calls_in_suffix.contains(call_id))
                || item
                    .output_call_id()
                    .is_some_and(|call_id| self.kept_in_suffix.contains(call_id))
        };
        let answered =
            |item: &InputItem, record: bool| record && !retained_async_call(item, &self.open) && !in_suffix(item);
        StoredCarryOver {
            answered: self
                .own_items
                .iter()
                .zip(records)
                .filter(|(item, record)| late(item) || answered(item, *record))
                .map(|(item, _)| item.clone())
                .collect(),
            own_open_calls: self
                .own_items
                .iter()
                .filter(|item| retained_async_call(item, &self.open))
                .cloned()
                .collect(),
        }
    }
}

/// The request's own items a compaction stores again (see [`CompactionCarryOver::stored`]).
pub(super) struct StoredCarryOver {
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

    pub(super) fn into_inputs(self) -> Vec<InputItem> {
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

/// The items a `compaction_trigger` turn stores again before its compaction output: the request's
/// late outputs and answered async records, then its own open async calls.
pub(crate) fn carried_over_inputs(items: &[InputItem], own_items: &[InputItem]) -> Vec<InputItem> {
    CompactionCarryOver::new(items, own_items, &[]).stored().into_inputs()
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
        let own = vec![user_message(), call.clone(), first, last.clone()];
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

        let stored = CompactionCarryOver::new(&own, &own, &[]).stored().into_inputs();
        assert_eq!(ids(&stored), ["call", "2"], "the call and its last output");

        let suffix = [call, last];
        let stored = CompactionCarryOver::new(&own[..1], &own, &suffix)
            .stored()
            .into_inputs();
        assert!(stored.is_empty(), "the suffix keeps them: {stored:?}");
    }
}
