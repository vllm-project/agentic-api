//! Step 1 of the conversation pipeline — history rehydration.
//!
//! Builds a [`RequestContext`] by loading prior turns from storage and
//! injecting them into the enriched request before it is forwarded to the LLM.

use std::convert::identity;
use std::mem;
use std::sync::Arc;

use futures::future::OptionFuture;

use super::session::{ResponseContinuation, ResponseSession};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::{CheckpointLimits, ValidatedTreeCheckpoint};
use crate::executor::pending_calls::CallLedger;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::telemetry::metrics::Stage;
use crate::storage::{ResponseCheckpoint, ResponseMetadata};
use crate::types::io::InOutItem;
use crate::types::io::input::{is_search_item, retain_window};
use crate::types::io::{InputItem, ResponsesInput, resolve_tool_choice, resolve_tools};
use crate::types::request_response::RequestPayload;
use crate::utils::uuid7_str;

/// Step 1 — Build [`RequestContext`] by rehydrating conversation history.
///
/// `request` is moved into the context as `enriched_request`; one clone is taken
/// for `original_request` so the engine retains an unmodified copy for persistence
/// and ID resolution.
///
/// Dispatches based on the supplied history source, independently of `store`:
/// - `previous_response_id`: rehydrate from the prior response checkpoint
/// - `conversation`:         rehydrate from the conversation
/// - no ids:                 normalize the complete history supplied by the client
///
/// # Errors
/// Returns [`ExecutorError`] if storage is unavailable, a referenced ID does not exist, or the
/// new/resolved input contains unsupported message content.
pub async fn rehydrate_conversation(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<RequestContext> {
    rehydrate_with_continuation(request, exec_ctx, None).await
}

/// Resolve one turn in a transient session without changing the durable Conversations API.
///
/// # Errors
/// Returns an error when the session is busy/closed or the requested history is unavailable.
pub async fn rehydrate_in_session(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
    session: &ResponseSession,
) -> ExecutorResult<RequestContext> {
    let continuation = session.begin(request.previous_response_id.as_deref())?;
    rehydrate_with_continuation(request, exec_ctx, Some(continuation)).await
}

#[tracing::instrument(name = "agentic.rehydrate", skip_all, fields(
    agentic.rehydrate.source = super::telemetry::stages::StateSource::from_request(&request).as_str()
))]
pub(crate) async fn rehydrate_with_continuation(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
    continuation: Option<ResponseContinuation>,
) -> ExecutorResult<RequestContext> {
    let timer = exec_ctx.metrics.stage(Stage::Rehydrate);
    // Keep the large request future off the caller's stack.
    let rehydrated = Box::pin(async move {
        // Every typed entry point reaches this check: executor-routed HTTP, WebSocket, split execution, embedders.
        request.ensure_supported_parameters(
            exec_ctx.model_capabilities.for_model(&request.model),
            exec_ctx.responses_config.prompt_cache_options_supported,
        )?;
        validate_multi_agent_request(&request)?;
        // Stored turns never supply `max_tool_calls`, so the request value is final here.
        request.max_tool_calls_limit()?;
        // Fail before storage work for explicitly declared tools and new content;
        // check again once stored effective settings and history are resolved.
        exec_ctx
            .gateway_executors
            .validate_declarations(request.tool_declarations().as_deref())?;
        request.input.validate()?;
        let response_id = uuid7_str("resp_");
        // Persistence keeps the public items. Tool lowering belongs to the enriched
        // inference copy, including when a later turn loads these items from storage.
        let new_input_items = request.input.clone().into_canonical_items();

        // One clone for the unmodified original; `request` is moved as enriched_request.
        let original_request = request.clone();
        let mut ctx = RequestContext {
            multi_agent_tree: None,
            enriched_request: request,
            original_request,
            new_input_items,
            response_id,
            conversation_id: None,
            checkpoint: None,
            continuation,
        };

        let conversation = ctx.original_request.conversation.is_some();
        let previous = ctx.original_request.previous_response_id.is_some();
        if conversation && previous {
            return Err(ExecutorError::InvalidRequest(
                "provide only one of conversation or previous_response_id".into(),
            ));
        }
        let snapshot = OptionFuture::from(conversation.then(|| exec_ctx.conv_handler.rehydrate(&ctx)))
            .await
            .transpose()?;
        match exec_ctx.resp_handler.rehydrate(&mut ctx, snapshot).await? {
            Some(checkpoint) if conversation => from_conversation(&mut ctx, checkpoint, exec_ctx)?,
            loaded if previous => from_response(&mut ctx, loaded, exec_ctx)?,
            _ => from_responses(&mut ctx)?,
        }

        // Stateless multi-agent runs use the existing automatic per-agent compactor.
        // The explicit trigger extension cannot return a resumable stateless tree.
        if !ctx.original_request.store
            && ctx.original_request.input.has_compaction_trigger()
            && ctx
                .enriched_request
                .multi_agent
                .as_ref()
                .is_some_and(|config| config.enabled)
        {
            return Err(ExecutorError::InvalidRequest(
                "compaction_trigger is not supported with multi_agent and store: false; use automatic compaction"
                    .into(),
            ));
        }
        exec_ctx
            .gateway_executors
            .apply_configured_mcp_defaults(ctx.enriched_request.tools.as_deref_mut());
        exec_ctx
            .gateway_executors
            .validate_declarations(ctx.enriched_request.tool_declarations().as_deref())?;
        ctx.enriched_request.input.validate()?;
        validate_multi_agent_request(&ctx.enriched_request)?;
        Ok(ctx)
    })
    .await;
    timer.finish_result(&rehydrated);
    rehydrated
}

