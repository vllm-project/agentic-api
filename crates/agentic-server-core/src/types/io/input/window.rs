//! The compaction window: which items a compaction checkpoint keeps in the model context.
//!
//! The latest checkpoint supersedes the items before it, except completed user messages since
//! the previous checkpoint and async calls still open at the checkpoint, whose outputs may still
//! arrive and must find their calls.

use std::borrow::Borrow;
use std::collections::HashSet;
use std::hash::Hash;

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
    pub(crate) fn retains_item<S: Borrow<str> + Eq + Hash>(
        self,
        index: usize,
        item: &InputItem,
        open: &HashSet<S>,
    ) -> bool {
        self.retains_user_item(index, item) || (index < self.latest_index && retained_async_call(item, open))
    }
}

#[must_use]
pub(crate) fn latest_compaction_window<T: Borrow<InputItem>>(items: &[T]) -> Option<CompactionWindow> {
    let latest_index = items
        .iter()
        .rposition(|item| matches!(item.borrow(), InputItem::Compaction(_)))?;
    let retained_start = items[..latest_index]
        .iter()
        .rposition(|item| matches!(item.borrow(), InputItem::Compaction(_)))
        .map_or(0, |index| index + 1);
    Some(CompactionWindow {
        latest_index,
        retained_start,
    })
}

/// Call IDs of async calls in `items` that have no output in `items`.
#[must_use]
pub(crate) fn open_async_call_ids<T: Borrow<InputItem>>(items: &[T]) -> HashSet<&str> {
    let mut open = HashSet::new();
    for item in items {
        let item = item.borrow();
        if item.is_async_call() {
            open.extend(item.call_id());
        } else if let Some(call_id) = item.output_call_id() {
            open.remove(call_id);
        }
    }
    open
}

/// Whether `item` is an async call whose call ID is in `ids`.
///
/// A summary cannot stand in for a call whose output may still arrive: the output must find
/// its call. Such calls stay in the model context even though a checkpoint supersedes the
/// items around them.
pub(crate) fn retained_async_call<S: Borrow<str> + Eq + Hash>(item: &InputItem, ids: &HashSet<S>) -> bool {
    item.is_async_call() && item.call_id().is_some_and(|call_id| ids.contains(call_id))
}

/// Marks, by position, the async call records a retained history keeps through a compaction:
/// every async call, and the last output that answers each one.
///
/// Async calls accept outputs at any time, so a later output must still find and validate against
/// its call, and an answered call must still read as answered. One output per call is enough for
/// both, so repeated outputs do not accumulate across compactions. The model view hides these
/// records unless the call is still open or answered again after the checkpoint ([`model_items`]).
#[must_use]
pub(crate) fn async_records(items: &[InputItem]) -> Vec<bool> {
    let calls: HashSet<&str> = items
        .iter()
        .filter(|item| item.is_async_call())
        .filter_map(InputItem::call_id)
        .collect();
    let mut answered = HashSet::new();
    let mut records = vec![false; items.len()];
    for (record, item) in records.iter_mut().zip(items).rev() {
        *record = item.is_async_call()
            || item
                .output_call_id()
                .is_some_and(|call_id| calls.contains(call_id) && answered.insert(call_id));
    }
    records
}

/// The model-visible items of the latest compaction window.
///
/// An async call from before the checkpoint stays visible while it is open there, or when a further
/// output for it arrives after the checkpoint (async calls accept repeated outputs), so the model
/// never receives an output without its call.
pub(crate) fn model_items<T: Borrow<InputItem>>(items: Vec<T>) -> impl Iterator<Item = T> {
    let window = latest_compaction_window(&items);
    let open: HashSet<String> = window.map_or_else(HashSet::new, |window| {
        let (before, after) = items.split_at(window.latest_index());
        let mut kept = open_async_call_ids(before);
        kept.extend(after.iter().filter_map(|item| item.borrow().output_call_id()));
        // Own the IDs so selection can move output projections without cloning payloads.
        kept.into_iter().map(str::to_owned).collect()
    });
    items.into_iter().enumerate().filter_map(move |(index, item)| {
        let input = item.borrow();
        let visible = input.is_model_visible()
            && window.is_none_or(|window| index >= window.latest_index() || window.retains_item(index, input, &open));
        visible.then_some(item)
    })
}

/// The items of the latest compaction window, plus the async call records from before it
/// ([`async_records`]) and earlier items that `keep` selects.
#[must_use]
pub(crate) fn retain_window(history: Vec<InputItem>, keep: impl Fn(&InputItem) -> bool) -> Vec<InputItem> {
    let Some(window) = latest_compaction_window(&history) else {
        return history;
    };
    let retained: Vec<bool> = {
        let records = async_records(&history[..window.latest_index()]);
        history
            .iter()
            .enumerate()
            .map(|(index, item)| {
                index >= window.latest_index() || window.retains_user_item(index, item) || records[index] || keep(item)
            })
            .collect()
    };
    history
        .into_iter()
        .zip(retained)
        .filter_map(|(item, retained)| retained.then_some(item))
        .collect()
}
