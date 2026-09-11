use super::tool_search::ToolSearchStreamState;
use super::{
    HasTranslator, MAX_PENDING_FUNCTION_BYTES, ToolEvent, ToolTranslator, Translation, TranslationContext,
    TranslationOutcome, resolved_output_index,
};
use crate::events::{EventFrame, EventPayload, SSEEventType, SSEItemType};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::{CodexNamespaceHandler, CustomHandler, FunctionHandler, ToolSearchHandler, ToolType, tool_search};
use crate::utils::common::serialize_to_string;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTerminalState {
    Open,
    Completed,
    Aborted,
}

#[derive(Debug, Default)]
struct PendingFunctionCall {
    output_index: u32,
    internal_item_id: Option<String>,
    frames: Vec<EventFrame>,
    bytes: usize,
}

#[derive(Debug)]
enum ActiveCall {
    Client(Box<dyn ToolTranslator>),
    // Gateway execution produces this call's public lifecycle.
    Gateway,
}

/// Routes inline to the translator for each call and bounds events awaiting a name.
#[derive(Debug)]
pub(in crate::executor) struct TranslationDispatcher {
    context: TranslationContext,
    active: HashMap<u32, ActiveCall>,
    pending_unnamed: HashMap<u32, PendingFunctionCall>,
    pending_bytes: usize,
    first_gateway_output_index: Option<u32>,
    tool_search: ToolSearchStreamState,
    terminal: StreamTerminalState,
}

impl TranslationDispatcher {
    pub(in crate::executor) fn new(context: TranslationContext) -> Self {
        Self {
            context,
            active: HashMap::new(),
            pending_unnamed: HashMap::new(),
            pending_bytes: 0,
            first_gateway_output_index: None,
            tool_search: ToolSearchStreamState::default(),
            terminal: StreamTerminalState::Open,
        }
    }

