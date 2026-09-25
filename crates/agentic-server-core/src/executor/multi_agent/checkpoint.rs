//! Validate durable trees before constructing live registries or discovering tools.
use std::collections::{HashMap, HashSet};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::pending_calls::{CallKind, pending_calls};
use crate::types::agent_tree::{AgentPhase, AgentState, StoredTreeSnapshot};
use crate::types::client_calls::ClientCallKind;
use crate::types::io::InputItem;
use crate::utils::common::serialized_size_up_to;

pub struct CheckpointLimits {
    pub max_agents: usize,
    pub max_mailbox_messages: usize,
    pub max_client_calls: usize,
    pub max_bytes: usize,
}

#[derive(Debug)]
pub struct ValidatedTreeCheckpoint(StoredTreeSnapshot);

impl CheckpointLimits {
    pub(in crate::executor) fn for_response(max_bytes: usize) -> Self {
        Self {
            max_agents: 1024,
            max_mailbox_messages: 1024,
            max_client_calls: 4096,
            max_bytes,
        }
    }
}

impl ValidatedTreeCheckpoint {
    /// # Errors
    /// Rejects unsupported versions, invalid references, unsafe states and resource limits.
    pub fn from_stored(snapshot: StoredTreeSnapshot, limits: &CheckpointLimits) -> ExecutorResult<Self> {
        if snapshot.version != 1 || !snapshot.config.enabled || snapshot.config.max_concurrent_subagents == Some(0) {
            return Err(invalid("unsupported checkpoint version or configuration"));
        }
        if snapshot.agents.is_empty()
            || snapshot.agents.len() > limits.max_agents
            || snapshot.client_calls.len() > limits.max_client_calls
            || serialized_size_up_to(&snapshot, limits.max_bytes)
                .map_err(ExecutorError::JsonError)?
                .is_none()
        {
            return Err(invalid("checkpoint exceeds retention limits"));
        }
        let mut agents = HashSet::new();
        let mut unresolved = HashMap::new();
        for agent in &snapshot.agents {
            if agents.contains(&agent.identity) || agent.mailbox.len() > limits.max_mailbox_messages {
                return Err(invalid("duplicate agent or oversized mailbox"));
            }
            match &agent.parent {
                None if agents.is_empty() && agent.identity.is_root() => {}
                Some(parent)
                    if agents.contains(parent)
                        && agent
                            .identity
                            .as_str()
                            .rsplit_once('/')
                            .is_some_and(|(path, _)| path == parent.as_str()) => {}
                _ => return Err(invalid("invalid checkpoint parent reference")),
            }
            if matches!(
                agent.state,
                AgentState::Active(AgentPhase::Inferring | AgentPhase::ExecutingTools)
            ) {
                return Err(invalid("checkpoint contains live work"));
            }
            let calls = pending_calls(&agent.history)?;
            if let Some(wait) = &agent.wait {
                if !calls.iter().any(|call| call.call_id == wait.call_id)
                    || !agent.history.iter().any(|item| {
                        matches!(item, InputItem::FunctionCall(call)
                        if call.call_id == wait.call_id && call.name == "wait_agent")
                    })
                {
                    return Err(invalid("checkpoint wait has no matching collaboration call"));
                }
            }
            for call in calls {
                if agent.wait.as_ref().is_some_and(|wait| wait.call_id == call.call_id) {
                    continue;
                }
                let kind = match call.kind {
                    CallKind::Function => ClientCallKind::Function,
                    CallKind::Shell => ClientCallKind::Shell,
                    CallKind::Custom => return Err(invalid("unsupported unresolved call kind")),
                };
                unresolved.insert((agent.identity.clone(), call.call_id), kind);
            }
            agents.insert(agent.identity.clone());
        }
        for agent in &snapshot.agents {
            if agent.mailbox.iter().any(|mail| !agents.contains(&mail.sender.agent)) {
                return Err(invalid("checkpoint mail has an unknown sender"));
            }
        }
        let mut ids = HashSet::new();
        for call in &snapshot.client_calls {
            if !ids.insert(&call.call_id) || !agents.contains(&call.owner.agent_turn.agent) {
                return Err(invalid("duplicate client call or unknown owner"));
            }
            let kind = unresolved.remove(&(call.owner.agent_turn.agent.clone(), call.call_id.as_str().to_owned()));
            if (!call.resolved && kind != Some(call.owner.kind)) || (call.resolved && kind.is_some()) {
                return Err(invalid("client-call checkpoint disagrees with canonical history"));
            }
        }
        if !unresolved.is_empty() {
            return Err(invalid("unresolved call has no ownership record"));
        }
        Ok(Self(snapshot))
    }

    pub(in crate::executor) fn into_snapshot(self) -> StoredTreeSnapshot {
        self.0
    }
}

fn invalid(message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}
