//! Non-wire bookkeeping for output already retained in canonical turn history.

use super::io::OutputItem;

/// Prefix of public output whose replay representation is already recorded.
///
/// Only engine orchestration advances this marker. External/split callers use
/// `Default`, which retains all output. It is never serialized or stored as data.
#[derive(Debug, Default, Clone, Copy)]
pub struct RecordedOutputPrefix(usize);

impl RecordedOutputPrefix {
    pub(crate) fn record_through(&mut self, output_count: usize) {
        self.0 = self.0.max(output_count);
    }

    pub(crate) fn retains_output(self, index: usize, item: &OutputItem) -> bool {
        index >= self.0 || matches!(item, OutputItem::McpListTools(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        event::MessageStatus,
        io::{McpListTools, OutputMessage},
    };

    #[test]
    fn recorded_prefix_is_monotonic_and_retains_discovery_and_new_output() {
        let mut prefix = RecordedOutputPrefix::default();
        let message = OutputItem::Message(OutputMessage::new("msg_1", MessageStatus::Completed));
        let discovery = OutputItem::McpListTools(McpListTools::new("mcp_1", "fixture", vec![]));
        assert!(prefix.retains_output(0, &message));
        prefix.record_through(4);
        prefix.record_through(2);
        assert!(!prefix.retains_output(3, &message));
        assert!(prefix.retains_output(4, &message));
        assert!(prefix.retains_output(0, &discovery));
    }
}
