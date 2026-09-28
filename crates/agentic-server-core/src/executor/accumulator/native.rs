//! Internal ingestion for native semantic inference events.

use super::ResponseAccumulator;
use crate::events::{EventFrame, EventPayload, SSEEventType, SSEItemType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::{InputTokenDetails, OutputTokenDetails, ResponseUsage};
use serde_json::{Map, Value, json};
use vllm_responses::{InferenceEvent, InferenceFailure, InferenceUsage, OutputIndex};

impl ResponseAccumulator {
    /// Processes pre-collected semantic inference events synchronously.
    ///
    /// This is the typed counterpart to [`Self::from_sse_lines`]. Callers that
    /// already have native model events must use it instead of serializing an
    /// SSE stream solely for response assembly.
    ///
    /// # Errors
    ///
    /// Returns an error when the event sequence violates the Responses
    /// lifecycle or conflicts with accumulated output.
    pub fn from_inference_events(
        response_id: String,
        events: impl IntoIterator<Item = InferenceEvent>,
        conversation_id: Option<&str>,
    ) -> ExecutorResult<Self> {
        let mut acc = Self::with_validation(
            response_id,
            conversation_id.map(str::to_string),
            super::Validation::Strict,
        );
        for event in events {
            let _ = acc.process_inference_event(event)?;
        }
        acc.finish_strict_stream()?;
        Ok(acc)
    }

    /// Validates and folds one typed native inference event.
    ///
    /// The semantic event contract is owned by `vllm-responses`; this
    /// adapter only materializes the existing internal wire frame required by
    /// validation, translation, and client delivery.
    pub(in crate::executor) fn process_inference_event(
        &mut self,
        event: InferenceEvent,
    ) -> ExecutorResult<Option<EventFrame>> {
        let frame = semantic_event_frame(event, &self.response_id)?;
        self.process_event_frame(frame)
    }

    /// Validates and folds a normalized internal wire event.
    ///
    /// HTTP/SSE parsing and [`Self::process_inference_event`] share this
    /// lifecycle folding path.
    pub(in crate::executor) fn process_event_frame(&mut self, frame: EventFrame) -> ExecutorResult<Option<EventFrame>> {
        let disposition = self.process_normalized_event(&frame)?;
        Ok(disposition.into_frame(frame))
    }
}

fn semantic_event_frame(event: InferenceEvent, response_id: &str) -> ExecutorResult<EventFrame> {
    let frame = match event {
        InferenceEvent::Started => response_frame(SSEEventType::ResponseCreated, response_id, "in_progress", None),
        InferenceEvent::InProgress => {
            response_frame(SSEEventType::ResponseInProgress, response_id, "in_progress", None)
        }
        event @ (InferenceEvent::TextStarted { .. }
        | InferenceEvent::TextDelta { .. }
        | InferenceEvent::TextCompleted { .. }) => text_event_frame(event),
        event @ (InferenceEvent::ReasoningStarted { .. }
        | InferenceEvent::ReasoningTextDelta { .. }
        | InferenceEvent::ReasoningCompleted { .. }) => reasoning_event_frame(event),
        event @ (InferenceEvent::FunctionCallStarted { .. }
        | InferenceEvent::FunctionCallArgumentsDelta { .. }
        | InferenceEvent::FunctionCallCompleted { .. }) => function_event_frame(event),
        InferenceEvent::Completed { usage } => {
            response_frame(SSEEventType::ResponseCompleted, response_id, "completed", usage)
        }
        InferenceEvent::Incomplete { usage, reason } => {
            let mut frame = response_frame(SSEEventType::ResponseIncomplete, response_id, "incomplete", usage);
            if let Some(reason) = reason {
                frame.wire.rest.entry("response").and_modify(|response| {
                    response["incomplete_details"] = json!({"reason": reason});
                });
            }
            frame
        }
        InferenceEvent::Failed { usage, failure } => failure_frame(response_id, usage, &failure),
        _ => {
            return Err(ExecutorError::StreamError(
                "unsupported native inference event".to_owned(),
            ));
        }
    };

    Ok(frame)
}

fn text_event_frame(event: InferenceEvent) -> EventFrame {
    match event {
        InferenceEvent::TextStarted { item_id, output_index } => output_item_added(
            output_index,
            &item_id,
            SSEItemType::Message,
            json!({"type":"message","role":"assistant","content":[],"status":"in_progress"}),
            None,
            None,
        ),
        InferenceEvent::TextDelta {
            item_id,
            output_index,
            delta,
        } => delta_frame(
            SSEEventType::OutputTextDelta,
            EventPayload::TextDelta {
                delta: delta.clone(),
                item_id: item_id.clone(),
                output_index: Some(output_index.0),
                content_index: 0,
            },
            output_index,
            item_id,
            "delta",
            Value::String(delta),
            Some(("content_index", Value::from(0))),
        ),
        InferenceEvent::TextCompleted {
            item_id,
            output_index,
            text,
        } => output_item_done(
            output_index,
            item_id,
            SSEItemType::Message,
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}],"status":"completed"}),
        ),
        _ => unreachable!("text event matcher only calls this with text events"),
    }
}

