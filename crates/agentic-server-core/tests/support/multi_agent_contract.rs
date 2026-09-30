//! Integration-only comparison of independently recorded provider exchanges.
#[path = "multi_agent_code_interpreter.rs"]
mod code_interpreter;
use flate2::read::GzDecoder;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use agentic_core::events::{SSEEventType, normalize_sse_line};
use agentic_core::types::agent_commands::CollaborationResult;
use agentic_core::types::client_calls::ClientCallKind;
use agentic_core::types::io::{
    InputItem, MessagePhase, MultiAgentAction, MultiAgentCallOutput, MultiAgentCallOutputContent, OutputItem,
};
use agentic_core::types::request_response::ResponsePayload;
use agentic_core::types::tools::{CodexNamespaceMember, ResponsesTool, ToolSearchStatus};
use serde_json::Value;

use crate::support::{Cassette, Turn, responses_turns};

pub struct RecordedSession {
    exchanges: Vec<RecordedExchange>,
}

impl RecordedSession {
    fn delegated_agents(&self) -> HashSet<&str> {
        self.exchanges
            .iter()
            .flat_map(|exchange| &exchange.response.output)
            .filter_map(|item| item.agent().map(|agent| agent.agent_name.as_str()))
            .filter(|name| name.strip_prefix("/root/").is_some_and(|child| !child.contains('/')))
            .collect()
    }
}

struct RecordedExchange {
    response: ResponsePayload,
    previous_response_id: Option<String>,
    input: Vec<InputItem>,
    stream: bool,
    max_concurrent_subagents: Option<u64>,
    parallel_tool_calls: Option<bool>,
    kinds: HashSet<String>,
}

/// Model-dependent text, IDs, agent names and action counts may differ between
/// providers. Identity relationships within each trace must still be exact.
pub struct ComparisonPolicy {
    pub require_reference_tool_kinds: bool,
    /// Requested independent tasks, excluding optional retries in the reference.
    pub minimum_delegated_agents: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("multi-agent contract mismatch: {0}")]
pub struct ContractMismatch(String);

fn mismatch(message: impl Into<String>) -> ContractMismatch {
    ContractMismatch(message.into())
}

impl RecordedSession {
    pub fn load(path: &Path) -> Result<Self, ContractMismatch> {
        let content = if path.extension().is_some_and(|extension| extension == "gz") {
            let file = std::fs::File::open(path).map_err(|error| mismatch(error.to_string()))?;
            let mut content = String::new();
            GzDecoder::new(file)
                .read_to_string(&mut content)
                .map_err(|error| mismatch(error.to_string()))?;
            content
        } else {
            std::fs::read_to_string(path).map_err(|error| mismatch(error.to_string()))?
        };
        let cassette: Cassette = serde_yaml::from_str(&content).map_err(|error| mismatch(error.to_string()))?;
        let mut exchanges = Vec::new();
        for turn in responses_turns(&cassette) {
            if !turn.request.body.store || turn.request.body.extra["multi_agent"]["enabled"] != true {
                // Continuations may inherit configuration from stored state.
                if !turn.request.body.store || turn.request.body.previous_response_id.is_none() {
                    return Err(mismatch("recording must use stored multi-agent requests"));
                }
            }
            let (terminal, response) = read_recorded_response(turn)?;
            let input = match &turn.request.body.input {
                Value::Array(items) => {
                    serde_json::from_value(Value::Array(items.clone())).map_err(|error| mismatch(error.to_string()))?
                }
                _ => Vec::new(),
            };
            let kinds = terminal["output"]
                .as_array()
                .ok_or_else(|| mismatch("missing output array"))?
                .iter()
                .filter_map(|item| item["type"].as_str().map(str::to_owned))
                .collect();
            exchanges.push(RecordedExchange {
                response,
                previous_response_id: turn.request.body.previous_response_id.clone(),
                input,
                stream: turn.request.body.stream,
                max_concurrent_subagents: turn.request.body.extra["multi_agent"]["max_concurrent_subagents"].as_u64(),
                parallel_tool_calls: turn.request.body.parallel_tool_calls,
                kinds,
            });
        }
        let session = Self { exchanges };
        session.validate()?;
        Ok(session)
    }

