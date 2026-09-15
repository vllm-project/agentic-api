//! Identity and lifecycle of per-round output items.
//!
//! [`SlotMap`] resolves output indexes and item IDs, enforces active/completed
//! transitions, detects duplicate or conflicting completion, and dispatches to
//! the per-kind state in [`super::active`]. Retained-byte accounting is owned by
//! each slot's [`RetainedAccount`]: incremental growth is charged by the active
//! state as it happens, and completion and finalization reconcile against the
//! comprehensive [`RetainedSize`] measurement of the completed item.

use super::Validation;
use super::active::ActiveItem;
use std::collections::HashMap;

use indexmap::IndexMap;

use crate::events::{EventPayload, SSEItemType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{ExecutorResponseBudget, RetainedAccount, RetainedSize};
use crate::types::io::{OutputItem, ReasoningOutput};
use crate::utils::common::deserialize_from_value_opt;
use crate::utils::uuid7_str;

/// A supplied u32 index or an explicitly allocated recovery index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct OutputIndex(u32);

impl OutputIndex {
    pub(super) fn new(index: u32) -> Self {
        Self(index)
    }
    pub(super) fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(super) struct ItemIdentity<'a> {
    pub(super) index: Option<OutputIndex>,
    pub(super) item_id: Option<&'a str>,
    pub(super) item_type: SSEItemType,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotAction {
    Open,
    Mutate,
    Complete,
}

/// The only owner of per-round item identity and lifecycle state.
#[derive(Debug, Default)]
pub(super) struct SlotMap {
    slots: IndexMap<OutputIndex, Slot>,
    indexes_by_id: HashMap<String, OutputIndex>,
}

impl SlotMap {
    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn has_active(&self) -> bool {
        self.slots
            .values()
            .any(|slot| matches!(slot.state, SlotState::Active(_)))
    }

    pub(super) fn get(&self, index: OutputIndex) -> Option<&Slot> {
        self.slots.get(&index)
    }

    pub(super) fn drain_output_with_budget(
        &mut self,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Vec<OutputItem>> {
        self.slots.sort_keys();
        self.indexes_by_id.clear();
        let mut items = Vec::new();
        for (_, mut slot) in self.slots.drain(..) {
            if let Some(item) = slot.state.finalize() {
                // Final reconciliation verifies incremental accounting against
                // the comprehensive measurement of what is actually retained.
                slot.account.reconcile(budget, item.retained_bytes())?;
                items.push(item);
            }
        }
        Ok(items)
    }

    /// Resolves without mutating either map; a rejected event cannot bind an ID.
    fn resolve(
        &self,
        identity: ItemIdentity<'_>,
        action: SlotAction,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let index_only_shell_command = identity.item_type == SSEItemType::ShellCall && action == SlotAction::Mutate;
        if validation == Validation::Strict
            && (identity.index.is_none() || (identity.item_id.is_none() && !index_only_shell_command))
        {
            return Err(invalid("upstream item requires 'output_index' and a non-empty item ID"));
        }
        let by_id = identity.item_id.and_then(|id| self.indexes_by_id.get(id)).copied();
        if let (Some(index), Some(known)) = (identity.index, by_id)
            && index != known
        {
            return Err(identity_mismatch());
        }
        let index = if let Some(index) = identity.index.or(by_id) {
            index
        } else {
            // Without an index, an unknown ID cannot identify an anonymous
            // slot. Do not guess by kind or silently create a duplicate.
            if action != SlotAction::Open
                && self.slots.values().any(|slot| {
                    slot.state.item_type() == Some(identity.item_type)
                        && (slot.item_id.is_none() || identity.item_id.is_none())
                })
            {
                return Err(invalid("upstream item has ambiguous identity without 'output_index'"));
            }
            if action == SlotAction::Mutate {
                return Ok(None);
            }
            self.unused_index()?
        };
        let Some(slot) = self.slots.get(&index) else {
            return match action {
                SlotAction::Open => Ok(Some(index)),
                SlotAction::Complete if validation == Validation::Lenient => Ok(Some(index)),
                _ if validation == Validation::Strict => Err(no_active_item()),
                _ => Ok(None),
            };
        };
        if action == SlotAction::Open {
            return Err(invalid(
                "upstream stream repeats output item or reuses its output_index",
            ));
        }
        if let (Some(bound), Some(supplied)) = (slot.item_id.as_deref(), identity.item_id)
            && bound != supplied
        {
            return Err(identity_mismatch());
        }
        if slot.state.item_type() != Some(identity.item_type) {
            return if action == SlotAction::Mutate && validation == Validation::Lenient {
                Ok(None)
            } else {
                Err(identity_mismatch())
            };
        }
        if matches!(slot.state, SlotState::Done(_))
            && (action != SlotAction::Complete || validation == Validation::Strict)
        {
            return if validation == Validation::Strict {
                Err(no_active_item())
            } else {
                Ok(None)
            };
        }
        Ok(Some(index))
    }

    fn unused_index(&self) -> ExecutorResult<OutputIndex> {
        (0..=u32::MAX)
            .map(OutputIndex)
            .find(|index| !self.slots.contains_key(index))
            .ok_or_else(|| invalid("upstream stream exhausted output indexes"))
    }

    fn insert(&mut self, index: OutputIndex, item_id: Option<&str>, state: SlotState, account: RetainedAccount) {
        if let Some(id) = item_id {
            self.indexes_by_id.insert(id.to_owned(), index);
        }
        self.slots.insert(
            index,
            Slot {
                item_id: item_id.map(str::to_owned),
                state,
                account,
            },
        );
    }

    fn bind_id(&mut self, index: OutputIndex, item_id: Option<&str>) {
        if let Some(id) = item_id
            && let Some(slot) = self.slots.get_mut(&index)
            && slot.item_id.is_none()
        {
            slot.item_id = Some(id.to_owned());
            self.indexes_by_id.insert(id.to_owned(), index);
        }
    }

    pub(super) fn open(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validation: Validation,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Open, validation)? else {
            return Ok(None);
        };
        if let Some(item) = ActiveItem::from_added(payload) {
            // The opening snapshot is measured with the same rules as the
            // completed item, and charged before the slot retains it.
            let mut account = RetainedAccount::default();
            account.charge(budget, item.retained_bytes())?;
            self.insert(index, identity.item_id, SlotState::Active(item), account);
            return Ok(Some(index));
        }
        Ok(None)
    }

    pub(super) fn apply(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validation: Validation,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Mutate, validation)? else {
            return Ok(None);
        };
        let Some(slot) = self.slots.get_mut(&index) else {
            return Ok(None);
        };
        if let SlotState::Active(item) = &mut slot.state {
            item.apply_event(payload, &mut slot.account, budget)?;
        }
        self.bind_id(index, identity.item_id);
        Ok(Some(index))
    }

    pub(super) fn complete(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validated_done_item: Option<&OutputItem>,
        validation: Validation,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Complete, validation)? else {
            return Ok(None);
        };
        let EventPayload::OutputItemDone { item: raw_item, .. } = payload else {
            return Ok(None);
        };
        let parsed = validated_done_item.cloned().or_else(|| {
            deserialize_from_value_opt::<OutputItem>(raw_item.clone()).or_else(|| {
                (identity.item_type == SSEItemType::Reasoning)
                    .then(|| ReasoningOutput::try_from(payload).ok().map(OutputItem::Reasoning))
                    .flatten()
            })
        });
        if let Some(slot) = self.slots.get(&index) {
            if let SlotState::Active(active) = &slot.state
                && let Some(shell) = active.shell_call().filter(|shell| shell.tracks_commands())
                && (shell.has_unfinished_commands()
                    || !matches!(parsed.as_ref(), Some(OutputItem::ShellCall(completed))
                        if completed.action.commands == shell.item.action.commands))
            {
                return Err(invalid("shell item done has unfinished or contradictory commands"));
            }
            if let SlotState::Done(previous) = &slot.state
                && parsed
                    .as_ref()
                    .is_some_and(|candidate| semantically_equal(previous, candidate))
            {
                self.bind_id(index, identity.item_id);
                return Ok(None);
            }
            let candidate = slot.state.completion_candidate(
                payload,
                parsed.as_ref(),
                slot.item_id.as_deref().or(identity.item_id).unwrap_or(""),
            );
            if let SlotState::Done(previous) = &slot.state {
                if candidate
                    .as_ref()
                    .is_some_and(|candidate| semantically_equal(previous, candidate))
                {
                    self.bind_id(index, identity.item_id);
                    return Ok(None);
                }
                return Err(invalid(format!(
                    "upstream stream contains conflicting repeated output item.done for output[{}]",
                    index.get()
                )));
            }
            if let Some(item) = candidate {
                // The completed item is measured once, comprehensively; only
                // what streaming could not account for (metadata supplied at
                // completion, containers of done-only parts) is charged here.
                let mut account = slot.account;
                account.reconcile(budget, item.retained_bytes())?;
                self.bind_id(index, identity.item_id);
                if let Some(slot) = self.slots.get_mut(&index) {
                    slot.state = SlotState::Done(item);
                    slot.account = account;
                }
                return Ok(Some(index));
            }
            return Ok(None);
        }
        if let Some(
            mut item @ (OutputItem::Reasoning(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::McpCall(_)
            | OutputItem::McpListTools(_)
            | OutputItem::Compaction(_)),
        ) = parsed
        {
            if let OutputItem::WebSearchCall(call) = &mut item
                && call.id.is_empty()
            {
                call.id = uuid7_str("ws_");
            }
            let mut account = RetainedAccount::default();
            account.charge(budget, item.retained_bytes())?;
            self.insert(index, identity.item_id, SlotState::Done(item), account);
            return Ok(Some(index));
        }
        Ok(None)
    }
}

fn invalid(message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}

fn identity_mismatch() -> ExecutorError {
    invalid("upstream stream event does not match its active output item: inconsistent item ID, output_index, or kind")
}

fn no_active_item() -> ExecutorError {
    invalid("upstream stream event has no active output item")
}

fn semantically_equal(left: &OutputItem, right: &OutputItem) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[derive(Debug)]
pub(super) struct Slot {
    pub(super) item_id: Option<String>,
    pub(super) state: SlotState,
    /// Retained bytes charged for this slot so far.
    pub(super) account: RetainedAccount,
}

/// A completed item owns no streaming buffers and cannot accept further deltas.
pub(super) enum SlotState {
    Active(ActiveItem),
    Done(OutputItem),
}

impl std::fmt::Debug for SlotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active(item) => f.debug_tuple("Active").field(item).finish(),
            Self::Done(item) => f.debug_tuple("Done").field(&SSEItemType::try_from(item).ok()).finish(),
        }
    }
}

impl SlotState {
    pub(super) fn item_type(&self) -> Option<SSEItemType> {
        match self {
            Self::Active(item) => Some(item.item_type()),
            Self::Done(item) => SSEItemType::try_from(item).ok(),
        }
    }

    pub(super) fn done_item(&self) -> Option<&OutputItem> {
        match self {
            Self::Active(_) => None,
            Self::Done(item) => Some(item),
        }
    }

    pub(super) fn finalize(self) -> Option<OutputItem> {
        match self {
            Self::Active(item) => item.finalize(),
            Self::Done(item) => Some(item),
        }
    }

    /// Resolves authoritative completion with the same per-kind fallbacks for
    /// an active item and a repeat of a retained completed item.
    pub(super) fn completion_candidate(
        &self,
        payload: &EventPayload,
        done_item: Option<&OutputItem>,
        item_id: &str,
    ) -> Option<OutputItem> {
        let mut candidate = match self {
            Self::Active(item) => item.clone(),
            Self::Done(item) => ActiveItem::from_completed(item.clone())?,
        };
        candidate.merge_completion(payload, done_item, item_id);
        candidate.finalize()
    }
}
