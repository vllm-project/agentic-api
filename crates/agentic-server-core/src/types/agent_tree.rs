//! Versioned canonical tree checkpoint. This is never an upstream or public wire item.
use super::agent::{AgentIdentity, AgentMail, AgentTurnId};
use super::client_calls::{ClientCallId, ClientCallOwner};
use super::io::{InputItem, MultiAgentConfig};
use super::tools::ResponsesTool;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentPhase {
    Runnable,
    Inferring,
    ExecutingTools,
    WaitingForClientOutputs,
    WaitingForMailbox,
}

/// Internal lifecycle state; not the public `list_agents` status schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentState {
    Active(AgentPhase),
    Idle,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAgent {
    pub identity: AgentIdentity,
    pub parent: Option<AgentIdentity>,
    pub turn: AgentTurnId,
    pub state: AgentState,
    pub mailbox: Vec<AgentMail>,
    pub history: Vec<InputItem>,
    pub loaded_tools: Vec<ResponsesTool>,
    pub last_task: String,
    pub final_answer: Option<String>,
    pub rounds: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<StoredAgentWait>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAgentWait {
    pub call_id: String,
    pub deadline_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredClientCall {
    pub call_id: ClientCallId,
    pub owner: ClientCallOwner,
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTreeSnapshot {
    pub version: u32,
    pub config: MultiAgentConfig,
    pub agents: Vec<StoredAgent>,
    pub client_calls: Vec<StoredClientCall>,
}
