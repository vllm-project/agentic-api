use std::{collections::HashSet, num::NonZeroUsize};

use super::super::agent_turn::AgentTurn;
use crate::executor::{
    error::{ExecutorError, ExecutorResult},
    multi_agent::{
        AgentPhase, AgentRegistry, AgentState, CheckpointLimits, PendingClientCalls, RegistryLimits,
        ValidatedTreeCheckpoint,
    },
    pipeline::AgentPipeline,
    request::{ExecutionContext, RequestContext},
    response_budget::ExecutorResponseBudget,
};
use crate::tool::ToolSearchHandler;
use crate::types::{
    agent::{AgentCompletion, AgentIdentity, AgentMail, AgentMailContent, AgentTurnKey},
    agent_tree::{StoredAgent, StoredTreeSnapshot},
    client_calls::{ClientToolOutput, ClientToolOutputBatch},
    io::{InputItem, InputMessage, InputMessageContent, OutputItem, ResponsesInput},
    request_response::ContextManagement,
};
use crate::utils::common::serialized_size_up_to;

use super::{
    AgentContext, DEFAULT_COMPACT_THRESHOLD, MAX_AGENTS, MAX_CALLS, MAX_MAIL, MAX_ROUNDS, call_error, invalid,
    registry_error,
};

/// Apply a warmup turn through the same ownership validation as a generated
/// continuation, without building registries or starting inference tasks.
pub(in crate::executor::engine) fn prepare_without_inference(
    request: &mut RequestContext,
    exec: &ExecutionContext,
) -> ExecutorResult<()> {
    let Some(config) = request
        .enriched_request
        .multi_agent
        .clone()
        .filter(|config| config.enabled)
    else {
        return Ok(());
    };
    let budget = ExecutorResponseBudget::with_limit(exec.responses_config.max_retained_bytes);
    let mut restored = restore_agents(request, exec, &budget)?;
    if restored.continuing_tree {
        restored.continue_input(&request.response_id, &request.new_input_items)?;
    }
    // Keep only shared settings in the template; canonical agent histories are
    // moved into preparation and back instead of copying them for every agent.
    let mut settings = request.enriched_request.clone();
    settings.input = ResponsesInput::Items(Vec::new());
    for agent in &mut restored.agents {
        let mut effective = settings.clone();
        effective.input = ResponsesInput::Items(std::mem::take(&mut agent.history));
        let search = ToolSearchHandler::prepare_request(
            &mut effective,
            &agent.loaded_tools,
            request.original_request.tools.is_some(),
        )?;
        agent.history = match effective.input {
            ResponsesInput::Items(history) => history,
            input @ ResponsesInput::Text(_) => Vec::from(&input),
        };
        if let Some(search) = search {
            agent.loaded_tools = search.into_public_metadata().loaded_tools;
        }
        restored.registry.checkpoint_agent(agent).map_err(registry_error)?;
    }
    request.multi_agent_tree = Some(ValidatedTreeCheckpoint::from_stored(
        StoredTreeSnapshot {
            version: 1,
            config,
            agents: restored.agents,
            client_calls: restored.pending.checkpoint(),
        },
        &CheckpointLimits::for_response(exec.responses_config.max_retained_bytes),
    )?);
    Ok(())
}

impl RestoredAgents {
    pub(super) fn continue_input(&mut self, response_id: &str, input: &[InputItem]) -> ExecutorResult<()> {
        let mut outputs = Vec::new();
        let mut root_input = Vec::new();
        for item in input {
            match item {
                InputItem::FunctionCallOutput(output) => outputs.push(ClientToolOutput::Function(output.clone())),
                InputItem::ShellCallOutput(output) => outputs.push(ClientToolOutput::Shell(output.clone())),
                InputItem::CustomToolCallOutput(output) => outputs.push(ClientToolOutput::Custom(output.clone())),
                InputItem::ToolSearchOutput(output) => outputs.push(ClientToolOutput::ToolSearch(output.clone())),
                InputItem::Message(_) => root_input.push(item.clone()),
                _ => {
                    return Err(invalid(
                        "multi-agent continuation accepts new messages and pending function, shell, custom, or tool-search outputs",
                    ));
                }
            }
        }
        let ids = outputs
            .iter()
            .map(|output| output.call_id().to_owned())
            .collect::<Vec<_>>();
        if self
            .pending
            .pending()
            .any(|call| !ids.iter().any(|id| id == call.call_id.as_str()))
        {
            return Err(invalid("provide outputs for every outstanding multi-agent client call"));
        }
        self.pending
            .accept_outputs(ClientToolOutputBatch {
                response_id: response_id.to_owned(),
                outputs,
            })
            .map_err(|error| invalid(&error.to_string()))?;
        let mut resumed = HashSet::new();
        for id in ids {
            let routed = self
                .pending
                .take_accepted(&id)
                .expect("accepted output retained until transfer");
            let key = routed.owner.agent_turn;
            self.agents
                .iter_mut()
                .find(|agent| agent.identity == key.agent)
                .expect("validated call owner")
                .history
                .push(routed.output.into());
            if let Some(agent) = self
                .registry
                .get(&key.agent)
                .filter(|agent| agent.state == AgentState::Active(AgentPhase::WaitingForClientOutputs))
            {
                resumed.insert(AgentTurnKey {
                    agent: key.agent,
                    turn: agent.turn,
                });
            }
        }
        for key in resumed {
            self.registry
                .set_phase(&key, AgentPhase::Runnable)
                .map_err(registry_error)?;
        }
        // Client outputs resume only their owners. Parents parked at the previous
        // response boundary remain asleep until registry mailbox delivery wakes them.
        if !root_input.is_empty() {
            self.registry.resume_root();
            self.agents
                .iter_mut()
                .find(|agent| agent.identity.is_root())
                .expect("root exists")
                .history
                .extend(root_input);
        }
        Ok(())
    }
}
pub(super) struct RestoredAgents {
    pub(super) registry: AgentRegistry,
    pub(super) pending: PendingClientCalls,
    pub(super) agents: Vec<StoredAgent>,
    pub(super) continuing_tree: bool,
}

