//! Multi-agent presentation: the response owner's relay projects each agent
//! round's local output indexes onto public ones before presenting its frames.
use super::{AgentRoundId, StreamRelay};
use crate::events::{EventFrame, SSEEventType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::collaboration::attribution;
use crate::tool::mcp::handler::started_list_tools_output_item;
use crate::types::agent::AgentIdentity;
use crate::types::event::MessageStatus;
use crate::types::io::{OutputItem, OutputMessageContent};
use crate::utils::common::serialize_to_value;
use serde_json::Value;

impl StreamRelay {
    pub(in crate::executor) fn has_live_agent_items(&self, agent: &AgentIdentity) -> bool {
        self.projection.has_live_items(agent)
    }

    pub(in crate::executor) async fn accept_agent_frame(
        &mut self,
        source: &AgentRoundId,
        mut frame: EventFrame,
    ) -> ExecutorResult<()> {
        if let Some(local) = frame.wire.output_index {
            let index = if frame.event_type == SSEEventType::OutputItemAdded {
                let item = frame
                    .wire
                    .rest
                    .get_mut("item")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| ExecutorError::StreamError("missing translated output item".into()))?;
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ExecutorError::StreamError("missing translated item ID".into()))?;
                let index = self.projection.added(source, local, id)?;
                item.insert(
                    "agent".into(),
                    serialize_to_value(&attribution(&source.agent)).map_err(ExecutorError::JsonError)?,
                );
                index
            } else {
                self.projection.index(source, local)?
            };
            frame.wire.output_index = Some(index as u64);
        }
        frame.wire.agent = Some(attribution(&source.agent));
        self.emit_local(&mut frame).await?;
        Ok(())
    }

    pub(in crate::executor) async fn emit_agent_item(&mut self, item: &OutputItem) -> ExecutorResult<usize> {
        let index = item
            .agent()
            .zip(item.id())
            .and_then(|(agent, id)| self.projection.take_item(&agent.agent_name, id));
        if let Some(index) = index {
            let mut frame = EventFrame::synthetic(
                SSEEventType::OutputItemDone,
                serde_json::Map::from_iter([
                    ("output_index".into(), index.into()),
                    (
                        "item".into(),
                        serialize_to_value(item).map_err(ExecutorError::JsonError)?,
                    ),
                ]),
            )
            .ok_or_else(|| ExecutorError::StreamError("missing item-done representation".into()))?;
            frame.wire.agent = item.agent().cloned();
            self.emit_local(&mut frame).await?;
            Ok(index)
        } else {
            let index = self.projection.reserve()?;
            if self.is_live() {
                self.emit_materialized_item(item, index).await?;
            }
            Ok(index)
        }
    }

    pub(in crate::executor) fn finish_agent_source(&mut self, source: &AgentRoundId) {
        self.projection.finish_source(source);
    }

    /// Present an already ingested completed item as its full public lifecycle.
    /// Used by concurrent round work: no task owns the public sequence or terminal.
    async fn emit_materialized_item(&mut self, item: &OutputItem, output_index: usize) -> ExecutorResult<()> {
        use serde_json::json;
        let mut started = item.clone();
        match &mut started {
            OutputItem::Message(message) => {
                message.content.clear();
                message.status = MessageStatus::InProgress;
            }
            OutputItem::FunctionCall(call) => {
                call.arguments.clear();
                call.status = MessageStatus::InProgress;
            }
            OutputItem::McpListTools(item) => started = started_list_tools_output_item(item),
            _ => {}
        }
        let mut events = vec![(
            SSEEventType::OutputItemAdded,
            json!({"output_index": output_index, "item": serialize_to_value(&started).map_err(ExecutorError::JsonError)?}),
        )];
        match item {
            OutputItem::McpListTools(list) => {
                events.push((
                    SSEEventType::McpListToolsInProgress,
                    json!({"output_index":output_index,"item_id":list.id}),
                ));
                events.push((
                    SSEEventType::McpListToolsCompleted,
                    json!({"output_index":output_index,"item_id":list.id}),
                ));
            }
            OutputItem::Message(message) => {
                for (content_index, part) in message.content.iter().enumerate() {
                    let mut added = part.clone();
                    match &mut added {
                        OutputMessageContent::InputText(text) => text.text.clear(),
                        OutputMessageContent::OutputText(text) => text.text.clear(),
                    }
                    events.push((SSEEventType::ContentPartAdded, json!({"output_index":output_index,"item_id":message.id,"content_index":content_index,"part":added})));
                    if let OutputMessageContent::OutputText(text) = part {
                        events.push((SSEEventType::OutputTextDelta, json!({"output_index":output_index,"item_id":message.id,"content_index":content_index,"delta":text.text})));
                        events.push((SSEEventType::OutputTextDone, json!({"output_index":output_index,"item_id":message.id,"content_index":content_index,"text":text.text})));
                    }
                    events.push((
                        SSEEventType::ContentPartDone,
                        json!({"output_index":output_index,"item_id":message.id,"content_index":content_index,"part":part}),
                    ));
                }
            }
            OutputItem::FunctionCall(call) => {
                events.push((
                    SSEEventType::FunctionCallArgumentsDelta,
                    json!({"output_index":output_index,"item_id":call.id,"delta":call.arguments}),
                ));
                events.push((
                    SSEEventType::FunctionCallArgumentsDone,
                    json!({"output_index":output_index,"item_id":call.id,"name":call.name,"arguments":call.arguments}),
                ));
            }
            _ => {}
        }
        events.push((
            SSEEventType::OutputItemDone,
            json!({"output_index":output_index,"item":item}),
        ));
        for (kind, fields) in events {
            let mut frame =
                EventFrame::synthetic(kind, fields.as_object().expect("event fields are an object").clone())
                    .ok_or_else(|| ExecutorError::StreamError("output event has no wire representation".into()))?;
            frame.wire.agent = item.agent().cloned();
            self.emit_local(&mut frame).await?;
        }
        Ok(())
    }
}
