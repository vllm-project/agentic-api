//! Account for typed reasoning without materializing an untyped JSON copy.

use super::{ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS, ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS, InputTokenEstimate};
use crate::types::{ReasoningOutput, ReasoningStatus};

pub(super) fn add_reasoning(estimate: &mut InputTokenEstimate, reasoning: &ReasoningOutput) {
    estimate.add_text(&reasoning.id);
    estimate.add_optional_text(reasoning.status.map(ReasoningStatus::as_str));
    for content in &reasoning.content {
        estimate.add_tokens(ESTIMATED_CONTENT_PART_OVERHEAD_TOKENS);
        estimate.add_text(&content.text);
    }
    for summary in &reasoning.summary {
        // Preserve the previous estimate: one object, two strings, and field names.
        estimate.add_tokens(3 * ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS);
        estimate.add_text("type");
        estimate.add_text("summary_text");
        estimate.add_text("text");
        estimate.add_text(&summary.text);
    }
    if let Some(encrypted_content) = &reasoning.encrypted_content {
        estimate.add_tokens(ESTIMATED_JSON_VALUE_OVERHEAD_TOKENS);
        estimate.add_text(encrypted_content.as_str());
    }
}
