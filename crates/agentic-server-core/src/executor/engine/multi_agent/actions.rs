use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    multi_agent::{AgentPhase, AgentState, collaboration::attribution},
    pipeline::AgentPipeline,
};
use crate::types::{
    agent::{AgentCompletion, AgentIdentity, AgentTurnKey},
    agent_commands::{AgentCommand, AgentListing, AgentTarget, CollaborationResult, SpawnAgent},
    agent_tree::{StoredAgent, StoredAgentWait},
    io::output::FunctionToolCall,
    io::{MultiAgentAction, MultiAgentCall, OutputItem},
};
use crate::utils::common::{serialized_size_up_to, uuid7_str};

use super::{AgentContext, MultiAgentRun, fork_history, invalid, listing_status, now_ms, registry_error};

impl MultiAgentRun {
    pub(super) async fn action(
        &mut self,
        turn: &AgentTurnKey,
        action: MultiAgentAction,
        call: FunctionToolCall,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        let command = AgentCommand::parse(action, &call.arguments);
        let arguments = command.as_ref().map_or_else(
            |_| self.sealer.seal(&call.arguments),
            |command| self.sealer.arguments(command),
        )?;
        self.publish(
            OutputItem::MultiAgentCall(MultiAgentCall {
                id: uuid7_str("mac_"),
                call_id: call.call_id.clone(),
                action,
                arguments,
                agent: Some(attribution(&turn.agent)),
            }),
            pipeline,
        )
        .await?;
        let result = match command {
            Err(error) => Some(CollaborationResult::Error {
                error: error.to_string(),
            }),
            Ok(_) if self.interrupted.contains(turn) => Some(CollaborationResult::Error {
                error: "Agent was interrupted before executing this collaboration action.".into(),
            }),
            Ok(command) => match self.dispatch(turn, command, &call.call_id, pipeline).await {
                Ok(None) => return Ok(()), // A mailbox wait completes later.
                Ok(result) => result,
                Err(ExecutorError::InvalidRequest(error)) => Some(CollaborationResult::Error { error }),
                Err(error) => return Err(error),
            },
        };
        self.finish_action(turn, action, call.call_id, result, pipeline).await
    }

    pub(super) async fn dispatch(
        &mut self,
        turn: &AgentTurnKey,
        command: AgentCommand,
        call_id: &str,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<Option<CollaborationResult>> {
        match command {
            AgentCommand::Spawn(task) => self.spawn_agent(turn, task, pipeline).await,
            AgentCommand::Send(task) => {
                let target = self.target(&turn.agent, &task.target)?;
                self.registry
                    .send_message(turn, &target, &task.message)
                    .map_err(registry_error)?;
                self.contexts.get_mut(&target).expect("target exists").generation += 1;
                self.publish_mail(&turn.agent, &target, &task.message, pipeline).await?;
                Ok(Some(CollaborationResult::Wait {
                    message: String::new(),
                    timed_out: false,
                }))
            }
            AgentCommand::Followup(task) => {
                let target = self.target(&turn.agent, &task.target)?;
                let active = self
                    .registry
                    .get(&target)
                    .is_some_and(|agent| matches!(agent.state, AgentState::Active(_)));
                if !active {
                    self.check_admission()?;
                }
                self.registry
                    .followup_task(turn, &target, &task.message)
                    .map_err(registry_error)?;
                let context = self.contexts.get_mut(&target).expect("target was resolved");
                context.generation += 1;
                context.stored.last_task.clone_from(&task.message);
                if !active {
                    context.execution.restart();
                    context.stored.final_answer = None;
                }
                self.publish_mail(&turn.agent, &target, &task.message, pipeline).await?;
                Ok(Some(CollaborationResult::Wait {
                    message: String::new(),
                    timed_out: false,
                }))
            }
            AgentCommand::List(_) => Ok(Some(CollaborationResult::Listing {
                agents: self
                    .registry
                    .agents()
                    .map(|agent| AgentListing {
                        agent_name: agent.identity.to_string(),
                        agent_status: listing_status(
                            agent.state,
                            self.contexts[agent.identity].stored.final_answer.as_deref(),
                        ),
                    })
                    .collect(),
            })),
            AgentCommand::Interrupt(task) => self.interrupt_agent(turn, task, pipeline).await,
            AgentCommand::Wait(wait) => {
                if !(10_000..=3_600_000).contains(&wait.timeout_ms) {
                    return Err(invalid("timeout_ms must be between 10000 and 3600000"));
                }
                if self.contexts[&turn.agent].stored.wait.is_some() {
                    return Err(invalid("the calling agent already has an outstanding wait"));
                }
                self.contexts
                    .get_mut(&turn.agent)
                    .expect("calling agent exists")
                    .stored
                    .wait = Some(StoredAgentWait {
                    call_id: call_id.to_owned(),
                    deadline_ms: now_ms().saturating_add(i64::try_from(wait.timeout_ms).unwrap_or(i64::MAX)),
                });
                self.registry
                    .set_phase(turn, AgentPhase::WaitingForMailbox)
                    .map_err(registry_error)?;
                Ok(None)
            }
        }
    }

    pub(super) async fn spawn_agent(
        &mut self,
        turn: &AgentTurnKey,
        task: SpawnAgent,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<Option<CollaborationResult>> {
        self.check_admission()?;
        if !task
            .task_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid(
                "task_name must contain lowercase letters, digits or underscores",
            ));
        }
        let source = &self.contexts[&turn.agent];
        let history = fork_history(&source.stored.history, &task.fork_turns)?;
        let loaded_tools = source.stored.loaded_tools.clone();
        let bytes = serialized_size_up_to(&history, self.max_retained_bytes)
            .map_err(ExecutorError::JsonError)?
            .ok_or_else(|| invalid("forked context exceeds the retained-byte limit"))?;
        self.budget.consume(bytes)?;
        // Never fork unresolved calls: an output owned by the parent
        // must not become a second unresolved call in the child.
        let mut execution = source.execution.clone();
        execution.restart();
        let request = source.request.clone();
        let tool_search = source.tool_search.clone();
        let child = self
            .registry
            .register_child(turn, &task.task_name, &task.message)
            .map_err(registry_error)?;
        self.contexts.insert(
            child.agent.clone(),
            AgentContext {
                discovery: Vec::new(),
                generation: 0,
                compacted_generation: None,
                compacting: false,
                stored: StoredAgent {
                    identity: child.agent.clone(),
                    parent: Some(turn.agent.clone()),
                    turn: child.turn,
                    state: AgentState::Active(AgentPhase::Runnable),
                    mailbox: Vec::new(),
                    history,
                    loaded_tools,
                    last_task: task.message.clone(),
                    final_answer: None,
                    rounds: 0,
                    wait: None,
                },
                execution,
                request,
                tool_search,
            },
        );
        self.publish_mail(&turn.agent, &child.agent, &task.message, pipeline)
            .await?;
        Ok(Some(CollaborationResult::Spawned {
            task_name: child.agent.to_string(),
        }))
    }

