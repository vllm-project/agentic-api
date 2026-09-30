//! Plaintext model-facing collaboration commands. Public transcript items are separate.

use serde::{Deserialize, Serialize};

use super::io::MultiAgentAction;
use crate::utils::common::deserialize_from_str;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnAgent {
    pub task_name: String,
    pub message: String,
    #[serde(default = "all_turns")]
    pub fork_turns: String,
}

fn all_turns() -> String {
    "all".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTarget {
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTask {
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitAgent {
    #[serde(default = "wait_timeout")]
    pub timeout_ms: u64,
}

fn wait_timeout() -> u64 {
    30_000
}

/// Read-only listing has no argument-dependent behavior. Ignore extra object
/// fields from the model so wrapper fields cannot cause a retry loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAgents {}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum AgentCommand {
    Spawn(SpawnAgent),
    Send(AgentTask),
    Followup(AgentTask),
    Wait(WaitAgent),
    Interrupt(AgentTarget),
    List(ListAgents),
}

impl AgentCommand {
    /// # Errors
    /// Returns a decoding error for malformed arguments or missing required fields.
    pub fn parse(action: MultiAgentAction, arguments: &str) -> Result<Self, serde_json::Error> {
        match action {
            MultiAgentAction::SpawnAgent => deserialize_from_str(arguments).map(Self::Spawn),
            MultiAgentAction::SendMessage => deserialize_from_str(arguments).map(Self::Send),
            MultiAgentAction::FollowupTask => deserialize_from_str(arguments).map(Self::Followup),
            MultiAgentAction::WaitAgent => deserialize_from_str(arguments).map(Self::Wait),
            MultiAgentAction::InterruptAgent => deserialize_from_str(arguments).map(Self::Interrupt),
            MultiAgentAction::ListAgents => deserialize_from_str(arguments).map(Self::List),
        }
    }
}

/// Results of hosted actions. An action error is a successful tool response,
/// distinct from an infrastructure failure of the containing response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CollaborationResult {
    Spawned { task_name: String },
    Interrupted { previous_status: AgentListingStatus },
    Listing { agents: Vec<AgentListing> },
    Wait { message: String, timed_out: bool },
    Error { error: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentListingStatus {
    Running,
    Completed(Option<String>),
    Interrupted,
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentListing {
    pub agent_name: String,
    pub agent_status: AgentListingStatus,
}

impl MultiAgentAction {
    #[must_use]
    pub fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "spawn_agent" => Some(Self::SpawnAgent),
            "send_message" => Some(Self::SendMessage),
            "followup_task" => Some(Self::FollowupTask),
            "wait_agent" => Some(Self::WaitAgent),
            "interrupt_agent" => Some(Self::InterruptAgent),
            "list_agents" => Some(Self::ListAgents),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_agents_ignores_extra_fields_without_changing_the_command() {
        for arguments in [
            "{}",
            r#"{"parameters":{}}"#,
            r#"{"functions":[{"name":"get_proposal"}]}"#,
            r#"{"function_results":{"get_proposal":"untrusted model text"}}"#,
        ] {
            let command = AgentCommand::parse(MultiAgentAction::ListAgents, arguments).unwrap();
            assert!(matches!(command, AgentCommand::List(_)));
            assert_eq!(serde_json::to_string(&command).unwrap(), "{}");
        }
    }

    #[test]
    fn list_agents_still_rejects_malformed_json() {
        for arguments in ["", "{", "null", "true", "42", r#""not an object""#] {
            assert!(AgentCommand::parse(MultiAgentAction::ListAgents, arguments).is_err());
        }
    }

    #[test]
    fn other_collaboration_commands_still_reject_unknown_fields() {
        for (action, arguments) in [
            (
                MultiAgentAction::SpawnAgent,
                r#"{"task_name":"worker","message":"work","extra":true}"#,
            ),
            (
                MultiAgentAction::SendMessage,
                r#"{"target":"worker","message":"hello","extra":true}"#,
            ),
            (
                MultiAgentAction::FollowupTask,
                r#"{"target":"worker","message":"work","extra":true}"#,
            ),
            (MultiAgentAction::WaitAgent, r#"{"timeout_ms":30000,"extra":true}"#),
            (MultiAgentAction::InterruptAgent, r#"{"target":"worker","extra":true}"#),
        ] {
            assert!(AgentCommand::parse(action, arguments).is_err());
        }
    }
}
