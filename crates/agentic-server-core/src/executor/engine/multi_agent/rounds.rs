use super::super::{
    accumulate_usage,
    agent_turn::{AgentTurn, RoundDecision, RoundResult},
};
use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    multi_agent::{
        AgentPhase, AgentState, AgentTaskCompletion, AgentTaskOutcome, CompactionCommit, CompactionPlan,
        CompactionResult, collaboration::attribution,
    },
    pipeline::{AgentFrameSink, AgentPipeline, AgentRoundId},
    request::{ExecutionContext, RequestContext},
};
use crate::types::{
    agent::{AgentCompletion, AgentTurnKey},
    client_calls::{ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration},
    io::output::MessagePhase,
    io::{InputItem, MultiAgentAction, OutputItem, OutputMessageContent, ResponsesInput},
    request_response::IncompleteDetails,
};

use super::{
    CompletedRound, CompletedWork, MAX_RUN_ROUNDS, MultiAgentRun, call_error, invalid, mail_input, registry_error,
};

impl MultiAgentRun {
    pub(super) fn schedule(
        &mut self,
        exec: &ExecutionContext,
        auth: Option<&str>,
        pipeline: &AgentPipeline,
    ) -> ExecutorResult<()> {
        if self.root_finished {
            return Ok(());
        }
        let runnable = self
            .registry
            .agents()
            .filter(|agent| agent.state == AgentState::Active(AgentPhase::Runnable))
            .map(|agent| AgentTurnKey {
                agent: agent.identity.clone(),
                turn: agent.turn,
            })
            .collect::<Vec<_>>();
        for turn in runnable {
            if self.contexts[&turn.agent].stored.wait.is_some() || self.contexts[&turn.agent].compacting {
                continue;
            }
            if self
                .pending
                .pending()
                .any(|call| call.owner.agent_turn.agent == turn.agent)
            {
                self.registry
                    .set_phase(&turn, AgentPhase::WaitingForClientOutputs)
                    .map_err(registry_error)?;
                continue;
            }
            if self.rounds >= MAX_RUN_ROUNDS {
                self.payload.status = "incomplete".into();
                self.payload.incomplete_details = Some(IncompleteDetails {
                    reason: Some("multi-agent inference-round budget exhausted".into()),
                });
                self.settle_turn(&turn, &AgentCompletion::Interrupted)
                    .map_err(registry_error)?;
                continue;
            }
            let mail = self.registry.take_mailbox(&turn).map_err(registry_error)?;
            let context = self
                .contexts
                .get_mut(&turn.agent)
                .expect("registry and contexts have identical membership");
            for message in mail {
                context.generation += 1;
                context.stored.history.push(mail_input(&turn.agent, &message));
            }
            if context.compacted_generation != Some(context.generation) {
                if let Some(plan) = CompactionPlan::prepare(
                    &turn.agent,
                    context.generation,
                    &context.stored.history,
                    &context.request,
                )? {
                    context.compacting = true;
                    let exec = exec.clone();
                    let auth = auth.map(str::to_owned);
                    self.tasks
                        .spawn_turn(turn.clone(), async move {
                            plan.execute(&exec, auth.as_deref())
                                .await
                                .map(CompletedWork::Compaction)
                        })
                        .map_err(|error| invalid(&error.to_string()))?;
                    self.registry
                        .set_phase(&turn, AgentPhase::Inferring)
                        .map_err(registry_error)?;
                    continue;
                }
            }
            self.spawn_round(&turn, exec, auth, pipeline)?;
        }
        Ok(())
    }

