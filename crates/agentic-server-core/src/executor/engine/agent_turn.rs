//! Execution of one agent's inference rounds and built-in tools.
//!
//! The response-level engine owns this turn and supplies the shared byte budget.
//! Each turn retains its registry and round progress and exclusively borrows its
//! pipeline. The pipeline owns ingestion, not tool-loop or agent-tree decisions.
//! These steps neither spawn agents nor finalize/persist a public response.

mod hosted_search;
mod round_policy;

use super::accumulate_usage;
use round_policy::{AsyncContinuations, ClientCalls, classify_round};
use std::{collections::HashMap, num::NonZeroUsize};

use tracing::Instrument as _;

use crate::executor::compaction::maybe_compact_context;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::{
    BuiltInToolCallBudget, GatewayCallResult, GatewayScheduler, emit_gateway_start_events,
    execute_and_emit_output_calls, public_output_items, relay_round_events,
};
use crate::executor::pipeline::AgentPipeline;
use crate::executor::request::ExecutionContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::telemetry::metrics::Stage;
use crate::executor::upstream::{fetch_blocking_payload, fetch_stream_payload};
use crate::executor::{inference::UpstreamHeaders, relay::Release};
use crate::tool::{ToolRegistry, record_discovered_mcp_tools, responses_declarations};
use crate::types::io::input::InputFunctionToolCall;
use crate::types::io::{InOutItem, InputItem, OutputItem, ResponsesInput};
use crate::types::request_response::ResponsePayload;

/// Outcome of inspecting one inference round's output, deciding whether the
/// gateway tool loop should run another round, stop, or surface a partial result.
#[derive(Debug)]
pub(super) enum RoundDecision {
    /// Gateway-owned calls were resolved this round; loop again with their
    /// outputs appended to the conversation.
    Continue,
    /// No gateway work remains — the turn is final and the loop terminates.
    Done,
    /// One or more calls are client-owned (`function`, `custom`, or Codex
    /// `namespace` tools); hand the turn back to the caller to execute.
    RequiresClientAction,
    /// The round cap was hit before the model stopped requesting tools. The
    /// response is returned with `status: "incomplete"` rather than as an error.
    Incomplete(String),
    /// The provider returned a failed or incomplete response. Preserve its
    /// status and details, and do not start another inference round.
    UpstreamTerminal,
}

/// One inference round and its tool work, before public response finalization.
pub(super) struct RoundResult {
    pub(super) payload: ResponsePayload,
    pub(super) decision: RoundDecision,
}

/// Builds the agent's registry. With tool search active, the registry takes the request's prepared catalog.
pub(super) async fn build_tool_registry(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    response_budget: &ExecutorResponseBudget,
    history: Option<&ResponsesInput>,
) -> ExecutorResult<ToolRegistry> {
    let mut executors = exec_ctx.gateway_executors.request_scoped();
    executors.include_file_search_results = agent
        .request
        .original_request
        .include
        .as_ref()
        .is_some_and(|include| include.iter().any(|field| field == "file_search_call.results"));
    let history = history.unwrap_or(&agent.request.enriched_request.input);
    let mut registry: ToolRegistry = match agent.request.enriched_request.tools.as_mut() {
        Some(tools) => {
            // The Responses adapter boundary: the registry is built from the
            // converted declarations, and what MCP discovery recorded on them
            // is written back into the request's wire tools so the discovered
            // tools go upstream and are stored with the request.
            let mut declarations = responses_declarations(tools);
            let policy = exec_ctx.gateway_scheduler_policy.clone();
            let registry = ToolRegistry::build_with_handlers_guarded(
                &mut declarations,
                &mut executors,
                Some(history),
                |bytes| response_budget.consume(bytes),
                move || policy.acquire_materialization_permit(),
            )
            .await?;
            record_discovered_mcp_tools(declarations, tools);
            registry
        }
        None => ToolRegistry::default(),
    };
    registry.cache_listed_mcp_tools(history);
    hosted_search::attach_tool_search(agent, &mut registry)?;
    Ok(registry)
}

