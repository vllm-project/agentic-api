//! Identity and lifecycle of per-round output items.
//!
//! [`SlotMap`] resolves output indexes and item IDs, enforces active/completed
//! transitions, detects duplicate or conflicting completion, and dispatches to
//! the per-kind state in [`super::active`]. Retained-byte accounting is owned by
//! each slot's [`RetainedAccount`]: incremental growth is charged by the active
//! state as it happens, and completion and finalization reconcile against the
//! comprehensive [`RetainedSize`] measurement of the completed item.

mod identity;

use super::Validation;
use super::active::ActiveItem;
use std::collections::HashMap;

use indexmap::IndexMap;

use crate::events::{EventPayload, SSEItemType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{ExecutorResponseBudget, RetainedAccount, RetainedSize};
use crate::types::io::{AgentAttribution, OutputItem, ReasoningOutput};
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
    /// Shared content events resolve their item kind from the existing slot.
    pub(super) item_type: Option<SSEItemType>,
    pub(super) event_agent: Option<&'a AgentAttribution>,
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
    // Lenient completion may recover omitted fields from streaming state. That
    // compatibility behavior is not proof that interrupting upstream is safe.
    completion_uncertain: bool,
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

    pub(super) fn at_steering_boundary(&self) -> bool {
        !self.slots.is_empty() && !self.has_active() && !self.completion_uncertain
    }

    pub(super) fn get(&self, index: OutputIndex) -> Option<&Slot> {
        self.slots.get(&index)
    }

    pub(super) fn drain_output_with_budget(
        &mut self,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Vec<OutputItem>> {
        // Collaboration items have no public incomplete status. Do not fabricate
        // completion from an opening snapshot on truncated lenient streams.
        if self
            .slots
            .values()
            .any(|slot| matches!(&slot.state, SlotState::Active(item) if item.item_type().is_collaboration()))
        {
            return Err(invalid("upstream stream ended with unfinished collaboration items"));
        }
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

    fn insert(
        &mut self,
        index: OutputIndex,
        identity: ItemIdentity<'_>,
        state: SlotState,
        mut account: RetainedAccount,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<()> {
        let item_id = identity.item_id;
        if let Some(agent) = identity.event_agent {
            account.charge(budget, agent.retained_bytes())?;
        }
        if let Some(id) = item_id {
            // Identity indexes share the logical item ID charge. A pending kind
            // without its typed item must still pay before the indexes grow.
            if state.id() != Some(id) {
                account.charge(budget, id.len())?;
            }
            self.indexes_by_id.insert(id.to_owned(), index);
        }
        self.slots.insert(
            index,
            Slot {
                item_id: item_id.map(str::to_owned),
                event_agent: identity.event_agent.cloned(),
                state,
                account,
            },
        );
        Ok(())
    }

    fn bind_identity(
        &mut self,
        index: OutputIndex,
        identity: ItemIdentity<'_>,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<()> {
        if let Some(agent) = identity.event_agent
            && let Some(slot) = self.slots.get_mut(&index)
            && slot.event_agent.is_none()
        {
            slot.account.charge(budget, agent.retained_bytes())?;
            slot.event_agent = Some(agent.clone());
        }
        if let Some(id) = identity.item_id
            && let Some(slot) = self.slots.get_mut(&index)
            && slot.item_id.is_none()
        {
            if slot.state.id() != Some(id) {
                slot.account.charge(budget, id.len())?;
            }
            slot.item_id = Some(id.to_owned());
            self.indexes_by_id.insert(id.to_owned(), index);
        }
        Ok(())
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
            self.insert(index, identity, SlotState::Active(item), account, budget)?;
            return Ok(Some(index));
        }
        if identity.item_type.is_some_and(SSEItemType::is_collaboration) {
            return Err(invalid("collaboration item has an invalid opening snapshot"));
        }
        // An unmodeled opening may represent hosted work. Ignoring it for
        // lenient compatibility cannot establish a safe interruption point.
        self.completion_uncertain = true;
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
        if let EventPayload::FunctionCallArgsDelta {
            call_id: Some(observed),
            ..
        }
        | EventPayload::FunctionCallArgsDone {
            call_id: Some(observed),
            ..
        } = payload
            && let Some(previous) = slot.state.call_id()
            && !previous.is_empty()
            && !observed.is_empty()
            && previous != observed
        {
            self.completion_uncertain = true;
        }
        if let SlotState::Active(item) = &mut slot.state {
            item.apply_event(payload, &mut slot.account, budget)?;
        }
        self.bind_identity(index, identity, budget)?;
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
        let mut parsed = validated_done_item.cloned().or_else(|| {
            deserialize_from_value_opt::<OutputItem>(raw_item.clone()).or_else(|| {
                (identity.item_type == Some(SSEItemType::Reasoning))
                    .then(|| ReasoningOutput::try_from(payload).ok().map(OutputItem::Reasoning))
                    .flatten()
            })
        });
        if let Some(previous) = self.slots.get(&index).and_then(|slot| slot.state.call_id())
            && !previous.is_empty()
            && parsed.as_ref().and_then(super::identity::output_item_call_id) != Some(previous)
        {
            self.completion_uncertain = true;
        }
        if let Some(OutputItem::CodeInterpreterCall(call)) = &mut parsed
            && call.id.is_empty()
            && let Some(SlotState::Done(OutputItem::CodeInterpreterCall(previous))) =
                self.slots.get(&index).map(|slot| &slot.state)
        {
            call.id.clone_from(&previous.id);
        }
        if let Some(slot) = self.slots.get(&index) {
            if let SlotState::Active(active) = &slot.state {
                self.completion_uncertain |= !active.validate_completion(parsed.as_ref(), raw_item)?;
            }
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
                self.bind_identity(index, identity, budget)?;
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
                    self.bind_identity(index, identity, budget)?;
                    return Ok(None);
                }
                return Err(invalid(format!(
                    "upstream stream contains conflicting repeated output item.done for output[{}]",
                    index.get()
                )));
            }
            if let Some(item) = candidate {
                // Recovery can synthesize completed fields from an unfinished
                // stream. Preserve that existing behavior, but require the
                // actual done snapshot to agree before cutting upstream.
                self.completion_uncertain |= !parsed
                    .as_ref()
                    .is_some_and(|original| semantically_equal(&item, original));
                // The completed item is measured once, comprehensively; only
                // what streaming could not account for (metadata supplied at
                // completion, containers of done-only parts) is charged here.
                let mut account = slot.account;
                account.reconcile(
                    budget,
                    item.retained_bytes() + slot.event_agent.as_ref().map_or(0, RetainedSize::retained_bytes),
                )?;
                if let Some(slot) = self.slots.get_mut(&index) {
                    slot.state = SlotState::Done(item);
                    slot.account = account;
                }
                self.bind_identity(index, identity, budget)?;
                return Ok(Some(index));
            }
            return Ok(None);
        }
        // A lenient done-only recovery has no observed opening to validate.
        // Retain ordinary compatibility, but await the natural terminal before
        // treating this response as settled.
        self.completion_uncertain = true;
        self.complete_done_only(index, identity, parsed, budget)
    }

    fn complete_done_only(
        &mut self,
        index: OutputIndex,
        identity: ItemIdentity<'_>,
        parsed: Option<OutputItem>,
        budget: Option<&ExecutorResponseBudget>,
    ) -> ExecutorResult<Option<OutputIndex>> {
        if let Some(
            mut item @ (OutputItem::Reasoning(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::CodeInterpreterCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::McpCall(_)
            | OutputItem::McpListTools(_)
            | OutputItem::Compaction(_)
            | OutputItem::MultiAgentCall(_)
            | OutputItem::MultiAgentCallOutput(_)
            | OutputItem::AgentMessage(_)),
        ) = parsed
        {
            if let OutputItem::WebSearchCall(call) = &mut item
                && call.id.is_empty()
            {
                call.id = uuid7_str("ws_");
            }
            if let OutputItem::CodeInterpreterCall(call) = &mut item
                && call.id.is_empty()
            {
                call.id = uuid7_str("ci_");
            }
            let mut account = RetainedAccount::default();
            account.charge(budget, item.retained_bytes())?;
            self.insert(index, identity, SlotState::Done(item), account, budget)?;
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
    /// Observed event attribution, independent of embedded-item attribution.
    /// An omitted agent is unspecified and must never become an implicit root.
    event_agent: Option<AgentAttribution>,
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
    fn call_id(&self) -> Option<&str> {
        match self {
            Self::Active(ActiveItem::FunctionCall(call)) => Some(&call.item.call_id),
            Self::Active(ActiveItem::CustomToolCall(call)) => Some(&call.item.call_id),
            Self::Active(ActiveItem::ShellCall(call)) => Some(&call.item.call_id),
            Self::Active(ActiveItem::ToolSearchCall { item }) => Some(&item.call_id),
            Self::Done(item) => super::identity::output_item_call_id(item),
            Self::Active(_) => None,
        }
    }

    fn id(&self) -> Option<&str> {
        match self {
            Self::Active(item) => item.id(),
            Self::Done(item) => item.id(),
        }
    }

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