    pub(in crate::executor) fn translate(
        &mut self,
        frame: EventFrame,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        ToolSearchStreamState::validate_frame(&self.context, &frame)?;
        self.tool_search.track_native_tool_search(&frame)?;
        self.terminal = match frame.event_type {
            SSEEventType::ResponseCompleted => StreamTerminalState::Completed,
            SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete => StreamTerminalState::Aborted,
            _ => self.terminal,
        };
        let mut translated = match &frame.payload {
            EventPayload::OutputItemAdded {
                item_id,
                item_type: SSEItemType::FunctionCall,
                output_index,
                name: Some(name),
                ..
            } => self.start_call(
                item_id,
                name,
                resolved_output_index(*output_index)?,
                Some(frame.clone()),
                call,
            ),
            EventPayload::OutputItemAdded {
                item_id,
                item_type: SSEItemType::FunctionCall,
                output_index,
                name: None,
                ..
            } => {
                let item_id = item_id.clone();
                self.buffer_unnamed(&item_id, resolved_output_index(*output_index)?, frame, call)
            }
            EventPayload::FunctionCallArgsDelta {
                item_id, output_index, ..
            } => self.translate_delta(item_id, resolved_output_index(*output_index)?, frame.clone(), call),
            EventPayload::FunctionCallArgsDone {
                item_id,
                name,
                output_index,
                ..
            } => self.finish_arguments(
                item_id,
                name,
                resolved_output_index(*output_index)?,
                frame.clone(),
                call,
            ),
            EventPayload::OutputItemDone {
                item_id,
                item_type: SSEItemType::FunctionCall,
                output_index,
                item,
            } => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                self.finish_call(
                    item_id,
                    name,
                    resolved_output_index(*output_index)?,
                    frame.clone(),
                    call,
                )
            }
            _ => Ok(Translation {
                frames: vec![frame],
                defer_from_output_index: None,
            }),
        }?;
        for frame in &mut translated.frames {
            self.context.restore_stream_event_wire(&mut frame.wire)?;
        }
        translated.defer_from_output_index = self.defer_from_output_index();
        Ok(translated)
    }

    pub(in crate::executor) fn validate_json_body(&self, body: &str) -> ExecutorResult<()> {
        self.context.validate_json_body(body)
    }

    pub(in crate::executor) fn finish(self) -> ExecutorResult<TranslationOutcome> {
        let unfinished_tool_search_item_ids = self.unfinished_tool_search_item_ids();
        if self.terminal != StreamTerminalState::Aborted
            && (!unfinished_tool_search_item_ids.is_empty() || self.tool_search.has_unfinished_native())
        {
            return Err(tool_search::invalid_upstream_search_call().into());
        }
        Ok(TranslationOutcome {
            context: self.context,
            unfinished_tool_search_item_ids,
        })
    }

    fn start_call(
        &mut self,
        item_id: &str,
        name: &str,
        output_index: u32,
        original: Option<EventFrame>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        let tool_type = self.context.tool_type(name);
        // Exhaustive routing is the extension point for supported tool types.
        let mut translator: Box<dyn ToolTranslator> = match tool_type {
            ToolType::Function => Box::new(FunctionHandler::new_translator()),
            ToolType::CodexNamespace => Box::new(CodexNamespaceHandler::new_translator()),
            ToolType::Custom => Box::new(CustomHandler::new_translator()),
            ToolType::Shell if !self.context.is_gateway_owned(name) => {
                Box::new(crate::tool::ShellHandler::new_translator())
            }
            ToolType::ToolSearch => Box::new(ToolSearchHandler::new_translator()),
            ToolType::Shell
            | ToolType::Mcp
            | ToolType::WebSearch
            | ToolType::FileSearch
            | ToolType::CodeInterpreter => {
                self.active.insert(output_index, ActiveCall::Gateway);
                if self.first_gateway_output_index.is_none_or(|first| output_index < first) {
                    self.first_gateway_output_index = Some(output_index);
                }
                return Ok(Translation::default());
            }
        };
        let frames = translator.translate(
            ToolEvent::Added {
                item_id,
                name,
                output_index,
                original,
            },
            call,
        )?;
        if tool_type == ToolType::ToolSearch {
            let call = call.ok_or_else(|| ExecutorError::Tool(tool_search::invalid_upstream_search_call()))?;
            self.tool_search.start_synthetic(output_index, &call.item.call_id)?;
        }
        self.active.insert(output_index, ActiveCall::Client(translator));
        Ok(Translation {
            frames,
            defer_from_output_index: None,
        })
    }

    fn translate_delta(
        &mut self,
        item_id: &str,
        output_index: u32,
        original: EventFrame,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        let frames = match self.active.get_mut(&output_index) {
            Some(ActiveCall::Client(translator)) => translator.translate(ToolEvent::Delta(original), call)?,
            Some(ActiveCall::Gateway) => Vec::new(),
            None => return self.buffer_unnamed(item_id, output_index, original, call),
        };
        Ok(Translation {
            frames,
            defer_from_output_index: None,
        })
    }

    fn finish_arguments(
        &mut self,
        item_id: &str,
        name: &str,
        output_index: u32,
        original: EventFrame,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        let mut translated = self.resolve_pending(item_id, name, output_index, call)?;
        match self.active.get_mut(&output_index) {
            Some(ActiveCall::Client(translator)) => translated
                .frames
                .extend(translator.translate(ToolEvent::ArgumentsDone(original), call)?),
            Some(ActiveCall::Gateway) => {}
            None => translated.frames.push(original),
        }
        Ok(translated)
    }

    fn finish_call(
        &mut self,
        item_id: &str,
        name: &str,
        output_index: u32,
        original: EventFrame,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        let mut translated = self.resolve_pending(item_id, name, output_index, call)?;
        match self.active.remove(&output_index) {
            Some(ActiveCall::Client(mut translator)) => {
                translated
                    .frames
                    .extend(translator.translate(ToolEvent::Done(original), call)?);
                if translator.unfinished_tool_search_item_id().is_some() {
                    self.active.insert(output_index, ActiveCall::Client(translator));
                }
            }
            Some(ActiveCall::Gateway) => {}
            None => translated.frames.push(original),
        }
        Ok(translated)
    }

    fn resolve_pending(
        &mut self,
        item_id: &str,
        name: &str,
        output_index: u32,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        if self.active.contains_key(&output_index) {
            return Ok(Translation::default());
        }

        let pending = self.take_pending(output_index);
        let original_added = pending.iter().find(|frame| {
            matches!(
                frame.payload,
                EventPayload::OutputItemAdded {
                    item_type: SSEItemType::FunctionCall,
                    ..
                }
            )
        });
        let mut translated = self.start_call(item_id, name, output_index, original_added.cloned(), call)?;

        for frame in pending {
            if let EventPayload::FunctionCallArgsDelta { output_index, .. } = &frame.payload {
                let delta =
                    self.translate_delta(item_id, resolved_output_index(*output_index)?, frame.clone(), call)?;
                translated.frames.extend(delta.frames);
            }
        }
        Ok(translated)
    }

    fn unfinished_tool_search_item_ids(&self) -> std::collections::HashSet<String> {
        let active = self.active.values().filter_map(|active| match active {
            ActiveCall::Client(translator) => translator.unfinished_tool_search_item_id().map(str::to_owned),
            ActiveCall::Gateway => None,
        });
        let pending = self
            .context
            .tool_search_is_active()
            .then(|| {
                self.pending_unnamed
                    .values()
                    .filter_map(|pending| pending.internal_item_id.clone())
            })
            .into_iter()
            .flatten();
        active.chain(pending).collect()
    }

    pub(in crate::executor) fn defer_from_output_index(&self) -> Option<u32> {
        self.first_gateway_output_index
            .into_iter()
            .chain(self.pending_unnamed.values().map(|pending| pending.output_index))
            .min()
    }

    fn buffer_unnamed(
        &mut self,
        item_id: &str,
        output_index: u32,
        frame: EventFrame,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Translation> {
        let bytes = serialize_to_string(&frame.wire)
            .map_err(ExecutorError::JsonError)?
            .len();
        if self.pending_bytes.saturating_add(bytes) > MAX_PENDING_FUNCTION_BYTES {
            return Err(ExecutorError::StreamError(format!(
                "unnamed function-call SSE exceeded {MAX_PENDING_FUNCTION_BYTES} buffered bytes"
            )));
        }
        let pending = self
            .pending_unnamed
            .entry(output_index)
            .or_insert_with(|| PendingFunctionCall {
                output_index,
                ..PendingFunctionCall::default()
            });
        if let Some(internal_item_id) = call
            .map(|call| call.item.id.as_str())
            .filter(|item_id| !item_id.is_empty())
        {
            pending.internal_item_id = Some(internal_item_id.to_owned());
        } else if pending.internal_item_id.is_none() && !item_id.is_empty() {
            pending.internal_item_id = Some(item_id.to_owned());
        }
        pending.frames.push(frame);
        pending.bytes = pending.bytes.saturating_add(bytes);
        self.pending_bytes = self.pending_bytes.saturating_add(bytes);
        Ok(Translation::default())
    }

    fn take_pending(&mut self, output_index: u32) -> Vec<EventFrame> {
        let Some(pending) = self.pending_unnamed.remove(&output_index) else {
            return Vec::new();
        };
        self.pending_bytes = self.pending_bytes.saturating_sub(pending.bytes);
        pending.frames
    }
}
