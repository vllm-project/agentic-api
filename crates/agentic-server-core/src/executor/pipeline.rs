//! Request-owned pipeline with a synchronous ingestion core and awaited stream delivery.
mod agent_delivery;
mod delivery;
mod ingest;
mod projection;
pub(super) use projection::AgentRoundId;

pub(super) use agent_delivery::{AgentFrame, AgentFrameSink};
pub(super) use delivery::{emit_deferred_stream_events, emit_gateway_event};
pub(super) use ingest::RoundIngestion;

use crate::events::{ClassifiedSseLine, EventFrame, SseLine};
use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::translate::{Translation, TranslationContext};
use crate::tool::{ToolRegistry, ToolSearchMetadata, ToolSearchState};
use crate::types::agent::AgentIdentity;
use crate::types::io::{InputMessage, OutputItem};
use crate::types::request_response::ResponsePayload;
use delivery::StreamDelivery;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(super) struct StreamPayload {
    pub(super) payload: ResponsePayload,
    pub(super) deferred_events: Vec<EventFrame>,
}

/// Lives for the response, preserving gateway event numbering across inference rounds.
pub(super) struct AgentPipeline {
    pub(super) request: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    delivery: StreamDelivery,
    round: Option<RoundIngestion>,
    cancellation: CancellationToken,
    agent_guidance: Option<InputMessage>,
}

impl AgentPipeline {
    pub(super) fn set_agent_guidance(&mut self, guidance: InputMessage) {
        self.agent_guidance = Some(guidance);
    }

    pub(super) fn agent_guidance(&self) -> Option<&InputMessage> {
        self.agent_guidance.as_ref()
    }

    pub(super) fn has_live_agent_items(&self, agent: &AgentIdentity) -> bool {
        self.delivery.has_live_agent_items(agent)
    }
    pub(super) async fn accept_agent_frame(&mut self, source: &AgentRoundId, frame: EventFrame) -> ExecutorResult<()> {
        self.delivery.accept_agent_frame(source, frame).await
    }

    pub(super) async fn emit_agent_item(&mut self, item: &OutputItem) -> ExecutorResult<usize> {
        self.delivery.emit_agent_item(item).await
    }

    pub(super) fn finish_agent_source(&mut self, source: &AgentRoundId) {
        self.delivery.finish_agent_source(source);
    }
    pub(super) fn set_agent_frame_sink(&mut self, sink: AgentFrameSink) {
        self.delivery.accumulator.agent_sink = Some(sink);
    }
    pub(super) fn stream_sender(&self) -> Option<Sender<StreamEvent>> {
        self.delivery.sender.clone()
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
            delivery: StreamDelivery::new(sender),
            round: None,
            cancellation: CancellationToken::new(),
            agent_guidance: None,
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
            delivery: StreamDelivery::with_max_stream_event_bytes(sender, max_stream_event_bytes),
            round: None,
            cancellation: CancellationToken::new(),
            agent_guidance: None,
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

    pub(super) fn parts_mut(
        &mut self,
    ) -> (
        &mut RequestContext,
        Option<(&mut GatewayStreamAccumulator, &Sender<StreamEvent>)>,
    ) {
        (
            &mut self.request,
            self.delivery
                .sender
                .as_ref()
                .map(|sender| (&mut self.delivery.accumulator, sender)),
        )
    }

    pub(super) fn into_parts(self) -> (RequestContext, GatewayStreamAccumulator) {
        (self.request, self.delivery.accumulator)
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
    ) -> ExecutorResult<StreamPayload> {
        self.begin_round(validation, context, budget)?;
        futures::pin_mut!(body);
        while let Some(line) = body.next().await {
            let translation = self.push(SseLine::parse(&line?))?;
            self.delivery
                .accept(translation, &self.request, registry, output_offset)
                .await?;
        }
        let payload = self.finish()?;
        Ok(StreamPayload {
            payload,
            deferred_events: self.delivery.take_deferred_events(),
        })
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
