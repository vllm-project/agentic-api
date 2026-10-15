//! Detects calls in an item sequence that never received a resolving output.
//!
//! Gateway-owned calls are always resolved within the same turn (their
//! output is appended before the turn ends), so anything still unresolved
//! after scanning a full item sequence is, by construction, something the
//! *client* owed a resolution for.

use std::collections::HashSet;

use indexmap::IndexMap;

use super::{ExecutorError, ExecutorResult};
use crate::tool::shell::SHELL_FUNCTION_NAME;
use crate::types::client_calls::ClientCallId;
use crate::types::io::InputItem;
use crate::types::tools::{ResponsesTool, ToolSearchExecution};
use crate::types::websocket::SteeringRequiredInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallKind {
    Function,
    Custom,
    Shell,
    ToolSearch,
}

impl CallKind {
    const fn call_item_name(self) -> &'static str {
        match self {
            Self::Function => "function_call",
            Self::Custom => "custom_tool_call",
            Self::Shell => "shell_call",
            Self::ToolSearch => "tool_search_call",
        }
    }

    const fn output_item_name(self) -> &'static str {
        match self {
            Self::Function => "function_call_output",
            Self::Custom => "custom_tool_call_output",
            Self::Shell => "shell_call_output",
            Self::ToolSearch => "tool_search_output",
        }
    }
}

/// A client-owned call (plain `function`, Codex `namespace` member, or
/// `custom` tool) with no later matching output in the same item sequence.
#[derive(Debug)]
pub(super) struct PendingCall {
    pub(super) call_id: String,
    pub(super) kind: CallKind,
    function_name: Option<String>,
    namespaced: bool,
}

impl PendingCall {
    /// Canonical history normalizes public custom and shell calls into functions. Restore
    /// only their output kind from the same parent's public tool declarations;
    /// namespace members remain functions even when their member name matches.
    /// Unsupported or incomplete records retain their original identity. This
    /// projection neither admits steering nor accepts tool outputs.
    pub(super) fn into_steering_required_input(
        mut self,
        tools: &[ResponsesTool],
    ) -> Result<SteeringRequiredInput, Self> {
        let Ok(call_id) = ClientCallId::try_from(std::mem::take(&mut self.call_id)) else {
            // ClientCallId rejects only an empty ID; taking it leaves that
            // rejected record unchanged and avoids cloning successful IDs.
            return Err(self);
        };
        match self.kind {
            CallKind::Function => {
                if let Some(name) = self.function_name.take() {
                    if !self.namespaced {
                        for tool in tools {
                            match tool {
                                ResponsesTool::Shell(_) if name == SHELL_FUNCTION_NAME => {
                                    return Ok(SteeringRequiredInput::Shell { call_id });
                                }
                                ResponsesTool::Custom(declaration) if declaration.name.as_str() == name => {
                                    return Ok(SteeringRequiredInput::Custom { call_id });
                                }
                                _ => {}
                            }
                        }
                    }
                    return Ok(SteeringRequiredInput::Function { call_id, name });
                }
            }
            CallKind::Custom => return Ok(SteeringRequiredInput::Custom { call_id }),
            CallKind::Shell => return Ok(SteeringRequiredInput::Shell { call_id }),
            CallKind::ToolSearch => {
                return Ok(SteeringRequiredInput::ToolSearch {
                    call_id,
                    execution: ToolSearchExecution::Client,
                });
            }
        }
        self.call_id = call_id.into();
        Err(self)
    }
}

/// Scans `items` in order and returns every call left unresolved, in emission
/// order. Calls and outputs must have non-empty IDs and form a one-to-one,
/// same-kind relationship. Namespace member calls are represented as
/// `InputItem::FunctionCall`, so they're covered by the plain function check.
pub(super) fn pending_calls(items: &[InputItem]) -> ExecutorResult<Vec<PendingCall>> {
    scan_calls(items).map(|(pending, _)| pending)
}

/// Largest prefix whose tool calls all have matching outputs inside it.
pub(super) fn resolved_prefix_len(items: &[InputItem]) -> ExecutorResult<usize> {
    scan_calls(items).map(|(_, prefix)| prefix)
}