pub(super) fn restore_agents(
    request: &mut RequestContext,
    exec: &ExecutionContext,
    budget: &ExecutorResponseBudget,
) -> ExecutorResult<RestoredAgents> {
    let limits = RegistryLimits {
        max_agents: MAX_AGENTS,
        max_mailbox_messages: MAX_MAIL,
    };
    let tree = request
        .multi_agent_tree
        .take()
        .map(ValidatedTreeCheckpoint::into_snapshot);
    let continuing_tree = tree.is_some();
    if let Some(tree) = &tree {
        let bytes = serialized_size_up_to(tree, exec.responses_config.max_retained_bytes)
            .map_err(ExecutorError::JsonError)?
            .ok_or_else(|| invalid("multi-agent checkpoint exceeds the retained-byte limit"))?;
        budget.consume(bytes)?;
    }
    let mut registry = match &tree {
        Some(tree) => AgentRegistry::restore(&tree.agents, limits, budget.clone()).map_err(registry_error)?,
        None => AgentRegistry::with_budget(limits, budget.clone()).map_err(registry_error)?,
    };
    let response_id = request.response_id.clone();
    let pending = PendingClientCalls::restore(
        response_id.clone(),
        tree.as_ref().map_or(&[], |tree| tree.client_calls.as_slice()),
        &registry,
        MAX_CALLS,
        budget.clone(),
    )
    .map_err(call_error)?;
    let agents = if let Some(tree) = tree {
        tree.agents
    } else {
        let root = registry.resume_root();
        vec![StoredAgent {
            identity: root.agent,
            parent: None,
            turn: root.turn,
            state: AgentState::Active(AgentPhase::Runnable),
            mailbox: Vec::new(),
            history: match &request.enriched_request.input {
                ResponsesInput::Items(items) => items.clone(),
                ResponsesInput::Text(_) => Vec::from(&request.enriched_request.input),
            },
            loaded_tools: Vec::new(),
            last_task: String::new(),
            final_answer: None,
            rounds: 0,
            wait: None,
        }]
    };
    Ok(RestoredAgents {
        registry,
        pending,
        agents,
        continuing_tree,
    })
}

pub(super) async fn prepare_agent(
    mut stored: StoredAgent,
    parent: &RequestContext,
    exec: &ExecutionContext,
    budget: &ExecutorResponseBudget,
) -> ExecutorResult<AgentContext> {
    let mut request = parent.enriched_request.clone();
    request.input = ResponsesInput::Items(stored.history.clone());
    request.previous_response_id = None;
    request.conversation_id = None;
    let management = request.context_management.get_or_insert_with(Vec::new);
    if let Some(entry) = management.iter_mut().find(|entry| entry.type_ == "compaction") {
        entry.compact_threshold.get_or_insert(DEFAULT_COMPACT_THRESHOLD);
    } else {
        management.push(ContextManagement {
            type_: "compaction".into(),
            compact_threshold: Some(DEFAULT_COMPACT_THRESHOLD),
        });
    }
    let tool_search = ToolSearchHandler::prepare_request(
        &mut request,
        &stored.loaded_tools,
        parent.original_request.tools.is_some(),
    )?;
    // Discovery resolves public search pairs before this private history is used for inference.
    stored.history = match &request.input {
        ResponsesInput::Items(items) => items.clone(),
        ResponsesInput::Text(_) => Vec::from(&request.input),
    };
    let ctx = RequestContext {
        multi_agent_tree: None,
        original_request: request.clone(),
        enriched_request: request,
        new_input_items: Vec::new(),
        response_id: parent.response_id.clone(),
        conversation_id: None,
        conversation_version: None,
        continuation: None,
    };
    let mut pipeline = AgentPipeline::new(ctx, tool_search, None);
    let turn = AgentTurn::new(&mut pipeline, exec, budget, MAX_ROUNDS).await?;
    let mut execution = turn.execution_state();
    let discovery = execution.take_discovery_output();
    stored
        .history
        .extend(discovery.iter().filter_map(OutputItem::to_input_item));
    let tool_search = pipeline.tool_search_state().cloned();
    let (ctx, _) = pipeline.into_parts();
    Ok(AgentContext {
        discovery,
        discovery_dirty: false,
        generation: 0,
        compacted_generation: None,
        compacting: false,
        stored,
        execution,
        request: ctx.enriched_request,
        tool_search,
    })
}

