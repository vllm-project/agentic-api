//! Step 3 of the conversation pipeline — response persistence.
//!
//! Writes the completed response and output items to storage, routing to the
//! appropriate handler based on whether the turn belongs to a conversation.

use std::collections::HashMap;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::ValidatedTreeCheckpoint;
use crate::executor::prepare::prepare_request_tools;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::session::ResponseContinuation;
use crate::executor::telemetry::metrics::Stage;
use crate::storage::{ResponseMetadata, StorageError};
use crate::tool::{ToolSearchMetadata, ToolSearchState};
use crate::types::event::ResponseStatus;
use crate::types::io::InOutItem;
use crate::types::io::OutputItem;
use crate::types::io::input::model_items;
use crate::types::request_response::ResponsePayload;
use tracing::error;

/// What a finished turn stores beside its input items.
pub(crate) enum TurnOutput<'a> {
    /// A generated response: its public output, and its snapshot when the request is stored.
    Response(&'a ResponsePayload),
    /// A turn that generated no output, such as an empty session turn.
    Empty,
    /// A standalone compaction's window: stored whatever `store` says, so `previous_response_id` can continue it.
    Compaction,
}

/// Decides whether a completed turn reaches the storage handlers.
///
/// A `store: false` request leaves no durable row, including when it reads a
/// stored parent or explicit conversation. Response-scoped session continuations
/// still reach their handler to publish a connection-local checkpoint. Explicit
/// conversations do not publish those checkpoints or append unstored turns.
#[must_use]
pub(crate) fn should_persist(ctx: &RequestContext) -> bool {
    ctx.original_request.store || (ctx.continuation.is_some() && ctx.original_request.conversation.is_none())
}

