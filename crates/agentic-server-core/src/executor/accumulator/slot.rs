//! Typed output-item state and completion merging for response accumulation.

use super::Validation;
use super::completion::MergeDone;
use std::collections::HashMap;

use indexmap::IndexMap;

use crate::events::types::ShellCommandUpdate;
use crate::events::{EventPayload, SSEItemType};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::event::MessageStatus;
use crate::types::io::output::McpListTools;
use crate::types::io::{
    ApplyDone, CompactionItem, CustomToolCall, FunctionToolCall, McpCall, OutputItem, OutputMessage, OutputTextContent,
    ReasoningOutput, ShellCall, ToolSearchCall, WebSearchCall,
};
use crate::utils::common::deserialize_from_value_opt;
use crate::utils::uuid7_str;

/// A supplied u32 index or an explicitly allocated recovery index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct OutputIndex(u32);

impl OutputIndex {
    pub(super) fn new(index: u32) -> Self {
        Self(index)
    }
    pub(super) fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(super) struct ItemIdentity<'a> {
    pub(super) index: Option<OutputIndex>,
    pub(super) item_id: Option<&'a str>,
    pub(super) item_type: SSEItemType,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotAction {
    Open,
    Mutate,
    Complete,
}

/// The only owner of per-round item identity and lifecycle state.
#[derive(Debug, Default)]
pub(super) struct SlotMap {
    slots: IndexMap<OutputIndex, Slot>,
    indexes_by_id: HashMap<String, OutputIndex>,
}

impl SlotMap {
    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn has_active(&self) -> bool {
        self.slots
            .values()
            .any(|slot| matches!(slot.state, SlotState::Active(_)))
    }

    pub(super) fn get(&self, index: OutputIndex) -> Option<&Slot> {
        self.slots.get(&index)
    }

    pub(super) fn drain_output(&mut self) -> impl Iterator<Item = OutputItem> + '_ {
        self.slots.sort_keys();
        self.indexes_by_id.clear();
        self.slots.drain(..).filter_map(|(_, slot)| slot.state.finalize())
    }

    /// Resolves without mutating either map; a rejected event cannot bind an ID.
    fn resolve(
        &self,
        identity: ItemIdentity<'_>,
        action: SlotAction,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let index_only_shell_command = identity.item_type == SSEItemType::ShellCall && action == SlotAction::Mutate;
        if validation == Validation::Strict
            && (identity.index.is_none() || (identity.item_id.is_none() && !index_only_shell_command))
        {
            return Err(invalid("upstream item requires 'output_index' and a non-empty item ID"));
        }
        let by_id = identity.item_id.and_then(|id| self.indexes_by_id.get(id)).copied();
        if let (Some(index), Some(known)) = (identity.index, by_id)
            && index != known
        {
            return Err(identity_mismatch());
        }
        let index = if let Some(index) = identity.index.or(by_id) {
            index
        } else {
            // Without an index, an unknown ID cannot identify an anonymous
            // slot. Do not guess by kind or silently create a duplicate.
            if action != SlotAction::Open
                && self.slots.values().any(|slot| {
                    slot.state.item_type() == Some(identity.item_type)
                        && (slot.item_id.is_none() || identity.item_id.is_none())
                })
            {
                return Err(invalid("upstream item has ambiguous identity without 'output_index'"));
            }
            if action == SlotAction::Mutate {
                return Ok(None);
            }
            self.unused_index()?
        };
        let Some(slot) = self.slots.get(&index) else {
            return match action {
                SlotAction::Open => Ok(Some(index)),
                SlotAction::Complete if validation == Validation::Lenient => Ok(Some(index)),
                _ if validation == Validation::Strict => Err(no_active_item()),
                _ => Ok(None),
            };
        };
        if action == SlotAction::Open {
            return Err(invalid(
                "upstream stream repeats output item or reuses its output_index",
            ));
        }
        if let (Some(bound), Some(supplied)) = (slot.item_id.as_deref(), identity.item_id)
            && bound != supplied
        {
            return Err(identity_mismatch());
        }
        if slot.state.item_type() != Some(identity.item_type) {
            return if action == SlotAction::Mutate && validation == Validation::Lenient {
                Ok(None)
            } else {
                Err(identity_mismatch())
            };
        }
        if matches!(slot.state, SlotState::Done(_))
            && (action != SlotAction::Complete || validation == Validation::Strict)
        {
            return if validation == Validation::Strict {
                Err(no_active_item())
            } else {
                Ok(None)
            };
        }
        Ok(Some(index))
    }

