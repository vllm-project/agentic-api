//! Request-owned pipeline with a synchronous ingestion core and awaited stream delivery.
mod delivery;
mod ingest;

pub(super) use delivery::emit_deferred_stream_events;
pub(super) use ingest::RoundIngestion;

use crate::events::{ClassifiedSseLine, EventFrame, SseLine};
use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, StreamEvent};
use crate::executor::request::RequestContext;
use crate::executor::translate::{Translation, TranslationContext};
use crate::tool::{ToolRegistry, ToolSearchMetadata, ToolSearchState};
use crate::types::request_response::ResponsePayload;
use delivery::StreamDelivery;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::Sender;

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
}

impl AgentPipeline {
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

    fn begin_round(&mut self, validation: Validation, context: TranslationContext) -> ExecutorResult<()> {
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
    ) -> ExecutorResult<StreamPayload> {
        self.begin_round(validation, context)?;
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
    ) -> ExecutorResult<ResponsePayload> {
        self.begin_round(validation, context)?;
        self.round
            .as_mut()
            .expect("JSON runner just started its round")
            .load_json_body(body)?;
        self.finish()
    }
}

#[cfg(test)]
mod driver_tests;
