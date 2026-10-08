//! Ordered client delivery for one public response.
//!
//! [`StreamRelay`] owns everything between a translated frame and the client: the
//! client sink, cross-round presentation state, the current round's output offset,
//! the deferred-frame buffer, and the limits on them. The method a caller uses
//! states a frame's origin. [`StreamRelay::emit_local`] takes gateway-synthesized
//! events, which already carry public IDs and absolute output indexes.
//! [`StreamRelay::accept`] and [`StreamRelay::emit_upstream`] take translated
//! provider events, which receive the public response IDs and are rebased onto the
//! round. Every frame leaves through the private `send`, and every deferred frame
//! through [`StreamRelay::release_deferred`]. No response assembly or tool-call
//! translation lives here.
mod agent_sink;
mod agents;
mod deferred;
mod projection;
#[cfg(test)]
mod tests;

pub(super) use agent_sink::{AgentFrame, AgentFrameSink};
pub(super) use deferred::Release;
pub(super) use projection::AgentRoundId;

use crate::config::DEFAULT_MAX_STREAM_EVENT_BYTES;
use crate::events::{EventFrame, SSEEventType, WireEvent};
use crate::executor::error::ExecutorResult;
use crate::executor::gateway::{
    emit_gateway_completed_events, emit_gateway_start_events, mcp_list_tools_event_plans, public_output_items,
};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent, emit_sse_frame_limited};
use crate::executor::request::RequestContext;
use crate::executor::response_events::ResponseEventSink;
use crate::executor::translate::Translation;
use crate::tool::ToolRegistry;
use crate::tool::mcp::handler::list_tools_output_item;
use deferred::DeferredFrames;
use projection::SourceProjection;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

/// Bounds on what one relay holds or sends. Exceeding a limit is an error that
/// names it; nothing is dropped to stay under one. The client queue depth belongs
/// to the owner that creates the bounded channel and holds its receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RelayLimits {
    /// Upstream frames withheld behind a hidden gateway call.
    pub(super) deferred_frames: usize,
    /// Serialized wire bytes of those frames, and of any single one of them.
    pub(super) deferred_bytes: usize,
    /// One serialized client event, including its SSE framing.
    pub(super) event_bytes: usize,
}

impl RelayLimits {
    pub(super) const fn with_event_bytes(event_bytes: usize) -> Self {
        Self {
            deferred_frames: 1024,
            deferred_bytes: 256 * 1024,
            event_bytes,
        }
    }
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self::with_event_bytes(DEFAULT_MAX_STREAM_EVENT_BYTES)
    }
}

/// Where presented frames go, fixed by the relay's constructor.
enum RelaySink {
    /// Collect-only and JSON execution present nothing and defer nothing.
    Detached,
    /// SSE: this relay stamps each frame and awaits the bounded client queue.
    Client(Sender<StreamEvent>),
    /// A retained WebSocket response: the sink shares its sequence with
    /// out-of-band acknowledgements, so it stamps under its own lock.
    Response(ResponseEventSink),
    /// A multi-agent round task: the response owner's relay projects and stamps.
    Agent(AgentFrameSink),
}

pub(super) struct StreamRelay {
    sink: RelaySink,
    /// Cross-round sequence numbers and response-start deduplication for
    /// [`RelaySink::Client`]. Cloning it copies a few scalars; `send` relies on
    /// that to commit only after enqueueing, and `engine/streaming.rs` to render
    /// the terminal event speculatively and discard the clone if persistence fails.
    presentation: GatewayStreamAccumulator,
    projection: SourceProjection,
    /// The current inference round's offset into the public output.
    output_offset: usize,
    deferred: DeferredFrames,
    limits: RelayLimits,
}

impl StreamRelay {
    /// For collect-only and JSON execution, which present nothing.
    pub(super) fn detached() -> Self {
        Self::with_sink(RelaySink::Detached, RelayLimits::default())
    }

    /// SSE: stamp each frame and await the bounded client channel.
    pub(super) fn client(sender: Sender<StreamEvent>, limits: RelayLimits) -> Self {
        Self::with_sink(RelaySink::Client(sender), limits)
    }

    /// A retained WebSocket response: present through its shared sequence owner.
    pub(super) fn response(sink: ResponseEventSink, limits: RelayLimits) -> Self {
        Self::with_sink(RelaySink::Response(sink), limits)
    }

    /// A multi-agent round task: forward to the response owner, which projects and stamps.
    pub(super) fn agent(sink: AgentFrameSink, limits: RelayLimits) -> Self {
        Self::with_sink(RelaySink::Agent(sink), limits)
    }

    fn with_sink(sink: RelaySink, limits: RelayLimits) -> Self {
        Self {
            sink,
            presentation: GatewayStreamAccumulator::with_max_stream_event_bytes(limits.event_bytes),
            projection: SourceProjection::default(),
            output_offset: 0,
            deferred: DeferredFrames::default(),
            limits,
        }
    }

    /// Whether frames reach a client. Callers of a detached relay collect output instead.
    pub(super) const fn is_live(&self) -> bool {
        !matches!(self.sink, RelaySink::Detached)
    }

    /// Sequence number that the next presented frame will receive.
    pub(super) const fn upcoming_sequence_number(&self) -> u64 {
        self.presentation.upcoming_sequence_number()
    }

    /// Present a new inference round at `output_offset`. The previous round's
    /// deferred frames must already have been released.
    pub(super) fn begin_round(&mut self, output_offset: usize) -> ExecutorResult<()> {
        self.deferred.begin_round()?;
        self.output_offset = output_offset;
        Ok(())
    }

    pub(super) fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// Cross-round presentation state for the terminal event, which the stream
    /// yields itself after its producer finishes.
    pub(super) fn into_presentation(self) -> GatewayStreamAccumulator {
        self.presentation
    }

