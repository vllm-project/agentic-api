//! Route resolved history into the current request's agent contexts.
//! This reads public items; it does not load storage or accept a tree snapshot.

use super::context::RestoredAgents;
use super::{fork_history, invalid, mail_input, registry_error};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::collaboration::TranscriptSealer;
use crate::executor::multi_agent::{AgentPhase, AgentState};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::tool::ToolSearchState;
use crate::types::agent::{AgentCompletion, AgentIdentity, AgentTurnKey};
use crate::types::agent_commands::{AgentCommand, CollaborationResult};
use crate::types::agent_tree::StoredAgent;
use crate::types::client_calls::{ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration};
use crate::types::event::MessageStatus;
use crate::types::io::input::InputFunctionToolCall;
use crate::types::io::{
    AgentMessageContent, FunctionToolResultMessage, InOutItem, InputAgentMessage, InputContent, InputItem,
    InputMessage, InputMessageContent, InputMultiAgentCall, InputTextContent, MessagePhase,
    MultiAgentCallOutputContent, OutputMessageContent, ResponsesInput, ToolCallOutput,
};
use crate::types::request_response::RequestPayload;
use crate::utils::common::{deserialize_from_str, serialized_size_up_to};
use std::collections::HashMap;

impl RestoredAgents {
    pub(super) fn apply_responses(
        &mut self,
        request: &RequestContext,
        budget: &ExecutorResponseBudget,
        max_bytes: usize,
    ) -> ExecutorResult<()> {
        let settings = &request.enriched_request;
        let bytes = serialized_size_up_to(&settings.input, max_bytes)
            .map_err(ExecutorError::JsonError)?
            .ok_or_else(|| invalid("multi-agent input exceeds the retained-byte limit"))?;
        budget.consume(bytes)?;
        let sealer = TranscriptSealer::new()?;
        let input: Vec<_> = settings.input.input_items().collect();
        let mut calls = HashMap::new();
        let mut outputs = HashMap::new();
        for item in &input {
            match item.as_ref() {
                InputItem::MultiAgentCall(call) if calls.insert(call.call_id.as_str(), call).is_some() => {
                    return Err(invalid("duplicate collaboration call_id in input history"));
                }
                InputItem::MultiAgentCallOutput(output)
                    if outputs.insert(output.call_id.as_str(), output).is_some() =>
                {
                    return Err(invalid("duplicate collaboration output in input history"));
                }
                _ => {}
            }
        }
        let mut client = Vec::new();
        for (index, item) in input.iter().enumerate() {
            if matches!(item.as_ref(), InputItem::Message(message) if message.role != "assistant")
                || item.output_call_id().is_some()
            {
                client.push(InOutItem::Input(item.as_ref().clone()));
                continue;
            }
            if !client.is_empty() {
                self.park_history()?;
                self.continue_input(&request.response_id, &std::mem::take(&mut client))?;
            }
            match item.as_ref() {
                InputItem::MultiAgentCall(call) => {
                    let successful = outputs.get(call.call_id.as_str()).is_some_and(|output| {
                        let text: String = output
                            .output
                            .iter()
                            .map(|part| {
                                let MultiAgentCallOutputContent::OutputText(part) = part;
                                part.text.as_str()
                            })
                            .collect();
                        !matches!(
                            deserialize_from_str::<CollaborationResult>(&text),
                            Ok(CollaborationResult::Error { .. })
                        )
                    });
                    self.collaboration_history(call, successful, &sealer, settings, budget, max_bytes)?;
                }
                InputItem::MultiAgentCallOutput(output) => {
                    let call = calls
                        .get(output.call_id.as_str())
                        .ok_or_else(|| invalid("collaboration output has no matching call"))?;
                    if output.action != call.action {
                        return Err(invalid("collaboration output action does not match its call"));
                    }
                    let owner = identity(call.agent.as_ref().map(|agent| agent.agent_name.as_str()))?;
                    let text = output
                        .output
                        .iter()
                        .map(|part| {
                            let MultiAgentCallOutputContent::OutputText(part) = part;
                            part.text.as_str()
                        })
                        .collect();
                    self.agent_history(&owner)?.history.push(InputItem::FunctionCallOutput(
                        FunctionToolResultMessage {
                            id: output.id.clone(),
                            status: Some(MessageStatus::Completed),
                            call_id: output.call_id.clone(),
                            output: ToolCallOutput::Text(text),
                        },
                    ));
                }
                InputItem::AgentMessage(message) => {
                    let preceding = index.checked_sub(1).and_then(|index| match input[index].as_ref() {
                        InputItem::MultiAgentCall(call) => Some(call),
                        _ => None,
                    });
                    self.message_history(message, preceding, &sealer)?;
                }
                other => self.model_history(other.clone())?,
            }
        }
        self.park_history()?;
        self.continue_input(&request.response_id, &client)?;
        if self.pending.awaiting_outputs().next().is_none()
            && !self
                .registry
                .agents()
                .any(|agent| agent.state == AgentState::Active(AgentPhase::Runnable))
        {
            self.registry.resume_root();
        }
        Ok(())
    }

