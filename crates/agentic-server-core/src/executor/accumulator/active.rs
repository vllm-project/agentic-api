//! Per-kind state of an output item that is still streaming.
//!
//! Each kind owns its streaming buffers, applies its own incremental and
//! completion updates, and accounts for the retained growth of every update in
//! the same operation. Growth known in advance (a delta, a new container) is
//! charged before the buffer grows; growth decided by a completion policy is
//! measured around the one [`ApplyDone`]/[`MergeDone`] call that performs it,
//! so accounting can never disagree with what the merge actually retained.
//!
//! [`SlotMap`](super::slot::SlotMap) owns identity and lifecycle and dispatches
//! here; nothing in this module resolves indexes or IDs.
mod agent_message;
mod shell;
use agent_message::AgentMessageState;
use shell::ShellCallState;

use indexmap::IndexMap;

pub(super) use super::active_text::MessagePart;
use super::active_text::{MessageState, ReasoningState};
use super::completion::MergeDone;
use crate::events::{EventPayload, SSEItemType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{
    ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedAccount, RetainedSize,
};
use crate::types::event::MessageStatus;
use crate::types::io::output::McpListTools;
use crate::types::io::{
    ApplyDone, CodeInterpreterCall, CodeInterpreterCallStatus, CompactionItem, CustomToolCall, FunctionToolCall,
    McpCall, McpCallStatus, MultiAgentCall, MultiAgentCallOutput, OutputItem, OutputMessage, ReasoningOutput,
    ReasoningStatus, ShellCall, ShellCallStatus, ToolSearchCall, WebSearchCall, WebSearchCallStatus,
};
use crate::types::tools::ToolSearchStatus;

pub(super) type Budget<'a> = Option<&'a ExecutorResponseBudget>;

fn invalid(message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(message.to_owned())
}

#[derive(Clone)]
pub(super) struct FunctionCallState {
    pub(super) item: FunctionToolCall,
    pub(super) arguments: String,
}

impl FunctionCallState {
    fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        match payload {
            EventPayload::FunctionCallArgsDelta { delta, .. } => {
                account.charge(budget, delta.len())?;
                self.arguments.push_str(delta);
                Ok(())
            }
            EventPayload::FunctionCallArgsDone { .. } => account.grow(budget, self, Self::retained_bytes, |state| {
                state.item.apply_done(payload, &mut state.arguments);
            }),
            _ => Ok(()),
        }
    }

    fn finalize(mut self) -> OutputItem {
        if !self.arguments.is_empty() && self.item.arguments.is_empty() {
            self.item.arguments = self.arguments;
        }
        self.item.status = MessageStatus::Completed;
        OutputItem::FunctionCall(self.item)
    }
}

impl RetainedSize for FunctionCallState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.arguments.len()
    }
}

#[derive(Clone)]
pub(super) struct CustomToolCallState {
    pub(super) item: CustomToolCall,
    pub(super) input: String,
}

impl CustomToolCallState {
    fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        match payload {
            EventPayload::CustomToolCallInputDelta { delta, .. } => {
                account.charge(budget, delta.len())?;
                self.input.push_str(delta);
                Ok(())
            }
            EventPayload::CustomToolCallInputDone { .. } => account.grow(budget, self, Self::retained_bytes, |state| {
                state.item.apply_done(payload, &mut state.input);
            }),
            _ => Ok(()),
        }
    }

    fn finalize(mut self) -> OutputItem {
        if self.item.input.is_empty() {
            self.item.input = self.input;
        }
        self.item.status = Some(MessageStatus::Completed);
        OutputItem::CustomToolCall(self.item)
    }
}

impl RetainedSize for CustomToolCallState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.input.len()
    }
}

/// Tracks a single output item currently being streamed.
#[derive(Clone)]
pub(super) enum ActiveItem {
    MultiAgentCall { item: MultiAgentCall },
    MultiAgentCallOutput { item: MultiAgentCallOutput },
    AgentMessage(AgentMessageState),
    Message(MessageState),
    Reasoning(ReasoningState),
    FunctionCall(FunctionCallState),
    CustomToolCall(CustomToolCallState),
    ShellCall(ShellCallState),
    CodeInterpreterCall { item: Option<CodeInterpreterCall> },
    ToolSearchCall { item: ToolSearchCall },
    WebSearchCall { item: Option<WebSearchCall> },
    McpCall { item: McpCall },
    McpListTools { item: McpListTools },
    Compaction { item: CompactionItem },
}

