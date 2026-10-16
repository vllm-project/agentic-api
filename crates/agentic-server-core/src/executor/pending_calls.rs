//! Detects calls in an item sequence that never received a resolving output.
//!
//! Gateway-owned calls are always resolved within the same turn (their
//! output is appended before the turn ends), so anything still unresolved
//! after scanning a full item sequence is, by construction, something the
//! *client* owed a resolution for.
//!
//! An async call (`async: true` on its call item) may stay unresolved across
//! requests, and later outputs for it are accepted again, as `OpenAI` does. Its
//! marker is read from the call item, so re-declaring the tool without `async`
//! does not change a call already made.

use indexmap::IndexMap;

use super::{ExecutorError, ExecutorResult};
use crate::tool::{ToolError, shell::SHELL_FUNCTION_NAME};
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
    /// The call is async: the conversation may continue without its output.
    pub(super) async_execution: bool,
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
/// order. Calls and outputs must have non-empty IDs and the same kind. A
/// synchronous call takes exactly one output; an async call accepts repeated
/// outputs. An output whose `call_id` matches no call is rejected. Namespace
/// member calls are represented as `InputItem::FunctionCall`, so they're
/// covered by the plain function check.
pub(super) fn pending_calls(items: &[InputItem]) -> ExecutorResult<Vec<PendingCall>> {
    scan_calls(items).map(|ledger| ledger.pending().collect())
}

/// The first synchronous call left unresolved, which the client must answer
/// before the conversation can continue. Pending async calls do not count.
pub(super) fn first_unanswered_sync_call(items: &[InputItem]) -> ExecutorResult<Option<PendingCall>> {
    scan_calls(items).map(|ledger| ledger.pending().find(|call| !call.async_execution))
}

/// Largest prefix whose synchronous calls all have matching outputs inside it.
/// Pending async calls do not end the prefix; see [`open_async_calls`](crate::types::io::input::open_async_calls).
pub(super) fn resolved_prefix_len(items: &[InputItem]) -> ExecutorResult<usize> {
    scan_calls(items).map(|ledger| ledger.resolved_prefix)
}

#[derive(Debug)]
struct CallRecord {
    kind: CallKind,
    function_name: Option<String>,
    namespaced: bool,
    async_execution: bool,
    resolved: bool,
}

/// Every call seen so far, in emission order, with its resolution state.
#[derive(Debug, Default)]
struct CallLedger {
    calls: IndexMap<String, CallRecord>,
    unresolved_sync: usize,
    resolved_prefix: usize,
}

impl CallLedger {
    fn pending(self) -> impl Iterator<Item = PendingCall> {
        self.calls
            .into_iter()
            .filter(|(_, record)| !record.resolved)
            .map(|(call_id, record)| PendingCall {
                call_id,
                kind: record.kind,
                function_name: record.function_name,
                namespaced: record.namespaced,
                async_execution: record.async_execution,
            })
    }

    fn add_call(
        &mut self,
        call_id: &str,
        kind: CallKind,
        async_execution: bool,
        function_name: Option<&str>,
        namespaced: bool,
    ) -> ExecutorResult<()> {
        if call_id.is_empty() {
            return Err(ExecutorError::InvalidRequest(format!(
                "{} call_id must not be empty",
                kind.call_item_name()
            )));
        }
        if self.calls.contains_key(call_id) {
            return Err(ExecutorError::InvalidRequest(format!(
                "duplicate call_id '{call_id}' in {}",
                kind.call_item_name()
            )));
        }
        self.calls.insert(
            call_id.to_owned(),
            CallRecord {
                kind,
                function_name: function_name.map(str::to_owned),
                namespaced,
                async_execution,
                resolved: false,
            },
        );
        if !async_execution {
            self.unresolved_sync += 1;
        }
        Ok(())
    }

    fn resolve_call(&mut self, call_id: &str, output_kind: CallKind) -> ExecutorResult<()> {
        if call_id.is_empty() {
            return Err(ExecutorError::InvalidRequest(format!(
                "{} call_id must not be empty",
                output_kind.output_item_name()
            )));
        }
        let Some(record) = self.calls.get_mut(call_id) else {
            return Err(ToolError::UnknownCallOutput {
                call_id: call_id.to_owned(),
            }
            .into());
        };
        if record.kind != output_kind {
            return Err(ExecutorError::InvalidRequest(format!(
                "{} cannot resolve {} call_id '{call_id}'",
                output_kind.output_item_name(),
                record.kind.call_item_name()
            )));
        }
        match (record.resolved, record.async_execution) {
            (false, async_execution) => {
                record.resolved = true;
                if !async_execution {
                    self.unresolved_sync -= 1;
                }
                Ok(())
            }
            // OpenAI accepts repeated and conflicting outputs for an async call.
            (true, true) => Ok(()),
            (true, false) => Err(ExecutorError::InvalidRequest(format!(
                "{} references call_id '{call_id}' without a pending call",
                output_kind.output_item_name()
            ))),
        }
    }
}