/// One agent's tool registry and execution state, retained across its rounds.
///
/// The caller supplies the retained-byte budget so descendants can share the
/// same ceiling. Gateway tool permits already live in `ExecutionContext` and
/// therefore remain shared across independently constructed turns as well.
pub(super) struct AgentTurn<'a> {
    pub(super) pipeline: &'a mut AgentPipeline,
    pub(super) registry: ToolRegistry,
    exec_ctx: &'a ExecutionContext,
    max_rounds: NonZeroUsize,
    next_round: usize,
    tool_call_budget: BuiltInToolCallBudget,
    async_continuations: AsyncContinuations,
}

/// A resumable execution snapshot. Canonical input remains with the coordinator
/// while an owned copy of this state is used by cancellable round work.
#[derive(Clone)]
pub(super) struct AgentExecutionState {
    registry: ToolRegistry,
    max_rounds: NonZeroUsize,
    next_round: usize,
    tool_call_budget: BuiltInToolCallBudget,
    async_continuations: AsyncContinuations,
}

impl AgentExecutionState {
    /// Rebuild the registry after live discovery without restarting the round budget.
    pub(super) async fn refresh_tools(
        &mut self,
        agent: &mut AgentPipeline,
        exec: &ExecutionContext,
        budget: &ExecutorResponseBudget,
    ) -> ExecutorResult<()> {
        self.registry = build_tool_registry(agent, exec, budget, None).await?;
        Ok(())
    }

    pub(super) fn take_discovery_output(&mut self, history: &ResponsesInput) -> Vec<OutputItem> {
        // Discovery belongs to the response, including agents whose forked
        // context omits the root's already-published discovery item.
        self.registry.cache_listed_mcp_tools(history);
        let output = self
            .registry
            .mcp_list_tool_items()
            .cloned()
            .map(OutputItem::McpListTools)
            .collect();
        self.registry.clear_mcp_list_tool_items();
        output
    }
    pub(super) fn requires_client_action(&self, item: &OutputItem) -> bool {
        item.requires_client_action(&self.registry)
    }
    pub(super) fn restart(&mut self) {
        self.next_round = 0;
        self.async_continuations = AsyncContinuations::default();
    }
}

impl<'a> AgentTurn<'a> {
    pub(super) fn resume(
        pipeline: &'a mut AgentPipeline,
        exec_ctx: &'a ExecutionContext,
        state: AgentExecutionState,
    ) -> Self {
        Self {
            pipeline,
            exec_ctx,
            registry: state.registry,
            max_rounds: state.max_rounds,
            next_round: state.next_round,
            tool_call_budget: state.tool_call_budget,
            async_continuations: state.async_continuations,
        }
    }

    pub(super) fn execution_state(&self) -> AgentExecutionState {
        AgentExecutionState {
            registry: self.registry.clone(),
            max_rounds: self.max_rounds,
            next_round: self.next_round,
            tool_call_budget: self.tool_call_budget.clone(),
            async_continuations: self.async_continuations,
        }
    }
    pub(super) async fn new(
        agent: &'a mut AgentPipeline,
        exec_ctx: &'a ExecutionContext,
        response_budget: &ExecutorResponseBudget,
        max_rounds: NonZeroUsize,
        history: Option<&ResponsesInput>,
    ) -> ExecutorResult<Self> {
        let registry = build_tool_registry(agent, exec_ctx, response_budget, history).await?;
        hosted_search::reject_failed_mcp_listing(&registry)?;
        let tool_call_budget = BuiltInToolCallBudget::new(agent.request.original_request.max_tool_calls_limit()?);
        Ok(Self {
            pipeline: agent,
            registry,
            exec_ctx,
            max_rounds,
            next_round: 0,
            tool_call_budget,
            async_continuations: AsyncContinuations::default(),
        })
    }

