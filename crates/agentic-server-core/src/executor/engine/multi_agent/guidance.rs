//! Fresh, model-only ownership context; never part of the durable agent history.
use super::MultiAgentRun;
use crate::executor::multi_agent::{AgentState, collaboration::instructions};
use crate::types::agent::AgentTurnKey;
use crate::types::io::{InputMessage, InputMessageContent};

impl MultiAgentRun {
    pub(super) fn round_guidance(&self, turn: &AgentTurnKey) -> InputMessage {
        let context = &self.contexts[&turn.agent];
        let children = self
            .registry
            .agents()
            .filter(|agent| agent.parent == Some(&turn.agent))
            .map(|agent| format!("{}: {:?}", agent.identity, agent.state))
            .collect::<Vec<_>>();
        let ownership = if children.is_empty() && turn.agent.is_root() {
            "You have no direct children yet. Delegate independent tasks when useful; wait only after successful delegation."
                .to_owned()
        } else if children.is_empty() {
            "You have no direct children. No child result is outstanding. Complete your assigned work yourself. \
             Your parent's delegation request is already fulfilled by your existence. \
             Return your own findings without recreating the team or waiting for siblings."
                .to_owned()
        } else {
            format!(
                "Your direct children and their current states:\n{}",
                children.join("\n")
            )
        };
        let active_subagents = self
            .registry
            .agents()
            .filter(|agent| !agent.identity.is_root() && matches!(agent.state, AgentState::Active(_)))
            .count();
        let available_slots = self.limit.saturating_sub(active_subagents);
        let task = if turn.agent.is_root() {
            "Your assignment is the user's overall request."
        } else {
            &context.stored.last_task
        };
        InputMessage {
            role: "developer".into(),
            content: InputMessageContent::Text(format!(
                "{}\n\nCurrent agent ownership (gateway state):\n{}\n\n\
                 Shared subagent capacity: {} of {} slots are occupied; {} are free right now. \
                 In this model round, call spawn_agent at most {} times. If no slot is free, \
                 do not call spawn_agent; finish your own assignment or coordinate with existing agents. \
                 Other agents may claim a free slot before your calls execute.\n\nYour current assignment:\n{}",
                instructions(&turn.agent, self.limit),
                ownership,
                active_subagents,
                self.limit,
                available_slots,
                available_slots,
                task
            )),
            ..Default::default()
        }
    }
}