impl std::fmt::Debug for ActiveItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActiveItem::{:?} {{ .. }}", self.item_type())
    }
}

impl ActiveItem {
    pub(super) fn from_added(payload: &EventPayload) -> Option<Self> {
        let EventPayload::OutputItemAdded {
            item_type,
            initial_item,
            ..
        } = payload
        else {
            return None;
        };
        Some(match item_type {
            SSEItemType::MultiAgentCall => {
                let Some(OutputItem::MultiAgentCall(item)) = initial_item.as_deref() else {
                    return None;
                };
                Self::MultiAgentCall { item: item.clone() }
            }
            SSEItemType::MultiAgentCallOutput => {
                let Some(OutputItem::MultiAgentCallOutput(item)) = initial_item.as_deref() else {
                    return None;
                };
                Self::MultiAgentCallOutput { item: item.clone() }
            }
            SSEItemType::AgentMessage => {
                let Some(OutputItem::AgentMessage(item)) = initial_item.as_deref() else {
                    return None;
                };
                Self::AgentMessage(AgentMessageState::new(item.clone()))
            }
            SSEItemType::ShellCall => Self::ShellCall(ShellCallState::new(ShellCall::try_from(payload).ok()?)),
            SSEItemType::Reasoning => Self::Reasoning(ReasoningState::new(ReasoningOutput::try_from(payload).ok()?)),
            SSEItemType::FunctionCall => Self::FunctionCall(FunctionCallState {
                item: FunctionToolCall::try_from(payload).ok()?,
                arguments: String::with_capacity(128),
            }),
            SSEItemType::CodeInterpreterCall => Self::CodeInterpreterCall { item: None },
            SSEItemType::ToolSearchCall => Self::ToolSearchCall {
                item: ToolSearchCall::try_from(payload).ok()?,
            },
            SSEItemType::CustomToolCall => Self::CustomToolCall(CustomToolCallState {
                item: CustomToolCall::try_from(payload).ok()?,
                input: String::with_capacity(256),
            }),
            SSEItemType::Message => Self::Message(MessageState::new(OutputMessage::try_from(payload).ok()?)),
            SSEItemType::WebSearchCall => Self::WebSearchCall { item: None },
            SSEItemType::Compaction => Self::Compaction {
                item: CompactionItem::try_from(payload).ok()?,
            },
            SSEItemType::McpCall => Self::McpCall {
                item: McpCall::try_from(payload).ok()?,
            },
            SSEItemType::McpListTools => Self::McpListTools {
                item: McpListTools::try_from(payload).ok()?,
            },
        })
    }

    /// A temporary candidate for comparing a repeated completion. The retained
    /// slot remains Done, and never regains mutable streaming buffers.
    pub(super) fn from_completed(item: OutputItem) -> Option<Self> {
        Some(match item {
            OutputItem::ShellCall(item) => Self::ShellCall(ShellCallState::new(item)),
            OutputItem::Message(item) => Self::Message(MessageState {
                item,
                parts: IndexMap::new(),
            }),
            OutputItem::Reasoning(item) => Self::Reasoning(ReasoningState::new(item)),
            OutputItem::FunctionCall(item) => Self::FunctionCall(FunctionCallState {
                item,
                arguments: String::new(),
            }),
            OutputItem::CodeInterpreterCall(item) => Self::CodeInterpreterCall { item: Some(item) },
            OutputItem::ToolSearchCall(item) => Self::ToolSearchCall { item },
            OutputItem::CustomToolCall(item) => Self::CustomToolCall(CustomToolCallState {
                item,
                input: String::new(),
            }),
            OutputItem::WebSearchCall(item) => Self::WebSearchCall { item: Some(item) },
            OutputItem::McpCall(item) => Self::McpCall { item },
            OutputItem::McpListTools(item) => Self::McpListTools { item },
            OutputItem::Compaction(item) => Self::Compaction { item },
            OutputItem::MultiAgentCall(item) => Self::MultiAgentCall { item },
            OutputItem::MultiAgentCallOutput(item) => Self::MultiAgentCallOutput { item },
            OutputItem::AgentMessage(item) => Self::AgentMessage(AgentMessageState::new(item)),
            OutputItem::Unknown => return None,
        })
    }

