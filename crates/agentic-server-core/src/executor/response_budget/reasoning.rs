//! Retained-size measurement for typed reasoning items.

use super::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize, sum_retained};
use crate::types::io::{OpaqueReasoning, ReasoningOutput, ReasoningSummaryContent, ReasoningTextContent};

impl RetainedSize for ReasoningTextContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.text.len()
    }
}

impl RetainedSize for ReasoningSummaryContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.text.len()
    }
}

impl RetainedSize for OpaqueReasoning {
    fn retained_bytes(&self) -> usize {
        self.as_str().len()
    }
}

impl RetainedSize for ReasoningOutput {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.agent.retained_bytes()
            + self.id.len()
            + self.encrypted_content.retained_bytes()
            + sum_retained(&self.content)
            + sum_retained(&self.summary)
    }
}