/// Persists one finished turn: the only path from a turn to the storage handlers. A turn that is not stored, or a
/// response that did not complete, is returned unstored; only a write that reached storage is timed as the
/// `persist` stage.
///
/// # Errors
/// For a generated response, [`ExecutorError::Conflict`] and [`ExecutorError::PayloadTooLarge`] as they are and any
/// other failure logged and returned as [`ExecutorError::Persistence`]; for any other turn, the failure as it is.
pub(crate) async fn persist(
    ctx: RequestContext,
    output: TurnOutput<'_>,
    tool_search_metadata: Option<ToolSearchMetadata>,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<()> {
    if !matches!(output, TurnOutput::Compaction) && !should_persist(&ctx) {
        return Ok(());
    }
    if let TurnOutput::Response(payload) = output
        && (payload.id.is_empty()
            || !matches!(
                payload.status.parse::<ResponseStatus>().unwrap_or_default(),
                ResponseStatus::Completed | ResponseStatus::Incomplete
            ))
    {
        return Ok(());
    }
    let compaction = matches!(output, TurnOutput::Compaction);
    let generated = matches!(output, TurnOutput::Response(_));
    let timer = exec_ctx.metrics.stage(Stage::Persist);
    let persisted = Box::pin(write_turn(ctx, output, tool_search_metadata, exec_ctx)).await;
    match &persisted {
        // Without storage nothing was written, so there is no stage to time.
        Err(ExecutorError::Storage(StorageError::NotConfigured)) => timer.discard(),
        result => timer.finish_result(result),
    }
    match persisted {
        // A compaction still returns its window without storage; only continuing it later needs a store.
        Err(ExecutorError::Storage(StorageError::NotConfigured)) if compaction => Ok(()),
        // A turn that generated nothing reports the failure to its caller as it is, such as a parent that is gone.
        Err(error) if !generated => Err(error),
        Err(error @ (ExecutorError::Conflict(_) | ExecutorError::PayloadTooLarge(_))) => Err(error),
        Err(source) => {
            error!(error = ?source, "failed to persist response");
            Err(ExecutorError::Persistence(Box::new(source)))
        }
        Ok(()) => Ok(()),
    }
}

/// Persists a turn that generated no output, such as an empty session turn, after preparing its tools.
///
/// # Errors
/// As [`persist`], or an error preparing the request's tools.
pub async fn persist_turn(ctx: RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    if !should_persist(&ctx) {
        return Ok(());
    }
    let (ctx, tool_search_state) = prepare_request_tools(ctx)?;
    let tool_search_metadata = tool_search_state.map(ToolSearchState::into_public_metadata);
    persist(ctx, TurnOutput::Empty, tool_search_metadata, exec_ctx).await
}

#[tracing::instrument(name = "agentic.persist", skip_all, fields(
    agentic.persist.destination = if ctx.original_request.conversation.is_some() { "conversation" } else { "response" }
))]
async fn write_turn(
    mut ctx: RequestContext,
    output: TurnOutput<'_>,
    tool_search_metadata: Option<ToolSearchMetadata>,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<()> {
    let metadata = prepare_metadata(&mut ctx, tool_search_metadata);
    let (output_items, snapshot) = match output {
        TurnOutput::Response(payload) => (payload.output.as_slice(), ctx.original_request.store.then_some(payload)),
        TurnOutput::Empty | TurnOutput::Compaction => (&[][..], None),
    };
    if ctx.original_request.conversation.is_some() {
        let items = turn_items(&mut ctx, output_items, None);
        return exec_ctx.conv_handler.persist(&ctx, items, snapshot, &metadata).await;
    }

    // A session turn publishes a connection-local checkpoint; it is durable only when the request is stored.
    let continuation = ctx.continuation.take();
    let durable = continuation.is_none() || ctx.original_request.store;
    let mut items = turn_items(&mut ctx, output_items, continuation.as_ref());
    let checkpoint = continuation
        .as_ref()
        .map(|lease| lease.checkpoint(ctx.response_id.clone(), &metadata, &items, durable))
        .transpose()?;
    let transient_parent = continuation
        .as_ref()
        .filter(|lease| lease.parent.as_ref().is_some_and(|parent| !parent.durable));
    // A stored child of a transient parent stores the parent's full history: there is no stored parent to extend.
    if let Some(lease) = transient_parent
        && durable
    {
        items.splice(0..0, lease.parent_items().cloned().map(InOutItem::Input));
    }
    let history_item_ids = if durable {
        exec_ctx.resp_handler.persist(&ctx, items, snapshot, &metadata).await?
    } else {
        Vec::new()
    };
    if let Some((lease, checkpoint)) = continuation.zip(checkpoint) {
        lease.publish(checkpoint.with_history_item_ids(history_item_ids))?;
    }
    Ok(())
}

/// The turn's own history: the request's items, then its public output, less any output an in-turn session
/// compaction already absorbed ([`ResponseContinuation::retains_output`]).
fn turn_items(
    ctx: &mut RequestContext,
    output_items: &[OutputItem],
    continuation: Option<&ResponseContinuation>,
) -> Vec<InOutItem> {
    let mut items = std::mem::take(&mut ctx.new_input_items);
    items.extend(
        output_items
            .iter()
            .enumerate()
            .filter(|(index, item)| continuation.is_none_or(|lease| lease.retains_output(*index, item)))
            .map(|(_, item)| InOutItem::Output(item.clone())),
    );
    items
}

/// The metadata this turn stores for its continuation: the effective settings, the tool-search state, and the
/// multi-agent tree, moved out of the request context.
fn prepare_metadata(ctx: &mut RequestContext, tool_search_metadata: Option<ToolSearchMetadata>) -> ResponseMetadata {
    let (effective_tools, tool_search_loaded_tools) = match tool_search_metadata {
        Some(tool_search) => (tool_search.effective_tools, Some(tool_search.loaded_tools)),
        None => (ctx.enriched_request.tools.take(), None),
    };
    ResponseMetadata {
        multi_agent_tree: ctx.multi_agent_tree.take().map(ValidatedTreeCheckpoint::into_snapshot),
        model: std::mem::take(&mut ctx.enriched_request.model),
        previous_response_id: ctx.original_request.previous_response_id.clone(),
        effective_tools,
        tool_search_loaded_tools,
        effective_tool_choice: ctx
            .original_request
            .tool_choice
            .take()
            .or_else(|| ctx.enriched_request.tool_choice.take())
            .unwrap_or_default(),
        effective_instructions: ctx.enriched_request.instructions.take(),
    }
}

/// Stores a decoded turn for a caller that ran inference itself. A failed turn is
/// returned unstored, as the in-process flow does.
///
/// # Errors
/// [`ExecutorError::InvalidRequest`] for unusable IDs or an unfinished response,
/// [`ExecutorError::Conflict`] for an id already stored, or a storage error.
pub async fn commit(
    mut ctx: RequestContext,
    payload: ResponsePayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<ResponsePayload> {
    if ctx.response_id.is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "context has no reserved response id".to_owned(),
        ));
    }

    let status = payload.status.parse::<ResponseStatus>().unwrap_or_default();
    // Storing an unfinished turn would return an id that can never be continued.
    if status == ResponseStatus::InProgress {
        return Err(ExecutorError::InvalidRequest(format!(
            "upstream response status '{}' is not terminal",
            payload.status
        )));
    }
    if matches!(status, ResponseStatus::Completed | ResponseStatus::Incomplete) {
        // Without a session the caller's context carries no history: load the continued response once, for the
        // call-ID check and as the checkpoint this turn extends. A session keeps its parent in memory.
        let stored = match ctx.continuation {
            Some(_) => None,
            None => exec_ctx.resp_handler.rehydrate(&ctx).await?,
        };
        let history = match stored {
            Some((checkpoint, history)) => {
                ctx.checkpoint = Some(checkpoint);
                history
            }
            None => Vec::new(),
        };
        validate_output_call_ids(&ctx, &payload.output, &history)?;
    }

    Box::pin(persist(ctx, TurnOutput::Response(&payload), None, exec_ctx)).await?;
    Ok(payload)
}