    fn validate(&self) -> Result<(), ContractMismatch> {
        let mut pending = HashMap::new();
        let mut client_ids = HashSet::new();
        let mut previous = None;
        for exchange in &self.exchanges {
            if exchange.previous_response_id.as_deref() != previous {
                return Err(mismatch("broken previous_response_id chain"));
            }
            for input in &exchange.input {
                let (id, kind) = match input {
                    InputItem::FunctionCallOutput(output) => (&output.call_id, ClientCallKind::Function),
                    InputItem::ShellCallOutput(output) => (&output.call_id, ClientCallKind::Shell),
                    InputItem::CustomToolCallOutput(output) => (&output.call_id, ClientCallKind::Custom),
                    InputItem::ToolSearchOutput(output) => {
                        if output.status != ToolSearchStatus::Completed {
                            return Err(mismatch("client tool-search output is not completed"));
                        }
                        (&output.call_id, ClientCallKind::ToolSearch)
                    }
                    _ => continue,
                };
                if pending.remove(id) != Some(kind) {
                    return Err(mismatch("unknown, duplicate or wrong-kind client output"));
                }
            }
            if !pending.is_empty() {
                return Err(mismatch("continuation omitted pending client outputs"));
            }
            let mut collaboration = HashMap::<&str, (MultiAgentAction, Option<&str>)>::new();
            let mut ids = HashSet::new();
            for item in &exchange.response.output {
                if let Some(id) = item.id() {
                    if !ids.insert(id) {
                        return Err(mismatch("duplicate output item ID"));
                    }
                }
                let agent = item.agent().map(|agent| agent.agent_name.as_str());
                if agent.is_some_and(|name| name != "/root" && !name.starts_with("/root/")) {
                    return Err(mismatch("invalid agent ancestry"));
                }
                if let Some((id, kind)) = client_call(item) {
                    if id.is_empty() || agent.is_none() || !client_ids.insert(id) {
                        return Err(mismatch("invalid client-call owner or reused ID"));
                    }
                    pending.insert(id.to_owned(), kind);
                }
                match item {
                    OutputItem::MultiAgentCall(call) => {
                        if agent.is_none() || collaboration.insert(&call.call_id, (call.action, agent)).is_some() {
                            return Err(mismatch("invalid collaboration call identity"));
                        }
                    }
                    OutputItem::MultiAgentCallOutput(output) => {
                        if collaboration.remove(output.call_id.as_str()) != Some((output.action, agent)) {
                            return Err(mismatch("unmatched collaboration output"));
                        }
                        validate_collaboration_result(output)?;
                    }
                    OutputItem::AgentMessage(message) if agent != Some(message.recipient.as_str()) => {
                        return Err(mismatch("agent_message attribution differs from recipient"));
                    }
                    _ => {}
                }
            }
            if !collaboration.is_empty() {
                return Err(mismatch("collaboration call has no output"));
            }
            previous = Some(exchange.response.id.as_str());
        }
        if self.exchanges.is_empty() {
            return Err(mismatch("no Responses exchanges"));
        }
        if !pending.is_empty() {
            return Err(mismatch("session ends with unresolved client calls"));
        }
        Ok(())
    }
}

/// Validate the JSON inside `output_text`, independently of model-dependent values.
fn validate_collaboration_result(output: &MultiAgentCallOutput) -> Result<(), ContractMismatch> {
    let text: String = output
        .output
        .iter()
        .map(|part| {
            let MultiAgentCallOutputContent::OutputText(part) = part;
            part.text.as_str()
        })
        .collect();
    if text.is_empty()
        && matches!(
            output.action,
            MultiAgentAction::SendMessage | MultiAgentAction::FollowupTask
        )
    {
        return Ok(());
    }
    let result: CollaborationResult = serde_json::from_str(&text).map_err(|error| {
        mismatch(format!(
            "{:?} result for {} has an invalid schema: {error}",
            output.action, output.call_id
        ))
    })?;
    if matches!(
        (output.action, result),
        (_, CollaborationResult::Error { .. })
            | (MultiAgentAction::SpawnAgent, CollaborationResult::Spawned { .. })
            | (MultiAgentAction::ListAgents, CollaborationResult::Listing { .. })
            | (
                MultiAgentAction::InterruptAgent,
                CollaborationResult::Interrupted { .. }
            )
            | (MultiAgentAction::WaitAgent, CollaborationResult::Wait { .. })
    ) {
        Ok(())
    } else {
        Err(mismatch(format!(
            "{:?} result for {} has the wrong action schema",
            output.action, output.call_id
        )))
    }
}

