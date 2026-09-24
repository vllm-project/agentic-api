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

use indexmap::IndexMap;

pub(super) use super::active_text::StreamedPart;
use super::active_text::{MessageState, ReasoningState};
use super::completion::MergeDone;
use crate::events::types::ShellCommandUpdate;
use crate::events::{EventPayload, SSEItemType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{
    ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedAccount, RetainedSize,
};
use crate::types::event::MessageStatus;
use crate::types::io::output::McpListTools;
use crate::types::io::{
    ApplyDone, CodeInterpreterCall, CompactionItem, CustomToolCall, FunctionToolCall, McpCall, OutputItem,
    OutputMessage, ReasoningOutput, ShellCall, ToolSearchCall, WebSearchCall,
};

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

#[derive(Clone)]
pub(super) struct ShellCallState {
    pub(super) item: ShellCall,
    /// Per-command completion flags once the first command event arrives.
    command_stream: Option<Vec<bool>>,
    /// Text of the command currently streaming; moved into the item on done.
    command: String,
}

impl ShellCallState {
    fn new(item: ShellCall) -> Self {
        Self {
            item,
            command_stream: None,
            command: String::new(),
        }
    }

    pub(super) fn has_unfinished_commands(&self) -> bool {
        self.command_stream
            .as_deref()
            .is_some_and(|done| done.iter().any(|complete| !complete))
    }

    pub(super) fn tracks_commands(&self) -> bool {
        self.command_stream.is_some()
    }

    fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        let EventPayload::ShellCallCommand {
            command_index, update, ..
        } = payload
        else {
            return Ok(());
        };
        let index = *command_index as usize;
        let done = self.command_stream.as_deref().unwrap_or_default();
        match update {
            ShellCommandUpdate::Added(command) => {
                if index != done.len() || self.item.action.commands.len() != done.len() || done.last() == Some(&false) {
                    return Err(invalid("shell command added out of order"));
                }
                account.charge(budget, RETAINED_CONTAINER_OVERHEAD_BYTES + command.len())?;
                self.item.action.commands.push(String::new());
                self.command.clone_from(command);
                self.command_stream.get_or_insert_with(Vec::new).push(false);
                Ok(())
            }
            ShellCommandUpdate::Delta(delta) => {
                if done.get(index) != Some(&false) {
                    return Err(invalid("shell command delta has no active command"));
                }
                account.charge(budget, delta.len())?;
                self.command.push_str(delta);
                Ok(())
            }
            ShellCommandUpdate::Done(command) => {
                if done.get(index) != Some(&false) || self.command != *command {
                    return Err(invalid(
                        "shell command done is repeated or contradicts streamed command",
                    ));
                }
                account.grow(
                    budget,
                    self,
                    |state| state.item.action.commands[index].len() + state.command.len(),
                    |state| {
                        state.item.apply_done(payload, &mut state.command);
                        if let Some(done) = state.command_stream.as_mut() {
                            done[index] = true;
                        }
                    },
                )
            }
        }
    }
}

impl RetainedSize for ShellCallState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.command.len()
    }
}

/// Tracks a single output item currently being streamed.
#[derive(Clone)]
pub(super) enum ActiveItem {
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
        let EventPayload::OutputItemAdded { item_type, .. } = payload else {
            return None;
        };
        Some(match item_type {
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
            SSEItemType::Message => Self::Message(MessageState {
                item: OutputMessage::try_from(payload).ok()?,
                parts: IndexMap::new(),
            }),
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
            Self::Message(state) => state.apply(payload, account, budget),
            Self::Reasoning(state) => state.apply(payload, account, budget),
            Self::FunctionCall(state) => state.apply(payload, account, budget),
            Self::CustomToolCall(state) => state.apply(payload, account, budget),
            Self::ShellCall(state) => state.apply(payload, account, budget),
            Self::CodeInterpreterCall { .. }
            | Self::ToolSearchCall { .. }
            | Self::WebSearchCall { .. }
            | Self::McpCall { .. }
            | Self::McpListTools { .. }
            | Self::Compaction { .. } => Ok(()),
        }
    }

    /// Merge an `output_item.done` payload with the same per-kind policy for a
    /// typed completed item (`MergeDone`) or a raw payload (`ApplyDone`).
    pub(super) fn merge_completion(&mut self, payload: &EventPayload, done_item: Option<&OutputItem>, item_id: &str) {
        match (self, done_item) {
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
        }
    }
}