    fn agent_history(&mut self, owner: &AgentIdentity) -> ExecutorResult<&mut StoredAgent> {
        self.agents
            .iter_mut()
            .find(|agent| &agent.identity == owner)
            .ok_or_else(|| invalid("input item refers to an unknown agent"))
    }

    fn history_turn(&mut self, owner: &AgentIdentity) -> ExecutorResult<AgentTurnKey> {
        self.registry.resume_idle(owner);
        let view = self
            .registry
            .get(owner)
            .ok_or_else(|| invalid("input item refers to an unknown agent"))?;
        let turn = AgentTurnKey {
            agent: owner.clone(),
            turn: view.turn,
        };
        self.registry
            .set_phase(&turn, AgentPhase::Runnable)
            .map_err(registry_error)?;
        let mail = self.registry.take_mailbox(&turn).map_err(registry_error)?;
        self.agent_history(owner)?
            .history
            .extend(mail.iter().map(|mail| mail_input(owner, mail)));
        Ok(turn)
    }

    fn park_history(&mut self) -> ExecutorResult<()> {
        // Interrupt requests may precede output from the target's owned round.
        // Retain that boundary before settling, without scheduling another round.
        for turn in self.interrupted.drain() {
            self.registry
                .settle_turn(&turn, &AgentCompletion::Interrupted)
                .map_err(registry_error)?;
        }
        for agent in self
            .registry
            .agents()
            .map(|agent| (agent.identity.clone(), agent.turn, agent.state))
            .collect::<Vec<_>>()
        {
            if !matches!(agent.2, AgentState::Active(_)) {
                continue;
            }
            let unread_message = self
                .agents
                .iter()
                .find(|context| context.identity == agent.0)
                .is_some_and(|context| {
                    matches!(context.history.last(),
                    Some(InputItem::Message(message)) if message.role != "assistant")
                });
            let phase = if self
                .pending
                .awaiting_outputs()
                .any(|call| call.owner.agent_turn.agent == agent.0)
            {
                AgentPhase::WaitingForClientOutputs
            } else if unread_message {
                AgentPhase::Runnable
            } else {
                AgentPhase::WaitingForMailbox
            };
            self.registry
                .set_phase(
                    &AgentTurnKey {
                        agent: agent.0,
                        turn: agent.1,
                    },
                    phase,
                )
                .map_err(registry_error)?;
        }
        Ok(())
    }

