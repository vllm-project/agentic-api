use super::{Budget, invalid};
use crate::events::EventPayload;
use crate::executor::error::ExecutorResult;
use crate::executor::response_budget::{RetainedAccount, RetainedSize};
use crate::types::io::{AgentMessage, AgentMessageContent};
use indexmap::IndexMap;

/// Encrypted parts have a completed-only lifecycle. Keep their observed indexes
/// until item completion so the authoritative snapshot cannot contradict them.
#[derive(Clone)]
pub(in crate::executor::accumulator) struct AgentMessageState {
    pub(super) item: AgentMessage,
    pub(super) parts: IndexMap<u32, AgentMessageContent>,
}

impl AgentMessageState {
    pub(super) fn new(item: AgentMessage) -> Self {
        Self {
            item,
            parts: IndexMap::new(),
        }
    }

    pub(super) fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        let EventPayload::AgentMessageContentDone {
            content_index, part, ..
        } = payload
        else {
            return Ok(());
        };
        if self.parts.contains_key(content_index) {
            return Err(invalid("agent message repeats a completed content part"));
        }
        account.charge(budget, part.retained_bytes())?;
        self.parts.insert(*content_index, part.clone());
        Ok(())
    }

    pub(super) fn validate_completion(&self, done: &AgentMessage) -> ExecutorResult<()> {
        if self.item.author != done.author || self.item.recipient != done.recipient || self.item.agent != done.agent {
            return Err(invalid(
                "agent message completion changes author, recipient, or attribution",
            ));
        }
        for (index, part) in &self.parts {
            if done.content.get(*index as usize) != Some(part) {
                return Err(invalid("agent message completion contradicts a completed content part"));
            }
        }
        Ok(())
    }
}

impl RetainedSize for AgentMessageState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.parts.values().map(RetainedSize::retained_bytes).sum::<usize>()
    }
}
