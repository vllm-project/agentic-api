use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::multi_agent::AgentAttribution;

/// Lifecycle status for a shell call or shell call output item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ShellCallStatus {
    InProgress,
    Completed,
    Incomplete,
}

impl From<crate::types::event::MessageStatus> for ShellCallStatus {
    fn from(status: crate::types::event::MessageStatus) -> Self {
        match status {
            crate::types::event::MessageStatus::InProgress => Self::InProgress,
            crate::types::event::MessageStatus::Completed => Self::Completed,
        }
    }
}

/// A supplied shell limit can be numeric or explicitly null.
/// The enclosing `Option` distinguishes either case from an omitted field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum ShellCallLimit {
    Value(u64),
    Null,
}

/// Commands and execution limits requested by a model-generated shell call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ShellCallAction {
    pub commands: Vec<String>,
    /// `None` omits the field; `Some(ShellCallLimit::Null)` preserves an explicit null.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_limit",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout_ms: Option<ShellCallLimit>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_limit",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_length: Option<ShellCallLimit>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

fn deserialize_optional_limit<'de, D>(deserializer: D) -> Result<Option<ShellCallLimit>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ShellCallLimit::deserialize(deserializer).map(Some)
}

/// A model-generated request to execute one or more shell commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ShellCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub action: ShellCallAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ShellCallStatus>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

/// Outcome of one command in a shell call output.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellCallOutcome {
    Exit {
        exit_code: i32,
    },
    Timeout,
    #[serde(other)]
    Unknown,
}

/// Captured output and outcome for one command in a shell call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ShellCallOutputContent {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub outcome: ShellCallOutcome,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

/// Output supplied for a previously emitted shell call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ShellCallOutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_length: Option<u64>,
    pub output: Vec<ShellCallOutputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ShellCallStatus>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_preserves_absent_null_and_numeric_limits_and_environment() {
        for mut action in [
            serde_json::json!({"commands": ["pwd"]}),
            serde_json::json!({"commands": ["pwd"], "timeout_ms": null, "max_output_length": null}),
            serde_json::json!({"commands": ["pwd"], "timeout_ms": 1000, "max_output_length": 4096}),
        ] {
            action["extension"] = serde_json::json!(true);
            let mut wire = serde_json::json!({"id": "sh_test", "call_id": "call_test", "action": action});
            for environment in [
                None,
                Some(serde_json::Value::Null),
                Some(serde_json::json!({"type": "local"})),
            ] {
                if let Some(environment) = environment {
                    wire["environment"] = environment;
                }
                let call: ShellCall = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(serde_json::to_value(call).unwrap(), wire);
            }
        }
    }

    #[test]
    fn shell_call_round_trips_with_limits_and_extra_fields() {
        let value = serde_json::json!({
            "id": "sh_1",
            "call_id": "call_1",
            "action": {
                "commands": ["pwd", "cargo test"],
                "timeout_ms": 120_000,
                "max_output_length": 4096,
                "future_action_field": true
            },
            "status": "in_progress",
            "future_item_field": "kept"
        });

        let call: ShellCall = serde_json::from_value(value).unwrap();
        assert_eq!(call.action.commands, ["pwd", "cargo test"]);
        assert_eq!(call.action.timeout_ms, Some(ShellCallLimit::Value(120_000)));
        assert_eq!(call.status, Some(ShellCallStatus::InProgress));

        let serialized = serde_json::to_value(call).unwrap();
        assert_eq!(serialized["future_item_field"], "kept");
        assert_eq!(serialized["action"]["future_action_field"], true);
    }

    #[test]
    fn shell_call_output_round_trips_exit_and_timeout_outcomes() {
        let value = serde_json::json!({
            "id": "sho_1",
            "call_id": "call_1",
            "max_output_length": 4096,
            "output": [
                {
                    "stdout": "ok\n",
                    "stderr": "",
                    "outcome": {"type": "exit", "exit_code": 0}
                },
                {
                    "stdout": "",
                    "stderr": "timed out",
                    "outcome": {"type": "timeout"}
                }
            ],
            "status": "completed"
        });

        let output: ShellCallOutputMessage = serde_json::from_value(value).unwrap();
        assert_eq!(output.output.len(), 2);
        assert_eq!(output.output[0].outcome, ShellCallOutcome::Exit { exit_code: 0 });
        assert_eq!(output.output[1].outcome, ShellCallOutcome::Timeout);
        assert_eq!(output.status, Some(ShellCallStatus::Completed));

        let serialized = serde_json::to_value(output).unwrap();
        assert_eq!(serialized["output"][0]["outcome"]["type"], "exit");
        assert_eq!(serialized["output"][1]["outcome"]["type"], "timeout");
    }
}