fn reasoning_event_frame(event: InferenceEvent) -> EventFrame {
    match event {
        InferenceEvent::ReasoningStarted { item_id, output_index } => output_item_added(
            output_index,
            &item_id,
            SSEItemType::Reasoning,
            json!({"type":"reasoning","summary":[],"content":[],"status":"in_progress"}),
            None,
            None,
        ),
        InferenceEvent::ReasoningTextDelta {
            item_id,
            output_index,
            delta,
        } => delta_frame(
            SSEEventType::ReasoningTextDelta,
            EventPayload::ReasoningTextDelta {
                delta: delta.clone(),
                item_id: item_id.clone(),
                output_index: Some(output_index.0),
                content_index: 0,
            },
            output_index,
            item_id,
            "delta",
            Value::String(delta),
            Some(("content_index", Value::from(0))),
        ),
        InferenceEvent::ReasoningCompleted {
            item_id,
            output_index,
            text,
        } => output_item_done(
            output_index,
            item_id,
            SSEItemType::Reasoning,
            json!({"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":text}],"status":"completed"}),
        ),
        _ => unreachable!("reasoning event matcher only calls this with reasoning events"),
    }
}

fn function_event_frame(event: InferenceEvent) -> EventFrame {
    match event {
        InferenceEvent::FunctionCallStarted {
            item_id,
            output_index,
            call_id,
            name,
        } => output_item_added(
            output_index,
            &item_id,
            SSEItemType::FunctionCall,
            json!({"type":"function_call","call_id":call_id,"name":name,"arguments":"","status":"in_progress"}),
            Some(call_id),
            Some(name),
        ),
        InferenceEvent::FunctionCallArgumentsDelta {
            item_id,
            output_index,
            delta,
        } => delta_frame(
            SSEEventType::FunctionCallArgumentsDelta,
            EventPayload::FunctionCallArgsDelta {
                delta: delta.clone(),
                call_id: None,
                item_id: item_id.clone(),
                output_index: Some(output_index.0),
            },
            output_index,
            item_id,
            "delta",
            Value::String(delta),
            None,
        ),
        InferenceEvent::FunctionCallCompleted {
            item_id,
            output_index,
            call_id,
            name,
            arguments,
        } => output_item_done(
            output_index,
            item_id,
            SSEItemType::FunctionCall,
            json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments,"status":"completed"}),
        ),
        _ => unreachable!("function event matcher only calls this with function-call events"),
    }
}

