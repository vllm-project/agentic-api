//! One inference round: the upstream call, its stage timing, and the token
//! usage the upstream reported for it.

use tracing::Instrument as _;

use super::EngineOrchestration;
use crate::events::EventFrame;
use crate::executor::error::ExecutorResult;
use crate::executor::telemetry::metrics::Stage;
use crate::executor::telemetry::stages;
use crate::executor::upstream::{fetch_blocking_payload, fetch_stream_payload};
use crate::types::request_response::ResponsePayload;

impl EngineOrchestration<'_> {
    pub(super) async fn fetch_round(
        &mut self,
        auth: Option<&str>,
        stream_upstream: bool,
        round: usize,
        output_offset: usize,
    ) -> ExecutorResult<(ResponsePayload, Vec<EventFrame>)> {
        let round_span = stages::inference_round(round);
        let exec_ctx = self.exec_ctx;
        let (payload, deferred_events) = if stream_upstream {
            let timer = exec_ctx.metrics.stage(Stage::Inference);
            let fetched = fetch_stream_payload(
                self.agent,
                exec_ctx,
                auth,
                &self.registry,
                output_offset,
                &self.response_budget,
            )
            .instrument(round_span)
            .await;
            timer.finish_result(&fetched);
            let stream_payload = fetched?;
            if round == 0 {
                self.registry.clear_mcp_list_tool_items();
            }
            (stream_payload.payload, stream_payload.deferred_events)
        } else {
            let timer = exec_ctx.metrics.stage(Stage::Inference);
            let fetched =
                fetch_blocking_payload(self.agent, exec_ctx, auth, &self.registry, Some(&self.response_budget))
                    .instrument(round_span)
                    .await;
            timer.finish_result(&fetched);
            (fetched?, Vec::new())
        };
        // Only what this round's upstream response reported; an absent
        // `usage` records nothing.
        exec_ctx.metrics.record_response_usage(payload.usage.as_ref());
        Ok((payload, deferred_events))
    }
}
