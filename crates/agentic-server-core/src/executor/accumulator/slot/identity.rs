//! Resolve slot identifiers without mutating retained item state.

use super::super::Validation;
use super::{ItemIdentity, OutputIndex, SlotAction, SlotMap, SlotState, identity_mismatch, invalid, no_active_item};
use crate::events::SSEItemType;
use crate::executor::error::ExecutorResult;

impl SlotMap {
    /// Resolves without mutating either map; a rejected event cannot bind an ID.
    pub(super) fn resolve(
        &self,
        identity: ItemIdentity<'_>,
        action: SlotAction,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let index_only_shell_command =
            identity.item_type == Some(SSEItemType::ShellCall) && action == SlotAction::Mutate;
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
                    slot.state.item_type() == identity.item_type
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
        if let (Some(bound), Some(supplied)) = (&slot.event_agent, identity.event_agent)
            && bound != supplied
        {
            return Err(invalid("upstream stream changes an output item's event attribution"));
        }
        let kind_matches = match identity.item_type {
            Some(kind) => slot.state.item_type() == Some(kind),
            None => {
                action == SlotAction::Mutate
                    && matches!(
                        slot.state.item_type(),
                        Some(SSEItemType::Message | SSEItemType::AgentMessage)
                    )
            }
        };
        if !kind_matches {
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
}
