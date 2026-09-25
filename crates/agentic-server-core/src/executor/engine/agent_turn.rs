//! Execution of one agent's inference rounds and built-in tools.
//!
//! The response-level engine owns this turn and supplies the shared byte budget.
//! Each turn retains its registry and round progress and exclusively borrows its
//! pipeline. The pipeline owns ingestion, not tool-loop or agent-tree decisions.
//! These steps neither spawn agents nor finalize/persist a public response.

use super::accumulate_usage;
use std::num::NonZeroUsize;

use tracing::{Instrument as _, debug};

use crate::events::EventFrame;
use crate::executor::compaction::maybe_compact_context;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway::history::append_input_item;
use crate::executor::gateway::{
    GatewayCallResult, GatewayScheduler, append_gateway_calls_to_new_input, append_output_items_to_input,
    append_tool_outputs, emit_gateway_completed_events, emit_gateway_start_events, execute_and_emit_output_calls,
    has_client_owned_calls, public_output_items,
};
use crate::executor::pipeline::{AgentPipeline, emit_deferred_stream_events};
use crate::executor::rehydrate::prepare_reasoning_for_vllm;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::upstream::{fetch_blocking_payload, fetch_stream_payload};
use crate::tool::{ShellHandler, ToolRegistry, mcp};
use crate::types::io::{InputItem, OutputItem, ResponsesInput, ToolChoice};
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

/// Classify one turn's output into a [`RoundDecision`].
///
/// Order matters: client-owned calls take precedence (they must be handed back
/// even when gateway calls are also present in the same turn), then a
/// no-gateway-work turn is `Done`. Otherwise gateway tools ran — the loop would
/// continue, unless this was the last permitted round, in which case the budget
/// is exhausted and the turn is `Incomplete`.
///
/// `round` is zero-based; `max_rounds` is the total budget.
fn classify_round(
    has_client_owned_calls: bool,
    gateway_results: &[GatewayCallResult],
    round: usize,
    max_rounds: usize,
) -> RoundDecision {
    if has_client_owned_calls {
        RoundDecision::RequiresClientAction
    } else if gateway_results.is_empty() {
        RoundDecision::Done
    } else if round + 1 >= max_rounds {
        RoundDecision::Incomplete(format!("gateway tool execution exceeded {max_rounds} rounds"))
    } else {
        RoundDecision::Continue
    }
}

pub(super) async fn build_tool_registry(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    response_budget: &ExecutorResponseBudget,
) -> ExecutorResult<ToolRegistry> {
    let mut executors = exec_ctx.gateway_executors.request_scoped();
    let mut registry: ToolRegistry = match agent.request.enriched_request.tools.as_mut() {
        Some(tools) => {
            let policy = exec_ctx.gateway_scheduler_policy.clone();
            ToolRegistry::build_with_handlers_guarded(
                tools,
                &mut executors,
                |bytes| response_budget.consume(bytes),
                move || policy.acquire_materialization_permit(),
            )
            .await?
        }
        None => ToolRegistry::default(),
    };
    if let Some(state) = agent.tool_search_state().filter(|state| state.is_active()) {
        registry
            .validate_tool_availability(state.withheld_function_names(), state.synthetic_tool_search().is_some())?;
    }
    registry.cache_listed_mcp_tools(&agent.request.enriched_request.input);
    Ok(registry)
}

fn prepare_initial_reasoning_for_vllm(input: &mut ResponsesInput, round: usize, compacted: bool) -> ExecutorResult<()> {
    if round == 0 && !compacted {
        return prepare_reasoning_for_vllm(input);
    }
    Ok(())
}

fn record_round_history(
    ctx: &mut RequestContext,
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    public_output_count: usize,
) {
    // Explicit conversations append public output through their durable handler;
    // the session lease still serializes execution but must not record it twice.
    if let Some(continuation) = ctx
        .continuation
        .as_mut()
        .filter(|_| ctx.original_request.conversation_id.is_none())
    {
        // The canonical sequence includes reasoning and intermediate messages in
        // their original positions, followed by this round's tool call outputs.
        // Discovery records are appended separately from the public response.
        ctx.new_input_items.extend(
            output_items
                .iter()
                .filter(|item| !matches!(item, OutputItem::McpListTools(_)))
                .filter_map(OutputItem::to_input_item),
        );
        continuation.mark_outputs_recorded(public_output_count);
    } else {
        append_gateway_calls_to_new_input(ctx, output_items, registry);
    }
}

/// One agent's tool registry and execution state, retained across its rounds.
///
/// The caller supplies the retained-byte budget so descendants can share the
/// same ceiling. Gateway tool permits already live in `ExecutionContext` and
/// therefore remain shared across independently constructed turns as well.
pub(super) struct AgentTurn<'a> {
    pub(super) pipeline: &'a mut AgentPipeline,
    registry: ToolRegistry,
    exec_ctx: &'a ExecutionContext,
    max_rounds: NonZeroUsize,
    next_round: usize,
}