    /// Identity already included in this typed state's retained measurement.
    pub(super) fn id(&self) -> Option<&str> {
        match self {
            Self::Message(state) => Some(&state.item.id),
            Self::Reasoning(state) => Some(&state.item.id),
            Self::FunctionCall(state) => Some(&state.item.id),
            Self::CustomToolCall(state) => Some(&state.item.id),
            Self::ShellCall(state) => state.item.id.as_deref(),
            Self::CodeInterpreterCall { item } => item.as_ref().map(|item| item.id.as_str()),
            Self::ToolSearchCall { item } => Some(&item.id),
            Self::WebSearchCall { item } => item.as_ref().map(|item| item.id.as_str()),
            Self::McpCall { item } => Some(&item.id),
            Self::McpListTools { item } => Some(&item.id),
            Self::Compaction { item } => item.id.as_deref(),
            Self::MultiAgentCall { item } => Some(&item.id),
            Self::MultiAgentCallOutput { item } => Some(&item.id),
            Self::AgentMessage(state) => Some(&state.item.id),
        }
    }

    pub(super) fn item_type(&self) -> SSEItemType {
        match self {
            Self::Message(_) => SSEItemType::Message,
            Self::Reasoning(_) => SSEItemType::Reasoning,
            Self::FunctionCall(_) => SSEItemType::FunctionCall,
            Self::CodeInterpreterCall { .. } => SSEItemType::CodeInterpreterCall,
            Self::ToolSearchCall { .. } => SSEItemType::ToolSearchCall,
            Self::CustomToolCall(_) => SSEItemType::CustomToolCall,
            Self::ShellCall(_) => SSEItemType::ShellCall,
            Self::WebSearchCall { .. } => SSEItemType::WebSearchCall,
            Self::McpCall { .. } => SSEItemType::McpCall,
            Self::McpListTools { .. } => SSEItemType::McpListTools,
            Self::Compaction { .. } => SSEItemType::Compaction,
            Self::MultiAgentCall { .. } => SSEItemType::MultiAgentCall,
            Self::MultiAgentCallOutput { .. } => SSEItemType::MultiAgentCallOutput,
            Self::AgentMessage(_) => SSEItemType::AgentMessage,
        }
    }

    pub(super) fn shell_call(&self) -> Option<&ShellCallState> {
        match self {
            Self::ShellCall(state) => Some(state),
            _ => None,
        }
    }

    /// Fold a resolved incremental event and account for its retained growth.
    /// Identity and lifecycle were already checked by the slot map.
    pub(super) fn apply_event(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        match self {
            Self::AgentMessage(state) => state.apply(payload, account, budget),
            Self::Message(state) => state.apply(payload, account, budget),
            Self::Reasoning(state) => state.apply(payload, account, budget),
            Self::FunctionCall(state) => state.apply(payload, account, budget),
            Self::CustomToolCall(state) => state.apply(payload, account, budget),
            Self::ShellCall(state) => state.apply(payload, account, budget),
            Self::CodeInterpreterCall { .. }
            | Self::MultiAgentCall { .. }
            | Self::MultiAgentCallOutput { .. }
            | Self::ToolSearchCall { .. }
            | Self::WebSearchCall { .. }
            | Self::McpCall { .. }
            | Self::McpListTools { .. }
            | Self::Compaction { .. } => Ok(()),
        }
    }