fn read_recorded_response(turn: &Turn) -> Result<(Value, ResponsePayload), ContractMismatch> {
    let mut terminal = turn.response.body.clone();
    let mut added = BTreeMap::new();
    let mut done = BTreeMap::new();
    let mut expected_sequence = 0;
    if let Some(events) = &turn.response.sse {
        for frame in events
            .iter()
            .flat_map(|event| event.lines())
            .filter_map(normalize_sse_line)
        {
            if frame.sequence_number() != Some(expected_sequence) {
                return Err(mismatch("non-contiguous SSE sequence"));
            }
            expected_sequence += 1;
            match frame.event_type {
                SSEEventType::OutputItemAdded => {
                    let index = frame
                        .wire
                        .output_index
                        .ok_or_else(|| mismatch("missing output index"))?;
                    // Added items may omit fields supplied by later events
                    // (for example a local shell action).
                    let id = frame.wire.rest["item"]["id"].as_str().map(str::to_owned);
                    if added.insert(index, id).is_some() {
                        return Err(mismatch("reused output index"));
                    }
                }
                SSEEventType::OutputItemDone => {
                    let index = frame
                        .wire
                        .output_index
                        .ok_or_else(|| mismatch("missing output index"))?;
                    let item = frame.wire.rest["item"].clone();
                    let typed: OutputItem =
                        serde_json::from_value(item.clone()).map_err(|error| mismatch(error.to_string()))?;
                    if added.get(&index) != Some(&typed.id().map(str::to_owned)) || done.insert(index, item).is_some() {
                        return Err(mismatch("item completion has no matching unique addition"));
                    }
                }
                SSEEventType::ResponseCompleted | SSEEventType::ResponseIncomplete | SSEEventType::ResponseFailed => {
                    if frame.wire.agent.is_some() {
                        return Err(mismatch("response lifecycle is agent-attributed"));
                    }
                    terminal = Some(frame.wire.rest["response"].clone());
                }
                _ => {}
            }
        }
    }
    let terminal = terminal.ok_or_else(|| mismatch("missing terminal response"))?;
    let response: ResponsePayload =
        serde_json::from_value(terminal.clone()).map_err(|error| mismatch(error.to_string()))?;
    if turn.request.body.stream {
        if added.len() != done.len() || done.len() != response.output.len() {
            return Err(mismatch("incomplete SSE item lifecycle"));
        }
        for (index, item) in terminal["output"]
            .as_array()
            .ok_or_else(|| mismatch("missing output array"))?
            .iter()
            .enumerate()
        {
            let mut wire = done
                .remove(&(index as u64))
                .ok_or_else(|| mismatch("missing terminal output index"))?;
            let mut final_item = item.clone();
            opaque_content(&mut wire);
            opaque_content(&mut final_item);
            if wire != final_item {
                return Err(mismatch(format!("item {index} differs from terminal snapshot")));
            }
        }
    }
    code_interpreter::validate_stream(turn, &response)?;
    Ok((terminal, response))
}

fn opaque_content(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "encrypted_content" && value.is_string() {
                    *value = Value::String("opaque".into());
                } else {
                    opaque_content(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                opaque_content(value);
            }
        }
        _ => {}
    }
}

