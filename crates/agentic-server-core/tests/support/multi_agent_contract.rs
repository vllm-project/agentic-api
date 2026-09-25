//! Integration-only comparison of independently recorded provider exchanges.
use flate2::read::GzDecoder;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use agentic_core::events::{SSEEventType, normalize_sse_line};
use agentic_core::types::io::{InputItem, MessagePhase, MultiAgentAction, OutputItem};
use agentic_core::types::request_response::ResponsePayload;
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
    kinds: HashSet<String>,
}

/// Model-dependent text, IDs, agent names and action counts may differ between
/// providers. Identity relationships within each trace must still be exact.
pub struct ComparisonPolicy {
    pub require_reference_tool_kinds: bool,
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
                kinds,
            });
        }
        let session = Self { exchanges };
        session.validate()?;
        Ok(session)
    }

    fn validate(&self) -> Result<(), ContractMismatch> {
        let mut pending = HashMap::new();
        let mut previous = None;
        for exchange in &self.exchanges {
            if exchange.previous_response_id.as_deref() != previous {
                return Err(mismatch("broken previous_response_id chain"));
            }
            for input in &exchange.input {
                let (id, kind) = match input {
                    InputItem::FunctionCallOutput(output) => (&output.call_id, "function"),
                    InputItem::ShellCallOutput(output) => (&output.call_id, "shell"),
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
                    }
                    OutputItem::FunctionCall(call) => {
                        if agent.is_none() || pending.insert(call.call_id.clone(), "function").is_some() {
                            return Err(mismatch("invalid function-call owner or ID"));
                        }
                    }
                    OutputItem::ShellCall(call) => {
                        if agent.is_none() || pending.insert(call.call_id.clone(), "shell").is_some() {
                            return Err(mismatch("invalid shell-call owner or ID"));
                        }
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
        Ok(())
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
    let required_agents = reference
        .delegated_agents()
        .len()
        .min(usize::try_from(reference_limit).unwrap_or(usize::MAX));
    if gateway.delegated_agents().len() < required_agents {
        return Err(mismatch(
            "gateway delegated fewer independent child tasks than reference",
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
            "shell_call",
            "web_search_call",
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
    if last
        .output
        .iter()
        .any(|item| matches!(item, OutputItem::FunctionCall(_) | OutputItem::ShellCall(_)))
    {
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
