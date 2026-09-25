//! Canonical inference-round ordering, independent of durable/transient storage.

use crate::executor::{RequestContext, gateway::append_gateway_calls_to_new_input};
use crate::tool::ToolRegistry;
use crate::types::{io::OutputItem, reasoning_replay::ReasoningReplayPolicy};

pub(super) fn record_round_history(
    ctx: &mut RequestContext,
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    public_output_count: usize,
    policy: ReasoningReplayPolicy,
) {
    // Opaque replay needs the same order in storage as in the live tool loop.
    // Preserve existing vLLM persistence and response-session behavior.
    let canonical = policy == ReasoningReplayPolicy::OpaqueResponses
        || (ctx.continuation.is_some() && ctx.original_request.conversation_id.is_none());
    if canonical {
        ctx.new_input_items.extend(
            output_items
                .iter()
                .filter(|item| !matches!(item, OutputItem::McpListTools(_)))
                .filter_map(OutputItem::to_input_item),
        );
        ctx.recorded_output_prefix.record_through(public_output_count);
    } else {
        append_gateway_calls_to_new_input(ctx, output_items, registry);
    }
}