pub fn assert_multi_agent_contract(
    reference: &RecordedSession,
    gateway: &RecordedSession,
    policy: &ComparisonPolicy,
) -> Result<(), ContractMismatch> {
    let transport = reference.exchanges[0].stream;
    let reference_limit = reference.exchanges[0].max_concurrent_subagents.unwrap_or(3);
    let gateway_limit = gateway.exchanges[0].max_concurrent_subagents.unwrap_or(3);
    if gateway_limit != reference_limit {
        return Err(mismatch("max_concurrent_subagents differs from reference"));
    }
    let required_agents = policy.minimum_delegated_agents.unwrap_or_else(|| {
        reference
            .delegated_agents()
            .len()
            .min(usize::try_from(reference_limit).unwrap_or(usize::MAX))
    });
    if reference.delegated_agents().len() < required_agents || gateway.delegated_agents().len() < required_agents {
        return Err(mismatch(
            "recording delegated fewer independent child tasks than requested",
        ));
    }
    for exchange in &gateway.exchanges {
        if exchange.stream != transport || exchange.response.status != "completed" {
            return Err(mismatch("transport or terminal status differs"));
        }
    }
    // Compare capabilities across the complete session: the model may request
    // extra batches of client outputs without changing the ownership contract.
    if policy.require_reference_tool_kinds {
        let reference_kinds = reference
            .exchanges
            .iter()
            .flat_map(|exchange| &exchange.kinds)
            .collect::<HashSet<_>>();
        let gateway_kinds = gateway
            .exchanges
            .iter()
            .flat_map(|exchange| &exchange.kinds)
            .collect::<HashSet<_>>();
        for kind in [
            "function_call",
            "custom_tool_call",
            "tool_search_call",
            "shell_call",
            "web_search_call",
            "code_interpreter_call",
            "mcp_call",
            "multi_agent_call",
        ] {
            if reference_kinds.iter().any(|value| value.as_str() == kind)
                && !gateway_kinds.iter().any(|value| value.as_str() == kind)
            {
                return Err(mismatch(format!("gateway did not exercise {kind}")));
            }
        }
    }
    let last = &gateway
        .exchanges
        .last()
        .ok_or_else(|| mismatch("empty gateway session"))?
        .response;
    if last.output.iter().any(|item| client_call(item).is_some()) {
        return Err(mismatch("gateway session ends with pending client calls"));
    }
    if !last.output.iter().any(|item| {
        matches!(item, OutputItem::Message(message)
        if message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root")
            && message.phase == Some(MessagePhase::FinalAnswer))
    }) {
        return Err(mismatch("gateway session has no root final answer"));
    }
    Ok(())
}

fn client_call(item: &OutputItem) -> Option<(&str, ClientCallKind)> {
    match item {
        OutputItem::FunctionCall(call) => Some((&call.call_id, ClientCallKind::Function)),
        OutputItem::ShellCall(call) => Some((&call.call_id, ClientCallKind::Shell)),
        OutputItem::CustomToolCall(call) => Some((&call.call_id, ClientCallKind::Custom)),
        OutputItem::ToolSearchCall(call) if call.status == ToolSearchStatus::Completed => {
            Some((&call.call_id, ClientCallKind::ToolSearch))
        }
        _ => None,
    }
}