fn scan_calls(items: &[InputItem]) -> ExecutorResult<(Vec<PendingCall>, usize)> {
    let mut seen_call_ids = HashSet::new();
    let mut pending = IndexMap::new();
    let mut resolved_prefix = 0;
    for (index, item) in items.iter().enumerate() {
        match item {
            InputItem::FunctionCall(call) => {
                add_call(
                    &call.call_id,
                    CallKind::Function,
                    Some(&call.name),
                    call.namespace.is_some(),
                    &mut seen_call_ids,
                    &mut pending,
                )?;
            }
            InputItem::CustomToolCall(call) => {
                add_call(
                    &call.call_id,
                    CallKind::Custom,
                    None,
                    false,
                    &mut seen_call_ids,
                    &mut pending,
                )?;
            }
            InputItem::FunctionCallOutput(output) => {
                resolve_call(&output.call_id, CallKind::Function, &mut pending)?;
            }
            InputItem::CustomToolCallOutput(output) => {
                resolve_call(&output.call_id, CallKind::Custom, &mut pending)?;
            }
            InputItem::ShellCall(call) => {
                add_call(
                    &call.call_id,
                    CallKind::Shell,
                    None,
                    false,
                    &mut seen_call_ids,
                    &mut pending,
                )?;
            }
            InputItem::ShellCallOutput(output) => {
                resolve_call(&output.call_id, CallKind::Shell, &mut pending)?;
            }
            InputItem::ToolSearchCall(call) => {
                add_call(
                    &call.call_id,
                    CallKind::ToolSearch,
                    None,
                    false,
                    &mut seen_call_ids,
                    &mut pending,
                )?;
            }
            InputItem::ToolSearchOutput(output) => {
                resolve_call(&output.call_id, CallKind::ToolSearch, &mut pending)?;
            }
            InputItem::Message(_)
            | InputItem::CodeInterpreterCall(_)
            | InputItem::Reasoning(_)
            | InputItem::McpListTools(_)
            | InputItem::Compaction(_)
            | InputItem::CompactionTrigger
            | InputItem::MultiAgentCall(_)
            | InputItem::MultiAgentCallOutput(_)
            | InputItem::AgentMessage(_)
            | InputItem::Unknown => {}
        }
        if pending.is_empty() {
            resolved_prefix = index + 1;
        }
    }
    Ok((
        pending
            .into_iter()
            .map(|(call_id, (kind, function_name, namespaced))| PendingCall {
                call_id: call_id.to_owned(),
                kind,
                function_name: function_name.map(str::to_owned),
                namespaced,
            })
            .collect(),
        resolved_prefix,
    ))
}

fn add_call<'a>(
    call_id: &'a str,
    kind: CallKind,
    function_name: Option<&'a str>,
    namespaced: bool,
    seen_call_ids: &mut HashSet<&'a str>,
    pending: &mut IndexMap<&'a str, (CallKind, Option<&'a str>, bool)>,
) -> ExecutorResult<()> {
    if call_id.is_empty() {
        return Err(ExecutorError::InvalidRequest(format!(
            "{} call_id must not be empty",
            kind.call_item_name()
        )));
    }
    if !seen_call_ids.insert(call_id) {
        return Err(ExecutorError::InvalidRequest(format!(
            "duplicate call_id '{call_id}' in {}",
            kind.call_item_name()
        )));
    }
    pending.insert(call_id, (kind, function_name, namespaced));
    Ok(())
}