    fn unused_index(&self) -> ExecutorResult<OutputIndex> {
        (0..=u32::MAX)
            .map(OutputIndex)
            .find(|index| !self.slots.contains_key(index))
            .ok_or_else(|| invalid("upstream stream exhausted output indexes"))
    }

    fn insert(&mut self, index: OutputIndex, item_id: Option<&str>, state: SlotState) {
        if let Some(id) = item_id {
            self.indexes_by_id.insert(id.to_owned(), index);
        }
        self.slots.insert(
            index,
            Slot {
                item_id: item_id.map(str::to_owned),
                state,
            },
        );
    }

    fn bind_id(&mut self, index: OutputIndex, item_id: Option<&str>) {
        if let Some(id) = item_id
            && let Some(slot) = self.slots.get_mut(&index)
            && slot.item_id.is_none()
        {
            slot.item_id = Some(id.to_owned());
            self.indexes_by_id.insert(id.to_owned(), index);
        }
    }

    pub(super) fn open(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Open, validation)? else {
            return Ok(None);
        };
        if let Some(item) = ActiveItem::from_added(payload) {
            self.insert(index, identity.item_id, SlotState::Active(item));
            return Ok(Some(index));
        }
        Ok(None)
    }

    pub(super) fn apply(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Mutate, validation)? else {
            return Ok(None);
        };
        if let Some(Slot {
            state: SlotState::Active(item),
            ..
        }) = self.slots.get_mut(&index)
        {
            item.apply_event(payload)?;
        }
        self.bind_id(index, identity.item_id);
        Ok(Some(index))
    }

    pub(super) fn complete(
        &mut self,
        identity: ItemIdentity<'_>,
        payload: &EventPayload,
        validated_done_item: Option<&OutputItem>,
        validation: Validation,
    ) -> ExecutorResult<Option<OutputIndex>> {
        let Some(index) = self.resolve(identity, SlotAction::Complete, validation)? else {
            return Ok(None);
        };
        let EventPayload::OutputItemDone { item: raw_item, .. } = payload else {
            return Ok(None);
        };
        let parsed = validated_done_item.cloned().or_else(|| {
            deserialize_from_value_opt::<OutputItem>(raw_item.clone()).or_else(|| {
                (identity.item_type == SSEItemType::Reasoning)
                    .then(|| ReasoningOutput::try_from(payload).ok().map(OutputItem::Reasoning))
                    .flatten()
            })
        });
        if let Some(slot) = self.slots.get(&index) {
            if let SlotState::Active(ActiveItem::ShellCall {
                item,
                command_stream: Some(done),
                ..
            }) = &slot.state
                && (done.iter().any(|complete| !complete)
                    || !matches!(parsed.as_ref(), Some(OutputItem::ShellCall(completed))
                        if completed.action.commands == item.action.commands))
            {
                return Err(invalid("shell item done has unfinished or contradictory commands"));
            }
            if let SlotState::Done(previous) = &slot.state
                && parsed
                    .as_ref()
                    .is_some_and(|candidate| semantically_equal(previous, candidate))
            {
                self.bind_id(index, identity.item_id);
                return Ok(None);
            }
            let candidate = slot.state.completion_candidate(
                payload,
                parsed.as_ref(),
                slot.item_id.as_deref().or(identity.item_id).unwrap_or(""),
            );
            if let SlotState::Done(previous) = &slot.state {
                if candidate
                    .as_ref()
                    .is_some_and(|candidate| semantically_equal(previous, candidate))
                {
                    self.bind_id(index, identity.item_id);
                    return Ok(None);
                }
                return Err(invalid(format!(
                    "upstream stream contains conflicting repeated output item.done for output[{}]",
                    index.get()
                )));
            }
            if let Some(item) = candidate {
                self.bind_id(index, identity.item_id);
                if let Some(slot) = self.slots.get_mut(&index) {
                    slot.state = SlotState::Done(item);
                }
                return Ok(Some(index));
            }
            return Ok(None);
        }
        if let Some(
            mut item @ (OutputItem::Reasoning(_)
            | OutputItem::FunctionCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::McpCall(_)
            | OutputItem::McpListTools(_)
            | OutputItem::Compaction(_)),
        ) = parsed
        {
            if let OutputItem::WebSearchCall(call) = &mut item
                && call.id.is_empty()
            {
                call.id = uuid7_str("ws_");
            }
            self.insert(index, identity.item_id, SlotState::Done(item));
            return Ok(Some(index));
        }
        Ok(None)
    }
}

