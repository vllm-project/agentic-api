//! The explicit `/v1/responses/compact` operation.

use super::{compact_items_with_trigger, request_payload};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::persist::persist_prepared_turn;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::executor::request::ExecutionContext;
use crate::executor::telemetry::FailureCategory;
use crate::executor::telemetry::metrics::Stage;
use crate::executor::telemetry::stages::CompactionTrigger;
use crate::tool::ToolSearchState;
use crate::types::io::ResponsesInput;
use crate::types::request_response::{CompactRequest, CompactedResponse};
use crate::utils::common::utcnow_str;

/// Compact direct input or a stored previous-response chain into a reusable item window.
///
/// # Errors
///
/// Returns an invalid-request error when neither input nor a previous response ID is supplied,
/// and propagates history, inference, and persistence failures.
pub async fn compact_response(
    request: CompactRequest,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
) -> ExecutorResult<CompactedResponse> {
    if request.input.is_none() && request.previous_response_id.is_none() {
        return Err(ExecutorError::InvalidRequest(
            "compaction requires input or previous_response_id".to_owned(),
        ));
    }

    let mut payload = request_payload(
        request.model,
        request.input.unwrap_or_else(|| ResponsesInput::Items(Vec::new())),
        request.instructions,
    );
    payload.previous_response_id = request.previous_response_id;
    let ctx = rehydrate_conversation(payload, exec_ctx).await?;
    let (mut ctx, tool_search_state) =
        prepare_request_tools(ctx, &exec_ctx.conv_handler, &exec_ctx.resp_handler).await?;
    let tool_search_metadata = tool_search_state.map(ToolSearchState::into_public_metadata);
    let input = std::mem::replace(&mut ctx.enriched_request.input, ResponsesInput::Items(Vec::new()));
    let (output, usage) = compact_items_with_trigger(
        &ctx.enriched_request,
        input,
        exec_ctx,
        auth,
        CompactionTrigger::Explicit,
    )
    .await?;

    let response_id = ctx.response_id.clone();
    ctx.new_input_items.clone_from(&output);
    let timer = exec_ctx.metrics.stage(Stage::Persist);
    match persist_prepared_turn(
        ctx,
        tool_search_metadata,
        Vec::new(),
        &exec_ctx.conv_handler,
        &exec_ctx.resp_handler,
    )
    .await
    {
        Ok(()) => timer.finish(None),
        // Without storage nothing was written, so there is no stage to time.
        Err(ExecutorError::Storage(crate::StorageError::NotConfigured)) => timer.discard(),
        Err(error) => {
            timer.finish(Some(FailureCategory::from(&error)));
            return Err(error);
        }
    }

    Ok(CompactedResponse {
        id: response_id,
        object: "response.compaction".to_owned(),
        created_at: utcnow_str(),
        output,
        usage,
    })
}