fn resolve_call(
    call_id: &str,
    output_kind: CallKind,
    pending: &mut IndexMap<&str, (CallKind, Option<&str>, bool)>,
) -> ExecutorResult<()> {
    if call_id.is_empty() {
        return Err(ExecutorError::InvalidRequest(format!(
            "{} call_id must not be empty",
            output_kind.output_item_name()
        )));
    }
    let Some((call_kind, _, _)) = pending.shift_remove(call_id) else {
        return Err(ExecutorError::InvalidRequest(format!(
            "{} references call_id '{call_id}' without a pending call",
            output_kind.output_item_name()
        )));
    };
    if call_kind != output_kind {
        return Err(ExecutorError::InvalidRequest(format!(
            "{} cannot resolve {} call_id '{call_id}'",
            output_kind.output_item_name(),
            call_kind.call_item_name()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_prefix_never_splits_interleaved_calls_and_outputs() {
        let items = vec![
            InputItem::Unknown,
            function_call("a"),
            function_call("b"),
            function_call_output("a"),
            InputItem::Unknown,
        ];
        assert_eq!(resolved_prefix_len(&items).unwrap(), 1);
        let mut resolved = items;
        resolved.push(function_call_output("b"));
        assert_eq!(resolved_prefix_len(&resolved).unwrap(), resolved.len());
    }
    use crate::types::io::{
        CustomToolCall, CustomToolCallOutputMessage, FunctionToolResultMessage, InputFunctionToolCall,
        InputToolSearchCall, ResponsesInput, ShellCall, ShellCallAction, ShellCallOutputMessage, ToolCallOutput,
    };
    use crate::types::tools::{ToolSearchExecution, ToolSearchStatus};

    fn function_call(call_id: &str) -> InputItem {
        named_function_call(call_id, "get_weather", None)
    }

    fn named_function_call(call_id: &str, name: &str, namespace: Option<&str>) -> InputItem {
        InputItem::FunctionCall(InputFunctionToolCall {
            agent: None,
            id: None,
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            arguments: "{}".to_owned(),
            status: None,
        })
    }

    fn function_call_output(call_id: &str) -> InputItem {
        InputItem::FunctionCallOutput(FunctionToolResultMessage {
            call_id: call_id.to_owned(),
            output: ToolCallOutput::Text(String::new()),
        })
    }

    fn custom_tool_call(call_id: &str) -> InputItem {
        InputItem::CustomToolCall(CustomToolCall {
            agent: None,
            id: String::new(),
            status: None,
            call_id: call_id.to_owned(),
            name: "freeform".to_owned(),
            input: String::new(),
        })
    }

    fn custom_tool_call_output(call_id: &str) -> InputItem {
        InputItem::CustomToolCallOutput(CustomToolCallOutputMessage {
            call_id: call_id.to_owned(),
            name: None,
            output: ToolCallOutput::Text(String::new()),
        })
    }

    fn shell_call(call_id: &str) -> InputItem {
        InputItem::ShellCall(ShellCall {
            agent: None,
            id: None,
            call_id: call_id.to_owned(),
            action: ShellCallAction {
                commands: vec!["pwd".to_owned()],
                timeout_ms: None,
                max_output_length: None,
                extra: std::collections::HashMap::new(),
            },
            status: None,
            extra: std::collections::HashMap::new(),
        })
    }

    fn shell_call_output(call_id: &str) -> InputItem {
        InputItem::ShellCallOutput(ShellCallOutputMessage {
            id: None,
            call_id: call_id.to_owned(),
            max_output_length: None,
            output: Vec::new(),
            status: None,
            extra: std::collections::HashMap::new(),
        })
    }

    #[test]
    fn resolved_calls_are_not_pending() {
        let items = vec![function_call("call_1"), function_call_output("call_1")];
        assert!(pending_calls(&items).expect("valid call/output pair").is_empty());
    }

    #[test]
    fn unresolved_function_call_is_reported_in_order() {
        let items = vec![
            function_call("call_1"),
            function_call("call_2"),
            function_call_output("call_1"),
        ];
        let pending = pending_calls(&items).expect("valid partial call/output sequence");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].call_id, "call_2");
    }

    #[test]
    fn unresolved_custom_tool_call_is_reported() {
        let items = vec![custom_tool_call("call_1")];
        let pending = pending_calls(&items).expect("valid unresolved custom call");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].call_id, "call_1");
    }

    #[test]
    fn custom_tool_call_output_resolves_custom_tool_call() {
        let items = vec![custom_tool_call("call_1"), custom_tool_call_output("call_1")];
        assert!(pending_calls(&items).expect("valid custom call/output pair").is_empty());
    }

    #[test]
    fn shell_call_output_resolves_shell_call() {
        let items = vec![shell_call("call_1"), shell_call_output("call_1")];
        assert!(pending_calls(&items).expect("valid shell call/output pair").is_empty());
    }

    #[test]
    fn empty_items_have_no_pending_calls() {
        assert!(pending_calls(&[]).expect("empty history is valid").is_empty());
    }

    #[test]
    fn steering_stubs_preserve_function_names_and_unresolved_emission_order() {
        let mut items = vec![
            function_call("resolved"),
            function_call("weather"),
            custom_tool_call("custom"),
            named_function_call("deadline", "get_deadline", Some("project")),
            function_call_output("resolved"),
        ];
        items.extend(Vec::<InputItem>::from(&ResponsesInput::Text(
            "change the scope".to_owned(),
        )));
        let required = pending_calls(&items)
            .unwrap()
            .into_iter()
            .map(|pending| pending.into_steering_required_input(&[]))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(required.len(), 3);
        assert!(matches!(&required[0], SteeringRequiredInput::Function { call_id, name }
            if call_id.as_str() == "weather" && name == "get_weather"));
        assert!(matches!(&required[1], SteeringRequiredInput::Custom { call_id }
            if call_id.as_str() == "custom"));
        assert!(matches!(&required[2], SteeringRequiredInput::Function { call_id, name }
            if call_id.as_str() == "deadline" && name == "get_deadline"));

        // Out-of-order resolution removes only the matching obligation. User
        // steering never supplies or substitutes for these owed tool outputs.
        items.push(function_call_output("deadline"));
        items.push(custom_tool_call_output("custom"));
        let required = pending_calls(&items)
            .unwrap()
            .into_iter()
            .map(|pending| pending.into_steering_required_input(&[]))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(required.len(), 1);
        assert!(matches!(&required[0], SteeringRequiredInput::Function { call_id, name }
            if call_id.as_str() == "weather" && name == "get_weather"));
    }

    #[test]
    fn shell_and_search_steering_obligations_preserve_public_output_kinds() {
        let search = InputItem::ToolSearchCall(InputToolSearchCall {
            agent: None,
            id: "ts_search".to_owned(),
            call_id: "search".to_owned(),
            execution: ToolSearchExecution::Client,
            arguments: serde_json::json!({}),
            status: ToolSearchStatus::Completed,
        });
        for (item, expected) in [
            (
                shell_call("shell"),
                serde_json::json!({"type":"shell_call_output", "call_id":"shell"}),
            ),
            (
                search,
                serde_json::json!({"type":"tool_search_output", "call_id":"search", "execution":"client"}),
            ),
        ] {
            let pending = pending_calls(&[item]).unwrap().pop().unwrap();
            let required = pending.into_steering_required_input(&[]).unwrap();
            assert_eq!(crate::utils::common::serialize_to_value(&required).unwrap(), expected);
        }
    }

    #[test]
    fn steering_projection_restores_custom_kind_without_changing_canonical_resolution() {
        let tools: Vec<ResponsesTool> = crate::utils::common::deserialize_from_value(serde_json::json!([
            {"type":"custom", "name":"set_temperature_unit"}
        ]))
        .unwrap();
        let mut items = vec![
            named_function_call("custom", "set_temperature_unit", None),
            function_call("weather"),
            named_function_call("namespace", "set_temperature_unit", Some("units")),
        ];
        let required = pending_calls(&items)
            .unwrap()
            .into_iter()
            .map(|pending| pending.into_steering_required_input(&tools))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(matches!(&required[0], SteeringRequiredInput::Custom { call_id }
            if call_id.as_str() == "custom"));
        assert!(matches!(&required[1], SteeringRequiredInput::Function { call_id, name }
            if call_id.as_str() == "weather" && name == "get_weather"));
        assert!(matches!(&required[2], SteeringRequiredInput::Function { call_id, name }
            if call_id.as_str() == "namespace" && name == "set_temperature_unit"));

        // Public custom output normalization still resolves the canonical
        // function obligation. Projection must not change scanner semantics.
        items.push(function_call_output("custom"));
        let pending = pending_calls(&items).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].call_id, "weather");
        assert_eq!(pending[1].call_id, "namespace");
    }

    #[test]
    fn steering_conversion_returns_incomplete_records_without_inventing_fields() {
        for (kind, call_id, function_name) in [
            (CallKind::Function, "missing-name", None),
            (CallKind::Function, "", Some("get_weather")),
            (CallKind::Custom, "", None),
        ] {
            let rejected = PendingCall {
                call_id: call_id.to_owned(),
                kind,
                function_name: function_name.map(str::to_owned),
                namespaced: false,
            }
            .into_steering_required_input(&[])
            .unwrap_err();
            assert_eq!(rejected.kind, kind);
            assert_eq!(rejected.call_id, call_id);
            assert_eq!(rejected.function_name.as_deref(), function_name);
        }
    }

    #[test]
    fn normalized_shell_stubs_require_the_public_shell_declaration() {
        let tools: Vec<ResponsesTool> = crate::utils::common::deserialize_from_value(serde_json::json!([
            {"type":"shell", "environment":{"type":"local"}}
        ]))
        .unwrap();
        for (name, namespace, declarations, shell) in [
            ("shell", None, tools.as_slice(), true),
            ("shell", Some("project"), tools.as_slice(), false),
            ("shell", None, &[][..], false),
            ("get_weather", None, tools.as_slice(), false),
        ] {
            let pending = pending_calls(&[named_function_call("call_tool", name, namespace)])
                .unwrap()
                .pop()
                .unwrap();
            let required = pending.into_steering_required_input(declarations).unwrap();
            assert_eq!(matches!(required, SteeringRequiredInput::Shell { .. }), shell);
            if !shell {
                assert!(matches!(required, SteeringRequiredInput::Function { name: actual, .. } if actual == name));
            }
        }
    }

    #[test]
    fn recorded_client_calls_keep_public_obligations_after_normalization() {
        use crate::utils::common::{deserialize_from_value, serialize_to_value};

        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_agent");
        for (provider, model) in [
            ("openai-reference", "gpt-5.6-sol"),
            ("gateway", "Qwen-Qwen3.6-35B-A3B-FP8"),
        ] {
            let mut seen = HashSet::new();
            for scenario in ["client-owned-tools", "mixed-tools"] {
                let path = directory.join(format!("multi-agent-{provider}-{scenario}-{model}-nonstreaming.yaml"));
                let cassette: serde_json::Value =
                    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                for turn in cassette["turns"].as_array().unwrap() {
                    let tools: Vec<ResponsesTool> =
                        deserialize_from_value(turn["request"]["body"]["tools"].clone()).unwrap();
                    for item in turn["response"]["body"]["output"].as_array().unwrap() {
                        let output_type = match item["type"].as_str().unwrap() {
                            "function_call" => "function_call_output",
                            "custom_tool_call" => "custom_tool_call_output",
                            "shell_call" => "shell_call_output",
                            "tool_search_call" => "tool_search_output",
                            _ => continue,
                        };
                        let mut expected = serde_json::json!({"type": output_type, "call_id": item["call_id"]});
                        if output_type == "function_call_output" {
                            expected["name"] = item["name"].clone();
                        }
                        if output_type == "tool_search_output" {
                            expected["execution"] = serde_json::json!("client");
                        }
                        let input = ResponsesInput::Items(vec![deserialize_from_value(item.clone()).unwrap()]);
                        let normalized = input.normalized_model_input();
                        for history in [&input, normalized.as_ref()] {
                            let ResponsesInput::Items(items) = history else {
                                unreachable!()
                            };
                            let required = pending_calls(items)
                                .unwrap()
                                .pop()
                                .unwrap()
                                .into_steering_required_input(&tools)
                                .unwrap();
                            assert_eq!(
                                serialize_to_value(&required).unwrap(),
                                expected,
                                "{provider} {scenario}"
                            );
                        }
                        seen.insert(output_type);
                    }
                }
            }
            assert_eq!(
                seen,
                HashSet::from([
                    "function_call_output",
                    "custom_tool_call_output",
                    "shell_call_output",
                    "tool_search_output",
                ]),
                "{provider}"
            );
        }
    }

    fn assert_invalid(items: &[InputItem], expected: &str) {
        let error = pending_calls(items).expect_err("invalid call/output sequence must be rejected");
        assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }

    #[test]
    fn empty_and_duplicate_call_ids_are_rejected() {
        assert_invalid(&[function_call("")], "function_call call_id must not be empty");
        assert_invalid(&[custom_tool_call("")], "custom_tool_call call_id must not be empty");
        assert_invalid(&[shell_call("")], "shell_call call_id must not be empty");
        assert_invalid(
            &[
                function_call("call_1"),
                function_call("call_1"),
                function_call_output("call_1"),
            ],
            "duplicate call_id 'call_1'",
        );
        assert_invalid(
            &[
                function_call("call_1"),
                function_call_output("call_1"),
                custom_tool_call("call_1"),
            ],
            "duplicate call_id 'call_1'",
        );
    }

    #[test]
    fn outputs_must_resolve_exactly_one_call_of_the_same_kind() {
        assert_invalid(
            &[function_call_output("")],
            "function_call_output call_id must not be empty",
        );
        assert_invalid(&[function_call_output("call_1")], "without a pending call");
        assert_invalid(
            &[
                function_call("call_1"),
                function_call_output("call_1"),
                function_call_output("call_1"),
            ],
            "without a pending call",
        );
        assert_invalid(
            &[function_call("call_1"), custom_tool_call_output("call_1")],
            "cannot resolve function_call call_id 'call_1'",
        );
        assert_invalid(
            &[shell_call("call_1"), function_call_output("call_1")],
            "cannot resolve shell_call call_id 'call_1'",
        );
    }
}
