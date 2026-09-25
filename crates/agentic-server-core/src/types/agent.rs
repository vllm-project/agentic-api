//! Gateway agent identities, distinct from optional public transcript attribution.
//!
//! These values identify an agent and one activation within a stored tree. They
//! contain no task handles and can survive interruption and checkpoint encoding.
//! Tree restoration must additionally validate membership and references.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical task path within one agent tree, such as `/root/review`.
///
/// This validates path structure, not the public collaboration tool's task-name
/// schema. Relative target resolution belongs to the coordinator. Identical paths
/// in independent continuation branches do not identify the same live agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentIdentity(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("agent identity must be /root or a descendant path with nonempty, non-relative components")]
pub struct InvalidAgentIdentity;

impl AgentIdentity {
    #[must_use]
    pub fn root() -> Self {
        Self("/root".into())
    }

    /// Construct a direct child's canonical identity without path normalization.
    ///
    /// # Errors
    /// Rejects an empty label, `/`, control characters, `.` or `..` components.
    pub fn child(&self, task_name: &str) -> Result<Self, InvalidAgentIdentity> {
        if !valid_component(task_name) {
            return Err(InvalidAgentIdentity);
        }
        Ok(Self(format!("{}/{task_name}", self.0)))
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "/root"
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_component(component: &str) -> bool {
    !component.is_empty()
        && !matches!(component, "." | "..")
        && !component.contains('/')
        && !component.chars().any(char::is_control)
}

impl TryFrom<String> for AgentIdentity {
    type Error = InvalidAgentIdentity;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let mut components = value.split('/');
        if components.next() != Some("") || components.next() != Some("root") || !components.all(valid_component) {
            return Err(InvalidAgentIdentity);
        }
        Ok(Self(value))
    }
}

impl From<AgentIdentity> for String {
    fn from(value: AgentIdentity) -> Self {
        value.0
    }
}

impl fmt::Display for AgentIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Identity of one activation. Resuming a paused turn retains this value;
/// starting a new follow-up activation creates a new one. Never use a Tokio
/// task ID or an agent's display status as a substitute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentTurnId(Uuid);

impl AgentTurnId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AgentTurnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Exact owner of work or a pending client call within a particular tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentTurnKey {
    pub agent: AgentIdentity,
    pub turn: AgentTurnId,
}

/// Canonical plaintext mail retained by the gateway, never a public
/// `agent_message` or an upstream provider's encrypted collaboration payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMail {
    pub sender: AgentTurnKey,
    pub content: AgentMailContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMailContent {
    Message(String),
    Task(String),
    TurnFinished(AgentCompletion),
}

/// Logical completion of a turn after its work has been reconciled and joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCompletion {
    Finished(String),
    Interrupted,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths_distinguish_root_children_and_nested_agents() {
        let root = AgentIdentity::root();
        let child = root.child("review").unwrap();
        let nested = child.child("tests").unwrap();
        assert!(root.is_root());
        assert!(!child.is_root());
        assert_eq!(nested.as_str(), "/root/review/tests");
        assert_eq!(AgentIdentity::try_from(nested.to_string()).unwrap(), nested);
        for label in ["", ".", "..", "a/b", "bad\nname"] {
            assert!(root.child(label).is_err(), "{label:?}");
        }
    }

    #[test]
    fn deserialization_cannot_bypass_canonical_identity_validation() {
        for path in [
            "",
            "root",
            "/rooted",
            "/other",
            "/root/",
            "/root//a",
            "/root/../a",
            "/root/a\n",
        ] {
            let encoded = serde_json::to_string(path).unwrap();
            assert!(serde_json::from_str::<AgentIdentity>(&encoded).is_err(), "{path:?}");
        }
    }

    #[test]
    fn serialized_ownership_preserves_old_turns_and_distinguishes_followups() {
        let agent = AgentIdentity::root().child("review").unwrap();
        let original = AgentTurnKey {
            agent: agent.clone(),
            turn: AgentTurnId::new(),
        };
        let encoded = serde_json::to_string(&original).unwrap();
        let restored: AgentTurnKey = serde_json::from_str(&encoded).unwrap();
        let followup = AgentTurnKey {
            agent,
            turn: AgentTurnId::new(),
        };
        assert_eq!(original, restored);
        assert_eq!(original.agent, followup.agent);
        assert_ne!(original.turn, followup.turn);
    }
}