    fn collaboration_history(
        &mut self,
        call: &InputMultiAgentCall,
        successful: bool,
        sealer: &TranscriptSealer,
        request: &RequestPayload,
        budget: &ExecutorResponseBudget,
        max_bytes: usize,
    ) -> ExecutorResult<()> {
        let owner = identity(call.agent.as_ref().map(|agent| agent.agent_name.as_str()))?;
        let turn = self.history_turn(&owner)?;
        let arguments = sealer.command(call.action, &call.arguments)?;
        let command = AgentCommand::parse(call.action, &arguments);
        if successful {
            match command
                .as_ref()
                .map_err(|_| invalid("invalid collaboration history arguments"))?
            {
                AgentCommand::Spawn(task) => {
                    let source = self.agent_history(&owner)?;
                    let history = fork_history(&source.history, &task.fork_turns)?;
                    let bytes = serialized_size_up_to(&history, max_bytes)
                        .map_err(ExecutorError::JsonError)?
                        .ok_or_else(|| invalid("forked context exceeds the retained-byte limit"))?;
                    budget.consume(bytes)?;
                    // Discovery is inherited independently of fork_turns, just as
                    // live spawning copies the parent's current loaded definitions.
                    let search_request = RequestPayload {
                        input: ResponsesInput::from(source.history.clone()),
                        tools: request.tools.clone(),
                        parallel_tool_calls: request.parallel_tool_calls,
                        ..Default::default()
                    };
                    let loaded_tools = ToolSearchState::build_with_loaded_tools(
                        &search_request,
                        &source.loaded_tools,
                        request.tools.is_some(),
                    )?
                    .into_public_metadata()
                    .loaded_tools;
                    let child = self
                        .registry
                        .register_child(&turn, &task.task_name, &task.message)
                        .map_err(registry_error)?;
                    self.agents.push(StoredAgent {
                        identity: child.agent,
                        parent: Some(owner.clone()),
                        turn: child.turn,
                        state: AgentState::Active(AgentPhase::Runnable),
                        mailbox: Vec::new(),
                        history,
                        loaded_tools,
                        last_task: task.message.clone(),
                        final_answer: None,
                        rounds: 0,
                        wait: None,
                    });
                }
                AgentCommand::Send(task) => {
                    let target = target(&owner, &task.target)?;
                    self.registry
                        .send_message(&turn, &target, &task.message)
                        .map_err(registry_error)?;
                }
                AgentCommand::Followup(task) => {
                    let target = target(&owner, &task.target)?;
                    if let Some(turn) = self.interrupted.iter().find(|turn| turn.agent == target).cloned() {
                        self.registry
                            .settle_turn(&turn, &AgentCompletion::Interrupted)
                            .map_err(registry_error)?;
                        self.interrupted.remove(&turn);
                    }
                    self.registry
                        .followup_task(&turn, &target, &task.message)
                        .map_err(registry_error)?;
                    let agent = self.agent_history(&target)?;
                    agent.last_task.clone_from(&task.message);
                    agent.final_answer = None;
                }
                AgentCommand::Interrupt(task) => {
                    let target = target(&owner, &task.target)?;
                    let agent = self
                        .registry
                        .get(&target)
                        .ok_or_else(|| invalid("unknown interrupted agent"))?;
                    if matches!(agent.state, AgentState::Active(_)) {
                        let key = AgentTurnKey {
                            agent: target,
                            turn: agent.turn,
                        };
                        self.interrupted.insert(key);
                    }
                }
                AgentCommand::Wait(_) | AgentCommand::List(_) => {}
            }
        }
        self.agent_history(&owner)?
            .history
            .push(InputItem::FunctionCall(InputFunctionToolCall {
                id: call.id.clone(),
                agent: None,
                call_id: call.call_id.clone(),
                name: call.action.tool_name().into(),
                namespace: None,
                arguments,
                status: Some(MessageStatus::Completed),
                async_execution: false,
            }));
        Ok(())
    }