fn invalid(message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}

fn identity_mismatch() -> ExecutorError {
    invalid("upstream stream event does not match its active output item: inconsistent item ID, output_index, or kind")
}

fn no_active_item() -> ExecutorError {
    invalid("upstream stream event has no active output item")
}

fn semantically_equal(left: &OutputItem, right: &OutputItem) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Tracks a single output item currently being streamed, together with its
/// accumulated text/arguments buffer.
#[derive(Clone)]
pub(super) enum ActiveItem {
    Message {
        item: OutputMessage,
        text: String,
    },
    Reasoning {
        item: ReasoningOutput,
    },
    FunctionCall {
        item: FunctionToolCall,
        arguments: String,
    },
    ToolSearchCall {
        item: ToolSearchCall,
    },
    CustomToolCall {
        item: CustomToolCall,
        input: String,
    },
    ShellCall {
        item: ShellCall,
        command_stream: Option<Vec<bool>>,
        command: String,
    },
    WebSearchCall {
        item: Option<WebSearchCall>,
    },
    McpCall {
        item: McpCall,
    },
    McpListTools {
        item: McpListTools,
    },
    Compaction {
        item: CompactionItem,
    },
}

impl std::fmt::Debug for ActiveItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { .. } => write!(f, "ActiveItem::Message {{ .. }}"),
            Self::Reasoning { .. } => write!(f, "ActiveItem::Reasoning {{ .. }}"),
            Self::FunctionCall { .. } => write!(f, "ActiveItem::FunctionCall {{ .. }}"),
            Self::ToolSearchCall { .. } => write!(f, "ActiveItem::ToolSearchCall {{ .. }}"),
            Self::CustomToolCall { .. } => write!(f, "ActiveItem::CustomToolCall {{ .. }}"),
            Self::ShellCall { .. } => write!(f, "ActiveItem::ShellCall {{ .. }}"),
            Self::WebSearchCall { .. } => write!(f, "ActiveItem::WebSearchCall {{ .. }}"),
            Self::McpCall { .. } => write!(f, "ActiveItem::McpCall {{ .. }}"),
            Self::McpListTools { .. } => write!(f, "ActiveItem::McpListTools {{ .. }}"),
            Self::Compaction { .. } => write!(f, "ActiveItem::Compaction {{ .. }}"),
        }
    }
}

