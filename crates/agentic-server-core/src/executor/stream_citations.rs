//! File citation events are reconciled against grounded completed content.
//!
//! Upstream file annotation events may precede the text needed to validate their
//! offsets. Their completed content is authoritative; emit its restored citations
//! before the corresponding done event, with request-local deduplication. Other
//! annotation kinds, including null annotations, continue to pass through.
use std::collections::HashSet;

use crate::events::{EventFrame, SSEEventType, WireEvent};
use crate::types::event::OutputTextFileCitationAdded;
use crate::types::io::FileCitation;
use crate::utils::common::serialize_to_value;

use super::error::{ExecutorError, ExecutorResult};

#[derive(Clone, Default)]
pub(super) struct StreamCitations {
    // Global output index avoids collisions when providers reuse IDs between rounds.
    emitted: HashSet<(u64, usize, usize)>,
}

impl StreamCitations {
    pub(super) fn before_done(&mut self, frame: &EventFrame, offset: usize) -> ExecutorResult<Vec<EventFrame>> {
        let mut events = Vec::new();
        match frame.event_type {
            SSEEventType::ContentPartDone => {
                if let (Some(item_id), Some(index), Some(content_index), Some(part)) = (
                    frame.wire.rest.get("item_id").and_then(serde_json::Value::as_str),
                    frame.wire.output_index,
                    frame.wire.rest.get("content_index").and_then(serde_json::Value::as_u64),
                    frame.wire.rest.get("part"),
                ) {
                    self.content(
                        &mut events,
                        item_id,
                        index,
                        usize::try_from(content_index).map_err(|_| {
                            ExecutorError::StreamError("annotation content index exceeds platform bounds".to_owned())
                        })?,
                        part,
                        offset,
                    )?;
                }
            }
            SSEEventType::OutputItemDone => {
                if let (Some(index), Some(item)) = (frame.wire.output_index, frame.wire.rest.get("item")) {
                    self.item(&mut events, index, item, offset)?;
                }
            }
            SSEEventType::ResponseCompleted | SSEEventType::ResponseIncomplete | SSEEventType::ResponseFailed => {
                if let Some(output) = frame
                    .wire
                    .rest
                    .get("response")
                    .and_then(|response| response.get("output"))
                    .and_then(serde_json::Value::as_array)
                {
                    for (index, item) in output.iter().enumerate() {
                        self.item(
                            &mut events,
                            u64::try_from(index).map_err(|_| {
                                ExecutorError::StreamError("annotation output index exceeds platform bounds".to_owned())
                            })?,
                            item,
                            offset,
                        )?;
                    }
                }
            }
            _ => {}
        }
        Ok(events)
    }

    fn item(
        &mut self,
        events: &mut Vec<EventFrame>,
        index: u64,
        item: &serde_json::Value,
        offset: usize,
    ) -> ExecutorResult<()> {
        if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            return Ok(());
        }
        if let (Some(id), Some(content)) = (
            item.get("id").and_then(serde_json::Value::as_str),
            item.get("content").and_then(serde_json::Value::as_array),
        ) {
            for (content_index, part) in content.iter().enumerate() {
                self.content(events, id, index, content_index, part, offset)?;
            }
        }
        Ok(())
    }

    fn content(
        &mut self,
        events: &mut Vec<EventFrame>,
        item_id: &str,
        index: u64,
        content_index: usize,
        part: &serde_json::Value,
        offset: usize,
    ) -> ExecutorResult<()> {
        if part.get("type").and_then(serde_json::Value::as_str) != Some("output_text") {
            return Ok(());
        }
        let Some(annotations) = part.get("annotations").and_then(serde_json::Value::as_array) else {
            return Ok(());
        };
        let global_index = index.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        for (annotation_index, annotation) in annotations.iter().enumerate() {
            if annotation.get("type").and_then(serde_json::Value::as_str) != Some("file_citation") {
                continue;
            }
            let citation: FileCitation =
                serde_json::from_value(annotation.clone()).map_err(ExecutorError::JsonError)?;
            if !self.emitted.insert((global_index, content_index, annotation_index)) {
                continue;
            }
            // Each entry needs at least this much upstream JSON. The shared 1 MiB
            // response budget bounds normal state; retain a defensive independent cap.
            if self.emitted.len() > super::response_budget::MAX_EXECUTOR_RESPONSE_BYTES / 24 {
                return Err(ExecutorError::StreamError(
                    "stream annotation state exceeded the response budget".to_owned(),
                ));
            }
            let event = OutputTextFileCitationAdded {
                item_id: item_id.to_owned(),
                output_index: index,
                content_index,
                annotation_index,
                sequence_number: 0,
                annotation: citation,
            };
            let wire: WireEvent = serde_json::from_value(serialize_to_value(&event).map_err(ExecutorError::JsonError)?)
                .map_err(ExecutorError::JsonError)?;
            events.push(EventFrame {
                event_type: SSEEventType::OutputTextAnnotationAdded,
                payload: crate::events::EventPayload::None,
                wire,
            });
        }
        Ok(())
    }
}
