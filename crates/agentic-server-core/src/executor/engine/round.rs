//! Orchestration of one inference round and its server-owned provenance.

use super::EngineOrchestration;
use crate::events::EventFrame;
use crate::executor::error::ExecutorResult;
use crate::executor::replay::record_round_provenance;
use crate::executor::upstream::{fetch_blocking_payload, fetch_stream_payload};
use crate::types::ResponsePayload;
use tracing::Instrument as _;

impl EngineOrchestration<'_> {
    pub(super) async fn fetch_round(
        &mut self,
        auth: Option<&str>,
        stream_upstream: bool,
        round: usize,
        output_offset: usize,
    ) -> ExecutorResult<(ResponsePayload, Vec<EventFrame>)> {
        let round_span = crate::executor::telemetry::stages::inference_round(round);
        let (mut payload, upstream_model, deferred_events) = if stream_upstream {
            let stream = fetch_stream_payload(
                self.agent,
                self.exec_ctx,
                auth,
                &self.registry,
                output_offset,
                &self.response_budget,
            )
            .instrument(round_span)
            .await?;
            if round == 0 {
                self.registry.clear_mcp_list_tool_items();
            }
            (stream.payload, stream.upstream_model, stream.deferred_events)
        } else {
            let response = fetch_blocking_payload(
                self.agent,
                self.exec_ctx,
                auth,
                &self.registry,
                Some(&self.response_budget),
            )
            .instrument(round_span)
            .await?;
            (response.payload, response.upstream_model, Vec::new())
        };
        record_round_provenance(
            &mut payload,
            upstream_model.as_ref(),
            self.exec_ctx,
            &self.agent.request,
            auth,
        )?;
        Ok((payload, deferred_events))
    }
}
