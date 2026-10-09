//! Stage timing for gateway-executed calls.

use tokio::sync::Semaphore;

use super::{GatewayCallPlan, GatewayCallResult, GatewayScheduler};
use crate::executor::error::ExecutorResult;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::telemetry::FailureCategory;
use crate::executor::telemetry::metrics::{ExecutorMetrics, Stage};
use crate::types::io::output::GatewayCallStatus;

impl GatewayScheduler {
    /// Time each executed call as an `agentic.stage.duration` sample.
    pub(in crate::executor) fn with_metrics(mut self, metrics: &ExecutorMetrics) -> Self {
        self.metrics = Some(metrics.clone());
        self
    }

    /// Time one executed call as a `tool` stage; a call that reports a failed
    /// status is a failed stage even though the round continues.
    pub(super) async fn timed_run(
        &self,
        plan: GatewayCallPlan,
        execution_slots: &Semaphore,
        response_budget: &ExecutorResponseBudget,
    ) -> ExecutorResult<GatewayCallResult> {
        let timer = self
            .metrics
            .as_ref()
            .map(|metrics| metrics.stage(Stage::Tool(plan.tool_type)));
        let result = self.run_one(plan, execution_slots, response_budget).await;
        if let Some(timer) = timer {
            timer.finish(match &result {
                Ok((_, GatewayCallStatus::Failed)) => Some(FailureCategory::Tool),
                Ok(_) => None,
                Err(error) => Some(FailureCategory::from(error)),
            });
        }
        result.map(|(result, _)| result)
    }
}
