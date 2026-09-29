//! Public Responses output shape for gateway-executed Python calls.

use serde::{Deserialize, Serialize};

use super::multi_agent::AgentAttribution;

/// One code-interpreter call exposed in a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CodeInterpreterCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    pub id: String,
    pub container_id: String,
    pub code: String,
    pub status: CodeInterpreterCallStatus,
    /// Execution outputs. `OpenAI` emits `null` while a call is in progress;
    /// gateway completion uses `Some`, including an empty output list.
    pub outputs: Option<Vec<CodeInterpreterCallOutput>>,
    /// Internal provenance for continuation. This is stored separately from
    /// the public item and cannot be selected through the Responses wire.
    #[serde(skip)]
    pub(crate) origin: CodeInterpreterCallOrigin,
}

impl PartialEq for CodeInterpreterCall {
    fn eq(&self, other: &Self) -> bool {
        self.agent == other.agent
            && self.id == other.id
            && self.container_id == other.container_id
            && self.code == other.code
            && self.status == other.status
            && self.outputs == other.outputs
    }
}

impl Eq for CodeInterpreterCall {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CodeInterpreterCallOrigin {
    #[default]
    Upstream,
    Gateway,
}

impl CodeInterpreterCall {
    /// Construct a code-interpreter call received from an upstream provider.
    ///
    /// The public wire does not carry execution origin. Gateway projections
    /// set their internal origin when they are created by the tool handler.
    #[must_use]
    pub fn new(
        id: String,
        container_id: String,
        code: String,
        status: CodeInterpreterCallStatus,
        outputs: Option<Vec<CodeInterpreterCallOutput>>,
    ) -> Self {
        Self {
            agent: None,
            id,
            container_id,
            code,
            status,
            outputs,
            origin: CodeInterpreterCallOrigin::Upstream,
        }
    }
}

/// Public lifecycle state for a code-interpreter call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CodeInterpreterCallStatus {
    InProgress,
    Interpreting,
    Completed,
    Failed,
    Incomplete,
}

/// Text or image output produced by a code-interpreter call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodeInterpreterCallOutput {
    Logs { logs: String },
    Image { url: String },
}

impl CodeInterpreterCallOutput {
    #[must_use]
    pub fn logs(logs: String) -> Self {
        Self::Logs { logs }
    }
}

/// Code-interpreter-specific Responses streaming events.
///
/// These events surround the ordinary `response.output_item.added` and
/// `response.output_item.done` lifecycle for a `code_interpreter_call` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type")]
pub enum CodeInterpreterCallStreamEvent {
    #[serde(rename = "response.code_interpreter_call.in_progress")]
    InProgress {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },
    #[serde(rename = "response.code_interpreter_call_code.delta")]
    CodeDelta {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
        delta: String,
    },
    #[serde(rename = "response.code_interpreter_call_code.done")]
    CodeDone {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
        code: String,
    },
    #[serde(rename = "response.code_interpreter_call.interpreting")]
    Interpreting {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },
    #[serde(rename = "response.code_interpreter_call.completed")]
    Completed {
        item_id: String,
        output_index: u32,
        sequence_number: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_round_trips_with_logs_and_image_outputs() {
        let call = CodeInterpreterCall {
            agent: None,
            id: "ci_1".to_owned(),
            container_id: "cntr_1".to_owned(),
            code: "print(42)".to_owned(),
            status: CodeInterpreterCallStatus::Completed,
            outputs: Some(vec![
                CodeInterpreterCallOutput::logs("42\n".to_owned()),
                CodeInterpreterCallOutput::Image {
                    url: "https://example.test/plot.png".to_owned(),
                },
            ]),
            origin: CodeInterpreterCallOrigin::Upstream,
        };
        let wire = serde_json::to_value(&call).expect("serialize call");
        assert_eq!(wire["outputs"][0]["type"], "logs");
        assert_eq!(wire["outputs"][1]["type"], "image");
        assert_eq!(wire["outputs"][1]["url"], "https://example.test/plot.png");
        assert!(wire.get("agent").is_none());
        assert_eq!(
            serde_json::from_value::<CodeInterpreterCall>(wire).expect("deserialize call"),
            call
        );
        let mut attributed = call.clone();
        attributed.agent = Some(AgentAttribution {
            agent_name: "/root/worker".to_owned(),
        });
        let attributed_wire = serde_json::to_value(&attributed).expect("serialize attributed call");
        assert_eq!(attributed_wire["agent"]["agent_name"], "/root/worker");
        assert_eq!(
            serde_json::from_value::<CodeInterpreterCall>(attributed_wire).expect("deserialize attributed call"),
            attributed
        );
        let mut gateway = call.clone();
        gateway.origin = CodeInterpreterCallOrigin::Gateway;
        let public = serde_json::to_value(&gateway).expect("serialize gateway call");
        assert_eq!(
            serde_json::from_value::<CodeInterpreterCall>(public).expect("deserialize public call"),
            gateway,
            "internal origin must not affect public item equality"
        );
    }

    #[test]
    fn stream_events_use_openai_wire_names_and_typed_fields() {
        let events = [
            CodeInterpreterCallStreamEvent::InProgress {
                item_id: "ci_1".to_owned(),
                output_index: 3,
                sequence_number: 8,
            },
            CodeInterpreterCallStreamEvent::CodeDelta {
                item_id: "ci_1".to_owned(),
                output_index: 3,
                sequence_number: 9,
                delta: "print(".to_owned(),
            },
            CodeInterpreterCallStreamEvent::CodeDone {
                item_id: "ci_1".to_owned(),
                output_index: 3,
                sequence_number: 10,
                code: "print(42)".to_owned(),
            },
            CodeInterpreterCallStreamEvent::Interpreting {
                item_id: "ci_1".to_owned(),
                output_index: 3,
                sequence_number: 11,
            },
            CodeInterpreterCallStreamEvent::Completed {
                item_id: "ci_1".to_owned(),
                output_index: 3,
                sequence_number: 12,
            },
        ];
        let expected_types = [
            "response.code_interpreter_call.in_progress",
            "response.code_interpreter_call_code.delta",
            "response.code_interpreter_call_code.done",
            "response.code_interpreter_call.interpreting",
            "response.code_interpreter_call.completed",
        ];

        for (event, expected_type) in events.into_iter().zip(expected_types) {
            let value = serde_json::to_value(&event).expect("serialize event");
            assert_eq!(value["type"], expected_type);
            assert_eq!(value["item_id"], "ci_1");
            assert_eq!(value["output_index"], 3);
            assert_eq!(
                serde_json::from_value::<CodeInterpreterCallStreamEvent>(value).expect("deserialize event"),
                event
            );
        }
    }
}
