//! Step 1 of the conversation pipeline — history rehydration.
//!
//! Builds a [`RequestContext`] by loading prior turns from storage and
//! injecting them into the enriched request before it is forwarded to the LLM.

use super::session::{ResponseCheckpoint, ResponseContinuation, ResponseSession, canonical_session_history};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::{CheckpointLimits, ValidatedTreeCheckpoint};
use crate::executor::pending_calls::first_unanswered_sync_call;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::telemetry::metrics::Stage;
use crate::storage::{ResponseMetadata, StorageError};
use crate::tool::ToolError;
use crate::types::io::InOutItem;
use crate::types::io::input::is_search_item;
use crate::types::io::{
    InputItem, OutputItem, ReasoningOutput, ReasoningTextContent, ResponsesInput, resolve_tool_choice, resolve_tools,
};
use crate::types::request_response::RequestPayload;
use crate::utils::uuid7_str;

fn has_plaintext_reasoning(reasoning: &ReasoningOutput) -> bool {
    reasoning.content.iter().any(|content| !content.text.is_empty())
}

fn has_opaque_reasoning_state(reasoning: &ReasoningOutput) -> bool {
    reasoning.encrypted_content.is_some()
}

/// Reject opaque reasoning that vLLM cannot replay before any normal inference call.
pub(super) fn validate_reasoning_for_vllm(input: &ResponsesInput) -> ExecutorResult<()> {
    let ResponsesInput::Items(items) = input else {
        return Ok(());
    };

    if items.iter().any(|item| {
        matches!(item, InOutItem::Input(InputItem::Reasoning(reasoning)) | InOutItem::Output(OutputItem::Reasoning(reasoning)) if has_opaque_reasoning_state(reasoning) && !has_plaintext_reasoning(reasoning))
    }) {
        return Err(ExecutorError::InvalidRequest(
            "reasoning item contains encrypted state without plaintext reasoning content and cannot be replayed to vLLM"
                .to_owned(),
        ));
    }

    Ok(())
}

/// Prepare reasoning in the vLLM-bound request copy.
///
/// vLLM can replay plaintext reasoning content but cannot interpret opaque provider state. Its generic Responses
/// conversion reads only the first reasoning content part and falls back to a summary when content is absent, while
/// its Harmony conversion joins all content parts. Normalize usable plaintext into one ordered, newline-delimited part
/// and remove summaries so both paths receive the same continuation state. Summary-only items have no usable vLLM
/// state and are omitted. [`RequestContext`] keeps the original request and new input items separately, so none of
/// these changes mutate persisted state.
pub(super) fn prepare_reasoning_for_vllm(input: &mut ResponsesInput) -> ExecutorResult<()> {
    // Validate the complete input before mutation so an error never leaves a partially prepared request behind.
    validate_reasoning_for_vllm(input)?;

    let ResponsesInput::Items(items) = input else {
        return Ok(());
    };

    items.retain_mut(|item| {
        let (InOutItem::Input(InputItem::Reasoning(reasoning)) | InOutItem::Output(OutputItem::Reasoning(reasoning))) =
            item
        else {
            return true;
        };
        if !has_plaintext_reasoning(reasoning) {
            return false;
        }

        let plaintext = reasoning
            .content
            .iter()
            .map(|content| content.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        reasoning.content = vec![ReasoningTextContent::new(plaintext)];
        reasoning.summary.clear();
        reasoning.encrypted_content = None;
        true
    });
    Ok(())
}

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
/// Returns [`ExecutorError`] if storage is unavailable, a referenced ID does not exist,
/// or the new/resolved input contains unsupported message files.
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
    // Boxed so this wrapper does not hold the request payload twice.
    let rehydrated = Box::pin(rehydrate_request(request, exec_ctx, continuation)).await;
    timer.finish_result(&rehydrated);
    rehydrated
}

