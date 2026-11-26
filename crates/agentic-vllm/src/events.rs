//! Adapt shared-vLLM output into existing core events without assembling it again.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use crate::Result;
use agentic_core::events::{EventFrame, SSEEventType, WireEvent};
use agentic_core::executor::error::ExecutorError;
use agentic_core::executor::inference::direct::{EventStream, GenerationControl};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{
    FunctionToolCall, InputTokenDetails, OutputItem, OutputMessage, OutputMessageContent, OutputTextContent,
    OutputTokenDetails, ReasoningOutput, ReasoningStatus, ReasoningTextContent, ResponseUsage,
};
use agentic_core::types::request_response::{IncompleteDetails, ResponsePayload, StandardResponseFields};
use agentic_core::utils::common::{serialize_to_value, utcnow_str};
use async_stream::stream;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use vllm_chat::{AssistantBlockKind, AssistantContentBlock, ChatEvent, ChatLlm, ChatTokenUsage, FinishReason};

use crate::runtime::backend_error;

pub(crate) fn convert(
    events: impl Stream<Item = vllm_chat::Result<ChatEvent>> + Send + 'static,
    control: GenerationControl,
    runtime: Arc<ChatLlm>,
) -> EventStream {
    Box::pin(stream! {
        // Hold the runtime until this generation has closed its engine stream.
        let mut adapter = EventAdapter::new(&control.response_id, &control.request_id, runtime.served_model_name());
        let _runtime = runtime;
        futures::pin_mut!(events);
        loop {
            let event = tokio::select! {
                biased;
                () = control.cancellation.cancelled() => {
                    yield Err(ExecutorError::StreamError("direct inference cancelled".to_owned()));
                    return;
                }
                event = events.next() => event,
            };
            let Some(event) = event else { return; };
            match event.map_err(|error| Box::new(backend_error(error))).and_then(|event| adapter.convert(event)) {
                Ok(frames) => {
                    for frame in frames { yield Ok(frame); }
                }
                Err(error) => { yield Err(*error); return; }
            }
        }
    })
}

/// vLLM numbers tool calls separately from text/reasoning blocks.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum SourceIndex {
    Block(usize),
    Tool(usize),
}

struct EventAdapter {
    request_id: String,
    response: ResponsePayload,
    indexes: HashMap<SourceIndex, u32>,
}

impl EventAdapter {
    fn new(response_id: &str, request_id: &str, model: &str) -> Self {
        Self {
            request_id: request_id.to_owned(),
            indexes: HashMap::new(),
            response: ResponsePayload {
                id: response_id.to_owned(),
                object: "response".to_owned(),
                created_at: utcnow_str(),
                model: model.to_owned(),
                status: "in_progress".to_owned(),
                output: Vec::new(),
                usage: None,
                incomplete_details: None,
                error: None,
                previous_response_id: None,
                conversation: None,
                max_tool_calls: None,
                service_tier: None,
                standard_fields: StandardResponseFields::default(),
                instructions: None,
                tools: None,
                tool_choice: None,
            },
        }
    }

    fn output_index(&mut self, source: SourceIndex) -> Result<u32> {
        let next = u32::try_from(self.indexes.len()).map_err(|_| protocol_error("too many direct output items"))?;
        Ok(*self.indexes.entry(source).or_insert(next))
    }

    fn item_id(&self, kind: AssistantBlockKind, index: u32) -> String {
        let prefix = match kind {
            AssistantBlockKind::Text => "msg",
            AssistantBlockKind::Reasoning => "rs",
            AssistantBlockKind::ToolCall => "fc",
        };
        format!("{prefix}_{}_{index}", self.request_id)
    }

