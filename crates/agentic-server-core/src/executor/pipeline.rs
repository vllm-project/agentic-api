//! Request-owned pipeline with a synchronous ingestion core and awaited stream delivery.
mod ingest;

pub(super) use ingest::RoundIngestion;

use crate::events::{ClassifiedSseLine, EventFrame, SseLine};
use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent};
use crate::executor::multi_agent::RunControlReceiver;
use crate::executor::relay::{AgentFrameSink, AgentRoundId, RelayLimits, StreamRelay};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::response_events::ResponseEventSink;
use crate::executor::translate::{Translation, TranslationContext};
use crate::tool::{ToolRegistry, ToolSearchMetadata, ToolSearchState};
use crate::types::agent::AgentIdentity;
use crate::types::io::{InputMessage, OutputItem};
use crate::types::request_response::ResponsePayload;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Lives for the response, preserving gateway event numbering across inference rounds.
pub(super) struct AgentPipeline {
    pub(super) control: Option<RunControlReceiver>,
    pub(super) request: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    relay: StreamRelay,
    round: Option<RoundIngestion>,
    cancellation: CancellationToken,
    agent_guidance: Option<InputMessage>,
    /// Set after a `max_tool_calls` refusal; later rounds omit gateway-executed tools.
    builtin_tools_withheld: bool,
}

impl AgentPipeline {
    pub(super) fn set_response_event_sink(&mut self, sink: ResponseEventSink) {
        self.relay.attach_response_sink(sink);
    }
    pub(super) fn set_agent_guidance(&mut self, guidance: InputMessage) {
        self.agent_guidance = Some(guidance);
    }

    pub(super) fn agent_guidance(&self) -> Option<&InputMessage> {
        self.agent_guidance.as_ref()
    }

    pub(super) fn withhold_builtin_tools(&mut self) {
        self.builtin_tools_withheld = true;
    }

    pub(super) const fn builtin_tools_withheld(&self) -> bool {
        self.builtin_tools_withheld
    }

    pub(super) fn has_live_agent_items(&self, agent: &AgentIdentity) -> bool {
        self.relay.has_live_agent_items(agent)
    }
    pub(super) async fn accept_agent_frame(&mut self, source: &AgentRoundId, frame: EventFrame) -> ExecutorResult<()> {
        self.relay.accept_agent_frame(source, frame).await
    }

    pub(super) async fn emit_agent_item(&mut self, item: &OutputItem) -> ExecutorResult<usize> {
        self.relay.emit_agent_item(item).await
    }

    pub(super) fn finish_agent_source(&mut self, source: &AgentRoundId) {
        self.relay.finish_agent_source(source);
    }
    pub(super) fn set_agent_frame_sink(&mut self, sink: AgentFrameSink) {
        self.relay.attach_agent_sink(sink);
    }
    pub(super) fn is_streaming(&self) -> bool {
        self.relay.is_live()
    }
    pub(super) fn relay_mut(&mut self) -> &mut StreamRelay {
        &mut self.relay
    }
    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
    pub(super) fn new(
        request: RequestContext,
        tool_search_state: Option<ToolSearchState>,
        sender: Option<Sender<StreamEvent>>,
    ) -> Self {
        Self {
            request,
            tool_search_state,
            relay: StreamRelay::new(sender, RelayLimits::default()),
            round: None,
            cancellation: CancellationToken::new(),
            agent_guidance: None,
            control: None,
            builtin_tools_withheld: false,
        }
    }

    pub(super) fn with_limits(
        request: RequestContext,
        tool_search_state: Option<ToolSearchState>,
        sender: Option<Sender<StreamEvent>>,
        max_stream_event_bytes: usize,
    ) -> Self {
        Self {
            request,
            tool_search_state,
            relay: StreamRelay::new(sender, RelayLimits::with_event_bytes(max_stream_event_bytes)),
            round: None,
            cancellation: CancellationToken::new(),
            agent_guidance: None,
            control: None,
            builtin_tools_withheld: false,
        }
    }

    pub(super) fn tool_search_state(&self) -> Option<&ToolSearchState> {
        self.tool_search_state.as_ref()
    }

    pub(super) fn ensure_request_prepared(&self) -> ExecutorResult<()> {
        crate::tool::tool_search::ensure_request_prepared(
            &self.request.enriched_request,
            self.tool_search_state.is_some(),
        )?;
        Ok(())
    }

    pub(super) fn take_tool_search_metadata(&mut self) -> Option<ToolSearchMetadata> {
        self.tool_search_state
            .take()
            .filter(ToolSearchState::is_active)
            .map(ToolSearchState::into_public_metadata)
    }

    /// The request beside its relay, for engine steps that present upstream frames.
    pub(super) fn parts_mut(&mut self) -> (&mut RequestContext, &mut StreamRelay) {
        (&mut self.request, &mut self.relay)
    }

    pub(super) fn into_parts(self) -> (RequestContext, GatewayStreamAccumulator) {
        (self.request, self.relay.into_presentation())
    }

    fn begin_round(
        &mut self,
        validation: Validation,
        context: TranslationContext,
        budget: Option<ExecutorResponseBudget>,
    ) -> ExecutorResult<()> {
        if self.round.is_some() {
            return Err(ExecutorError::InvalidRequest(
                "previous pipeline body did not finish".to_owned(),
            ));
        }
        self.round = Some(RoundIngestion::new(
            self.request.response_id.clone(),
            self.request.conversation_id.clone(),
            validation,
            context,
            budget,
        ));
        Ok(())
    }

    fn push(&mut self, line: ClassifiedSseLine) -> ExecutorResult<Translation> {
        self.round
            .as_mut()
            .expect("body runner starts a round before pushing input")
            .push(line)
    }

    fn finish(&mut self) -> ExecutorResult<ResponsePayload> {
        let round = self
            .round
            .take()
            .expect("body runner starts a round before finalization");
        let mut payload = round.finish(
            &self.request.enriched_request.model,
            self.request.original_request.previous_response_id.as_deref(),
            self.request.original_request.instructions.as_deref(),
        )?;
        self.request.inject_ids(&mut payload);
        Ok(payload)
    }

    /// Polls live input inline and awaits delivery before reading another framed line.
    pub(super) async fn run_with_stream_body(
        &mut self,
        body: impl Stream<Item = ExecutorResult<String>>,
        validation: Validation,
        context: TranslationContext,
        registry: &ToolRegistry,
        output_offset: usize,
        budget: Option<ExecutorResponseBudget>,
    ) -> ExecutorResult<ResponsePayload> {
        self.relay.begin_round(output_offset)?;
        self.begin_round(validation, context, budget)?;
        futures::pin_mut!(body);
        while let Some(line) = body.next().await {
            let translation = self.push(SseLine::parse(&line?))?;
            self.relay.accept(translation, &self.request, registry).await?;
        }
        // Frames still deferred stay with the relay until the engine releases them
        // around this round's gateway-executed calls.
        self.finish()
    }

    /// JSON shares finalization but retains its status instead of applying SSE EOF policy.
    pub(super) fn run_with_json_body(
        &mut self,
        body: &str,
        validation: Validation,
        context: TranslationContext,
        budget: Option<ExecutorResponseBudget>,
    ) -> ExecutorResult<ResponsePayload> {
        self.begin_round(validation, context, budget)?;
        self.round
            .as_mut()
            .expect("JSON runner just started its round")
            .load_json_body(body)?;
        self.finish()
    }
}

#[cfg(test)]
mod driver_tests;