fn scan_calls(items: &[InputItem]) -> ExecutorResult<CallLedger> {
    let mut ledger = CallLedger::default();
    for (index, item) in items.iter().enumerate() {
        match item {
            InputItem::FunctionCall(call) => {
                ledger.add_call(
                    &call.call_id,
                    CallKind::Function,
                    call.async_execution,
                    Some(&call.name),
                    call.namespace.is_some(),
                )?;
            }
            InputItem::CustomToolCall(call) => {
                ledger.add_call(&call.call_id, CallKind::Custom, call.async_execution, None, false)?;
            }
            InputItem::FunctionCallOutput(output) => ledger.resolve_call(&output.call_id, CallKind::Function)?,
            InputItem::CustomToolCallOutput(output) => ledger.resolve_call(&output.call_id, CallKind::Custom)?,
            InputItem::ShellCall(call) => ledger.add_call(&call.call_id, CallKind::Shell, false, None, false)?,
            InputItem::ShellCallOutput(output) => ledger.resolve_call(&output.call_id, CallKind::Shell)?,
            InputItem::ToolSearchCall(call) => {
                ledger.add_call(&call.call_id, CallKind::ToolSearch, false, None, false)?;
            }
            InputItem::ToolSearchOutput(output) => ledger.resolve_call(&output.call_id, CallKind::ToolSearch)?,
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
        if ledger.unresolved_sync == 0 {
            ledger.resolved_prefix = index + 1;
        }
    }
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::types::io::input::{async_records, open_async_call_ids};

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
            async_execution: false,
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
            async_execution: false,
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
                async_execution: false,
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
        // OpenAI's wording for an output whose call does not exist.
        assert_invalid(
            &[function_call_output("call_1")],
            "No tool call found for function call output with call_id call_1.",
        );
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

    fn async_function_call(call_id: &str) -> InputItem {
        let InputItem::FunctionCall(mut call) = function_call(call_id) else {
            unreachable!("function call helper");
        };
        call.async_execution = true;
        InputItem::FunctionCall(call)
    }

    fn async_custom_tool_call(call_id: &str) -> InputItem {
        let InputItem::CustomToolCall(mut call) = custom_tool_call(call_id) else {
            unreachable!("custom call helper");
        };
        call.async_execution = true;
        InputItem::CustomToolCall(call)
    }

    /// A pending async call does not block a follow-up; a synchronous one still does.
    #[test]
    fn only_synchronous_calls_must_be_answered_before_continuing() {
        let items = vec![async_function_call("call_async"), async_custom_tool_call("call_custom")];
        let pending = pending_calls(&items).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|call| call.async_execution));
        assert!(first_unanswered_sync_call(&items).unwrap().is_none());

        let mut with_sync = items;
        with_sync.push(function_call("call_sync"));
        let unanswered = first_unanswered_sync_call(&with_sync).unwrap().expect("sync call");
        assert_eq!(unanswered.call_id, "call_sync");
    }

    /// Recorded `OpenAI` behavior: repeated and conflicting outputs for an async call are accepted.
    #[test]
    fn async_calls_accept_repeated_outputs() {
        let items = vec![
            async_function_call("call_1"),
            function_call_output("call_1"),
            function_call_output("call_1"),
            async_custom_tool_call("call_2"),
            custom_tool_call_output("call_2"),
            custom_tool_call_output("call_2"),
        ];
        assert!(pending_calls(&items).unwrap().is_empty());
        assert_invalid(
            &[async_function_call("call_1"), custom_tool_call_output("call_1")],
            "cannot resolve function_call call_id 'call_1'",
        );
    }

    #[test]
    fn unknown_output_is_reported_as_an_unknown_call() {
        let error = pending_calls(&[async_function_call("call_1"), function_call_output("call_other")]).unwrap_err();
        assert!(matches!(
            error,
            ExecutorError::Tool(ToolError::UnknownCallOutput { call_id }) if call_id == "call_other"
        ));
    }

    #[test]
    fn pending_async_calls_do_not_end_the_resolved_prefix() {
        let items = vec![
            InputItem::Unknown,
            async_function_call("call_async"),
            InputItem::Unknown,
            function_call("call_sync"),
            InputItem::Unknown,
        ];
        assert_eq!(
            resolved_prefix_len(&items).unwrap(),
            3,
            "only the sync call ends the prefix"
        );
    }

    /// Repeated outputs for one async call do not accumulate across compactions: a retained history
    /// keeps the call and its latest output, so its size stays bounded per call.
    #[test]
    fn retained_history_keeps_one_output_per_async_call_across_compactions() {
        let compaction = |id: &str| -> InputItem {
            serde_json::from_value(serde_json::json!({"type": "compaction", "id": id, "encrypted_content": "summary"}))
                .unwrap()
        };
        let mut history = vec![async_function_call("call_poll")];
        for round in 0..20 {
            history.push(function_call_output("call_poll"));
            history.push(compaction(&format!("cmp_{round}")));
            history = crate::types::io::input::retain_window(history, |_| false);
        }
        let kinds: Vec<&str> = history
            .iter()
            .map(|item| match item {
                InputItem::FunctionCall(_) => "call",
                InputItem::FunctionCallOutput(_) => "output",
                InputItem::Compaction(_) => "compaction",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["call", "output", "compaction"]);
    }

    /// Compaction keeps every async call and the last output that answers it; the open ones are
    /// those without an output in the prefix.
    #[test]
    fn async_records_and_open_calls_in_a_prefix() {
        let prefix = vec![
            async_function_call("call_open"),
            async_function_call("call_answered"),
            function_call_output("call_answered"),
            function_call_output("call_answered"),
            function_call("call_sync"),
            function_call_output("call_sync"),
            async_custom_tool_call("call_custom_open"),
        ];
        assert_eq!(
            open_async_call_ids(&prefix),
            HashSet::from(["call_open", "call_custom_open"])
        );
        assert_eq!(
            async_records(&prefix),
            [true, true, false, true, false, false, true],
            "only the last output of the answered call is kept"
        );
    }
}
