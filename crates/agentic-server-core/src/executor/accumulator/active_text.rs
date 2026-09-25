//! Message and reasoning text state with incremental retained-byte accounting.

use super::active::Budget;
use crate::events::EventPayload;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::response_budget::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedAccount, RetainedSize};
use crate::types::event::MessageStatus;
use crate::types::io::{
    ApplyDone, OutputItem, OutputMessage, OutputMessageContent, OutputTextContent, ReasoningOutput,
};
use indexmap::IndexMap;
use std::collections::HashMap;

/// A part either accepts output-text updates or retains its completed typed
/// snapshot. Input text uses the completed-only path.
#[derive(Clone, Debug)]
pub(super) enum MessagePart {
    Streaming { text: String, streamed: bool },
    Completed(OutputMessageContent),
}

impl Default for MessagePart {
    fn default() -> Self {
        Self::Streaming {
            text: String::new(),
            streamed: false,
        }
    }
}

impl RetainedSize for MessagePart {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Streaming { text, .. } => RETAINED_CONTAINER_OVERHEAD_BYTES + "output_text".len() + text.len(),
            Self::Completed(part) => part.retained_bytes(),
        }
    }
}

/// Bytes retained by per-index streamed counters that hold no text yet: one
/// container per index plus the bytes charged for its deltas.
fn streamed_counter_bytes(counters: &HashMap<u32, usize>) -> usize {
    counters
        .values()
        .map(|bytes| RETAINED_CONTAINER_OVERHEAD_BYTES + bytes)
        .sum()
}

fn streamed_part_bytes(counters: &HashMap<u32, usize>, index: u32) -> usize {
    counters
        .get(&index)
        .map_or(0, |bytes| RETAINED_CONTAINER_OVERHEAD_BYTES + bytes)
}

/// Return the streamed part for `index`, charging its container before a new
/// entry exists so an empty part cannot grow the map for free.
fn part_mut<'a>(
    parts: &'a mut IndexMap<u32, MessagePart>,
    index: u32,
    account: &mut RetainedAccount,
    budget: Budget<'_>,
) -> ExecutorResult<&'a mut MessagePart> {
    if !parts.contains_key(&index) {
        account.charge(budget, MessagePart::default().retained_bytes())?;
    }
    Ok(parts.entry(index).or_default())
}

/// Record streamed bytes for `index`, charging the container of a new index.
fn count_streamed(
    counters: &mut HashMap<u32, usize>,
    index: u32,
    delta: &str,
    account: &mut RetainedAccount,
    budget: Budget<'_>,
) -> ExecutorResult<()> {
    if !counters.contains_key(&index) {
        account.charge(budget, RETAINED_CONTAINER_OVERHEAD_BYTES)?;
    }
    account.charge(budget, delta.len())?;
    *counters.entry(index).or_default() += delta.len();
    Ok(())
}

#[derive(Clone)]
pub(super) struct MessageState {
    pub(super) item: OutputMessage,
    pub(super) parts: IndexMap<u32, MessagePart>,
}

impl MessageState {
    pub(super) fn new(mut item: OutputMessage) -> Self {
        let parts = std::mem::take(&mut item.content)
            .into_iter()
            .zip(0u32..)
            .map(|(part, index)| (index, MessagePart::Completed(part)))
            .collect();
        Self { item, parts }
    }

