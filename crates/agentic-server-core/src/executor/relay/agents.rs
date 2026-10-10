//! Multi-agent presentation: the response owner's relay projects each agent
//! round's local output indexes onto public ones before presenting its frames.
use super::{AgentRoundId, StreamRelay};
use crate::events::{EventFrame, SSEEventType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::item_lifecycle::{item_done_frame, materialized_item_frames};
use crate::executor::multi_agent::collaboration::attribution;
use crate::types::agent::AgentIdentity;
use crate::types::io::OutputItem;
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

    /// Complete an item through the relay. An item that agent frames already
    /// presented needs only `output_item.done`; one the coordinator created gets
    /// its full public lifecycle at a newly reserved index.
    pub(in crate::executor) async fn emit_agent_item(&mut self, item: &OutputItem) -> ExecutorResult<usize> {
        let presented = item
            .agent()
            .zip(item.id())
            .and_then(|(agent, id)| self.projection.take_item(&agent.agent_name, id));
        if let Some(index) = presented {
            if self.is_live() {
                let mut frame = item_done_frame(item, index)?;
                frame.wire.agent = item.agent().cloned();
                self.emit_local(&mut frame).await?;
            }
            return Ok(index);
        }
        let index = self.projection.reserve()?;
        if self.is_live() {
            for frame in materialized_item_frames(item, index) {
                let mut frame = frame?;
                frame.wire.agent = item.agent().cloned();
                self.emit_local(&mut frame).await?;
            }
        }
        Ok(index)
    }

    pub(in crate::executor) fn finish_agent_source(&mut self, source: &AgentRoundId) {
        self.projection.finish_source(source);
    }
}