/// Use the submitted Responses history without accessing response or conversation storage.
fn from_responses(ctx: &mut RequestContext) -> ExecutorResult<()> {
    let items = InOutItem::into_input_items(ctx.new_input_items.clone());
    if items.iter().any(is_search_item) {
        CallLedger::scan(&items)?.require_answered()?;
    }
    ctx.enriched_request.input = ResponsesInput::from(items);
    Ok(())
}

fn validate_multi_agent_request(request: &RequestPayload) -> ExecutorResult<()> {
    let Some(config) = request.multi_agent.as_ref().filter(|config| config.enabled) else {
        return Ok(());
    };
    if config.max_concurrent_subagents == Some(0) {
        return Err(ExecutorError::InvalidRequest(
            "max_concurrent_subagents must be positive".into(),
        ));
    }
    if request.max_tool_calls.is_some() {
        return Err(ExecutorError::InvalidRequest(
            "max_tool_calls is not supported with multi_agent".into(),
        ));
    }
    // OpenAI rejects this combination with the same parameter and message. The gateway defaults
    // `parallel_tool_calls` to false, so only an explicit `true` conflicts.
    if request.parallel_tool_calls == Some(true) && request.declares_async_tools() {
        return Err(ExecutorError::UnsupportedValue {
            param: Some("multi_agent".into()),
            code: Some("unsupported_value"),
            message: "Async parallel tool calls are not yet supported in multi-agent mode.".into(),
        });
    }
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.summary.is_some() || reasoning.generate_summary.is_some())
    {
        return Err(ExecutorError::InvalidRequest(
            "reasoning.summary is not supported with multi_agent".into(),
        ));
    }
    Ok(())
}