fn response_frame(
    event_type: SSEEventType,
    response_id: &str,
    status: &str,
    usage: Option<InferenceUsage>,
) -> EventFrame {
    let usage = usage.map(response_usage);
    let mut response = Map::new();
    response.insert("id".to_owned(), Value::String(response_id.to_owned()));
    response.insert("status".to_owned(), Value::String(status.to_owned()));
    if let Some(usage) = usage {
        response.insert("usage".to_owned(), response_usage_value(usage));
    }
    EventFrame {
        event_type,
        payload: EventPayload::Response {
            id: response_id.to_owned(),
            status: status.to_owned(),
            usage,
        },
        wire: wire_event(
            event_type,
            None,
            event_map([("response".to_owned(), Value::Object(response))]),
        ),
    }
}

fn output_item_added(
    output_index: OutputIndex,
    item_id: &str,
    item_type: SSEItemType,
    mut item: Value,
    call_id: Option<String>,
    name: Option<String>,
) -> EventFrame {
    item["id"] = Value::String(item_id.to_owned());
    EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::OutputItemAdded {
            item_id: item_id.to_owned(),
            item_type,
            output_index: Some(output_index.0),
            name,
            namespace: None,
            call_id,
            shell_call: None,
        },
        wire: wire_event(
            SSEEventType::OutputItemAdded,
            Some(output_index),
            event_map([("item".to_owned(), item)]),
        ),
    }
}

fn output_item_done(output_index: OutputIndex, item_id: String, item_type: SSEItemType, mut item: Value) -> EventFrame {
    item["id"] = Value::String(item_id.clone());
    EventFrame {
        event_type: SSEEventType::OutputItemDone,
        payload: EventPayload::OutputItemDone {
            item_id,
            item_type,
            output_index: Some(output_index.0),
            item: item.clone(),
        },
        wire: wire_event(
            SSEEventType::OutputItemDone,
            Some(output_index),
            event_map([("item".to_owned(), item)]),
        ),
    }
}

fn delta_frame(
    event_type: SSEEventType,
    payload: EventPayload,
    output_index: OutputIndex,
    item_id: String,
    field: &str,
    value: Value,
    extra: Option<(&str, Value)>,
) -> EventFrame {
    let mut rest = event_map([
        ("item_id".to_owned(), Value::String(item_id)),
        (field.to_owned(), value),
    ]);
    if let Some((key, value)) = extra {
        rest.insert(key.to_owned(), value);
    }
    EventFrame {
        event_type,
        payload,
        wire: wire_event(event_type, Some(output_index), rest),
    }
}

fn failure_frame(response_id: &str, usage: Option<InferenceUsage>, failure: &InferenceFailure) -> EventFrame {
    let mut frame = response_frame(SSEEventType::ResponseFailed, response_id, "failed", usage);
    frame.wire.rest.entry("response").and_modify(|response| {
        response["error"] =
            json!({"code": failure.code.as_deref().unwrap_or("server_error"), "message": failure.message});
    });
    frame
}

fn response_usage(usage: InferenceUsage) -> ResponseUsage {
    let input_tokens = to_i64(usage.input_tokens);
    let output_tokens = to_i64(usage.output_tokens);
    ResponseUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens.saturating_add(output_tokens),
        input_tokens_details: InputTokenDetails {
            cached_tokens: to_i64(usage.cached_tokens),
        },
        output_tokens_details: OutputTokenDetails {
            reasoning_tokens: to_i64(usage.reasoning_tokens),
        },
    }
}

fn response_usage_value(usage: ResponseUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "input_tokens_details": {"cached_tokens": usage.input_tokens_details.cached_tokens},
        "output_tokens_details": {"reasoning_tokens": usage.output_tokens_details.reasoning_tokens},
    })
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn wire_event(event_type: SSEEventType, output_index: Option<OutputIndex>, rest: Map<String, Value>) -> WireEvent {
    let event_type = <&str>::try_from(event_type).expect("supported semantic event type");
    WireEvent {
        event_type: Some(event_type.to_owned()),
        sequence_number: None,
        output_index: output_index.map(|index| u64::from(index.0)),
        rest,
    }
}

fn event_map(entries: impl IntoIterator<Item = (String, Value)>) -> Map<String, Value> {
    entries.into_iter().collect()
}