    /// Completion snapshots may fill in results, but cannot reassign identity
    /// or contradict completed content parts. The returned proof distinguishes
    /// authoritative completion from tolerated recovery: only the former can
    /// close in-flight work before the upstream response itself terminates.
    pub(super) fn validate_completion(
        &self,
        done: Option<&OutputItem>,
        raw: &serde_json::Value,
    ) -> ExecutorResult<bool> {
        let text = |field| raw.get(field).is_some_and(serde_json::Value::is_string);
        Ok(match (self, done) {
            (Self::Message(state), Some(OutputItem::Message(done))) => {
                state.validate_completion(done)?;
                done.status == MessageStatus::Completed && raw.get("content").is_some_and(serde_json::Value::is_array)
            }
            (Self::FunctionCall(_), Some(OutputItem::FunctionCall(done))) => {
                done.status == MessageStatus::Completed
                    && text("status")
                    && text("arguments")
                    && !done.call_id.is_empty()
                    && !done.name.is_empty()
            }
            (Self::CustomToolCall(_), Some(OutputItem::CustomToolCall(done))) => {
                done.status.is_none_or(|status| status == MessageStatus::Completed)
                    && text("input")
                    && !done.call_id.is_empty()
                    && !done.name.is_empty()
            }
            (Self::Reasoning(_), Some(OutputItem::Reasoning(done))) => {
                done.status.is_none_or(|status| status != ReasoningStatus::InProgress)
                    && (raw.get("content").is_some() || raw.get("summary").is_some() || text("encrypted_content"))
            }
            (Self::CodeInterpreterCall { .. }, Some(OutputItem::CodeInterpreterCall(done))) => matches!(
                done.status,
                CodeInterpreterCallStatus::Completed
                    | CodeInterpreterCallStatus::Failed
                    | CodeInterpreterCallStatus::Incomplete
            ),
            (Self::WebSearchCall { .. }, Some(OutputItem::WebSearchCall(done))) => {
                matches!(
                    done.status,
                    WebSearchCallStatus::Completed | WebSearchCallStatus::Failed
                )
            }
            (Self::ToolSearchCall { .. }, Some(OutputItem::ToolSearchCall(done))) => {
                matches!(done.status, ToolSearchStatus::Completed | ToolSearchStatus::Incomplete)
            }
            (Self::McpCall { .. }, Some(OutputItem::McpCall(done))) => matches!(
                done.status,
                Some(McpCallStatus::Completed | McpCallStatus::Incomplete | McpCallStatus::Failed)
            ),
            (Self::McpListTools { .. }, Some(OutputItem::McpListTools(_))) => {
                // This item has no modeled status; an extension cannot prove
                // that hosted activity is finished. The tools field is required.
                raw.get("status").is_none()
            }
            (Self::ShellCall(_), Some(OutputItem::ShellCall(done))) => {
                done.status.is_none_or(|status| status != ShellCallStatus::InProgress)
            }
            (Self::Compaction { .. }, Some(OutputItem::Compaction(_))) => true,
            (Self::MultiAgentCall { item }, Some(OutputItem::MultiAgentCall(done))) => {
                if item.call_id != done.call_id || item.action != done.action || item.agent != done.agent {
                    return Err(invalid(
                        "collaboration call completion changes call_id, action, or attribution",
                    ));
                }
                false
            }
            (Self::MultiAgentCallOutput { item }, Some(OutputItem::MultiAgentCallOutput(done))) => {
                if item.call_id != done.call_id || item.action != done.action || item.agent != done.agent {
                    return Err(invalid(
                        "collaboration output completion changes call_id, action, or attribution",
                    ));
                }
                false
            }
            (Self::AgentMessage(state), Some(OutputItem::AgentMessage(done))) => {
                state.validate_completion(done)?;
                false
            }
            (Self::MultiAgentCall { .. } | Self::MultiAgentCallOutput { .. } | Self::AgentMessage(_), _) => {
                return Err(invalid("collaboration item requires a valid completion snapshot"));
            }
            _ => false,
        })
    }