    fn convert(&mut self, event: ChatEvent) -> Result<Vec<EventFrame>> {
        let frame = match event {
            ChatEvent::Start { .. } => {
                return Ok(vec![
                    self.response_frame(SSEEventType::ResponseCreated)?,
                    self.response_frame(SSEEventType::ResponseInProgress)?,
                ]);
            }
            ChatEvent::BlockStart { index, kind } => return self.block_start(index, kind),
            ChatEvent::BlockDelta { index, kind, delta, .. } => {
                let index = self.output_index(SourceIndex::Block(index))?;
                let event_type = match kind {
                    AssistantBlockKind::Text => SSEEventType::OutputTextDelta,
                    AssistantBlockKind::Reasoning => SSEEventType::ReasoningTextDelta,
                    AssistantBlockKind::ToolCall => {
                        return Err(protocol_error("tool call emitted as a text block").into());
                    }
                };
                delta_frame(event_type, index, self.item_id(kind, index), delta, true)?
            }
            ChatEvent::BlockEnd { index, block } => return self.block_end(index, block),
            ChatEvent::ToolCallStart { index, id, name } => {
                let index = self.output_index(SourceIndex::Tool(index))?;
                let item = self.function_item(index, id, name, String::new(), MessageStatus::InProgress);
                item_frame(SSEEventType::OutputItemAdded, index, &item)?
            }
            ChatEvent::ToolCallArgumentsDelta { index, delta } => {
                let index = self.output_index(SourceIndex::Tool(index))?;
                delta_frame(
                    SSEEventType::FunctionCallArgumentsDelta,
                    index,
                    self.item_id(AssistantBlockKind::ToolCall, index),
                    delta,
                    false,
                )?
            }
            ChatEvent::ToolCallEnd { index, call } => {
                let index = self.output_index(SourceIndex::Tool(index))?;
                let mut wire = wire(SSEEventType::FunctionCallArgumentsDone)?;
                wire.output_index = Some(u64::from(index));
                wire.rest.insert(
                    "item_id".to_owned(),
                    Value::String(self.item_id(AssistantBlockKind::ToolCall, index)),
                );
                wire.rest.insert("name".to_owned(), Value::String(call.name.clone()));
                wire.rest
                    .insert("arguments".to_owned(), Value::String(call.arguments.clone()));
                let arguments_done = EventFrame::try_from(wire).map_err(ExecutorError::from)?;
                let item = self.function_item(index, call.id, call.name, call.arguments, MessageStatus::Completed);
                return Ok(vec![
                    arguments_done,
                    item_frame(SSEEventType::OutputItemDone, index, &item)?,
                ]);
            }
            ChatEvent::LogprobsDelta { logprobs: None, .. } => return Ok(Vec::new()),
            ChatEvent::LogprobsDelta { .. } => return Err(protocol_error("unexpected direct logprobs output").into()),
            ChatEvent::Done {
                usage, finish_reason, ..
            } => {
                self.response.usage = Some(response_usage(usage)?);
                let (event_type, reason) = match finish_reason {
                    FinishReason::Stop(_) => (SSEEventType::ResponseCompleted, None),
                    FinishReason::Length => (SSEEventType::ResponseIncomplete, Some("max_output_tokens")),
                    FinishReason::Repetition(_) => (SSEEventType::ResponseIncomplete, Some("repetition")),
                    FinishReason::Abort => return Err(protocol_error("direct generation aborted").into()),
                    FinishReason::Error(_) => {
                        return Err(protocol_error("direct engine reported a request failure").into());
                    }
                };
                if reason.is_some() { "incomplete" } else { "completed" }.clone_into(&mut self.response.status);
                self.response.incomplete_details = reason.map(|reason| IncompleteDetails {
                    reason: Some(reason.to_owned()),
                });
                self.response_frame(event_type)?
            }
        };
        Ok(vec![frame])
    }