async fn rehydrate_request(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
    continuation: Option<ResponseContinuation>,
) -> ExecutorResult<RequestContext> {
    request.validate_prompt_cache_options(exec_ctx.responses_config.prompt_cache_options_supported)?;
    // Every typed entry point reaches this check: executor-routed HTTP, WebSocket, split execution, embedders.
    request.ensure_supported_parameters()?;
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
    let mut new_input_items = request.input.clone().into_canonical_items();
    exec_ctx.resp_handler.resolve_input_items(&mut new_input_items).await?;

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

    match (
        ctx.original_request.conversation.is_some(),
        ctx.original_request.previous_response_id.is_some(),
    ) {
        (true, true) => {
            return Err(ExecutorError::InvalidRequest(
                "provide only one of conversation or previous_response_id".into(),
            ));
        }
        (true, false) => from_conversation(&mut ctx, exec_ctx).await?,
        (false, true) => from_response(&mut ctx, exec_ctx).await?,
        (false, false) => from_responses(&mut ctx)?,
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
            "compaction_trigger is not supported with multi_agent and store: false; use automatic compaction".into(),
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
}

/// Use the submitted Responses history without accessing response or conversation storage.
fn from_responses(ctx: &mut RequestContext) -> ExecutorResult<()> {
    let items = InOutItem::into_input_items(ctx.new_input_items.clone());
    if items.iter().any(is_search_item) {
        reject_unanswered_sync_call(&items)?;
    }
    ctx.enriched_request.input = ResponsesInput::from(items);
    Ok(())
}

/// Refuses a history whose synchronous call has no output yet; the pending-call ledger also rejects a duplicate
/// call id, an output with no call, and an output for a call of another kind.
fn reject_unanswered_sync_call(items: &[InputItem]) -> ExecutorResult<()> {
    match first_unanswered_sync_call(items)? {
        Some(pending) => Err(ExecutorError::Tool(ToolError::MissingOutput {
            call_id: pending.call_id,
        })),
        None => Ok(()),
    }
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
            param: "multi_agent".into(),
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

/// Hydrates `ctx` from the previous response chain.
///
/// Loads the stored response and its history, inherits its effective settings, and continues its history with this
/// request's items. The new response is independent of any conversation associated with its parent.
async fn from_response(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    if ctx.continuation.is_some() {
        return from_session_response(ctx, exec_ctx).await;
    }
    let (stored, history) = exec_ctx
        .resp_handler
        .rehydrate(ctx)
        .await?
        .ok_or_else(missing_previous_response_id)?;
    let checkpoint = ResponseCheckpoint {
        response_id: stored.response_id,
        history: Vec::new(),
        metadata: stored.metadata,
        durable: true,
    };
    if !restore_agent_tree(ctx, &checkpoint.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        let items = with_request_items(ctx, InOutItem::into_input_items(history));
        set_model_input(ctx, items)?;
        apply_effective_settings(ctx, &checkpoint.metadata);
    }
    ctx.checkpoint = Some(checkpoint);
    Ok(())
}

/// Hydrates `ctx` from the session's retained parent, loading and retaining a stored parent on first use.
async fn from_session_response(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let cached = ctx
        .continuation
        .as_ref()
        .and_then(|continuation| continuation.parent.clone());
    let parent = if let Some(parent) = cached {
        parent
    } else {
        let previous_id = ctx.original_request.previous_response_id.as_deref().unwrap_or_default();
        if !ctx.original_request.store {
            return Err(StorageError::PreviousResponseNotFound {
                id: previous_id.to_owned(),
            }
            .into());
        }
        let (stored, history) = exec_ctx
            .resp_handler
            .rehydrate(ctx)
            .await?
            .ok_or_else(missing_previous_response_id)?;
        let checkpoint = ResponseCheckpoint {
            response_id: stored.response_id,
            history: canonical_session_history(InOutItem::into_input_items(history)),
            metadata: stored.metadata,
            durable: true,
        };
        let continuation = ctx.continuation.as_ref().ok_or_else(|| {
            ExecutorError::InvalidRequest("session response requires a continuation lease".to_owned())
        })?;
        std::sync::Arc::new(continuation.retain_parent(checkpoint)?)
    };
    // The model input holds the parent's history; the turn's checkpoint keeps the rest.
    ctx.checkpoint = Some(ResponseCheckpoint {
        response_id: parent.response_id.clone(),
        history: Vec::new(),
        metadata: parent.metadata.clone(),
        durable: parent.durable,
    });
    if !restore_agent_tree(ctx, &parent.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        let items = canonical_session_history(with_request_items(ctx, parent.history.clone()));
        set_model_input(ctx, items)?;
        apply_effective_settings(ctx, &parent.metadata);
    }
    if let Some(continuation) = ctx.continuation.as_mut() {
        continuation.parent = Some(parent);
    }
    Ok(())
}

/// Response continuation runs only for a request with `previous_response_id`.
fn missing_previous_response_id() -> ExecutorError {
    ExecutorError::InvalidRequest("previous_response_id is required to continue a response".to_owned())
}

/// Hydrates `ctx` from the conversation store.
///
/// Gets or creates the conversation (depending on `store`) and rehydrates its history in parallel, then continues
/// that history with this request's items. A conversation turn does not inherit the stored settings.
async fn from_conversation(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let (conv_data, snapshot) = tokio::try_join!(
        async {
            if ctx.original_request.store {
                exec_ctx.conv_handler.get_or_create(ctx).await
            } else {
                exec_ctx.conv_handler.get(ctx).await
            }
        },
        exec_ctx.conv_handler.rehydrate(ctx),
    )?;
    ctx.conversation_id = Some(conv_data.conversation_id);
    let checkpoint = snapshot.turn_metadata.map(|metadata| ResponseCheckpoint {
        response_id: snapshot.latest_response_id.unwrap_or_default(),
        history: Vec::new(),
        metadata,
        durable: true,
    });
    let restored = match &checkpoint {
        Some(turn) => restore_agent_tree(ctx, &turn.metadata, exec_ctx.responses_config.max_retained_bytes)?,
        None => false,
    };
    if !restored {
        let items = with_request_items(ctx, InOutItem::into_input_items(snapshot.items));
        set_model_input(ctx, items)?;
    }
    ctx.checkpoint = checkpoint;
    Ok(())
}

/// The continued `history` followed by this request's own items.
fn with_request_items(ctx: &RequestContext, mut history: Vec<InputItem>) -> Vec<InputItem> {
    history.extend(InOutItem::into_input_items(ctx.new_input_items.clone()));
    history
}

/// Makes `items` the turn's model input, once no synchronous call in them is left unanswered.
fn set_model_input(ctx: &mut RequestContext, items: Vec<InputItem>) -> ExecutorResult<()> {
    reject_unanswered_sync_call(&items)?;
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
