//! Logical retained-size accounting for canonical input and accepted steering.
use super::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize, opt_len};
use crate::types::io::{InputContent, InputItem, InputMessage, InputMessageContent, RefusalContent, ResponsesInput};
use crate::types::websocket::SteeringInput;

impl RetainedSize for InputContent {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::InputText(part) | Self::OutputText(part) | Self::ReasoningText(part) => part.retained_bytes(),
            Self::InputImage(part) => part.retained_bytes(),
            Self::InputFile(part) => part.retained_bytes(),
            Self::Refusal(part) => part.retained_bytes(),
            Self::Unknown(kind) => RETAINED_CONTAINER_OVERHEAD_BYTES.saturating_add(kind.len()),
        }
    }
}

impl RetainedSize for InputMessageContent {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text(text) => RETAINED_CONTAINER_OVERHEAD_BYTES.saturating_add(text.len()),
            Self::Parts(parts) => parts.iter().fold(RETAINED_CONTAINER_OVERHEAD_BYTES, |bytes, part| {
                bytes.saturating_add(part.retained_bytes())
            }),
        }
    }
}

impl RetainedSize for InputMessage {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            .saturating_add(self.agent.retained_bytes())
            .saturating_add(opt_len(self.id.as_ref()))
            .saturating_add(self.role.len())
            .saturating_add(self.content.retained_bytes())
    }
}

impl RetainedSize for SteeringInput {
    /// Measure canonical input without serialization or a second input copy.
    /// This is logical retained size, not a heap or original-wire size bound.
    /// The queue owner must separately charge IDs, records, original-input
    /// echoes and pins, and own reservations through commitment or disposal.
    /// Measuring input does not reserve capacity in a steering queue.
    fn retained_bytes(&self) -> usize {
        match self.as_ref() {
            ResponsesInput::Text(text) => RETAINED_CONTAINER_OVERHEAD_BYTES.saturating_add(text.len()),
            ResponsesInput::Items(items) => items.iter().fold(RETAINED_CONTAINER_OVERHEAD_BYTES, |bytes, item| {
                let InputItem::Message(message) = item else {
                    // SteeringInput guarantees user messages. Fail closed if
                    // that invariant changes before this accounting is updated.
                    return usize::MAX;
                };
                bytes.saturating_add(message.retained_bytes())
            }),
        }
    }
}

impl RetainedSize for RefusalContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.refusal.len()
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}
