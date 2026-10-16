//! Typed client-call ownership and input batches for gateway coordination.
//! These are internal contracts, not additional Responses wire envelopes.

use std::borrow::Borrow;

use serde::{Deserialize, Serialize};

use super::agent::AgentTurnKey;
use super::io::{
    CustomToolCallOutputMessage, FunctionToolResultMessage, InputItem, ShellCallOutputMessage, ToolSearchOutputMessage,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientCallId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("client call_id must not be empty")]
pub struct EmptyClientCallId;

impl ClientCallId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientCallId {
    type Error = EmptyClientCallId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            Err(EmptyClientCallId)
        } else {
            Ok(Self(value))
        }
    }
}

impl From<ClientCallId> for String {
    fn from(value: ClientCallId) -> Self {
        value.0
    }
}

impl Borrow<str> for ClientCallId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Kinds qualified by the multi-agent HTTP reference recordings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientCallKind {
    Function,
    Shell,
    Custom,
    ToolSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCallOwner {
    pub agent_turn: AgentTurnKey,
    pub kind: ClientCallKind,
    /// The call is async: its owner does not wait for the output, and repeated outputs are
    /// accepted. Absent in checkpoints written before async tools, which therefore read as false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub async_execution: bool,
}

/// Registered only after tool ownership classification: built-in calls and
/// collaboration calls must not enter the client continuation table.
#[derive(Debug, Clone)]
pub struct ClientCallRegistration {
    pub call_id: ClientCallId,
    pub owner: ClientCallOwner,
}

#[derive(Debug, Clone)]
pub enum ClientToolOutput {
    Function(FunctionToolResultMessage),
    Shell(ShellCallOutputMessage),
    Custom(CustomToolCallOutputMessage),
    ToolSearch(ToolSearchOutputMessage),
}

impl ClientToolOutput {
    #[must_use]
    pub fn call_id(&self) -> &str {
        match self {
            Self::Function(output) => &output.call_id,
            Self::Shell(output) => &output.call_id,
            Self::Custom(output) => &output.call_id,
            Self::ToolSearch(output) => &output.call_id,
        }
    }

    #[must_use]
    pub fn kind(&self) -> ClientCallKind {
        match self {
            Self::Function(_) => ClientCallKind::Function,
            Self::Shell(_) => ClientCallKind::Shell,
            Self::Custom(_) => ClientCallKind::Custom,
            Self::ToolSearch(_) => ClientCallKind::ToolSearch,
        }
    }
}

impl From<ClientToolOutput> for InputItem {
    fn from(output: ClientToolOutput) -> Self {
        match output {
            ClientToolOutput::Function(output) => Self::FunctionCallOutput(output),
            ClientToolOutput::Shell(output) => Self::ShellCallOutput(output),
            ClientToolOutput::Custom(output) => Self::CustomToolCallOutput(output),
            ClientToolOutput::ToolSearch(output) => Self::ToolSearchOutput(output),
        }
    }
}

#[derive(Debug)]
pub struct ClientToolOutputBatch {
    pub response_id: String,
    pub outputs: Vec<ClientToolOutput>,
}

/// Transfer to canonical context does not itself authorize resuming a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCallResolution {
    Pending,
    Accepted,
    Transferred,
}

#[derive(Debug)]
pub struct RoutedClientOutput {
    pub owner: ClientCallOwner,
    pub output: ClientToolOutput,
}