fn validate_output_call_ids(
    ctx: &RequestContext,
    output_items: &[OutputItem],
    history: &[InOutItem],
) -> ExecutorResult<()> {
    let mut call_ids = HashMap::new();
    for (index, item) in output_items.iter().enumerate() {
        let (item_type, call_id) = match item {
            OutputItem::FunctionCall(call) => ("function_call", call.call_id.as_str()),
            // A hosted search has no call id to pair.
            OutputItem::ToolSearchCall(call) => match call.call_id.as_deref() {
                Some(call_id) => ("tool_search_call", call_id),
                None => continue,
            },
            OutputItem::CustomToolCall(call) => ("custom_tool_call", call.call_id.as_str()),
            OutputItem::ShellCall(call) => ("shell_call", call.call_id.as_str()),
            _ => continue,
        };
        if call_id.is_empty() {
            return Err(ExecutorError::InvalidRequest(format!(
                "upstream response output[{index}] {item_type} has no valid 'call_id'"
            )));
        }
        if let Some((first_index, _)) = call_ids.insert(call_id, (index, item_type)) {
            return Err(ExecutorError::InvalidRequest(format!(
                "upstream response output[{index}] {item_type} repeats 'call_id' from output[{first_index}]"
            )));
        }
    }
    if call_ids.is_empty() {
        return Ok(());
    }

    if ctx.continuation.is_some() {
        // Rehydration already combined the pinned parent with normalized input.
        // Validate that effective window, excluding calls superseded by compaction.
        let projected = ctx.enriched_request.input.input_items().collect();
        let items: Vec<_> = model_items(projected).collect();
        return validate_history_call_ids(items.iter().map(|item| item.call_id()), &call_ids);
    }
    validate_history_call_ids(history.iter().map(InOutItem::call_id), &call_ids)?;
    for (input_index, item) in ctx.new_input_items.iter().enumerate() {
        if let Some((output_index, item_type)) = item.call_id().and_then(|call_id| call_ids.get(call_id)) {
            return Err(ExecutorError::InvalidRequest(format!(
                "upstream response output[{output_index}] {item_type} repeats 'call_id' from request input[{input_index}]"
            )));
        }
    }
    Ok(())
}

fn validate_history_call_ids<'a>(
    history: impl Iterator<Item = Option<&'a str>>,
    call_ids: &HashMap<&str, (usize, &str)>,
) -> ExecutorResult<()> {
    for (history_index, call_id) in history.enumerate() {
        if let Some((output_index, item_type)) = call_id.and_then(|id| call_ids.get(id)) {
            return Err(ExecutorError::InvalidRequest(format!(
                "upstream response output[{output_index}] {item_type} repeats 'call_id' from continued history item[{history_index}]"
            )));
        }
    }
    Ok(())
}
