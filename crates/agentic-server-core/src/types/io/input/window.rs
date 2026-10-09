//! The compaction window: which items a compaction checkpoint keeps in the model context.
//!
//! The latest checkpoint supersedes the items before it, except completed user messages since
//! the previous checkpoint and async calls still open at the checkpoint, whose outputs may still
//! arrive and must find their calls.

use std::collections::HashSet;

use super::InputItem;
use crate::types::event::MessageStatus;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionWindow {
    latest_index: usize,
    retained_start: usize,
}

impl CompactionWindow {
    #[must_use]
    pub(crate) const fn latest_index(self) -> usize {
        self.latest_index
    }

    #[must_use]
    pub(crate) fn retains_user_item(self, index: usize, item: &InputItem) -> bool {
        index >= self.retained_start
            && index < self.latest_index
            && matches!(item, InputItem::Message(message)
                if message.role == "user"
                    && message.id.is_some()
                    && message.status == Some(MessageStatus::Completed))
    }

    /// Whether an item before the checkpoint stays in the window: a retained user message or
    /// an async call still open at the checkpoint (`open` from [`open_async_call_ids`] over
    /// the items before it).
    #[must_use]
    pub(crate) fn retains_item(self, index: usize, item: &InputItem, open: &HashSet<&str>) -> bool {
        self.retains_user_item(index, item) || (index < self.latest_index && retained_async_call(item, open))
    }
}

#[must_use]
pub(crate) fn latest_compaction_window(items: &[InputItem]) -> Option<CompactionWindow> {
    let latest_index = items
        .iter()
        .rposition(|item| matches!(item, InputItem::Compaction(_)))?;
    let retained_start = items[..latest_index]
        .iter()
        .rposition(|item| matches!(item, InputItem::Compaction(_)))
        .map_or(0, |index| index + 1);
    Some(CompactionWindow {
        latest_index,
        retained_start,
    })
}

/// Call IDs of async calls in `items` that have no output in `items`.
#[must_use]
pub(crate) fn open_async_call_ids(items: &[InputItem]) -> HashSet<&str> {
    let mut open = HashSet::new();
    for item in items {
        match item {
            InputItem::FunctionCall(call) if call.async_execution => {
                open.insert(call.call_id.as_str());
            }
            InputItem::CustomToolCall(call) if call.async_execution => {
                open.insert(call.call_id.as_str());
            }
            InputItem::FunctionCallOutput(output) => {
                open.remove(output.call_id.as_str());
            }
            InputItem::CustomToolCallOutput(output) => {
                open.remove(output.call_id.as_str());
            }
            _ => {}
        }
    }
    open
}

/// Whether `item` is an async call that is still open (its call ID is in `open`).
///
/// A summary cannot stand in for a call whose output may still arrive: the output must find
/// its call. Such calls stay in the model context even though a checkpoint supersedes the
/// items around them.
pub(crate) fn retained_async_call(item: &InputItem, open: &HashSet<&str>) -> bool {
    match item {
        InputItem::FunctionCall(call) => call.async_execution && open.contains(call.call_id.as_str()),
        InputItem::CustomToolCall(call) => call.async_execution && open.contains(call.call_id.as_str()),
        _ => false,
    }
}

/// The async call items in `items` that have no output in `items`, in order and in their
/// public form.
#[must_use]
pub(crate) fn open_async_calls(items: &[InputItem]) -> Vec<InputItem> {
    let open = open_async_call_ids(items);
    items
        .iter()
        .filter(|item| retained_async_call(item, &open))
        .cloned()
        .collect()
}

/// The model-visible items of the latest compaction window.
pub(crate) fn model_items(items: &[InputItem]) -> impl Iterator<Item = &InputItem> {
    let window = latest_compaction_window(items);
    let open = window.map_or_else(HashSet::new, |window| {
        open_async_call_ids(&items[..window.latest_index()])
    });
    items
        .iter()
        .enumerate()
        .filter(move |(index, item)| {
            item.is_model_visible()
                && window
                    .is_none_or(|window| *index >= window.latest_index() || window.retains_item(*index, item, &open))
        })
        .map(|(_, item)| item)
}

/// The items of the latest compaction window, plus earlier items that `keep` selects.
#[must_use]
pub(crate) fn retain_window(history: Vec<InputItem>, keep: impl Fn(&InputItem) -> bool) -> Vec<InputItem> {
    let Some(window) = latest_compaction_window(&history) else {
        return history;
    };
    let retained: Vec<bool> = {
        let open = open_async_call_ids(&history[..window.latest_index()]);
        history
            .iter()
            .enumerate()
            .map(|(index, item)| {
                index >= window.latest_index() || window.retains_item(index, item, &open) || keep(item)
            })
            .collect()
    };
    history
        .into_iter()
        .zip(retained)
        .filter_map(|(item, retained)| retained.then_some(item))
        .collect()
}