/// A resumable execution snapshot. Canonical input remains with the coordinator
/// while an owned copy of this state is used by cancellable round work.
#[derive(Clone)]
pub(super) struct AgentExecutionState {
    registry: ToolRegistry,
    max_rounds: NonZeroUsize,
    next_round: usize,
}

impl AgentExecutionState {
    pub(super) fn take_discovery_output(&mut self) -> Vec<OutputItem> {
        let output = self
            .registry
            .mcp_list_tool_items()
            .map(mcp::handler::list_tools_output_item)
            .collect();
        self.registry.clear_mcp_list_tool_items();
        output
    }
    pub(super) fn requires_client_action(&self, item: &OutputItem) -> bool {
        item.requires_client_action(&self.registry)
    }
    pub(super) fn restart(&mut self) {
        self.next_round = 0;
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
        }
    }

    pub(super) fn execution_state(&self) -> AgentExecutionState {
        AgentExecutionState {
            registry: self.registry.clone(),
            max_rounds: self.max_rounds,
            next_round: self.next_round,
        }
    }
    pub(super) async fn new(
        agent: &'a mut AgentPipeline,
        exec_ctx: &'a ExecutionContext,
        response_budget: &ExecutorResponseBudget,
        max_rounds: NonZeroUsize,
    ) -> ExecutorResult<Self> {
        let registry = build_tool_registry(agent, exec_ctx, response_budget).await?;
        Ok(Self {
            pipeline: agent,
            registry,
            exec_ctx,
            max_rounds,
            next_round: 0,
        })
    }

    pub(super) fn discovery_output(&self) -> Vec<OutputItem> {
        self.registry
            .mcp_list_tool_items()
            .map(mcp::handler::list_tools_output_item)
            .collect()
    }

    /// Advance through one inference round and its gateway-executed tools.
    /// The turn owns round numbering; the engine supplies the public output
    /// offset and shared budget. Registry and pipeline state survive each step.
    pub(super) async fn run_round(
        &mut self,
        output_offset: usize,
        auth: Option<&str>,
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
        prepare_initial_reasoning_for_vllm(
            &mut self.pipeline.request.enriched_request.input,
            round,
            compaction_usage.is_some(),
        )?;
        let round_span = crate::executor::telemetry::stages::inference_round(round);
        let (mut payload, deferred_stream_events) = if stream_upstream {
            let stream_payload = fetch_stream_payload(
                self.pipeline,
                self.exec_ctx,
                auth,
                &self.registry,
                output_offset,
                response_budget,
            )
            .instrument(round_span)
            .await?;
            if round == 0 {
                self.registry.clear_mcp_list_tool_items();
            }
            (stream_payload.payload, stream_payload.deferred_events)
        } else {
            (
                fetch_blocking_payload(
                    self.pipeline,
                    self.exec_ctx,
                    auth,
                    &self.registry,
                    Some(response_budget),
                )
                .instrument(round_span)
                .await?,
                Vec::new(),
            )
        };
        let mut round_usage = compaction_usage;
        accumulate_usage(&mut round_usage, payload.usage.take());
        payload.usage = round_usage;
        if matches!(payload.status.as_str(), "error" | "failed") {
            self.emit_failed_round_events(deferred_stream_events, output_offset)
                .await?;
            return Ok(RoundResult {
                payload,
                decision: RoundDecision::UpstreamTerminal,
            });
        }

        let current_output = std::mem::take(&mut payload.output);
        log_custom_tool_calls(&current_output, &self.pipeline.request.response_id);
        let has_client_owned = has_client_owned_calls(&current_output, &self.registry);
        let gateway_results = self
            .execute_round_output(&current_output, output_offset, deferred_stream_events, response_budget)
            .await?;
        payload.output = public_output_items(&current_output, &self.registry, &gateway_results)?;
        record_round_history(
            &mut self.pipeline.request,
            &current_output,
            &self.registry,
            output_offset + payload.output.len(),
        );

        // Completed gateway calls still need their outputs recorded when an
        // incomplete response or client call prevents another inference round.
        let decision = if payload.status == "incomplete" {
            RoundDecision::UpstreamTerminal
        } else {
            classify_round(has_client_owned, &gateway_results, round, self.max_rounds.get())
        };
        if matches!(decision, RoundDecision::Continue) || multi_agent {
            self.append_round_input(&current_output, multi_agent)?;
        }
        self.record_gateway_results(gateway_results);
        Ok(RoundResult { payload, decision })
    }

    fn append_round_input(&mut self, output: &[OutputItem], multi_agent: bool) -> ExecutorResult<()> {
        self.pipeline.request.enriched_request.tool_choice = Some(ToolChoice::Auto);
        if multi_agent {
            let canonical = output
                .iter()
                .map(|item| match item {
                    OutputItem::FunctionCall(call) if self.registry.is_client_shell_name(&call.name) => {
                        ShellHandler::output_item(call)
                            .ok_or_else(|| ExecutorError::ParseError("invalid client shell call".into()))
                    }
                    item => Ok(item.clone()),
                })
                .collect::<ExecutorResult<Vec<_>>>()?;
            for item in &canonical {
                // Shell ownership must survive checkpointing; normalize only for upstream inference.
                let input = match item {
                    OutputItem::ShellCall(call) => Some(InputItem::ShellCall(call.clone())),
                    item => item.to_input_item(),
                };
                if let Some(input) = input {
                    append_input_item(&mut self.pipeline.request.enriched_request.input, input);
                }
            }
        } else {
            append_output_items_to_input(&mut self.pipeline.request.enriched_request.input, output);
        }
        Ok(())
    }

    async fn emit_failed_round_events(
        &mut self,
        deferred_events: Vec<EventFrame>,
        output_offset: usize,
    ) -> ExecutorResult<()> {
        // A failed round skips tool execution, but its deferred public events
        // (including upstream diagnostics) still precede the terminal event.
        let (ctx, stream) = self.pipeline.parts_mut();
        if let Some((accumulator, sender)) = stream {
            emit_deferred_stream_events(deferred_events, ctx, accumulator, sender, output_offset).await?;
        }
        Ok(())
    }

    fn record_gateway_results(&mut self, results: Vec<GatewayCallResult>) {
        append_tool_outputs(
            &mut self.pipeline.request,
            results.into_iter().map(|result| result.input_item).collect(),
        );
    }

    async fn execute_round_output(
        &mut self,
        output_items: &[OutputItem],
        output_offset: usize,
        deferred_events: Vec<EventFrame>,
        response_budget: &ExecutorResponseBudget,
    ) -> ExecutorResult<Vec<GatewayCallResult>> {
        let (ctx, stream) = self.pipeline.parts_mut();
        let (stream_accumulator, stream_sender) = match (deferred_events.is_empty(), stream) {
            (false, Some(stream)) => stream,
            (_, stream) => {
                return execute_and_emit_output_calls(
                    output_items,
                    &self.registry,
                    output_offset,
                    self.exec_ctx.gateway_scheduler_policy.clone(),
                    response_budget,
                    stream,
                )
                .await;
            }
        };
        let mut events_by_output = Vec::with_capacity(output_items.len());
        events_by_output.resize_with(output_items.len(), Vec::new);
        let mut remaining_events = Vec::new();
        for frame in deferred_events {
            let Some(output_index) = frame
                .wire
                .output_index
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < events_by_output.len())
            else {
                remaining_events.push(frame);
                continue;
            };
            events_by_output[output_index].push(frame);
        }

        let mut scheduler = GatewayScheduler::plan(
            output_items,
            &self.registry,
            output_offset,
            self.exec_ctx.gateway_scheduler_policy.clone(),
        );
        let initial_event_run_len = scheduler.initial_event_run_len(output_items, &self.registry);
        emit_gateway_start_events(
            scheduler.event_plans().take(initial_event_run_len),
            stream_accumulator,
            stream_sender,
        )
        .await?;

        let gateway_results = scheduler.execute_with_budget(response_budget).await?;
        for (index, output_events) in events_by_output.iter_mut().enumerate() {
            if let Some(call_index) = scheduler.call_index_for_item(index) {
                let plan = scheduler
                    .event_plan(call_index)
                    .expect("scheduled call index always has an event plan");
                let result = std::slice::from_ref(&gateway_results[call_index]);
                if call_index >= initial_event_run_len {
                    emit_gateway_start_events(std::iter::once(plan), stream_accumulator, stream_sender).await?;
                }
                emit_gateway_completed_events(result, std::iter::once(plan), stream_accumulator, stream_sender).await?;
            }
            emit_deferred_stream_events(
                std::mem::take(output_events),
                ctx,
                stream_accumulator,
                stream_sender,
                output_offset,
            )
            .await?;
        }
        emit_deferred_stream_events(remaining_events, ctx, stream_accumulator, stream_sender, output_offset).await?;
        Ok(gateway_results)
    }
}

fn log_custom_tool_calls(output: &[OutputItem], response_id: &str) {
    for item in output {
        if let OutputItem::CustomToolCall(call) = item {
            debug!(
                response_id,
                call_id = %call.call_id,
                name = %call.name,
                input_bytes = call.input.len(),
                "custom tool call requires client execution"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::InputItem;

    #[test]
    fn round_policy_keeps_client_handoff_precedence_and_uses_the_turns_limit() {
        let results = [GatewayCallResult {
            item_index: 0,
            input_item: InputItem::Unknown,
            public_output: None,
        }];
        assert!(matches!(
            classify_round(true, &results, 0, 1),
            RoundDecision::RequiresClientAction
        ));
        assert!(matches!(classify_round(false, &[], 0, 1), RoundDecision::Done));
        assert!(matches!(classify_round(false, &results, 0, 2), RoundDecision::Continue));
        assert!(matches!(
            classify_round(false, &results, 1, 2),
            RoundDecision::Incomplete(_)
        ));
        // The legacy ten-round policy is supplied by the single-agent adapter,
        // not hardcoded into shared turn execution or used as a tree budget.
        assert!(matches!(
            classify_round(false, &results, 10, 12),
            RoundDecision::Continue
        ));
    }
}