    pub(super) fn spawn_round(
        &mut self,
        turn: &AgentTurnKey,
        exec: &ExecutionContext,
        auth: Option<&str>,
        pipeline: &AgentPipeline,
    ) -> ExecutorResult<()> {
        let context = &self.contexts[&turn.agent];
        let mut request = context.request.clone();
        request.input = ResponsesInput::Items(context.stored.history.clone());
        let ctx = RequestContext {
            multi_agent_tree: None,
            original_request: request.clone(),
            enriched_request: request,
            new_input_items: Vec::new(),
            response_id: self.payload.id.clone(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        };
        let sender = pipeline.stream_sender();
        let streaming = sender.is_some();
        let mut agent = AgentPipeline::with_limits(
            ctx,
            context.tool_search.clone(),
            sender,
            exec.responses_config.max_stream_event_bytes,
        );
        agent.set_agent_guidance(self.round_guidance(turn));
        if streaming {
            agent.set_agent_frame_sink(AgentFrameSink {
                agent: turn.agent.clone(),
                round: self.rounds,
                sender: self.frame_sender.clone(),
            });
        }
        let execution = context.execution.clone();
        let source = AgentRoundId {
            agent: turn.agent.clone(),
            round: self.rounds,
        };
        let exec = exec.clone();
        let auth = auth.map(str::to_owned);
        let budget = self.budget.clone();
        self.tasks
            .spawn_turn(turn.clone(), async move {
                let mut turn = AgentTurn::resume(&mut agent, &exec, execution);
                let result = turn.run_round(0, auth.as_deref(), streaming, &budget).await?;
                let execution = turn.execution_state();
                let tool_search = agent.tool_search_state().cloned();
                let (ctx, _) = agent.into_parts();
                Ok(CompletedWork::Round(Box::new(CompletedRound {
                    source,
                    result,
                    execution,
                    request: ctx.enriched_request,
                    tool_search,
                })))
            })
            .map_err(|error| invalid(&error.to_string()))?;
        self.registry
            .set_phase(turn, AgentPhase::Inferring)
            .map_err(registry_error)?;
        self.rounds += 1;
        Ok(())
    }

    pub(super) async fn complete(
        &mut self,
        completion: AgentTaskCompletion<CompletedWork>,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        let turn = completion.owner;
        let completed = match completion.outcome {
            AgentTaskOutcome::Finished(Ok(round)) => round,
            AgentTaskOutcome::Interrupted => {
                self.registry
                    .settle_turn(&turn, &AgentCompletion::Interrupted)
                    .map_err(registry_error)?;
                return Ok(());
            }
            AgentTaskOutcome::Finished(Err(error)) => {
                tracing::warn!(response_id = %self.payload.id, agent = %turn.agent,
                    turn = ?turn.turn, error_code = error.error_code(), "agent work failed");
                self.contexts
                    .get_mut(&turn.agent)
                    .expect("failed task has an owner")
                    .compacting = false;
                self.interrupted.remove(&turn);
                if turn.agent.is_root() || pipeline.has_live_agent_items(&turn.agent) {
                    return Err(error);
                }
                self.settle_turn(&turn, &AgentCompletion::Failed(error.to_string()))
                    .map_err(registry_error)?;
                return Ok(());
            }
            AgentTaskOutcome::JoinFailed(error) => {
                tracing::warn!(response_id = %self.payload.id, agent = %turn.agent,
                    turn = ?turn.turn, "agent task join failed");
                return Err(ExecutorError::StreamError(format!("agent round task failed: {error}")));
            }
        };
        let completed = match completed {
            CompletedWork::Round(round) => *round,
            CompletedWork::Compaction(result) => {
                let context = self.contexts.get_mut(&turn.agent).expect("compacting agent exists");
                context.compacting = false;
                let active = self
                    .registry
                    .get(&turn.agent)
                    .is_some_and(|agent| agent.turn == turn.turn && matches!(agent.state, AgentState::Active(_)));
                self.try_commit_compaction(result, pipeline).await?;
                if self.interrupted.remove(&turn) {
                    self.settle_turn(&turn, &AgentCompletion::Interrupted)
                        .map_err(registry_error)?;
                    return Ok(());
                }
                if active {
                    self.registry
                        .set_phase(&turn, AgentPhase::Runnable)
                        .map_err(registry_error)?;
                }
                return Ok(());
            }
        };
        self.complete_round(&turn, completed, pipeline).await
    }

    pub(super) async fn complete_round(
        &mut self,
        turn: &AgentTurnKey,
        completed: CompletedRound,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        let CompletedRound {
            source,
            result: RoundResult { mut payload, decision },
            execution,
            request,
            tool_search,
        } = completed;
        accumulate_usage(&mut self.payload.usage, payload.usage.take());
        self.registry
            .set_phase(turn, AgentPhase::Runnable)
            .map_err(registry_error)?;
        let context = self
            .contexts
            .get_mut(&turn.agent)
            .expect("completed work has a canonical context");
        context.execution = execution;
        context.stored.history = match &request.input {
            ResponsesInput::Items(items) => items.clone(),
            ResponsesInput::Text(_) => Vec::from(&request.input),
        };
        context.generation += 1;
        context.request = request;
        context.tool_search = tool_search;
        if let Some(state) = context.tool_search.clone() {
            context.stored.loaded_tools = state.into_public_metadata().loaded_tools;
        }
        context.stored.rounds += 1;
        let has_client_calls = self.register_client_calls(turn, &payload.output)?;
        let mut collaboration = false;
        let mut final_answer = String::new();
        let mut has_final_answer = false;
        for mut item in payload.output {
            if let OutputItem::FunctionCall(call) = &item {
                if let Some(action) = MultiAgentAction::from_tool_name(&call.name) {
                    collaboration = true;
                    self.action(turn, action, call.clone(), pipeline).await?;
                    continue;
                }
            }
            if let OutputItem::Message(message) = &mut item {
                if matches!(decision, RoundDecision::Done) && !collaboration && !has_client_calls {
                    message.phase = Some(MessagePhase::FinalAnswer);
                }
                if message.phase == Some(MessagePhase::FinalAnswer) {
                    has_final_answer = true;
                    final_answer.extend(message.content.iter().map(OutputMessageContent::text));
                }
            }
            item.set_agent(attribution(&turn.agent));
            self.publish(item, pipeline).await?;
        }
        pipeline.finish_agent_source(&source);
        if self.interrupted.remove(turn) {
            self.settle_turn(turn, &AgentCompletion::Interrupted)
                .map_err(registry_error)?;
            return Ok(());
        }
        if has_client_calls {
            self.registry
                .set_phase(turn, AgentPhase::WaitingForClientOutputs)
                .map_err(registry_error)?;
        } else if matches!(decision, RoundDecision::UpstreamTerminal | RoundDecision::Incomplete(_)) {
            if turn.agent.is_root() {
                self.payload.status = payload.status;
                self.payload.error = payload.error;
                self.payload.incomplete_details = payload.incomplete_details;
                if let RoundDecision::Incomplete(reason) = decision {
                    self.payload.status = "incomplete".into();
                    self.payload.incomplete_details = Some(IncompleteDetails { reason: Some(reason) });
                }
            }
            self.settle_turn(
                turn,
                &AgentCompletion::Failed("agent inference did not complete".into()),
            )
            .map_err(registry_error)?;
        } else if !collaboration && matches!(decision, RoundDecision::Done) && has_final_answer {
            self.contexts
                .get_mut(&turn.agent)
                .expect("agent exists")
                .stored
                .final_answer = Some(final_answer.clone());
            self.settle_turn(turn, &AgentCompletion::Finished(final_answer.clone()))
                .map_err(registry_error)?;
            if turn.agent.is_root() {
                self.finish_root(turn, pipeline).await?;
            }
            if let Some(parent) = self.registry.get(&turn.agent).and_then(|agent| agent.parent.cloned()) {
                self.publish_mail(&turn.agent, &parent, &final_answer, pipeline).await?;
            }
        }
        Ok(())
    }

    pub(super) fn register_client_calls(&mut self, turn: &AgentTurnKey, output: &[OutputItem]) -> ExecutorResult<bool> {
        let context = &self.contexts[&turn.agent];
        let mut registrations = Vec::new();
        for item in output {
            if !context.execution.requires_client_action(item) {
                continue;
            }
            let (call_id, kind) = match item {
                OutputItem::FunctionCall(call) if MultiAgentAction::from_tool_name(&call.name).is_none() => {
                    (call.call_id.as_str(), ClientCallKind::Function)
                }
                OutputItem::ShellCall(call) => (call.call_id.as_str(), ClientCallKind::Shell),
                OutputItem::CustomToolCall(_) | OutputItem::ToolSearchCall(_) => {
                    return Err(invalid(
                        "multi-agent client continuation supports function and local shell calls",
                    ));
                }
                _ => continue,
            };
            registrations.push(ClientCallRegistration {
                call_id: ClientCallId::try_from(call_id.to_owned()).map_err(|error| invalid(&error.to_string()))?,
                owner: ClientCallOwner {
                    agent_turn: turn.clone(),
                    kind,
                },
            });
        }
        self.pending
            .register_calls(&self.registry, &registrations)
            .map_err(call_error)?;
        Ok(!registrations.is_empty())
    }

    pub(super) async fn try_commit_compaction(
        &mut self,
        result: CompactionResult,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<CompactionCommit> {
        // Charge completed work exactly once, even when its snapshot is stale.
        accumulate_usage(&mut self.payload.usage, Some(result.usage()));
        let agent = result.agent().clone();
        let context = self
            .contexts
            .get_mut(&agent)
            .ok_or_else(|| invalid("compaction owner is missing"))?;
        let commit = result.commit(context.generation, &mut context.stored.history);
        if commit == CompactionCommit::Applied {
            context.compacted_generation = Some(context.generation);
            let item = context.stored.history.iter().rev().find_map(|item| {
                if let InputItem::Compaction(item) = item {
                    Some(item.clone())
                } else {
                    None
                }
            });
            if let Some(item) = item {
                let mut item = OutputItem::Compaction(item);
                item.set_agent(attribution(&agent));
                self.publish(item, pipeline).await?;
            }
        }
        Ok(commit)
    }
}