    /// Present a gateway-synthesized frame. It already carries public IDs and an
    /// absolute output index, so neither upstream transform applies.
    pub(super) async fn emit_local(&mut self, frame: &mut EventFrame) -> ExecutorResult<bool> {
        self.send(frame, 0).await
    }

    /// Present a translated provider frame: restore the public response IDs and
    /// rebase its output index onto the current round.
    pub(super) async fn emit_upstream(&mut self, frame: &mut EventFrame, ctx: &RequestContext) -> ExecutorResult<bool> {
        apply_context_response_ids(&mut frame.wire, ctx);
        self.send(frame, self.output_offset).await
    }

    /// One translation from ingestion: present or defer each frame, then release
    /// what a moved defer window no longer hides. Terminal response events are
    /// withheld for the engine.
    pub(super) async fn accept(
        &mut self,
        translation: Translation,
        ctx: &RequestContext,
        registry: &ToolRegistry,
    ) -> ExecutorResult<()> {
        for frame in &translation.frames {
            log_upstream_failure(frame, &ctx.response_id);
        }
        if !self.is_live() {
            return Ok(());
        }
        let moved = self
            .deferred
            .move_window(translation.defer_from_output_index.map(u64::from));
        for mut frame in translation.frames {
            if is_terminal_response_event(frame.event_type) {
                continue;
            }
            if self.deferred.withholds(&frame) {
                self.deferred.push(frame, &self.limits)?;
                continue;
            }
            let event_type = frame.event_type;
            if self.emit_upstream(&mut frame, ctx).await? && event_type == SSEEventType::ResponseInProgress {
                self.emit_mcp_discovery_lifecycle(registry).await?;
            }
        }
        if let Some(release) = moved {
            self.release_deferred(release, ctx).await?;
        }
        Ok(())
    }

    /// Present the deferred frames that `release` admits, in output order with
    /// index-less frames last. A frame and its bytes leave the buffer only after
    /// the sink accepts it, so a failed or cancelled release keeps the remainder.
    pub(super) async fn release_deferred(&mut self, release: Release, ctx: &RequestContext) -> ExecutorResult<()> {
        while let Some(frame) = self.deferred.next(release) {
            // Sending rebases the frame in place; keep the queued copy untouched.
            let mut frame = frame.clone();
            frame.wire.output_index = release.position(frame.wire.output_index);
            self.emit_upstream(&mut frame, ctx).await?;
            self.deferred.pop_front();
        }
        Ok(())
    }

    /// The one place a frame leaves the relay, and the only place an output offset
    /// is applied. Presentation state is committed only after the sink accepts the
    /// frame, so a failed serialization, closed receiver, or cancelled wait
    /// consumes no sequence number or response-start event.
    async fn send(&mut self, frame: &mut EventFrame, offset: usize) -> ExecutorResult<bool> {
        match &self.sink {
            RelaySink::Detached => Ok(false),
            RelaySink::Agent(sink) => {
                let mut frame = frame.clone();
                if let Some(index) = &mut frame.wire.output_index {
                    *index += offset as u64;
                }
                sink.send(&frame, self.limits.event_bytes).await?;
                Ok(true)
            }
            RelaySink::Response(sink) => sink.emit_frame(frame, offset).await,
            RelaySink::Client(sender) => {
                let mut published = self.presentation.clone();
                if !published.process_event(frame, offset) {
                    return Ok(false);
                }
                emit_sse_frame_limited(sender, frame, self.limits.event_bytes).await?;
                self.presentation = published;
                Ok(true)
            }
        }
    }

    async fn emit_mcp_discovery_lifecycle(&mut self, registry: &ToolRegistry) -> ExecutorResult<()> {
        let discovered_output = registry
            .mcp_list_tool_items()
            .map(list_tools_output_item)
            .collect::<Vec<_>>();
        let public_output = public_output_items(&discovered_output, registry, &[])?;
        let event_plans = mcp_list_tools_event_plans(&public_output, 0);

        emit_gateway_start_events(&event_plans, self).await?;
        emit_gateway_completed_events(&public_output, &event_plans, self).await
    }
}

fn log_upstream_failure(frame: &EventFrame, gateway_response_id: &str) {
    if frame.event_type != SSEEventType::ResponseFailed {
        return;
    }

    let response = frame.wire.rest.get("response").unwrap_or(&Value::Null);
    let error = &response["error"];
    let error_code = error.get("code").and_then(Value::as_str).unwrap_or_default();
    let error_message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or_default();
    let incomplete_reason = response["incomplete_details"]
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    tracing::warn!(
        response_id = %gateway_response_id,
        upstream_response_id = response["id"].as_str().unwrap_or_default(),
        error_code,
        error_message,
        incomplete_reason,
        "upstream response failed"
    );
}

fn is_terminal_response_event(event_type: SSEEventType) -> bool {
    matches!(
        event_type,
        SSEEventType::ResponseCompleted | SSEEventType::ResponseFailed | SSEEventType::ResponseIncomplete
    )
}

fn apply_context_response_ids(wire: &mut WireEvent, ctx: &RequestContext) {
    let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) else {
        return;
    };
    response.insert("id".to_owned(), Value::String(ctx.response_id.clone()));
    if let Some(previous_response_id) = &ctx.original_request.previous_response_id {
        response.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.clone()),
        );
    }
    if let Some(conversation_id) = &ctx.conversation_id {
        response.insert("conversation_id".to_owned(), Value::String(conversation_id.clone()));
    }
    let max_tool_calls = ctx
        .max_tool_calls()
        .map_or(Value::Null, |limit| Value::from(limit.get()));
    response.insert("max_tool_calls".to_owned(), max_tool_calls);
}
