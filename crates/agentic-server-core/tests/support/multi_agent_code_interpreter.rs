//! Assertions over real recordings, allowing model-dependent programs and call counts.
use std::collections::HashSet;

use agentic_core::events::{EventFrame, SSEEventType, normalize_sse_line};
use agentic_core::types::io::code_interpreter::{CodeInterpreterCallOutput, CodeInterpreterCallStatus};
use agentic_core::types::io::{MessagePhase, OutputItem};
use agentic_core::types::request_response::ResponsePayload;
use serde_json::Value;

use super::{ContractMismatch, RecordedSession, mismatch};
use crate::support::Turn;

impl RecordedSession {
    pub fn assert_code_interpreter(&self, require_execution_logs: bool) -> Result<(), ContractMismatch> {
        if self.exchanges.len() != 1 {
            return Err(mismatch(
                "hosted interpreter scenario must finish without client continuations",
            ));
        }
        let exchange = &self.exchanges[0];
        if exchange.parallel_tool_calls != Some(true) || exchange.max_concurrent_subagents != Some(2) {
            return Err(mismatch(
                "interpreter scenario must enable parallel tools and two child slots",
            ));
        }
        let mut executed = HashSet::new();
        let mut searched = HashSet::new();
        let mut finished = HashSet::new();
        for item in &exchange.response.output {
            let agent = item.agent().map(|agent| agent.agent_name.as_str());
            match item {
                OutputItem::CodeInterpreterCall(call) => {
                    let agent = agent
                        .filter(|name| name.starts_with("/root/"))
                        .ok_or_else(|| mismatch("interpreter call must belong to a child"))?;
                    if call.status != CodeInterpreterCallStatus::Completed
                        || call.code.trim().is_empty()
                        || call.container_id.is_empty()
                    {
                        return Err(mismatch("interpreter call has no completed program or container"));
                    }
                    // The OpenAI references expose outputs:null even on completion;
                    // the gateway recordings expose actual execution logs.
                    if require_execution_logs
                        && !call.outputs.as_ref().is_some_and(|outputs| {
                            outputs.iter().any(|output| {
                                matches!(output, CodeInterpreterCallOutput::Logs { logs } if !logs.trim().is_empty())
                            })
                        })
                    {
                        return Err(mismatch("gateway interpreter call has no execution logs"));
                    }
                    executed.insert(agent);
                }
                OutputItem::WebSearchCall(_) => {
                    searched.extend(agent);
                }
                OutputItem::Message(message) if message.phase == Some(MessagePhase::FinalAnswer) => {
                    finished.extend(agent);
                }
                _ => {}
            }
        }
        if executed.len() != 2 || !executed.is_subset(&searched) || !executed.is_subset(&finished) {
            return Err(mismatch(
                "both children must search, execute Python, and finish their assignments",
            ));
        }
        Ok(())
    }
}

pub(super) fn validate_stream(turn: &Turn, response: &ResponsePayload) -> Result<(), ContractMismatch> {
    if !response
        .output
        .iter()
        .any(|item| matches!(item, OutputItem::CodeInterpreterCall(_)))
    {
        return Ok(());
    }
    let Some(events) = &turn.response.sse else {
        return Ok(());
    };
    let frames: Vec<_> = events
        .iter()
        .flat_map(|event| event.lines())
        .filter_map(normalize_sse_line)
        .collect();
    for (index, item) in response.output.iter().enumerate() {
        let OutputItem::CodeInterpreterCall(call) = item else {
            continue;
        };
        let mut lifecycle = Vec::new();
        let mut code = String::new();
        for frame in frames.iter().filter(|frame| {
            frame_item_id(frame) == Some(call.id.as_str()) || frame.wire.output_index == Some(index as u64)
        }) {
            if frame.wire.output_index != Some(index as u64) || frame.wire.agent != call.agent {
                return Err(mismatch("interpreter stream changed output index or agent ownership"));
            }
            if frame_item_id(frame) != Some(call.id.as_str()) {
                return Err(mismatch("interpreter event has the wrong item ID"));
            }
            if frame.event_type == SSEEventType::CodeInterpreterCallCodeDelta {
                code.push_str(
                    frame
                        .wire
                        .rest
                        .get("delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| mismatch("missing interpreter code delta"))?,
                );
                // OpenAI sends many chunks; the gateway emits one complete chunk.
                if lifecycle.last() == Some(&SSEEventType::CodeInterpreterCallCodeDelta) {
                    continue;
                }
            }
            if frame.event_type == SSEEventType::CodeInterpreterCallCodeDone
                && frame.wire.rest.get("code").and_then(Value::as_str) != Some(call.code.as_str())
            {
                return Err(mismatch("interpreter code.done differs from the completed item"));
            }
            lifecycle.push(frame.event_type);
        }
        if code != call.code
            || lifecycle
                != [
                    SSEEventType::OutputItemAdded,
                    SSEEventType::CodeInterpreterCallInProgress,
                    SSEEventType::CodeInterpreterCallCodeDelta,
                    SSEEventType::CodeInterpreterCallCodeDone,
                    SSEEventType::CodeInterpreterCallInterpreting,
                    SSEEventType::CodeInterpreterCallCompleted,
                    SSEEventType::OutputItemDone,
                ]
        {
            return Err(mismatch(
                "interpreter stream has an incomplete or out-of-order lifecycle",
            ));
        }
    }
    Ok(())
}

fn frame_item_id(frame: &EventFrame) -> Option<&str> {
    frame
        .wire
        .rest
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| frame.wire.rest.get("item")?.get("id")?.as_str())
}