    /// Advance through one inference round and its gateway-executed tools.
    /// The turn owns round numbering; the engine supplies the public output
    /// offset and shared budget. Registry and pipeline state survive each step.
    pub(super) async fn run_round(
        &mut self,
        output_offset: usize,
        auth: Option<&UpstreamHeaders>,
        stream_upstream: bool,
        response_budget: &ExecutorResponseBudget,
    ) -> ExecutorResult<RoundResult> {
        let round = self.next_round;
        if round >= self.max_rounds.get() {
            return Err(ExecutorError::StreamError(
                "agent turn exceeded its inference-round limit".into(),
            ));
        }
        self.next_round += 1;
        let multi_agent = self
            .pipeline
            .request
            .enriched_request
            .multi_agent
            .as_ref()
            .is_some_and(|config| config.enabled);
        // The tree coordinator commits multi-agent summaries by generation.
        let compaction_usage = if multi_agent {
            None
        } else {
            maybe_compact_context(&mut self.pipeline.request, self.exec_ctx, auth).await?
        };
        if compaction_usage.is_some()
            && let Some(continuation) = &mut self.pipeline.request.continuation
        {
            continuation.mark_outputs_compacted(output_offset);
        }
        let round_span = crate::executor::telemetry::stages::inference_round(round);
        let timer = self.exec_ctx.metrics.stage(Stage::Inference);
        let fetched = if stream_upstream {
            fetch_stream_payload(
                self.pipeline,
                self.exec_ctx,
                auth,
                &self.registry,
                output_offset,
                response_budget,
            )
            .instrument(round_span)
            .await
        } else {
            fetch_blocking_payload(
                self.pipeline,
                self.exec_ctx,
                auth,
                &self.registry,
                Some(response_budget),
            )
            .instrument(round_span)
            .await
        };
        timer.finish_result(&fetched);
        let mut payload = fetched?;
        if stream_upstream && round == 0 {
            self.registry.clear_mcp_list_tool_items();
        }
        // Only what this round's upstream response reported; an absent
        // `usage` records nothing.
        self.exec_ctx.metrics.record_response_usage(payload.usage.as_ref());
        let mut round_usage = compaction_usage;
        accumulate_usage(&mut round_usage, payload.usage.take());
        payload.usage = round_usage;
        if matches!(payload.status.as_str(), "error" | "failed") {
            // A failed round skips tool execution, but its deferred public events
            // (including upstream diagnostics) still precede the terminal event.
            let (ctx, relay) = self.pipeline.parts_mut();
            relay.release_deferred(Release::All, ctx).await?;
            return Ok(RoundResult {
                payload,
                decision: RoundDecision::UpstreamTerminal,
            });
        }

        let current_output = std::mem::take(&mut payload.output);
        self.registry.ensure_loaded_calls(&current_output)?;
        let client_calls = ClientCalls::of(&current_output, &self.registry);
        let gateway_results = self
            .execute_round_output(&current_output, output_offset, response_budget)
            .await?;
        payload.output = public_output_items(&current_output, &self.registry, &gateway_results)?;
        if self.tool_call_budget.has_refused() {
            self.pipeline.withhold_builtin_tools();
        }

        let decision = self.round_decision(&mut payload, client_calls, &gateway_results, round);
        if matches!(decision, RoundDecision::Continue) || multi_agent {
            self.append_round_input(&current_output, &payload.output, multi_agent);
        }
        let hosted_loads = hosted_search::hosted_search_loads(&gateway_results);
        self.record_gateway_results(&current_output, gateway_results);
        // Loads persist with the response whatever this round decided.
        self.load_hosted_tools(&hosted_loads).await?;
        Ok(RoundResult { payload, decision })
    }

    fn round_decision(
        &mut self,
        payload: &mut ResponsePayload,
        client_calls: ClientCalls,
        gateway_results: &[GatewayCallResult],
        round: usize,
    ) -> RoundDecision {
        // Completed gateway calls still need their outputs recorded when an
        // incomplete response or client call prevents another inference round.
        let reason = payload
            .incomplete_details
            .as_ref()
            .and_then(|details| details.reason.as_deref());
        let locally_steered = self.pipeline.steering_requested() && reason == Some("steered");
        if locally_steered && client_calls.synchronous {
            // A completed client call still requires its actual output. Keep
            // the ordinary completed parent available for explicit continuation.
            "completed".clone_into(&mut payload.status);
            payload.incomplete_details = None;
            RoundDecision::RequiresClientAction
        } else if payload.status == "incomplete" {
            RoundDecision::UpstreamTerminal
        } else if self.pipeline.steering_requested()
            && !client_calls.synchronous
            && (!gateway_results.is_empty() || client_calls.asynchronous)
        {
            // The entire admitted gateway batch has settled and its events have
            // been emitted. Do not start another inference round after steering.
            RoundDecision::Incomplete("steered".to_owned())
        } else {
            classify_round(
                client_calls,
                gateway_results,
                round,
                self.max_rounds.get(),
                &mut self.async_continuations,
            )
        }
    }

