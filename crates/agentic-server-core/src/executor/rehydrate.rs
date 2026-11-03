//! Step 1 of the conversation pipeline — history rehydration.
//!
//! Builds a [`RequestContext`] by loading prior turns from storage and
//! injecting them into the enriched request before it is forwarded to the LLM.

use super::session::{ResponseContinuation, ResponseSession, canonical_session_history};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::multi_agent::{CheckpointLimits, ValidatedTreeCheckpoint};
use crate::executor::pending_calls::first_unanswered_sync_call;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::telemetry::metrics::Stage;
use crate::storage::{ConversationSnapshot, ResponseCheckpoint, ResponseMetadata, StorageError};
use crate::tool::ToolError;
use crate::types::io::InOutItem;
use crate::types::io::input::is_search_item;
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
    let (checkpoint, history) = exec_ctx
        .resp_handler
        .rehydrate(ctx)
        .await?
        .ok_or_else(missing_previous_response_id)?;
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
        let (mut checkpoint, history) = exec_ctx
            .resp_handler
            .rehydrate(ctx)
            .await?
            .ok_or_else(missing_previous_response_id)?;
        checkpoint.history = canonical_session_history(InOutItem::into_input_items(history));
        let continuation = ctx.continuation.as_ref().ok_or_else(|| {
            ExecutorError::InvalidRequest("session response requires a continuation lease".to_owned())
        })?;
        std::sync::Arc::new(continuation.retain_parent(checkpoint)?)
    };
    if !restore_agent_tree(ctx, &parent.metadata, exec_ctx.responses_config.max_retained_bytes)? {
        let items = canonical_session_history(with_request_items(ctx, parent.history.clone()));
        set_model_input(ctx, items)?;
        apply_effective_settings(ctx, &parent.metadata);
    }
    // The model input holds the parent's history; the turn's checkpoint keeps what persistence extends.
    ctx.checkpoint = Some(ResponseCheckpoint {
        response_id: parent.response_id.clone(),
        metadata: parent.metadata.clone(),
        durable: parent.durable,
        history_item_ids: parent.history_item_ids.clone(),
        history: Vec::new(),
    });
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
/// Rehydrates the conversation's history and continues it with this request's items. A missing conversation is
/// created by a stored turn and required by an unstored one. A conversation turn does not inherit the stored settings.
async fn from_conversation(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let snapshot = match exec_ctx.conv_handler.rehydrate(ctx).await {
        Err(ExecutorError::Storage(error)) if error.is_not_found() && ctx.original_request.store => {
            exec_ctx.conv_handler.get_or_create(ctx).await?;
            ConversationSnapshot::default()
        }
        result => result?,
    };
    ctx.conversation_id.clone_from(&ctx.original_request.conversation);

    if !restore_agent_tree(
        ctx,
        &snapshot.checkpoint.metadata,
        exec_ctx.responses_config.max_retained_bytes,
    )? {
        let items = with_request_items(ctx, InOutItem::into_input_items(snapshot.items));
        set_model_input(ctx, items)?;
    }
    ctx.checkpoint = Some(snapshot.checkpoint);
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
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use super::*;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::executor::pending_calls::pending_calls;
    use crate::storage::{
        ConversationStore, ResponseCheckpoint, ResponseMetadata, ResponseStore, create_pool_with_schema,
    };
    use crate::types::agent::{AgentIdentity, AgentTurnId};
    use crate::types::agent_tree::{AgentState, StoredAgent, StoredTreeSnapshot};
    use crate::types::conversations::ConversationMetadata;
    use crate::types::io::InOutItem;
    use crate::types::io::MultiAgentConfig;
    use crate::types::io::output::{McpListTools, OutputItem};
    use crate::types::request_response::{MaxToolCalls, RequestPayload};
    use std::num::NonZeroU64;

    #[test]
    fn session_history_without_compaction_is_unchanged() {
        let history: Vec<InputItem> = serde_json::from_value(serde_json::json!([
            {"type":"message", "role":"user", "content":"question"},
            {"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"counter", "tools":[]},
            {"type":"function_call", "call_id":"pending", "name":"lookup", "arguments":"{}"}
        ]))
        .unwrap();
        let expected = serde_json::to_value(&history).unwrap();
        assert_eq!(
            serde_json::to_value(canonical_session_history(history)).unwrap(),
            expected
        );
    }

    #[test]
    fn restored_session_keeps_canonical_compaction_and_mcp_discovery() {
        let history: Vec<InputItem> = serde_json::from_value(serde_json::json!([
            {"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"counter", "tools":[]},
            {"type":"message", "role":"assistant", "content":"obsolete detail"},
            {"type":"function_call", "call_id":"obsolete", "name":"lookup", "arguments":"{}"},
            {"type":"message", "role":"user", "content":"noncanonical old user"},
            {"type":"message", "role":"user", "id":"msg_keep", "status":"completed", "content":"canonical user"},
            {"type":"compaction", "id":"cmp_latest", "encrypted_content":"latest summary"},
            {"type":"function_call", "call_id":"live", "name":"lookup", "arguments":"{}"},
            {"type":"function_call_output", "call_id":"live", "output":"live result"}
        ]))
        .unwrap();
        let history = canonical_session_history(history);
        assert_eq!(history.len(), 5);
        assert!(matches!(history[0], InputItem::McpListTools(_)));
        assert!(matches!(&history[2], InputItem::Compaction(item)
            if item.id.as_deref() == Some("cmp_latest") && item.encrypted_content == "latest summary"));
        assert!(pending_calls(&history).unwrap().is_empty());
        let serialized = serde_json::to_value(&history).unwrap();
        assert_eq!(serialized[1]["id"], "msg_keep");
        assert_eq!(serialized[3]["call_id"], "live");
        assert_eq!(serialized[4]["call_id"], "live");
        assert!(!serialized.to_string().contains("obsolete"));
    }

    #[test]
    fn restored_session_uses_the_latest_window_and_preserves_live_pending_calls() {
        let history: Vec<InputItem> = serde_json::from_value(serde_json::json!([
            {"type":"message", "role":"user", "id":"msg_old", "status":"completed", "content":"old user"},
            {"type":"mcp_list_tools", "id":"mcpl_old", "server_label":"old", "tools":[]},
            {"type":"compaction", "id":"cmp_old", "encrypted_content":"old summary"},
            {"type":"message", "role":"user", "id":"msg_keep", "status":"completed", "content":"retained user"},
            {"type":"message", "role":"user", "id":"msg_incomplete", "status":"in_progress", "content":"superseded"},
            {"type":"mcp_list_tools", "id":"mcpl_new", "server_label":"new", "tools":[]},
            {"type":"compaction", "id":"cmp_new", "encrypted_content":"latest summary"},
            {"type":"function_call", "call_id":"live", "name":"lookup", "arguments":"{}"}
        ]))
        .unwrap();
        let history = canonical_session_history(history);
        let serialized = serde_json::to_value(&history).unwrap();
        assert_eq!(history.len(), 5);
        assert_eq!(serialized[0]["id"], "mcpl_old");
        assert_eq!(serialized[1]["id"], "msg_keep");
        assert_eq!(serialized[2]["id"], "mcpl_new");
        assert_eq!(serialized[3]["id"], "cmp_new");
        let pending = pending_calls(&history).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].call_id, "live");
    }

    fn request(conversation_id: Option<&str>, previous_response_id: Option<&str>) -> RequestPayload {
        RequestPayload {
            model: "test".into(),
            input: ResponsesInput::Text("new input".into()),
            store: true,
            previous_response_id: previous_response_id.map(str::to_owned),
            conversation: conversation_id.map(str::to_owned),
            ..Default::default()
        }
    }

    fn execution_context(conversation_store: ConversationStore, response_store: ResponseStore) -> ExecutionContext {
        ExecutionContext::new(
            ConversationHandler::new(conversation_store),
            ResponseHandler::new(response_store),
            Arc::new(reqwest::Client::new()),
            "http://localhost:8000".to_owned(),
        )
    }

    #[tokio::test]
    async fn multi_agent_session_admission_allows_both_storage_values() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        for store in [true, false] {
            let session = ResponseSession::new(NonZeroUsize::new(128).unwrap(), NonZeroUsize::new(65_536).unwrap());
            let mut request = request(None, None);
            request.store = store;
            request.multi_agent = Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(3),
            });
            let ctx = rehydrate_in_session(request, &exec_ctx, &session).await.unwrap();
            assert!(ctx.enriched_request.multi_agent.unwrap().enabled);
            assert!(ctx.continuation.is_some());
        }
    }

    #[test]
    fn multi_agent_compaction_trigger_is_allowed() {
        let mut request = request(None, None);
        request.input = ResponsesInput::from(vec![InputItem::CompactionTrigger]);
        request.multi_agent = serde_json::from_value(serde_json::json!({"enabled":true})).unwrap();
        validate_multi_agent_request(&request).unwrap();
    }

    #[test]
    fn client_tools_are_supported_with_multi_agent() {
        let mut request = request(None, None);
        request.tools = Some(
            serde_json::from_value(serde_json::json!([
                {"type":"custom","name":"echo"}, {"type":"tool_search","execution":"client"}
            ]))
            .unwrap(),
        );
        validate_multi_agent_request(&request).unwrap();
        request.multi_agent = serde_json::from_value(serde_json::json!({"enabled":false})).unwrap();
        validate_multi_agent_request(&request).unwrap();
        request.multi_agent = serde_json::from_value(serde_json::json!({"enabled":true})).unwrap();
        validate_multi_agent_request(&request).unwrap();
    }

    #[tokio::test]
    async fn invalid_max_tool_calls_is_rejected_before_history_access() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        for (limit, code) in [
            (serde_json::json!(0), "integer_below_min_value"),
            (serde_json::json!("2"), "invalid_type"),
        ] {
            for previous in [None, Some("resp_missing")] {
                let request: RequestPayload = serde_json::from_value(serde_json::json!({
                    "model": "test", "input": "hello", "previous_response_id": previous, "max_tool_calls": limit
                }))
                .unwrap();
                let error = rehydrate_conversation(request, &exec_ctx).await.unwrap_err();
                assert!(matches!(error, ExecutorError::InvalidMaxToolCalls(_)), "{error:?}");
                assert_eq!(error.error_code(), code);
            }
        }
    }

    #[tokio::test]
    async fn multi_agent_max_tool_calls_is_rejected_before_history_access() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        for stream in [false, true] {
            for limit in [0, 5] {
                for previous in [None, Some("resp_missing")] {
                    let request: RequestPayload = serde_json::from_value(serde_json::json!({
                        "model": "test", "input": "hello", "stream": stream,
                        "previous_response_id": previous,
                        "multi_agent": {"enabled": true}, "max_tool_calls": limit
                    }))
                    .unwrap();
                    let error = rehydrate_conversation(request, &exec_ctx).await.unwrap_err();
                    assert!(matches!(error, ExecutorError::InvalidRequest(message)
                        if message == "max_tool_calls is not supported with multi_agent"));
                }
            }
        }
    }

    /// Recorded `OpenAI` behavior: multi-agent mode rejects async tools with explicit parallel tool
    /// calls, and accepts them with parallel calls off. The gateway's default is `false`.
    #[test]
    fn multi_agent_rejects_async_tools_only_with_explicit_parallel_tool_calls() {
        let request = |parallel: serde_json::Value, async_tool: bool| -> RequestPayload {
            serde_json::from_value(serde_json::json!({
                "model": "test", "input": "hello", "store": true,
                "multi_agent": {"enabled": true},
                "parallel_tool_calls": parallel,
                "tools": [{"type": "function", "name": "get_weather", "async": async_tool}]
            }))
            .unwrap()
        };

        let error = validate_multi_agent_request(&request(serde_json::json!(true), true)).unwrap_err();
        assert!(matches!(
            error,
            ExecutorError::UnsupportedValue {
                param,
                code,
                message,
            } if param == "multi_agent" && code == Some("unsupported_value") && message == "Async parallel tool calls are not yet supported in multi-agent mode."
        ));
        for (parallel, async_tool) in [
            (serde_json::json!(false), true),
            (serde_json::Value::Null, true),
            (serde_json::json!(true), false),
        ] {
            validate_multi_agent_request(&request(parallel.clone(), async_tool))
                .unwrap_or_else(|error| panic!("parallel={parallel} async={async_tool}: {error}"));
        }
    }

    #[tokio::test]
    async fn inherited_multi_agent_rejects_max_tool_calls() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await.unwrap();
        let response_store = ResponseStore::new(pool.clone());
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await
            .unwrap();
        let metadata = ResponseMetadata {
            multi_agent_tree: Some(StoredTreeSnapshot {
                version: 1,
                config: MultiAgentConfig {
                    enabled: true,
                    max_concurrent_subagents: Some(3),
                },
                agents: vec![StoredAgent {
                    identity: AgentIdentity::root(),
                    parent: None,
                    turn: AgentTurnId::new(),
                    state: AgentState::Idle,
                    mailbox: Vec::new(),
                    history: Vec::new(),
                    loaded_tools: Vec::new(),
                    last_task: String::new(),
                    final_answer: None,
                    rounds: 0,
                    wait: None,
                }],
                client_calls: Vec::new(),
            }),
            ..Default::default()
        };
        response_store
            .persist("resp_tree", None, Vec::new(), None, &metadata)
            .await
            .unwrap();
        conversation_store
            .persist(
                &conversation.conversation_id,
                &ResponseCheckpoint::default(),
                "resp_conversation_tree",
                Vec::new(),
                None,
                &metadata,
            )
            .await
            .unwrap();
        let exec_ctx = execution_context(conversation_store, response_store);
        for stream in [false, true] {
            for (conversation_id, previous) in [
                (None, Some("resp_tree")),
                (Some(conversation.conversation_id.as_str()), None),
            ] {
                // No explicit multi_agent: admission must check again after restoring the tree.
                let mut request = request(conversation_id, previous);
                request.stream = stream;
                request.max_tool_calls = Some(MaxToolCalls::new(NonZeroU64::new(5).unwrap()));
                let error = rehydrate_conversation(request, &exec_ctx).await.unwrap_err();
                assert!(matches!(error, ExecutorError::InvalidRequest(message)
                    if message == "max_tool_calls is not supported with multi_agent"));
            }
        }
    }

    #[test]
    fn max_tool_calls_restriction_only_applies_to_enabled_multi_agent() {
        for config in [serde_json::Value::Null, serde_json::json!({"enabled": false})] {
            let request: RequestPayload = serde_json::from_value(serde_json::json!({
                "model": "test", "input": "hello", "multi_agent": config, "max_tool_calls": 5
            }))
            .unwrap();
            validate_multi_agent_request(&request).unwrap();
        }
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test", "input": "hello", "multi_agent": {"enabled": true}, "max_tool_calls": null
        }))
        .unwrap();
        validate_multi_agent_request(&request).unwrap();
    }

    #[tokio::test]
    async fn nonstored_multi_agent_needs_no_history_store_without_state_ids() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        for stream in [false, true] {
            let mut request = request(None, None);
            request.store = false;
            request.stream = stream;
            request.multi_agent = Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(3),
            });
            let ctx = rehydrate_conversation(request.clone(), &exec_ctx).await.unwrap();
            assert!(ctx.multi_agent_tree.is_none());
            assert_eq!(ctx.new_input_items.len(), 1);
            request.input.push(InputItem::CompactionTrigger);
            assert!(matches!(rehydrate_conversation(request, &exec_ctx).await,
                Err(ExecutorError::InvalidRequest(message)) if message.contains("compaction_trigger")));
        }
    }

    #[tokio::test]
    async fn omitted_or_disabled_multi_agent_preserves_rehydration() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        for store in [false, true] {
            for multi_agent in [
                None,
                Some(crate::types::io::MultiAgentConfig {
                    enabled: false,
                    max_concurrent_subagents: None,
                }),
            ] {
                let mut request = request(None, None);
                request.store = store;
                request.multi_agent = multi_agent;
                let expected_input = Vec::<InputItem>::from(&request.input);
                let ctx = rehydrate_conversation(request, &exec_ctx).await.unwrap();
                assert_eq!(ctx.original_request.store, store);
                assert_eq!(
                    serde_json::to_value(&ctx.enriched_request.input).unwrap(),
                    serde_json::to_value(expected_input).unwrap()
                );
            }
        }
    }

    #[tokio::test]
    async fn new_conversation_rehydration_has_no_turn_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert_eq!(
            ctx.conversation_id.as_deref(),
            Some(conversation.conversation_id.as_str())
        );
        assert!(
            ctx.checkpoint
                .is_some_and(|checkpoint| checkpoint.response_id.is_none())
        );
        Ok(())
    }

    #[tokio::test]
    async fn existing_conversation_rehydration_continues_the_latest_turn() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        let prior_items = Vec::<InputItem>::from(&ResponsesInput::Text("prior input".into()))
            .into_iter()
            .map(InOutItem::Input)
            .collect();
        conversation_store
            .persist(
                &conversation.conversation_id,
                &ResponseCheckpoint::default(),
                "resp_prior",
                prior_items,
                None,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert!(
            ctx.checkpoint
                .is_some_and(|checkpoint| checkpoint.response_id.as_deref() == Some("resp_prior"))
        );
        // The stored turn followed by this request's own input.
        assert_eq!(ctx.enriched_request.input.input_items().count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn conversation_rehydration_remembers_listed_mcp_servers() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        conversation_store
            .persist(
                &conversation.conversation_id,
                &ResponseCheckpoint::default(),
                "resp_prior",
                vec![InOutItem::Output(OutputItem::McpListTools(McpListTools::new(
                    "mcpl_prior",
                    "counter",
                    Vec::new(),
                )))],
                None,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        let ResponsesInput::Items(items) = &ctx.enriched_request.input else {
            panic!("rehydrated input should contain items");
        };
        assert!(
            matches!(items.first(), Some(InOutItem::Input(InputItem::McpListTools(list_tools))) if list_tools.server_label == "counter")
        );
        let ResponsesInput::Items(model_items) = ctx.enriched_request.input.model_input().into_owned() else {
            panic!("model input should contain items");
        };
        assert!(
            model_items
                .iter()
                .all(|item| !matches!(item, InOutItem::Input(InputItem::McpListTools(_))))
        );
        Ok(())
    }

    #[tokio::test]
    async fn request_without_continuation_has_no_checkpoint() -> Result<(), Box<dyn std::error::Error>> {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(None, None), &exec_ctx).await?;

        assert!(ctx.checkpoint.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn rehydration_remains_public_until_explicit_tool_search_preparation() {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "find weather tools",
            "store": false,
            "tools": [{
                "type": "tool_search",
                "execution": "client",
                "description": "Find a tool",
                "parameters": {"type": "object"}
            }]
        }))
        .expect("valid tool-search request");

        let ctx = rehydrate_conversation(request, &exec_ctx)
            .await
            .expect("blocking store:false search rehydrates");

        assert!(matches!(
            ctx.enriched_request.tools.as_deref(),
            Some([crate::types::tools::ResponsesTool::ToolSearch(search)])
                if search.execution == crate::types::tools::ToolSearchExecution::Client
        ));

        let (ctx, tool_search_state) = crate::executor::prepare::prepare_request_tools(ctx)
            .expect("explicit handler preparation accepts the rehydrated request");

        assert!(
            tool_search_state
                .as_ref()
                .is_some_and(crate::tool::ToolSearchState::is_active)
        );
        let upstream = ctx
            .enriched_request
            .to_upstream_request(false)
            .expect("prepared tool-search request lowers at the upstream boundary");
        assert!(matches!(
            upstream.tools.as_deref(),
            Some([crate::types::request_response::UpstreamTool::Function(function)])
                if function.name == "tool_search"
        ));
    }

    #[tokio::test]
    async fn rehydration_rejects_orphan_tool_search_outputs() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        let orphan: InputItem = serde_json::from_value(serde_json::json!({
            "type": "tool_search_output",
            "call_id": "call_search_1",
            "tools": []
        }))
        .expect("valid public output item");
        response_store
            .persist(
                "resp_search",
                None,
                vec![InOutItem::Input(orphan)],
                None,
                &ResponseMetadata::default(),
            )
            .await
            .expect("seed prior response");
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let error = rehydrate_conversation(request(None, Some("resp_search")), &exec_ctx)
            .await
            .expect_err("shared call validation rejects orphan stored search outputs");
        assert!(
            matches!(&error, ExecutorError::Tool(ToolError::UnknownCallOutput { call_id }) if call_id == "call_search_1"),
            "unexpected error: {error}"
        );
        assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stored_public_search_call_pairs_with_new_output_after_rehydration() {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory"))
            .await
            .expect("create response store");
        let response_store = ResponseStore::new(pool);
        let stored_call: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "tool_search_call",
            "id": "tsc_stored",
            "call_id": "call_search_stored",
            "execution": "client",
            "arguments": {"query": "weather"},
            "status": "completed"
        }))
        .expect("valid emitted public search call");
        let effective_tools = serde_json::from_value(serde_json::json!([
            {
                "type": "tool_search",
                "execution": "client",
                "description": "Find a tool",
                "parameters": {"type": "object"}
            },
            {
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object"},
                "defer_loading": true
            }
        ]))
        .expect("valid effective public declarations");
        let metadata = ResponseMetadata {
            multi_agent_tree: None,
            effective_tools: Some(effective_tools),
            ..ResponseMetadata::default()
        };
        response_store
            .persist(
                "resp_stored_search",
                None,
                vec![InOutItem::Output(stored_call)],
                None,
                &metadata,
            )
            .await
            .expect("persist public search call");
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);
        let mut continuation = request(None, Some("resp_stored_search"));
        continuation.input = serde_json::from_value(serde_json::json!([{
            "type": "tool_search_output",
            "call_id": "call_search_stored",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object"},
                "defer_loading": true
            }]
        }]))
        .expect("valid new public search output");

        let ctx = rehydrate_conversation(continuation, &exec_ctx)
            .await
            .expect("stored public call rehydrates before new output");
        let (ctx, tool_search_state) = crate::executor::prepare::prepare_request_tools(ctx)
            .expect("stored continuation derives valid tool-search state");

        let state = tool_search_state
            .as_ref()
            .expect("valid state was prepared after rehydration");
        assert!(state.is_active());
        assert_eq!(state.loaded_public_tools().len(), 1);
        assert!(matches!(
            &state.loaded_public_tools()[0],
            crate::types::tools::ResponsesTool::Function(function) if function.name.as_str() == "get_weather"
        ));
        let private_input =
            serde_json::to_value(&ctx.enriched_request.input).expect("prepared private history serializes");
        assert_eq!(private_input[0]["call_id"], "call_search_stored");
        assert_eq!(private_input[1]["call_id"], "call_search_stored");
    }

    #[tokio::test]
    async fn stored_code_interpreter_call_reaches_the_model_as_its_function_call()
    -> Result<(), Box<dyn std::error::Error>> {
        let projection: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "code_interpreter_call",
            "id": "ci_projection",
            "container_id": "cntr_projection",
            "code": "print(7)",
            "status": "completed",
            "outputs": [{"type": "logs", "logs": "7\n"}]
        }))?;
        let response_pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let response_store = ResponseStore::new(response_pool);
        let conversation_store = ConversationStore::new(conversation_pool);
        response_store
            .persist(
                "resp_projection",
                None,
                vec![InOutItem::Output(projection.clone())],
                None,
                &ResponseMetadata::default(),
            )
            .await?;
        let conversation = conversation_store
            .create("default_tenant", &ConversationMetadata::new(), &[])
            .await?;
        conversation_store
            .persist(
                &conversation.conversation_id,
                &ResponseCheckpoint::default(),
                "resp_conversation_projection",
                vec![InOutItem::Output(projection)],
                None,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(conversation_store, response_store);

        for request in [
            request(None, Some("resp_projection")),
            request(Some(&conversation.conversation_id), None),
        ] {
            let ctx = rehydrate_conversation(request, &exec_ctx).await?;
            // History keeps the public item; the upstream projection lowers it to the call the model made.
            let model_input = serde_json::to_value(ctx.enriched_request.input.normalized_model_input(&[])?)?;
            assert_eq!(model_input.as_array().map(Vec::len), Some(3));
            assert_eq!(model_input[0]["type"], "function_call");
            assert_eq!(model_input[0]["name"], "code_interpreter");
            assert_eq!(model_input[0]["call_id"], "ci_projection");
            assert_eq!(model_input[1]["type"], "function_call_output");
            assert_eq!(model_input[2]["role"], "user");
        }
        Ok(())
    }

    #[tokio::test]
    async fn previous_response_rehydration_checkpoints_the_parent() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let response_store = ResponseStore::new(pool);
        response_store
            .persist("resp_prior", None, Vec::new(), None, &ResponseMetadata::default())
            .await?;
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let ctx = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx).await?;

        assert!(
            ctx.checkpoint
                .is_some_and(|checkpoint| checkpoint.response_id.as_deref() == Some("resp_prior"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn previous_response_rehydration_remembers_listed_mcp_servers() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let response_store = ResponseStore::new(pool);
        response_store
            .persist(
                "resp_prior",
                None,
                vec![InOutItem::Output(OutputItem::McpListTools(McpListTools::new(
                    "mcpl_prior",
                    "counter",
                    Vec::new(),
                )))],
                None,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let ctx = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx).await?;

        let ResponsesInput::Items(items) = &ctx.enriched_request.input else {
            panic!("rehydrated input should contain items");
        };
        assert!(
            matches!(items.first(), Some(InOutItem::Input(InputItem::McpListTools(list_tools))) if list_tools.server_label == "counter")
        );
        let ResponsesInput::Items(model_items) = ctx.enriched_request.input.model_input().into_owned() else {
            panic!("model input should contain items");
        };
        assert!(
            model_items
                .iter()
                .all(|item| !matches!(item, InOutItem::Input(InputItem::McpListTools(_))))
        );
        Ok(())
    }
}