    fn model_history(&mut self, mut item: InputItem) -> ExecutorResult<()> {
        let owner = identity(item.take_agent().as_ref().map(|agent| agent.agent_name.as_str()))?;
        // Discovery is response metadata, not an inference turn. Retaining a
        // listing attributed to a completed child must not make that child active.
        if matches!(item, InputItem::McpListTools(_)) {
            self.agent_history(&owner)?.history.push(item);
            return Ok(());
        }
        let turn = self.history_turn(&owner)?;
        let call = match &item {
            InputItem::FunctionCall(call) => {
                Some((call.call_id.as_str(), ClientCallKind::Function, call.async_execution))
            }
            InputItem::ShellCall(call) => Some((call.call_id.as_str(), ClientCallKind::Shell, false)),
            InputItem::CustomToolCall(call) => {
                Some((call.call_id.as_str(), ClientCallKind::Custom, call.async_execution))
            }
            InputItem::ToolSearchCall(call) => call
                .call_id
                .as_deref()
                .map(|id| (id, ClientCallKind::ToolSearch, false)),
            _ => None,
        };
        if let Some((id, kind, async_execution)) = call {
            self.pending
                .register_calls(
                    &self.registry,
                    &[ClientCallRegistration {
                        call_id: ClientCallId::try_from(id.to_owned()).map_err(|error| invalid(&error.to_string()))?,
                        owner: ClientCallOwner {
                            agent_turn: turn,
                            kind,
                            async_execution,
                        },
                    }],
                )
                .map_err(super::call_error)?;
        }
        if let InputItem::Message(message) = &item
            && message.phase == Some(MessagePhase::FinalAnswer)
        {
            let text = match &message.content {
                InputMessageContent::Text(text) => text.clone(),
                InputMessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        InputContent::InputText(part) | InputContent::OutputText(part) => Some(part.text.as_str()),
                        _ => None,
                    })
                    .collect(),
            };
            self.agent_history(&owner)?
                .final_answer
                .get_or_insert_with(String::new)
                .push_str(&text);
        }
        self.agent_history(&owner)?.history.push(item);
        Ok(())
    }

    fn message_history(
        &mut self,
        message: &InputAgentMessage,
        preceding: Option<&InputMultiAgentCall>,
        sealer: &TranscriptSealer,
    ) -> ExecutorResult<()> {
        let author = identity(Some(&message.author))?;
        let recipient = identity(Some(&message.recipient))?;
        let mut parts = message_parts(&message.content, sealer)?;
        let text: String = parts
            .iter()
            .filter_map(|part| match part {
                InputContent::InputText(part) | InputContent::OutputText(part) => Some(part.text.as_str()),
                _ => None,
            })
            .collect();
        let text_only = parts
            .iter()
            .all(|part| matches!(part, InputContent::InputText(_) | InputContent::OutputText(_)));
        // Hosted actions emit their message immediately after the call. Match that
        // exact projection so a separate client-authored message is never discarded.
        if let Some(call) = preceding {
            let caller = identity(call.agent.as_ref().map(|agent| agent.agent_name.as_str()))?;
            let arguments = sealer.command(call.action, &call.arguments)?;
            let expected = match AgentCommand::parse(call.action, &arguments) {
                Ok(AgentCommand::Spawn(task)) => Some((target(&caller, &task.task_name)?, task.message)),
                Ok(AgentCommand::Send(task) | AgentCommand::Followup(task)) => {
                    Some((target(&caller, &task.target)?, task.message))
                }
                _ => None,
            };
            if text_only
                && caller == author
                && expected
                    .as_ref()
                    .is_some_and(|(target, body)| target == &recipient && body == &text)
            {
                return Ok(());
            }
        }
        if let Some(agent) = self.agents.iter().find(|agent| agent.identity == author)
            && agent.parent.as_ref() == Some(&recipient)
            && text_only
            && agent.final_answer.as_deref() == Some(text.as_str())
            && self
                .registry
                .get(&author)
                .is_some_and(|agent| matches!(agent.state, AgentState::Active(_)))
        {
            let turn = AgentTurnKey {
                agent: author.clone(),
                turn: self
                    .registry
                    .get(&author)
                    .expect("agent histories and registry have identical membership")
                    .turn,
            };
            self.registry
                .settle_turn(&turn, &AgentCompletion::Finished(text))
                .map_err(registry_error)?;
        } else {
            self.history_turn(&recipient)?;
            parts.insert(
                0,
                InputContent::InputText(InputTextContent::new(format!(
                    "Message Type: MESSAGE\nTask name: {recipient}\nSender: {author}\nPayload:\n"
                ))),
            );
            self.agent_history(&recipient)?
                .history
                .push(InputItem::Message(InputMessage {
                    role: "user".into(),
                    content: InputMessageContent::Parts(parts),
                    ..Default::default()
                }));
        }
        Ok(())
    }
}

fn message_parts(content: &[AgentMessageContent], sealer: &TranscriptSealer) -> ExecutorResult<Vec<InputContent>> {
    content
        .iter()
        .map(|part| {
            Ok(match part {
                AgentMessageContent::EncryptedContent { encrypted_content } => {
                    InputContent::InputText(InputTextContent::new(sealer.open(encrypted_content)?))
                }
                AgentMessageContent::InputText(part)
                | AgentMessageContent::Text(part)
                | AgentMessageContent::SummaryText(part)
                | AgentMessageContent::ReasoningText(part) => InputContent::InputText(part.clone()),
                AgentMessageContent::OutputText(part) => OutputMessageContent::OutputText(part.clone()).into(),
                AgentMessageContent::InputImage(part) | AgentMessageContent::ComputerScreenshot(part) => {
                    InputContent::InputImage(part.clone())
                }
                AgentMessageContent::InputFile(part) => InputContent::InputFile(part.clone()),
                AgentMessageContent::Refusal(part) => InputContent::Refusal(part.clone()),
            })
        })
        .collect()
}

fn identity(name: Option<&str>) -> ExecutorResult<AgentIdentity> {
    name.map_or_else(
        || Ok(AgentIdentity::root()),
        |name| AgentIdentity::try_from(name.to_owned()).map_err(|error| invalid(&error.to_string())),
    )
}

fn target(owner: &AgentIdentity, target: &str) -> ExecutorResult<AgentIdentity> {
    if target.starts_with('/') {
        identity(Some(target))
    } else {
        owner.child(target).map_err(|error| invalid(&error.to_string()))
    }
}