pub(super) fn fork_history(history: &[InputItem], fork_turns: &str) -> ExecutorResult<Vec<InputItem>> {
    let start = match fork_turns {
        "none" => history.len(),
        "all" => 0,
        count => {
            let count = count
                .parse::<NonZeroUsize>()
                .map_err(|_| invalid("fork_turns must be all, none, or a positive integer"))?;
            history
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, item)| matches!(item, InputItem::Message(message) if message.role == "user"))
                .nth(count.get() - 1)
                .map_or(0, |(index, _)| index)
        }
    };
    let retained = &history[start..];
    let completed = retained
        .iter()
        .filter_map(|item| match item {
            InputItem::FunctionCallOutput(output) => Some(output.call_id.as_str()),
            InputItem::ShellCallOutput(output) => Some(output.call_id.as_str()),
            InputItem::CustomToolCallOutput(output) => Some(output.call_id.as_str()),
            InputItem::ToolSearchOutput(output) => Some(output.call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let calls = retained
        .iter()
        .filter_map(|item| match item {
            InputItem::FunctionCall(call) => Some(call.call_id.as_str()),
            InputItem::ShellCall(call) => Some(call.call_id.as_str()),
            InputItem::CustomToolCall(call) => Some(call.call_id.as_str()),
            InputItem::ToolSearchCall(call) => Some(call.call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    Ok(retained
        .iter()
        .filter(|item| match item {
            InputItem::FunctionCall(call) => completed.contains(call.call_id.as_str()),
            InputItem::ShellCall(call) => completed.contains(call.call_id.as_str()),
            InputItem::CustomToolCall(call) => completed.contains(call.call_id.as_str()),
            InputItem::ToolSearchCall(call) => completed.contains(call.call_id.as_str()),
            InputItem::FunctionCallOutput(output) => calls.contains(output.call_id.as_str()),
            InputItem::ShellCallOutput(output) => calls.contains(output.call_id.as_str()),
            InputItem::CustomToolCallOutput(output) => calls.contains(output.call_id.as_str()),
            InputItem::ToolSearchOutput(output) => calls.contains(output.call_id.as_str()),
            _ => item.is_model_visible(),
        })
        .cloned()
        .collect())
}

pub(super) fn mail_input(recipient: &AgentIdentity, mail: &AgentMail) -> InputItem {
    let (kind, text) = match &mail.content {
        AgentMailContent::Message(text) | AgentMailContent::TurnFinished(AgentCompletion::Failed(text)) => {
            ("MESSAGE", text.as_str())
        }
        AgentMailContent::Task(text) => {
            return InputItem::Message(InputMessage {
                role: "user".into(),
                content: InputMessageContent::Text(format!(
                    "Message Type: NEW_TASK\nTask name: {recipient}\nSender: {}\n\n\
                     You are {recipient}. This is your assigned task, not a request to repeat your parent's delegation. \
                     Use inherited conversation as background and return your own findings to your parent when done.\n\
                     Payload:\n{text}",
                    mail.sender.agent
                )),
                ..Default::default()
            });
        }
        AgentMailContent::TurnFinished(AgentCompletion::Finished(text)) => ("FINAL_ANSWER", text.as_str()),
        AgentMailContent::TurnFinished(AgentCompletion::Interrupted) => ("MESSAGE", "Agent was interrupted."),
    };
    InputItem::Message(InputMessage {
        role: "user".into(),
        content: InputMessageContent::Text(format!(
            "Message Type: {kind}\nTask name: {recipient}\nSender: {}\nPayload:\n{text}",
            mail.sender.agent
        )),
        ..Default::default()
    })
}