    pub(super) async fn interrupt_agent(
        &mut self,
        turn: &AgentTurnKey,
        task: AgentTarget,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<Option<CollaborationResult>> {
        let target = self.target(&turn.agent, &task.target)?;
        if target == turn.agent {
            return Err(invalid("an agent cannot interrupt itself"));
        }
        let agent = self.registry.get(&target).expect("target was resolved");
        let previous_status = listing_status(agent.state, self.contexts[&target].stored.final_answer.as_deref());
        let key = AgentTurnKey {
            agent: target.clone(),
            turn: agent.turn,
        };
        if self.tasks.has_turn(&key) {
            // Finish the owned boundary before releasing its context.
            // This preserves emitted calls and built-in side effects.
            self.interrupted.insert(key);
            self.contexts.get_mut(&target).expect("target exists").generation += 1;
            return Ok(Some(CollaborationResult::Interrupted { previous_status }));
        }
        if matches!(agent.state, AgentState::Active(_)) {
            let wait = self
                .contexts
                .get_mut(&target)
                .expect("target exists")
                .stored
                .wait
                .take();
            if let Some(wait) = wait {
                self.finish_action(
                    &key,
                    MultiAgentAction::WaitAgent,
                    wait.call_id,
                    Some(CollaborationResult::Error {
                        error: "Agent was interrupted.".into(),
                    }),
                    pipeline,
                )
                .await?;
            }
            self.settle_turn(&key, &AgentCompletion::Interrupted)
                .map_err(registry_error)?;
        }
        Ok(Some(CollaborationResult::Interrupted { previous_status }))
    }

    pub(super) fn check_admission(&self) -> ExecutorResult<()> {
        let active = self
            .registry
            .agents()
            .filter(|agent| !agent.identity.is_root() && matches!(agent.state, AgentState::Active(_)))
            .count();
        if active >= self.limit {
            return Err(invalid("maximum concurrent subagent turns reached"));
        }
        Ok(())
    }

    pub(super) fn target(&self, caller: &AgentIdentity, target: &str) -> ExecutorResult<AgentIdentity> {
        let identity = if target.starts_with('/') {
            AgentIdentity::try_from(target.to_owned())
        } else {
            caller.child(target)
        }
        .map_err(|error| invalid(&error.to_string()))?;
        self.registry
            .get(&identity)
            .ok_or_else(|| invalid("target agent does not exist"))?;
        Ok(identity)
    }
}