    pub(super) fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        match payload {
            EventPayload::TextDelta {
                delta, content_index, ..
            } => {
                let part = part_mut(&mut self.parts, *content_index, account, budget)?;
                let MessagePart::Streaming { text, streamed } = part else {
                    return Err(invalid_message("output text follows a completed message part"));
                };
                account.charge(budget, delta.len())?;
                *streamed = true;
                text.push_str(delta);
            }
            EventPayload::TextDone {
                text, content_index, ..
            } => {
                let part = part_mut(&mut self.parts, *content_index, account, budget)?;
                // A done-only part adopts the completed text; a delta-streamed
                // part already holds it and the snapshot is not charged twice.
                let MessagePart::Streaming {
                    text: retained,
                    streamed,
                } = part
                else {
                    return Err(invalid_message("output text follows a completed message part"));
                };
                if !*streamed {
                    account.charge(budget, text.len().saturating_sub(retained.len()))?;
                    retained.clone_from(text);
                }
            }
            EventPayload::MessageContentDone {
                content_index, part, ..
            } => {
                let previous = self.parts.get(content_index);
                match previous {
                    Some(MessagePart::Completed(_)) => {
                        return Err(invalid_message("message repeats a completed content part"));
                    }
                    Some(MessagePart::Streaming { text, .. }) if !matches!(part, OutputMessageContent::OutputText(done) if done.text == *text) =>
                    {
                        return Err(invalid_message("completed message part contradicts output text"));
                    }
                    _ => {}
                }
                let previous_bytes = previous.map_or(0, RetainedSize::retained_bytes);
                account.charge(budget, part.retained_bytes().saturating_sub(previous_bytes))?;
                self.parts.insert(*content_index, MessagePart::Completed(part.clone()));
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn validate_completion(&self, done: &OutputMessage) -> ExecutorResult<()> {
        if self.item.role != done.role
            || (self.item.agent.is_some() && self.item.agent != done.agent)
            || (self.item.phase.is_some() && self.item.phase != done.phase)
        {
            return Err(invalid_message(
                "message completion changes role, phase, or attribution",
            ));
        }
        // Some providers number the first content part from one. The terminal
        // snapshot contains parts in index order, without their stream indexes.
        let mut ordered_parts: Vec<_> = self.parts.iter().collect();
        ordered_parts.sort_unstable_by_key(|(index, _)| *index);
        for (position, (_, part)) in ordered_parts.into_iter().enumerate() {
            if let MessagePart::Completed(part) = part
                && done.content.get(position) != Some(part)
            {
                return Err(invalid_message(
                    "message completion contradicts a completed content part",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn finalize(mut self) -> OutputItem {
        self.parts.sort_keys();
        for (_, part) in self.parts {
            match part {
                MessagePart::Completed(part) => self.item.content.push(part),
                MessagePart::Streaming { text, .. } if !text.is_empty() => {
                    self.item.content.push(OutputTextContent::new(text).into());
                }
                MessagePart::Streaming { .. } => {}
            }
        }
        self.item.status = MessageStatus::Completed;
        OutputItem::Message(self.item)
    }
}

fn invalid_message(message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(message.to_owned())
}

impl RetainedSize for MessageState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.parts.values().map(RetainedSize::retained_bytes).sum::<usize>()
    }
}

/// Reasoning keeps per-index byte counters for streamed deltas and adopts the
/// completed text from `reasoning_text.done` / `reasoning_summary_text.done`,
/// which is the authoritative representation of each part.
#[derive(Clone)]
pub(super) struct ReasoningState {
    pub(super) item: ReasoningOutput,
    content_streamed: HashMap<u32, usize>,
    summary_streamed: HashMap<u32, usize>,
}

impl ReasoningState {
    pub(super) fn new(item: ReasoningOutput) -> Self {
        Self {
            item,
            content_streamed: HashMap::new(),
            summary_streamed: HashMap::new(),
        }
    }

    pub(super) fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        match payload {
            EventPayload::ReasoningTextDelta {
                delta, content_index, ..
            } => count_streamed(&mut self.content_streamed, *content_index, delta, account, budget),
            EventPayload::ReasoningSummaryTextDelta {
                delta, summary_index, ..
            } => count_streamed(&mut self.summary_streamed, *summary_index, delta, account, budget),
            EventPayload::ReasoningTextDone { content_index, .. } => {
                let count = self.item.content.len();
                let index = usize::try_from(*content_index).unwrap_or(usize::MAX).min(count);
                account.grow(
                    budget,
                    self,
                    |state| {
                        // ApplyDone inserts at this index (clamping sparse indexes), or
                        // retains nothing for empty text. Measure only its inserted part.
                        let inserted = if state.item.content.len() > count {
                            state.item.content[index].retained_bytes()
                        } else {
                            0
                        };
                        inserted + streamed_part_bytes(&state.content_streamed, *content_index)
                    },
                    |state| {
                        state.content_streamed.remove(content_index);
                        state.item.apply_done(payload, &mut String::new());
                    },
                )
            }
            EventPayload::ReasoningSummaryTextDone { summary_index, .. } => {
                let count = self.item.summary.len();
                let index = usize::try_from(*summary_index).unwrap_or(usize::MAX).min(count);
                account.grow(
                    budget,
                    self,
                    |state| {
                        let inserted = if state.item.summary.len() > count {
                            state.item.summary[index].retained_bytes()
                        } else {
                            0
                        };
                        inserted + streamed_part_bytes(&state.summary_streamed, *summary_index)
                    },
                    |state| {
                        state.summary_streamed.remove(summary_index);
                        state.item.apply_done(payload, &mut String::new());
                    },
                )
            }
            _ => Ok(()),
        }
    }
}

impl RetainedSize for ReasoningState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes()
            + streamed_counter_bytes(&self.content_streamed)
            + streamed_counter_bytes(&self.summary_streamed)
    }
}