    fn block_start(&mut self, index: usize, kind: AssistantBlockKind) -> Result<Vec<EventFrame>> {
        let index = self.output_index(SourceIndex::Block(index))?;
        let item = text_item(
            self.item_id(kind, index),
            kind,
            String::new(),
            MessageStatus::InProgress,
        )?;
        let mut frames = vec![item_frame(SSEEventType::OutputItemAdded, index, &item)?];
        if kind == AssistantBlockKind::Text {
            frames.push(content_frame(
                SSEEventType::ContentPartAdded,
                index,
                self.item_id(kind, index),
                String::new(),
            )?);
        } else if kind == AssistantBlockKind::Reasoning {
            frames.push(reasoning_part_frame(
                SSEEventType::ReasoningPartAdded,
                index,
                self.item_id(kind, index),
                String::new(),
            )?);
        }
        Ok(frames)
    }

    fn block_end(&mut self, index: usize, block: AssistantContentBlock) -> Result<Vec<EventFrame>> {
        let index = self.output_index(SourceIndex::Block(index))?;
        let kind = block.kind();
        let text = match block {
            AssistantContentBlock::Text { text } | AssistantContentBlock::Reasoning { text } => text,
            AssistantContentBlock::ToolCall(_) => {
                return Err(protocol_error("tool call ended as a text block").into());
            }
        };
        let id = self.item_id(kind, index);
        let mut frames = vec![text_done_frame(kind, index, id.clone(), text.clone())?];
        frames.push(match kind {
            AssistantBlockKind::Text => content_frame(SSEEventType::ContentPartDone, index, id.clone(), text.clone())?,
            AssistantBlockKind::Reasoning => {
                reasoning_part_frame(SSEEventType::ReasoningPartDone, index, id.clone(), text.clone())?
            }
            AssistantBlockKind::ToolCall => {
                return Err(protocol_error("tool call ended as a text block").into());
            }
        });
        let item = text_item(id, kind, text, MessageStatus::Completed)?;
        frames.push(item_frame(SSEEventType::OutputItemDone, index, &item)?);
        Ok(frames)
    }

    fn response_frame(&self, event_type: SSEEventType) -> Result<EventFrame> {
        let mut wire = wire(event_type)?;
        let mut response = serialize_to_value(&self.response).map_err(ExecutorError::from)?;
        // Item events are authoritative. An empty terminal output would assert
        // that no items were generated and conflict with the shared accumulator.
        // Leave final response assembly to that accumulator.
        if let Some(fields) = response.as_object_mut() {
            fields.remove("output");
        }
        wire.rest.insert("response".to_owned(), response);
        Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
    }

    fn function_item(
        &self,
        index: u32,
        call_id: String,
        name: String,
        arguments: String,
        status: MessageStatus,
    ) -> OutputItem {
        OutputItem::FunctionCall(FunctionToolCall {
            id: self.item_id(AssistantBlockKind::ToolCall, index),
            call_id,
            name,
            arguments,
            status,
            namespace: None,
            async_execution: false,
            agent: None,
        })
    }
}

fn text_item(id: String, kind: AssistantBlockKind, text: String, status: MessageStatus) -> Result<OutputItem> {
    match kind {
        AssistantBlockKind::Text => {
            let mut message = OutputMessage::new(id, status);
            if status == MessageStatus::Completed {
                message.content.push(OutputTextContent::new(text).into());
            }
            Ok(OutputItem::Message(message))
        }
        AssistantBlockKind::Reasoning => {
            let mut reasoning = ReasoningOutput::new(id);
            reasoning.status = Some(match status {
                MessageStatus::InProgress => ReasoningStatus::InProgress,
                MessageStatus::Completed => ReasoningStatus::Completed,
            });
            if status == MessageStatus::Completed {
                reasoning.content.push(ReasoningTextContent::new(text));
            }
            Ok(OutputItem::Reasoning(reasoning))
        }
        AssistantBlockKind::ToolCall => Err(protocol_error("tool call emitted as a text block").into()),
    }
}

fn item_frame(event_type: SSEEventType, index: u32, item: &OutputItem) -> Result<EventFrame> {
    let mut wire = wire(event_type)?;
    wire.output_index = Some(u64::from(index));
    wire.rest.insert(
        "item".to_owned(),
        serialize_to_value(item).map_err(ExecutorError::from)?,
    );
    Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
}

