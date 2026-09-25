//! Wire types for the Responses multi-agent configuration and collaboration items.
//!
//! These values describe the public transcript, not executable collaboration commands or
//! scheduler state. Request admission owns defaults, deployment limits, and the `store: true`
//! requirement; deserializing these types does not enable multi-agent execution.

use serde::{Deserialize, Serialize};

use super::output::OutputTextContent;

/// Multi-agent options supplied in a Responses request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MultiAgentConfig {
    pub enabled: bool,
    /// Concurrent active descendant turns, excluding the root; not a lifetime agent count.
    ///
    /// Preserve omission on the wire. Admission resolves the documented default of three
    /// and validates the requested value before scheduling any work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_subagents: Option<u64>,
}

/// Agent attribution attached to an output item or streaming event.
///
/// An enclosing item or event can omit attribution. Event attribution and embedded-item
/// attribution are independent: discovery items can omit it even when their events carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AgentAttribution {
    /// Canonical task path, such as `/root` or `/root/assess_alpha`.
    pub agent_name: String,
}

/// The hosted collaboration actions exposed by the Responses multi-agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MultiAgentAction {
    SpawnAgent,
    SendMessage,
    FollowupTask,
    WaitAgent,
    InterruptAgent,
    ListAgents,
}

/// A public `multi_agent_call` item, executed by the API server.
///
/// The enclosing input/output item enum supplies the `type` discriminator. These items
/// have no call-execution status field in the recorded protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MultiAgentCall {
    pub id: String,
    pub call_id: String,
    pub action: MultiAgentAction,
    /// JSON encoded as a string on the wire. Message arguments can contain ciphertext.
    ///
    /// Preserve the string verbatim; do not execute it as a plaintext runtime command.
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

/// A text part returned by a hosted collaboration action.
///
/// Share the assistant output-text payload, preserving annotations and log probabilities,
/// including the distinction between absent metadata and empty arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MultiAgentCallOutputContent {
    OutputText(OutputTextContent),
}

/// A public `multi_agent_call_output` item produced by the API server.
///
/// The enclosing input/output item enum supplies the `type` discriminator. `call_id`
/// links this output to its collaboration call; `id` identifies the output item itself.
/// Output text can be an empty acknowledgement or a JSON-encoded action result, including
/// a recoverable action error. Preserve that text without interpreting it as an HTTP
/// error or an agent lifecycle transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MultiAgentCallOutput {
    pub id: String,
    pub call_id: String,
    pub action: MultiAgentAction,
    pub output: Vec<MultiAgentCallOutputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

/// A content part in a public inter-agent message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageContent {
    EncryptedContent {
        /// Opaque public content, distinct from canonical plaintext agent context.
        encrypted_content: String,
    },
}

