//! Response-owned tree driver. Only this task mutates canonical agent state.
//! Round tasks work from owned snapshots and return through the scoped task owner.

mod actions;
mod compaction;
mod context;
mod control;
mod delivery;
mod guidance;
mod rounds;
pub(super) use context::prepare_without_inference;
use context::{RestoredAgents, fork_history, mail_input, prepare_agent, restore_agents};

use indexmap::IndexMap;
use std::{
    collections::{BTreeMap, HashSet},
    num::NonZeroUsize,
    time::Duration,
};
use tokio::sync::mpsc;

use super::agent_turn::{AgentExecutionState, RoundResult};
use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    gateway::emit_response_start_events,
    multi_agent::{
        AgentRegistry, AgentState, CheckpointLimits, ClientCallError, CompactionResult, PendingClientCalls,
        RegistryError, RunControlReceiver, RunOwner, ValidatedTreeCheckpoint,
        collaboration::{TranscriptSealer, attribution},
    },
    pipeline::AgentPipeline,
    relay::{AgentFrame, AgentRoundId},
    request::ExecutionContext,
    response_budget::ExecutorResponseBudget,
};
use crate::tool::ToolSearchState;
use crate::types::{
    agent::{AgentIdentity, AgentTurnKey},
    agent_commands::{AgentListingStatus, CollaborationResult},
    agent_tree::{StoredAgent, StoredTreeSnapshot},
    io::{MultiAgentAction, MultiAgentConfig, OutputItem},
    request_response::{RequestPayload, ResponsePayload},
};
use crate::utils::common::utcnow_str;

// Deployment retention/work ceilings, independent of the public active-turn limit.
const MAX_AGENTS: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const MAX_MAIL: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
const MAX_CALLS: NonZeroUsize = NonZeroUsize::new(4096).unwrap();
const MAX_ROUNDS: NonZeroUsize = NonZeroUsize::new(128).unwrap();
const MAX_RUN_ROUNDS: usize = 1024;
const DEFAULT_COMPACT_THRESHOLD: u64 = 100_000;

struct AgentContext {
    discovery: Vec<OutputItem>,
    discovery_dirty: bool,
    generation: u64,
    compacted_generation: Option<u64>,
    compacting: bool,
    stored: StoredAgent,
    execution: AgentExecutionState,
    request: RequestPayload,
    tool_search: Option<ToolSearchState>,
}

struct CompletedRound {
    source: AgentRoundId,
    result: RoundResult,
    execution: AgentExecutionState,
    request: RequestPayload,
    tool_search: Option<ToolSearchState>,
}

enum CompletedWork {
    Round(Box<CompletedRound>),
    Compaction(CompactionResult),
}

pub(super) struct MultiAgentRun {
    control: Option<RunControlReceiver>,
    config: MultiAgentConfig,
    limit: usize,
    registry: AgentRegistry,
    contexts: IndexMap<AgentIdentity, AgentContext>,
    pending: PendingClientCalls,
    tasks: RunOwner<CompletedWork>,
    interrupted: HashSet<AgentTurnKey>,
    budget: ExecutorResponseBudget,
    sealer: TranscriptSealer,
    payload: ResponsePayload,
    rounds: usize,
    max_retained_bytes: usize,
    frame_sender: mpsc::Sender<AgentFrame>,
    frames: mpsc::Receiver<AgentFrame>,
    completed_items: BTreeMap<usize, OutputItem>,
}