fn delta_frame(
    event_type: SSEEventType,
    index: u32,
    item_id: String,
    delta: String,
    content: bool,
) -> Result<EventFrame> {
    let mut wire = wire(event_type)?;
    wire.output_index = Some(u64::from(index));
    wire.rest.insert("item_id".to_owned(), Value::String(item_id));
    wire.rest.insert("delta".to_owned(), Value::String(delta));
    if content {
        wire.rest.insert("content_index".to_owned(), json!(0));
    }
    Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
}

fn content_frame(event_type: SSEEventType, index: u32, item_id: String, text: String) -> Result<EventFrame> {
    let mut wire = wire(event_type)?;
    wire.output_index = Some(u64::from(index));
    wire.rest.insert("item_id".to_owned(), Value::String(item_id));
    wire.rest.insert("content_index".to_owned(), json!(0));
    wire.rest.insert(
        "part".to_owned(),
        serialize_to_value(&OutputMessageContent::OutputText(OutputTextContent::new(text)))
            .map_err(ExecutorError::from)?,
    );
    Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
}

fn reasoning_part_frame(event_type: SSEEventType, index: u32, item_id: String, text: String) -> Result<EventFrame> {
    let mut wire = wire(event_type)?;
    wire.output_index = Some(u64::from(index));
    wire.rest.insert("item_id".to_owned(), Value::String(item_id));
    wire.rest.insert("content_index".to_owned(), json!(0));
    wire.rest.insert(
        "part".to_owned(),
        serialize_to_value(&ReasoningTextContent::new(text)).map_err(ExecutorError::from)?,
    );
    Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
}

fn text_done_frame(kind: AssistantBlockKind, index: u32, item_id: String, text: String) -> Result<EventFrame> {
    let event_type = match kind {
        AssistantBlockKind::Text => SSEEventType::OutputTextDone,
        AssistantBlockKind::Reasoning => SSEEventType::ReasoningTextDone,
        AssistantBlockKind::ToolCall => return Err(protocol_error("tool call emitted as a text block").into()),
    };
    let mut wire = wire(event_type)?;
    wire.output_index = Some(u64::from(index));
    wire.rest.insert("item_id".to_owned(), Value::String(item_id));
    wire.rest.insert("content_index".to_owned(), json!(0));
    wire.rest.insert("text".to_owned(), Value::String(text));
    Ok(EventFrame::try_from(wire).map_err(ExecutorError::from)?)
}

fn wire(event_type: SSEEventType) -> Result<WireEvent> {
    let name = <&str>::try_from(event_type).map_err(|()| protocol_error("event has no wire name"))?;
    Ok(WireEvent::new(name))
}

fn response_usage(usage: ChatTokenUsage) -> Result<ResponseUsage> {
    if usage.reasoning_tokens > usage.output_token_count
        || usage.prompt_cache_usage.cached_token_count > usage.prompt_token_count
    {
        return Err(protocol_error("direct token usage has inconsistent subset counts").into());
    }
    let count =
        |value| -> Result<i64> { Ok(i64::try_from(value).map_err(|_| protocol_error("direct token usage overflow"))?) };
    let input_tokens = count(usage.prompt_token_count)?;
    let output_tokens = count(usage.output_token_count)?;
    Ok(ResponseUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens
            .checked_add(output_tokens)
            .ok_or_else(|| protocol_error("direct token usage overflow"))?,
        input_tokens_details: InputTokenDetails {
            cached_tokens: count(usage.prompt_cache_usage.cached_token_count)?,
            cache_write_tokens: Some(count(usage.prompt_cache_usage.cache_creation_token_count)?),
        },
        output_tokens_details: OutputTokenDetails {
            reasoning_tokens: count(usage.reasoning_tokens)?,
        },
    })
}

fn protocol_error(message: &str) -> ExecutorError {
    ExecutorError::StreamError(message.to_owned())
}