/// A public `agent_message` item delivered between agents.
///
/// The enclosing input/output item enum supplies the `type` discriminator. Attribution
/// identifies the recipient in the recordings; `author` identifies the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AgentMessage {
    pub id: String,
    pub author: String,
    pub recipient: String,
    pub content: Vec<AgentMessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_items_roundtrip_and_stay_out_of_model_context() {
        use crate::tool::ToolRegistry;
        use crate::types::io::{InputItem, OutputItem, ResponsesInput};
        let items = [
            serde_json::json!({"type": "multi_agent_call", "id": "mac_test", "call_id": "call_test",
                "action": "send_message", "arguments": "{ \"message\": \"opaque\" }",
                "agent": {"agent_name": "/root"}}),
            serde_json::json!({"type": "multi_agent_call_output", "id": "maco_test", "call_id": "call_test",
                "action": "send_message", "output": [{"type": "output_text", "text": ""}]}),
            serde_json::json!({"type": "agent_message", "id": "amsg_test", "author": "/root",
                "recipient": "/root/review", "agent": {"agent_name": "/root/review"},
                "content": [{"type": "encrypted_content", "encrypted_content": "opaque"}]}),
        ];
        let mut inputs = Vec::new();
        for wire in items {
            let output: OutputItem = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(output.id(), wire["id"].as_str());
            assert!(!output.requires_client_action(&ToolRegistry::default()));
            assert_eq!(serde_json::to_value(&output).unwrap(), wire);
            let input: InputItem = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&input).unwrap(), wire);
            assert_eq!(serde_json::to_value(output.to_input_item().unwrap()).unwrap(), wire);
            inputs.push(input);
        }
        let InputItem::MultiAgentCall(call) = &inputs[0] else {
            panic!("call")
        };
        let InputItem::MultiAgentCallOutput(output) = &inputs[1] else {
            panic!("call output")
        };
        assert_eq!(call.call_id, output.call_id);
        assert_ne!(call.id, output.id);
        assert_eq!(
            serde_json::to_value(ResponsesInput::Items(inputs).model_input()).unwrap(),
            serde_json::json!([])
        );
    }

    #[test]
    fn configuration_preserves_explicit_and_omitted_concurrency() {
        for limit in [None, Some(3)] {
            let mut wire = serde_json::json!({"enabled": true});
            if let Some(limit) = limit {
                wire["max_concurrent_subagents"] = serde_json::json!(limit);
            }
            let config: MultiAgentConfig = serde_json::from_value(wire.clone()).unwrap();
            assert!(config.enabled);
            assert_eq!(config.max_concurrent_subagents, limit);
            assert_eq!(serde_json::to_value(config).unwrap(), wire);
        }
    }

    #[test]
    fn collaboration_actions_use_protocol_names() {
        for (action, name) in [
            (MultiAgentAction::SpawnAgent, "spawn_agent"),
            (MultiAgentAction::SendMessage, "send_message"),
            (MultiAgentAction::FollowupTask, "followup_task"),
            (MultiAgentAction::WaitAgent, "wait_agent"),
            (MultiAgentAction::InterruptAgent, "interrupt_agent"),
            (MultiAgentAction::ListAgents, "list_agents"),
        ] {
            let wire = serde_json::json!(name);
            assert_eq!(serde_json::to_value(action).unwrap(), wire);
            assert_eq!(serde_json::from_value::<MultiAgentAction>(wire).unwrap(), action);
        }
    }

    #[test]
    fn call_preserves_opaque_arguments_and_optional_attribution() {
        let arguments = r#"{ "target": "review", "message": "opaque-test-value" }"#;
        for agent in [None, Some(serde_json::json!({"agent_name": "/root"}))] {
            let mut wire = serde_json::json!({
                "id": "mac_test",
                "call_id": "call_test",
                "action": "send_message",
                "arguments": arguments
            });
            if let Some(agent) = &agent {
                wire["agent"] = agent.clone();
            }
            let call: MultiAgentCall = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(call.arguments, arguments);
            assert_eq!(call.agent.is_some(), agent.is_some());
            assert_eq!(serde_json::to_value(call).unwrap(), wire);
        }
    }

    #[test]
    fn collaboration_output_preserves_acknowledgements_and_action_results() {
        for (action, text) in [
            ("spawn_agent", r#"{"task_name": "/root/review"}"#),
            ("send_message", ""),
            ("followup_task", ""),
            ("wait_agent", r#"{"message": "Wait timed out.", "timed_out": true}"#),
            ("wait_agent", r#"{"error": "timeout_ms must be at least 10000"}"#),
            ("interrupt_agent", r#"{"previous_status": "running"}"#),
            ("list_agents", r#"{"agents": []}"#),
        ] {
            let wire = serde_json::json!({
                "id": "maco_test",
                "call_id": "call_test",
                "action": action,
                "agent": {"agent_name": "/root"},
                "output": [{"type": "output_text", "text": text, "annotations": [], "logprobs": []}]
            });
            let output: MultiAgentCallOutput = serde_json::from_value(wire.clone()).unwrap();
            let MultiAgentCallOutputContent::OutputText(part) = &output.output[0];
            assert_eq!(part.text, text);
            assert_eq!(output.call_id, "call_test");
            assert_eq!(output.id, "maco_test");
            assert_eq!(serde_json::to_value(output).unwrap(), wire);
        }
    }

    #[test]
    fn collaboration_output_preserves_omitted_attribution_and_part_metadata() {
        let wire = serde_json::json!({
            "id": "maco_test",
            "call_id": "call_test",
            "action": "send_message",
            "output": [{"type": "output_text", "text": ""}]
        });
        let output: MultiAgentCallOutput = serde_json::from_value(wire.clone()).unwrap();
        assert!(output.agent.is_none());
        assert_eq!(serde_json::to_value(output).unwrap(), wire);
    }

    #[test]
    fn message_preserves_sender_recipient_and_encrypted_content() {
        let wire = serde_json::json!({
            "id": "amsg_test",
            "author": "/root/review",
            "recipient": "/root",
            "agent": {"agent_name": "/root"},
            "content": [{"type": "encrypted_content", "encrypted_content": "opaque-test-value"}]
        });
        let message: AgentMessage = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(message.author, "/root/review");
        assert_eq!(message.recipient, "/root");
        assert_eq!(
            message.content,
            [AgentMessageContent::EncryptedContent {
                encrypted_content: "opaque-test-value".into(),
            }]
        );
        assert_eq!(serde_json::to_value(message).unwrap(), wire);
    }
}