    /// Merge an `output_item.done` payload with the same per-kind policy for a
    /// typed completed item (`MergeDone`) or a raw payload (`ApplyDone`).
    pub(super) fn merge_completion(&mut self, payload: &EventPayload, done_item: Option<&OutputItem>, item_id: &str) {
        match (self, done_item) {
            (Self::MultiAgentCall { item }, Some(OutputItem::MultiAgentCall(done))) => item.clone_from(done),
            (Self::MultiAgentCallOutput { item }, Some(OutputItem::MultiAgentCallOutput(done))) => {
                item.clone_from(done);
            }
            (Self::AgentMessage(state), Some(OutputItem::AgentMessage(done))) => {
                state.item.clone_from(done);
                state.parts.clear();
            }
            (Self::ShellCall(state), Some(OutputItem::ShellCall(done))) => state.item.merge_done(done, ()),
            (Self::Message(state), Some(OutputItem::Message(done))) => {
                state.item.merge_done(done, &mut state.parts);
            }
            (Self::Reasoning(state), Some(OutputItem::Reasoning(done))) => state.item.merge_done(done, payload),
            (Self::FunctionCall(state), Some(OutputItem::FunctionCall(done))) => {
                state.item.merge_done(done, &mut state.arguments);
            }
            (Self::CodeInterpreterCall { item }, Some(OutputItem::CodeInterpreterCall(done))) => {
                item.merge_done(done, item_id);
            }
            (Self::ToolSearchCall { item }, Some(OutputItem::ToolSearchCall(done))) => item.merge_done(done, ()),
            (Self::CustomToolCall(state), Some(OutputItem::CustomToolCall(done))) => {
                state.item.merge_done(done, &mut state.input);
            }
            (Self::WebSearchCall { item }, Some(OutputItem::WebSearchCall(done))) => item.merge_done(done, item_id),
            (Self::McpCall { item }, Some(OutputItem::McpCall(done))) => item.merge_done(done, ()),
            (Self::McpListTools { item }, Some(OutputItem::McpListTools(done))) => item.merge_done(done, ()),
            (Self::Compaction { item }, Some(OutputItem::Compaction(done))) => item.merge_done(done, ()),
            (Self::ShellCall(state), None) => state.item.apply_done(payload, &mut state.command),
            (Self::Reasoning(state), None) => state.item.apply_done(payload, &mut String::new()),
            (Self::FunctionCall(state), None) => state.item.apply_done(payload, &mut state.arguments),
            (Self::ToolSearchCall { item }, None) => item.apply_done(payload, &mut String::new()),
            (Self::CustomToolCall(state), None) => state.item.apply_done(payload, &mut state.input),
            (Self::McpCall { item }, None) => item.apply_done(payload, &mut String::new()),
            (Self::McpListTools { item }, None) => item.apply_done(payload, &mut String::new()),
            (Self::Compaction { item }, None) => item.apply_done(payload, &mut String::new()),
            _ => {}
        }
    }

    pub(super) fn finalize(self) -> Option<OutputItem> {
        Some(match self {
            Self::ShellCall(state) => OutputItem::ShellCall(state.item),
            Self::Reasoning(state) => OutputItem::Reasoning(state.item),
            Self::FunctionCall(state) => state.finalize(),
            Self::CodeInterpreterCall { item } => OutputItem::CodeInterpreterCall(item?),
            Self::ToolSearchCall { item } => OutputItem::ToolSearchCall(item),
            Self::Message(state) => state.finalize(),
            Self::CustomToolCall(state) => state.finalize(),
            Self::WebSearchCall { item } => OutputItem::WebSearchCall(item?),
            Self::McpCall { item } => OutputItem::McpCall(item),
            Self::McpListTools { item } => OutputItem::McpListTools(item),
            Self::Compaction { item } => OutputItem::Compaction(item),
            Self::MultiAgentCall { item } => OutputItem::MultiAgentCall(item),
            Self::MultiAgentCallOutput { item } => OutputItem::MultiAgentCallOutput(item),
            Self::AgentMessage(state) => OutputItem::AgentMessage(state.item),
        })
    }
}

/// Bytes an in-flight item retains: its typed item plus its streaming buffers.
/// Measures the same fields as the completed item so reconciliation at
/// completion charges only what streaming could not know in advance.
impl RetainedSize for ActiveItem {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Message(state) => state.retained_bytes(),
            Self::Reasoning(state) => state.retained_bytes(),
            Self::FunctionCall(state) => state.retained_bytes(),
            Self::CustomToolCall(state) => state.retained_bytes(),
            Self::ShellCall(state) => state.retained_bytes(),
            Self::CodeInterpreterCall { item } => item
                .as_ref()
                .map_or(RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize::retained_bytes),
            Self::ToolSearchCall { item } => item.retained_bytes(),
            Self::WebSearchCall { item } => item
                .as_ref()
                .map_or(RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize::retained_bytes),
            Self::McpCall { item } => item.retained_bytes(),
            Self::McpListTools { item } => item.retained_bytes(),
            Self::Compaction { item } => item.retained_bytes(),
            Self::MultiAgentCall { item } => item.retained_bytes(),
            Self::MultiAgentCallOutput { item } => item.retained_bytes(),
            Self::AgentMessage(state) => state.retained_bytes(),
        }
    }
}
