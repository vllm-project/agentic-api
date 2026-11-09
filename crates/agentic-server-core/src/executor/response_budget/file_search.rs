//! Retained size accounting for file search output items.

use super::{
    AttributeValue, FileSearchCall, FileSearchCallResult, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize, opt_len,
    sum_retained,
};

impl RetainedSize for FileSearchCallResult {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + opt_len(self.file_id.as_ref())
            + opt_len(self.filename.as_ref())
            + self.score.map_or(0, |_| 8)
            + opt_len(self.text.as_ref())
            + self.attributes.as_ref().map_or(0, |attributes| {
                attributes
                    .iter()
                    .map(|(key, value)| {
                        key.len()
                            + match value {
                                AttributeValue::String(value) => value.len(),
                                AttributeValue::Number(_) | AttributeValue::Boolean(_) => 8,
                            }
                            + RETAINED_CONTAINER_OVERHEAD_BYTES
                    })
                    .sum::<usize>()
            })
    }
}

impl RetainedSize for FileSearchCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.agent.retained_bytes()
            + self.id.len()
            + self
                .queries
                .iter()
                .map(|query| RETAINED_CONTAINER_OVERHEAD_BYTES + query.len())
                .sum::<usize>()
            + self.results.as_ref().map_or(0, sum_retained)
    }
}