/// Hydrates `ctx` from the previous response chain `rehydrate` loaded.
///
/// Inherits the stored response's effective settings and continues its history with this request's items. The new
/// response is independent of any conversation associated with its parent.
fn from_response(
    ctx: &mut RequestContext,
    loaded: Option<ResponseCheckpoint>,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<()> {
    let mut checkpoint = loaded.ok_or_else(missing_previous_response_id)?;
    if ctx.continuation.is_some() {
        return from_session_response(ctx, checkpoint, exec_ctx);
    }
    let history = mem::take(&mut checkpoint.history);
    if !restore_agent_tree(ctx, &checkpoint.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        set_model_input(ctx, history, identity)?;
        apply_effective_settings(ctx, &checkpoint.metadata);
    }
    ctx.checkpoint = Some(checkpoint);
    Ok(())
}

/// Hydrates `ctx` from the session's parent: the one it retains, or a stored parent, which it retains on first use.
fn from_session_response(
    ctx: &mut RequestContext,
    mut checkpoint: ResponseCheckpoint,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<()> {
    let continuation = ctx
        .continuation
        .as_ref()
        .ok_or_else(|| ExecutorError::InvalidRequest("session response requires a continuation lease".to_owned()))?;
    let loaded = continuation
        .parent
        .is_none()
        .then(|| continuation.retain_parent(checkpoint.clone()))
        .transpose()?;
    let history = mem::take(&mut checkpoint.history);
    if !restore_agent_tree(ctx, &checkpoint.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        set_model_input(ctx, history, retain_window)?;
        apply_effective_settings(ctx, &checkpoint.metadata);
    }
    // The model input holds the parent's history; the turn's checkpoint keeps what persistence extends.
    ctx.checkpoint = Some(checkpoint);
    if let Some((continuation, parent)) = ctx.continuation.as_mut().zip(loaded) {
        continuation.parent = Some(Arc::new(parent));
    }
    Ok(())
}

/// Response continuation runs only for a request with `previous_response_id`.
fn missing_previous_response_id() -> ExecutorError {
    ExecutorError::InvalidRequest("previous_response_id is required to continue a response".to_owned())
}

/// Hydrates `ctx` from the conversation `rehydrate` loaded.
///
/// Continues the conversation's history with this request's items. A turn references an existing conversation only:
/// an unknown id is rejected as not-found rather than created implicitly, matching the `OpenAI` Conversations
/// contract. A conversation turn does not inherit the stored settings.
fn from_conversation(
    ctx: &mut RequestContext,
    mut checkpoint: ResponseCheckpoint,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<()> {
    ctx.conversation_id.clone_from(&ctx.original_request.conversation);
    let history = mem::take(&mut checkpoint.history);
    if !restore_agent_tree(ctx, &checkpoint.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        // A turn is admitted only if it answers the history's pending call, so another turn may have answered it first.
        checkpoint.requires_latest = CallLedger::scan(&history)?.awaits_output();
        set_model_input(ctx, history, identity)?;
    }
    // A multi-agent tree is the latest turn's snapshot, not appended history: a multi-agent turn, which continues or
    // builds one, must be stored right after the turn it continued, or it would replace another turn's tree.
    checkpoint.requires_latest |= ctx
        .enriched_request
        .multi_agent
        .as_ref()
        .is_some_and(|config| config.enabled);
    ctx.checkpoint = Some(checkpoint);
    Ok(())
}

/// Makes the continued `history` followed by this request's own items the turn's model input, once no synchronous call
/// in them is left unanswered. `window` keeps what the turn continues: a session keeps the window after the latest
/// compaction, which this request's items can move.
fn set_model_input(
    ctx: &mut RequestContext,
    mut history: Vec<InputItem>,
    window: fn(Vec<InputItem>) -> Vec<InputItem>,
) -> ExecutorResult<()> {
    history.extend(InOutItem::into_input_items(ctx.new_input_items.clone()));
    let items = window(history);
    CallLedger::scan(&items)?.require_answered()?;
    ctx.enriched_request.previous_response_id = None;
    ctx.enriched_request.input = ResponsesInput::from(items);
    Ok(())
}

pub(crate) fn apply_effective_settings(ctx: &mut RequestContext, stored: &ResponseMetadata) {
    let tools_explicitly_set = ctx.original_request.tools.is_some();
    ctx.enriched_request.tools = resolve_tools(
        ctx.original_request.tools.as_deref(),
        stored.effective_tools.as_deref(),
        tools_explicitly_set,
    );
    ctx.enriched_request.tool_choice = Some(resolve_tool_choice(
        ctx.original_request.tool_choice.as_ref(),
        &stored.effective_tool_choice,
        ctx.original_request.tool_choice.is_some() || tools_explicitly_set,
    ));
}

/// Public transcript rows are never merged into any agent's private context.
/// The coordinator validates this versioned snapshot and routes new outputs.
fn restore_agent_tree(ctx: &mut RequestContext, metadata: &ResponseMetadata, max_bytes: usize) -> ExecutorResult<bool> {
    let Some(tree) = &metadata.multi_agent_tree else {
        return Ok(false);
    };
    let tree = ValidatedTreeCheckpoint::from_stored(tree.clone(), &CheckpointLimits::for_response(max_bytes))?;
    apply_effective_settings(ctx, metadata);
    apply_agent_tree(ctx, tree)?;
    Ok(true)
}

fn apply_agent_tree(ctx: &mut RequestContext, tree: ValidatedTreeCheckpoint) -> ExecutorResult<()> {
    if ctx
        .original_request
        .multi_agent
        .as_ref()
        .is_some_and(|config| !config.enabled)
    {
        return Err(ExecutorError::InvalidRequest(
            "cannot disable multi_agent when continuing an agent tree".into(),
        ));
    }
    if ctx.enriched_request.multi_agent.is_none() {
        ctx.enriched_request.multi_agent = Some(tree.snapshot().config.clone());
    }
    let root = tree
        .snapshot()
        .agents
        .first()
        .filter(|agent| agent.identity.is_root())
        .ok_or_else(|| ExecutorError::InvalidRequest("multi-agent checkpoint has no root".into()))?;
    ctx.enriched_request.input = ResponsesInput::from(root.history.clone());
    ctx.enriched_request.previous_response_id = None;
    ctx.multi_agent_tree = Some(tree);
    Ok(())
}

#[cfg(test)]
mod tests;
