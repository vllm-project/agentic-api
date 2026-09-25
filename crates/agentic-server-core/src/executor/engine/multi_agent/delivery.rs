use crate::events::SSEEventType;

use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    multi_agent::{AgentPhase, AgentState, RegistryError, collaboration::attribution},
    pipeline::{AgentFrame, AgentPipeline, AgentRoundId},
    response_budget::RetainedSize,
};
use crate::types::{
    agent::{AgentCompletion, AgentIdentity, AgentTurnKey},
    agent_commands::CollaborationResult,
    io::output::OutputTextContent,
    io::{
        AgentMessage, AgentMessageContent, FunctionToolResultMessage, InputItem, MultiAgentAction,
        MultiAgentCallOutput, MultiAgentCallOutputContent, OutputItem, ToolCallOutput,
    },
};
use crate::utils::common::{serialize_to_string, uuid7_str};

use super::{MultiAgentRun, now_ms, registry_error};

impl MultiAgentRun {
    pub(super) async fn finish_action(
        &mut self,
        turn: &AgentTurnKey,
        action: MultiAgentAction,
        call_id: String,
        result: Option<CollaborationResult>,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        let text = match result {
            Some(CollaborationResult::Wait {
                message,
                timed_out: false,
            }) if message.is_empty() => String::new(),
            Some(result) => serialize_to_string(&result).map_err(ExecutorError::JsonError)?,
            None => String::new(),
        };
        self.contexts
            .get_mut(&turn.agent)
            .expect("caller exists")
            .stored
            .history
            .push(InputItem::FunctionCallOutput(FunctionToolResultMessage {
                call_id: call_id.clone(),
                output: ToolCallOutput::Text(text.clone()),
            }));
        self.publish(
            OutputItem::MultiAgentCallOutput(MultiAgentCallOutput {
                id: uuid7_str("maco_"),
                call_id,
                action,
                output: vec![MultiAgentCallOutputContent::OutputText(OutputTextContent::new(text))],
                agent: Some(attribution(&turn.agent)),
            }),
            pipeline,
        )
        .await
    }

    pub(super) async fn wake_waiters(&mut self, pipeline: &mut AgentPipeline) -> ExecutorResult<()> {
        let ready = self
            .registry
            .agents()
            .filter_map(|agent| {
                if !matches!(
                    agent.state,
                    AgentState::Active(AgentPhase::Runnable | AgentPhase::WaitingForMailbox)
                ) {
                    return None;
                }
                let wait = self.contexts[agent.identity].stored.wait.as_ref()?;
                (agent.queued_messages > 0 || wait.deadline_ms <= now_ms()).then(|| {
                    (
                        AgentTurnKey {
                            agent: agent.identity.clone(),
                            turn: agent.turn,
                        },
                        agent.queued_messages == 0,
                    )
                })
            })
            .collect::<Vec<_>>();
        for (turn, timed_out) in ready {
            let wait = self
                .contexts
                .get_mut(&turn.agent)
                .expect("waiter exists")
                .stored
                .wait
                .take()
                .expect("waiter has wait");
            self.finish_action(
                &turn,
                MultiAgentAction::WaitAgent,
                wait.call_id,
                Some(CollaborationResult::Wait {
                    message: if timed_out {
                        "Wait timed out."
                    } else {
                        "Agent mailbox updated."
                    }
                    .into(),
                    timed_out,
                }),
                pipeline,
            )
            .await?;
            self.registry
                .set_phase(&turn, AgentPhase::Runnable)
                .map_err(registry_error)?;
        }
        Ok(())
    }

    pub(super) fn settle_turn(
        &mut self,
        turn: &AgentTurnKey,
        completion: &AgentCompletion,
    ) -> Result<(), RegistryError> {
        let parent = self.registry.get(&turn.agent).and_then(|agent| agent.parent.cloned());
        self.registry.settle_turn(turn, completion)?;
        if let Some(parent) = parent {
            self.contexts.get_mut(&parent).expect("parent exists").generation += 1;
        }
        if !self.root_finished && self.registry.resume_queued_task(&turn.agent) {
            let context = self.contexts.get_mut(&turn.agent).expect("agent exists");
            context.execution.restart();
            context.generation += 1;
            context.stored.final_answer = None;
        }
        Ok(())
    }

    pub(super) async fn publish_mail(
        &mut self,
        sender: &AgentIdentity,
        recipient: &AgentIdentity,
        text: &str,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        self.publish(
            OutputItem::AgentMessage(AgentMessage {
                id: uuid7_str("amsg_"),
                author: sender.to_string(),
                recipient: recipient.to_string(),
                content: vec![AgentMessageContent::EncryptedContent {
                    encrypted_content: self.sealer.seal(text)?,
                }],
                agent: Some(attribution(recipient)),
            }),
            pipeline,
        )
        .await
    }

    pub(super) async fn publish(&mut self, item: OutputItem, pipeline: &mut AgentPipeline) -> ExecutorResult<()> {
        self.budget.consume(item.retained_bytes())?;
        let index = pipeline.emit_agent_item(&item).await?;
        self.completed_items.insert(index, item);
        Ok(())
    }

    pub(super) async fn deliver_frame(
        &mut self,
        event: AgentFrame,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        let AgentFrame {
            agent,
            round,
            frame,
            delivered,
        } = event;
        // Response lifecycle is emitted once by this engine. Item completion
        // follows registration and final public projection in complete().
        if !matches!(
            frame.event_type,
            SSEEventType::OutputItemDone
                | SSEEventType::ResponseCreated
                | SSEEventType::ResponseInProgress
                | SSEEventType::ResponseCompleted
                | SSEEventType::ResponseFailed
                | SSEEventType::ResponseIncomplete
        ) {
            pipeline
                .accept_agent_frame(&AgentRoundId { agent, round }, frame)
                .await?;
        }
        let _ = delivered.send(());
        Ok(())
    }
}