impl ActiveItem {
    fn from_added(payload: &EventPayload) -> Option<Self> {
        let EventPayload::OutputItemAdded { item_type, .. } = payload else {
            return None;
        };
        match item_type {
            SSEItemType::ShellCall => ShellCall::try_from(payload).ok().map(|item| Self::ShellCall {
                item,
                command_stream: None,
                command: String::new(),
            }),
            SSEItemType::Reasoning => ReasoningOutput::try_from(payload)
                .ok()
                .map(|item| ActiveItem::Reasoning { item }),
            SSEItemType::FunctionCall => {
                FunctionToolCall::try_from(payload)
                    .ok()
                    .map(|item| ActiveItem::FunctionCall {
                        item,
                        arguments: String::with_capacity(128),
                    })
            }
            SSEItemType::ToolSearchCall => ToolSearchCall::try_from(payload)
                .ok()
                .map(|item| ActiveItem::ToolSearchCall { item }),
            SSEItemType::CustomToolCall => {
                CustomToolCall::try_from(payload)
                    .ok()
                    .map(|item| ActiveItem::CustomToolCall {
                        item,
                        input: String::with_capacity(256),
                    })
            }
            SSEItemType::Message => OutputMessage::try_from(payload).ok().map(|item| ActiveItem::Message {
                item,
                text: String::with_capacity(256),
            }),
            SSEItemType::WebSearchCall => Some(ActiveItem::WebSearchCall { item: None }),
            SSEItemType::Compaction => CompactionItem::try_from(payload)
                .ok()
                .map(|item| ActiveItem::Compaction { item }),
            SSEItemType::McpCall => McpCall::try_from(payload).ok().map(|item| ActiveItem::McpCall { item }),
            SSEItemType::McpListTools => McpListTools::try_from(payload)
                .ok()
                .map(|item| ActiveItem::McpListTools { item }),
        }
    }

    pub(super) fn item_type(&self) -> SSEItemType {
        match self {
            Self::Message { .. } => SSEItemType::Message,
            Self::Reasoning { .. } => SSEItemType::Reasoning,
            Self::FunctionCall { .. } => SSEItemType::FunctionCall,
            Self::ToolSearchCall { .. } => SSEItemType::ToolSearchCall,
            Self::CustomToolCall { .. } => SSEItemType::CustomToolCall,
            Self::ShellCall { .. } => SSEItemType::ShellCall,
            Self::WebSearchCall { .. } => SSEItemType::WebSearchCall,
            Self::McpCall { .. } => SSEItemType::McpCall,
            Self::McpListTools { .. } => SSEItemType::McpListTools,
            Self::Compaction { .. } => SSEItemType::Compaction,
        }
    }

    // A temporary candidate for comparing a repeated completion. The retained
    // slot remains Done, and never regains mutable streaming buffers.
    fn from_completed(item: OutputItem) -> Option<Self> {
        Some(match item {
            OutputItem::ShellCall(item) => Self::ShellCall {
                item,
                command_stream: None,
                command: String::new(),
            },
            OutputItem::Message(item) => Self::Message {
                item,
                text: String::new(),
            },
            OutputItem::Reasoning(item) => Self::Reasoning { item },
            OutputItem::FunctionCall(item) => Self::FunctionCall {
                item,
                arguments: String::new(),
            },
            OutputItem::ToolSearchCall(item) => Self::ToolSearchCall { item },
            OutputItem::CustomToolCall(item) => Self::CustomToolCall {
                item,
                input: String::new(),
            },
            OutputItem::WebSearchCall(item) => Self::WebSearchCall { item: Some(item) },
            OutputItem::McpCall(item) => Self::McpCall { item },
            OutputItem::McpListTools(item) => Self::McpListTools { item },
            OutputItem::Compaction(item) => Self::Compaction { item },
            OutputItem::Unknown => return None,
        })
    }