impl RecordedSession {
    /// Check the scenario's public names and discovery results, allowing different
    /// agents, call counts and model text between providers.
    pub fn assert_client_owned_tools(&self) -> Result<(), ContractMismatch> {
        let mut discovered = HashSet::new();
        let mut called = HashSet::new();
        let mut custom = false;
        for exchange in &self.exchanges {
            for input in &exchange.input {
                if let InputItem::ToolSearchOutput(output) = input {
                    for tool in &output.tools {
                        match tool {
                            ResponsesTool::Function(function) => {
                                discovered.insert((None, function.name.as_str()));
                            }
                            ResponsesTool::Namespace(namespace) => {
                                for member in &namespace.tools {
                                    if let CodexNamespaceMember::Function(function) = member {
                                        discovered.insert((Some(namespace.name.as_str()), function.name.as_str()));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            for item in &exchange.response.output {
                match item {
                    OutputItem::FunctionCall(call) => {
                        let identity = (call.namespace.as_deref(), call.name.as_str());
                        if !discovered.contains(&identity) {
                            return Err(mismatch("function was not returned by earlier tool discovery"));
                        }
                        called.insert(identity);
                    }
                    OutputItem::CustomToolCall(call) if call.name == "agentic_raw_echo" => custom = true,
                    _ => {}
                }
            }
        }
        if !custom || !called.contains(&(None, "get_weather")) || !called.contains(&(Some("travel"), "get_timezone")) {
            return Err(mismatch(
                "client-owned scenario must exercise weather, travel.get_timezone and custom echo",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn action_output(action: MultiAgentAction, text: &str) -> MultiAgentCallOutput {
        // This outer wire item is valid even when text contains an invalid result schema.
        serde_json::from_value(json!({
            "id": "maco_test", "call_id": "call_test", "action": action,
            "output": [{"type": "output_text", "text": text}]
        }))
        .unwrap()
    }

    #[test]
    fn collaboration_results_accept_observed_schemas() {
        for (action, result) in [
            (
                MultiAgentAction::SpawnAgent,
                json!({"task_name": "/root/arbitrary_name"}),
            ),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [
                    {"agent_name": "/root", "agent_status": "running"},
                    {"agent_name": "/root/a", "agent_status": {"completed": null}},
                    {"agent_name": "/root/b", "agent_status": {"completed": "any answer"}},
                    {"agent_name": "/root/c", "agent_status": "interrupted"},
                    {"agent_name": "/root/d", "agent_status": "failed"}
                ]}),
            ),
            (MultiAgentAction::InterruptAgent, json!({"previous_status": "running"})),
            (
                MultiAgentAction::InterruptAgent,
                json!({"previous_status": {"completed": null}}),
            ),
            (
                MultiAgentAction::WaitAgent,
                json!({"message": "any notification", "timed_out": false}),
            ),
            (
                MultiAgentAction::WaitAgent,
                json!({"message": "another notification", "timed_out": true}),
            ),
        ] {
            validate_collaboration_result(&action_output(action, &result.to_string())).unwrap();
        }
        for action in [MultiAgentAction::SendMessage, MultiAgentAction::FollowupTask] {
            validate_collaboration_result(&action_output(action, "")).unwrap();
        }
    }

    #[test]
    fn collaboration_results_reject_missing_wrong_and_mismatched_fields() {
        for (action, result) in [
            (MultiAgentAction::SpawnAgent, json!({})),
            (MultiAgentAction::SpawnAgent, json!({"task_name": 42})),
            (MultiAgentAction::ListAgents, json!({"agents": "not an array"})),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [{"agent_name": "/root"}]}),
            ),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [{"agent_status": "running"}]}),
            ),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [{"task_name": "/root", "status": "running"}]}),
            ),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [{"agent_name": "/root", "agent_status": {"completed": 42}}]}),
            ),
            (
                MultiAgentAction::ListAgents,
                json!({"agents": [{"agent_name": "/root", "agent_status": "unknown"}]}),
            ),
            (MultiAgentAction::InterruptAgent, json!({"previous_status": null})),
            (MultiAgentAction::InterruptAgent, json!({"previous_status": "unknown"})),
            (MultiAgentAction::WaitAgent, json!({"message": "missing timeout"})),
            (
                MultiAgentAction::WaitAgent,
                json!({"message": "wrong timeout", "timed_out": "false"}),
            ),
            (MultiAgentAction::WaitAgent, json!({"message": 42, "timed_out": false})),
            (
                MultiAgentAction::WaitAgent,
                json!({"task_name": "/root/valid_but_wrong_action"}),
            ),
            (MultiAgentAction::SpawnAgent, json!({"agents": []})),
        ] {
            let error = validate_collaboration_result(&action_output(action, &result.to_string())).unwrap_err();
            assert!(error.to_string().contains("call_test"));
        }
        for text in ["", "not JSON", "{", "null"] {
            assert!(validate_collaboration_result(&action_output(MultiAgentAction::ListAgents, text)).is_err());
        }
    }

    #[test]
    fn collaboration_error_results_require_a_string_for_every_action() {
        for action in [
            MultiAgentAction::SpawnAgent,
            MultiAgentAction::SendMessage,
            MultiAgentAction::FollowupTask,
            MultiAgentAction::WaitAgent,
            MultiAgentAction::InterruptAgent,
            MultiAgentAction::ListAgents,
        ] {
            validate_collaboration_result(&action_output(action, r#"{"error":"any model-visible error"}"#)).unwrap();
            for text in [r#"{"error":42}"#, r#"{"error":null}"#] {
                assert!(validate_collaboration_result(&action_output(action, text)).is_err());
            }
        }
    }
}