impl MultiAgentRun {
    pub(super) async fn new(pipeline: &mut AgentPipeline, exec: &ExecutionContext) -> ExecutorResult<Self> {
        let request = &pipeline.request;
        let config = request
            .enriched_request
            .multi_agent
            .clone()
            .ok_or_else(|| invalid("missing multi-agent configuration"))?;
        let limit = usize::try_from(config.max_concurrent_subagents.unwrap_or(3))
            .map_err(|_| invalid("invalid max_concurrent_subagents"))?;
        let budget = ExecutorResponseBudget::with_limit(exec.responses_config.max_retained_bytes);
        let mut restored = restore_agents(&mut pipeline.request, exec, &budget)?;
        if restored.continuing_tree {
            restored.continue_input(&pipeline.request.response_id, &pipeline.request.new_input_items)?;
        }
        let RestoredAgents {
            registry,
            pending,
            agents,
            ..
        } = restored;
        if registry
            .agents()
            .filter(|agent| !agent.identity.is_root() && matches!(agent.state, AgentState::Active(_)))
            .count()
            > limit
        {
            return Err(invalid(
                "max_concurrent_subagents is below the stored active-agent count",
            ));
        }
        let mut contexts = IndexMap::new();
        for stored in agents {
            let context = prepare_agent(stored, &pipeline.request, exec, &budget).await?;
            contexts.insert(context.stored.identity.clone(), context);
        }
        let payload = ResponsePayload {
            id: pipeline.request.response_id.clone(),
            object: "response".into(),
            created_at: utcnow_str(),
            model: pipeline.request.enriched_request.model.clone(),
            status: "completed".into(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: pipeline.request.original_request.previous_response_id.clone(),
            conversation: pipeline.request.conversation_id.clone(),
            instructions: pipeline.request.original_request.instructions.clone(),
            max_tool_calls: None,
            service_tier: None,
            tools: pipeline.request.enriched_request.tools.clone(),
            tool_choice: pipeline.request.enriched_request.tool_choice.clone(),
            standard_fields: crate::types::request_response::StandardResponseFields::default(),
        };
        // One bounded event in flight, plus one size-limited awaited frame per
        // active worker. Client backpressure reaches upstream readers.
        let (frame_sender, frames) = mpsc::channel(1);
        Ok(Self {
            control: pipeline.control.take(),
            frame_sender,
            frames,
            completed_items: BTreeMap::new(),
            config,
            limit,
            registry,
            contexts,
            pending,
            tasks: RunOwner::new(limit),
            interrupted: HashSet::new(),
            budget,
            sealer: TranscriptSealer::new()?,
            payload,
            rounds: 0,
            max_retained_bytes: exec.responses_config.max_retained_bytes,
        })
    }

    pub(super) async fn run(
        mut self,
        pipeline: &mut AgentPipeline,
        exec: &ExecutionContext,
        auth: Option<&str>,
    ) -> ExecutorResult<ResponsePayload> {
        // A retained (WebSocket) owner cancels through the pipeline token, and the
        // tasks are still cancelled and joined below. A dropped HTTP stream drops
        // this future instead, and `RunOwner`'s drop aborts the tasks unjoined.
        let cancellation = pipeline.cancellation_token();
        let result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(3600), self.drive(pipeline, exec, auth)) => {
                result.unwrap_or_else(|_| Err(ExecutorError::StreamError("multi-agent response exceeded its one-hour runtime limit".into())))
            }
            () = cancellation.cancelled() => Err(ExecutorError::StreamError("multi-agent response cancelled".into())),
        };
        // Explicitly join on both successful and failed execution; no late task
        // can modify state or emit after the terminal decision.
        let tasks = std::mem::replace(&mut self.tasks, RunOwner::new(0));
        let completions = tasks.cancel_and_join().await;
        if result.is_ok() && !completions.is_empty() {
            return Err(invalid("multi-agent driver finished with live round work"));
        }
        if let Err(error) = &result {
            tracing::warn!(response_id = %self.payload.id,
                error_code = error.error_code(), "multi-agent response execution failed");
        }
        result?;
        let mut agents = Vec::with_capacity(self.contexts.len());
        for (_, mut context) in self.contexts {
            self.registry
                .checkpoint_agent(&mut context.stored)
                .map_err(registry_error)?;
            agents.push(context.stored);
        }
        pipeline.request.multi_agent_tree = Some(ValidatedTreeCheckpoint::from_stored(
            StoredTreeSnapshot {
                version: 1,
                config: self.config,
                agents,
                client_calls: self.pending.checkpoint(),
            },
            &CheckpointLimits::for_response(self.max_retained_bytes),
        )?);
        self.payload.output = self.completed_items.into_values().collect();
        Ok(self.payload)
    }

    async fn drive(
        &mut self,
        pipeline: &mut AgentPipeline,
        exec: &ExecutionContext,
        auth: Option<&str>,
    ) -> ExecutorResult<()> {
        emit_response_start_events(&self.payload, pipeline.relay_mut()).await?;
        let discovery = self
            .contexts
            .iter_mut()
            .flat_map(|(identity, context)| {
                std::mem::take(&mut context.discovery)
                    .into_iter()
                    .map(|mut item| {
                        item.set_agent(attribution(identity));
                        item
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for item in discovery {
            self.publish(item, pipeline).await?;
        }
        loop {
            while let Some(completion) = self.tasks.try_join_next() {
                self.complete(completion, pipeline).await?;
            }
            self.wake_waiters(pipeline).await?;
            self.schedule(exec, auth, pipeline)?;
            if self.tasks.is_empty() {
                let waiting = self
                    .contexts
                    .values()
                    .filter_map(|context| context.stored.wait.as_ref())
                    .map(|wait| wait.deadline_ms)
                    .min();
                // A live socket does not keep an otherwise quiescent response open.
                // Pending client outputs are persisted for explicit continuation.
                if self.pending.pending().next().is_some() || waiting.is_none() {
                    let waiters = self
                        .contexts
                        .iter()
                        .filter_map(|(identity, context)| {
                            context.stored.wait.as_ref().map(|_| AgentTurnKey {
                                agent: identity.clone(),
                                turn: self.registry.get(identity).expect("stored agent is registered").turn,
                            })
                        })
                        .collect::<Vec<_>>();
                    for turn in waiters {
                        let wait = self
                            .contexts
                            .get_mut(&turn.agent)
                            .expect("waiter exists")
                            .stored
                            .wait
                            .take()
                            .expect("wait exists");
                        self.finish_action(
                            &turn,
                            MultiAgentAction::WaitAgent,
                            wait.call_id,
                            Some(CollaborationResult::Wait {
                                message: "Agents are waiting for client tool outputs.".into(),
                                timed_out: false,
                            }),
                            pipeline,
                        )
                        .await?;
                    }
                    if let Some(control) = &mut self.control {
                        control.finish();
                    }
                    break;
                }
            }
            let deadline = self
                .contexts
                .values()
                .filter_map(|context| context.stored.wait.as_ref())
                .map(|wait| wait.deadline_ms)
                .min();
            tokio::select! {
                Some(frame) = self.frames.recv() => self.deliver_frame(frame, pipeline).await?,
                command = control::receive(&mut self.control), if self.control.is_some() => {
                    let command = command.ok_or_else(|| invalid("multi-agent run control disconnected"))?;
                    command.apply(|input| self.accept_live_outputs(input));
                }
                completion = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Some(completion) = completion { self.complete(completion, pipeline).await?; }
                }
                () = wait_until(deadline), if deadline.is_some() => {}
            }
        }
        Ok(())
    }
}

fn listing_status(state: AgentState, final_answer: Option<&str>) -> AgentListingStatus {
    match state {
        AgentState::Active(_) => AgentListingStatus::Running,
        AgentState::Idle => AgentListingStatus::Completed(final_answer.map(str::to_owned)),
        AgentState::Interrupted => AgentListingStatus::Interrupted,
        AgentState::Failed => AgentListingStatus::Failed,
    }
}
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
async fn wait_until(deadline: Option<i64>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(deadline.saturating_sub(now_ms())).unwrap_or(0),
            ))
            .await;
        }
        None => std::future::pending::<()>().await,
    }
}
fn invalid(message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}
fn registry_error(error: RegistryError) -> ExecutorError {
    match error {
        RegistryError::Budget(error) => *error,
        other => invalid(&other.to_string()),
    }
}
fn call_error(error: ClientCallError) -> ExecutorError {
    match error {
        ClientCallError::Budget(error) => *error,
        other => invalid(&other.to_string()),
    }
}

#[cfg(test)]
#[path = "multi_agent_tests.rs"]
mod tests;