    /// Folds a resolved event; the accumulator checks identity and lifecycle first.
    pub(super) fn apply_event(&mut self, payload: &EventPayload) -> ExecutorResult<()> {
        match self {
            Self::ShellCall {
                item,
                command_stream,
                command: buffer,
            } => {
                if let EventPayload::ShellCallCommand {
                    command_index, update, ..
                } = payload
                {
                    let index = *command_index as usize;
                    let done = command_stream.as_deref().unwrap_or_default();
                    match update {
                        ShellCommandUpdate::Added(command) => {
                            if index != done.len()
                                || item.action.commands.len() != done.len()
                                || done.last() == Some(&false)
                            {
                                return Err(invalid("shell command added out of order"));
                            }
                            item.action.commands.push(String::new());
                            buffer.clone_from(command);
                            command_stream.get_or_insert_with(Vec::new).push(false);
                        }
                        ShellCommandUpdate::Delta(delta) => {
                            if done.get(index) != Some(&false) {
                                return Err(invalid("shell command delta has no active command"));
                            }
                            buffer.push_str(delta);
                        }
                        ShellCommandUpdate::Done(command) => {
                            if done.get(index) != Some(&false) || *buffer != *command {
                                return Err(invalid(
                                    "shell command done is repeated or contradicts streamed command",
                                ));
                            }
                            item.apply_done(payload, buffer);
                            command_stream.as_mut().expect("active command stream")[index] = true;
                        }
                    }
                }
            }
            Self::Message { text, .. } => {
                if let EventPayload::TextDelta { delta, .. } = payload {
                    text.push_str(delta);
                }
            }
            Self::Reasoning { item } => {
                if matches!(
                    payload,
                    EventPayload::ReasoningTextDone { .. } | EventPayload::ReasoningSummaryTextDone { .. }
                ) {
                    item.apply_done(payload, &mut String::new());
                }
            }
            Self::FunctionCall { item, arguments } => match payload {
                EventPayload::FunctionCallArgsDelta { delta, .. } => arguments.push_str(delta),
                EventPayload::FunctionCallArgsDone { .. } => item.apply_done(payload, arguments),
                _ => {}
            },
            Self::CustomToolCall { item, input } => match payload {
                EventPayload::CustomToolCallInputDelta { delta, .. } => input.push_str(delta),
                EventPayload::CustomToolCallInputDone { .. } => item.apply_done(payload, input),
                _ => {}
            },
            Self::ToolSearchCall { .. }
            | Self::WebSearchCall { .. }
            | Self::McpCall { .. }
            | Self::McpListTools { .. }
            | Self::Compaction { .. } => {}
        }
        Ok(())
    }

    pub(super) fn finalize(self) -> Option<OutputItem> {
        match self {
            Self::ShellCall { item, .. } => Some(OutputItem::ShellCall(item)),
            Self::Reasoning { item } => Some(OutputItem::Reasoning(item)),
            Self::FunctionCall { mut item, arguments } => {
                if !arguments.is_empty() && item.arguments.is_empty() {
                    item.arguments = arguments;
                }
                item.status = MessageStatus::Completed;
                Some(OutputItem::FunctionCall(item))
            }
            Self::ToolSearchCall { item } => Some(OutputItem::ToolSearchCall(item)),
            Self::Message { mut item, text } => {
                if !text.is_empty() {
                    item.content.push(OutputTextContent::new(text));
                }
                item.status = MessageStatus::Completed;
                Some(OutputItem::Message(item))
            }
            Self::CustomToolCall { mut item, input } => {
                if item.input.is_empty() {
                    item.input = input;
                }
                item.status = Some(MessageStatus::Completed);
                Some(OutputItem::CustomToolCall(item))
            }
            Self::WebSearchCall { item } => item.map(OutputItem::WebSearchCall),
            Self::McpCall { item } => Some(OutputItem::McpCall(item)),
            Self::McpListTools { item } => Some(OutputItem::McpListTools(item)),
            Self::Compaction { item } => Some(OutputItem::Compaction(item)),
        }
    }
}

#[derive(Debug)]
pub(super) struct Slot {
    pub(super) item_id: Option<String>,
    pub(super) state: SlotState,
}