    fn append_round_input(&mut self, output: &[OutputItem], public: &[OutputItem], multi_agent: bool) {
        self.pipeline.request.relax_tool_choice();
        if multi_agent {
            // Preserve client-call kinds for checkpoint validation and output routing.
            // Reuse the public projection; built-in calls retain their canonical history.
            let client_calls: HashMap<_, _> = public
                .iter()
                .filter_map(|item| match item {
                    OutputItem::ShellCall(call) => Some((call.call_id.as_str(), item)),
                    OutputItem::CustomToolCall(call) => Some((call.call_id.as_str(), item)),
                    OutputItem::ToolSearchCall(call) => call.call_id.as_deref().map(|call_id| (call_id, item)),
                    _ => None,
                })
                .collect();
            for item in output.iter().filter(|item| self.registry.records_output_item(item)) {
                let item = match item {
                    OutputItem::FunctionCall(call) => client_calls.get(call.call_id.as_str()).copied().unwrap_or(item),
                    item => item,
                };
                self.pipeline
                    .request
                    .enriched_request
                    .input
                    .extend(item.to_input_item());
            }
        } else {
            let recorded = output
                .iter()
                .filter(|item| self.registry.records_output_item(item))
                .filter_map(OutputItem::to_input_item);
            self.pipeline.request.enriched_request.input.extend(recorded);
        }
    }

    /// Give the model each built-in tool's output. File-search calls and their outputs also stay in item history so
    /// a later turn can use the same retrieved context, even when the public item omits `results`.
    fn record_gateway_results(&mut self, output: &[OutputItem], results: Vec<GatewayCallResult>) {
        for result in results {
            if matches!(&result.public_output, Some(OutputItem::FileSearchCall(_)))
                && let Some(OutputItem::FunctionCall(call)) = output.get(result.item_index)
            {
                self.pipeline
                    .request
                    .new_input_items
                    .push(InOutItem::Input(InputItem::FunctionCall(InputFunctionToolCall::from(
                        call.clone(),
                    ))));
                self.pipeline
                    .request
                    .new_input_items
                    .extend(result.input.iter().cloned().map(InOutItem::Input));
            }
            self.pipeline.request.enriched_request.input.extend(result.input);
        }
    }

    async fn execute_round_output(
        &mut self,
        output_items: &[OutputItem],
        output_offset: usize,
        response_budget: &ExecutorResponseBudget,
    ) -> ExecutorResult<Vec<GatewayCallResult>> {
        let (ctx, relay) = self.pipeline.parts_mut();
        if !relay.has_deferred() {
            return execute_and_emit_output_calls(
                output_items,
                &self.registry,
                output_offset,
                self.exec_ctx,
                response_budget,
                &mut self.tool_call_budget,
                relay,
            )
            .await;
        }

        let scheduler = GatewayScheduler::plan_with_budget(
            output_items,
            &self.registry,
            output_offset,
            self.exec_ctx.gateway_scheduler_policy.clone(),
            &mut self.tool_call_budget,
        )
        .with_metrics(&self.exec_ctx.metrics);
        let initial_event_run_len = scheduler.initial_event_run_len(output_items, &self.registry);
        emit_gateway_start_events(scheduler.event_plans().take(initial_event_run_len), relay).await?;

        // Calls run concurrently; the wire still follows output order.
        let gateway_results = relay_round_events(
            &scheduler,
            response_budget,
            output_items.len(),
            relay,
            ctx,
            initial_event_run_len,
        )
        .await?;
        Ok(gateway_results)
    }
}