/// A completed item owns no streaming buffers and cannot accept further deltas.
pub(super) enum SlotState {
    Active(ActiveItem),
    Done(OutputItem),
}

impl std::fmt::Debug for SlotState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active(item) => f.debug_tuple("Active").field(item).finish(),
            Self::Done(item) => f.debug_tuple("Done").field(&SSEItemType::try_from(item).ok()).finish(),
        }
    }
}

impl SlotState {
    pub(super) fn item_type(&self) -> Option<SSEItemType> {
        match self {
            Self::Active(item) => Some(item.item_type()),
            Self::Done(item) => SSEItemType::try_from(item).ok(),
        }
    }

    pub(super) fn done_item(&self) -> Option<&OutputItem> {
        match self {
            Self::Active(_) => None,
            Self::Done(item) => Some(item),
        }
    }

    pub(super) fn finalize(self) -> Option<OutputItem> {
        match self {
            Self::Active(item) => item.finalize(),
            Self::Done(item) => Some(item),
        }
    }

    /// Resolves authoritative completion with the same per-kind fallbacks for
    /// an active item and a repeat of a retained completed item.
    pub(super) fn completion_candidate(
        &self,
        payload: &EventPayload,
        done_item: Option<&OutputItem>,
        item_id: &str,
    ) -> Option<OutputItem> {
        let mut candidate = match self {
            Self::Active(item) => item.clone(),
            Self::Done(item) => ActiveItem::from_completed(item.clone())?,
        };
        apply_output_item_done(&mut candidate, payload, done_item, item_id);
        candidate.finalize()
    }
}

fn apply_output_item_done(
    active: &mut ActiveItem,
    payload: &EventPayload,
    done_item: Option<&OutputItem>,
    item_id: &str,
) {
    match (active, done_item) {
        (ActiveItem::ShellCall { item, .. }, Some(OutputItem::ShellCall(done))) => item.merge_done(done, ()),
        (ActiveItem::Message { item, text }, Some(OutputItem::Message(done))) => item.merge_done(done, text),
        (ActiveItem::Reasoning { item }, Some(OutputItem::Reasoning(done))) => item.merge_done(done, payload),
        (ActiveItem::FunctionCall { item, arguments }, Some(OutputItem::FunctionCall(done))) => {
            item.merge_done(done, arguments);
        }
        (ActiveItem::ToolSearchCall { item }, Some(OutputItem::ToolSearchCall(done))) => item.merge_done(done, ()),
        (ActiveItem::CustomToolCall { item, input }, Some(OutputItem::CustomToolCall(done))) => {
            item.merge_done(done, input);
        }
        (ActiveItem::WebSearchCall { item }, Some(OutputItem::WebSearchCall(done))) => item.merge_done(done, item_id),
        (ActiveItem::McpCall { item }, Some(OutputItem::McpCall(done))) => item.merge_done(done, ()),
        (ActiveItem::McpListTools { item }, Some(OutputItem::McpListTools(done))) => item.merge_done(done, ()),
        (ActiveItem::Compaction { item }, Some(OutputItem::Compaction(done))) => item.merge_done(done, ()),
        (ActiveItem::ShellCall { item, command, .. }, None) => item.apply_done(payload, command),
        (ActiveItem::Reasoning { item }, None) => item.apply_done(payload, &mut String::new()),
        (ActiveItem::FunctionCall { item, arguments }, None) => item.apply_done(payload, arguments),
        (ActiveItem::ToolSearchCall { item }, None) => item.apply_done(payload, &mut String::new()),
        (ActiveItem::CustomToolCall { item, input }, None) => item.apply_done(payload, input),
        (ActiveItem::McpCall { item }, None) => item.apply_done(payload, &mut String::new()),
        (ActiveItem::McpListTools { item }, None) => item.apply_done(payload, &mut String::new()),
        (ActiveItem::Compaction { item }, None) => item.apply_done(payload, &mut String::new()),
        _ => {}
    }
}
