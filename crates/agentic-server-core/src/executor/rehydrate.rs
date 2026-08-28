//! Step 1 of the conversation pipeline — history rehydration.
//!
//! Builds a [`RequestContext`] by loading prior turns from storage and
//! injecting them into the enriched request before it is forwarded to the LLM.

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::storage::InOutItem;
use crate::types::io::{InputItem, ResponsesInput, resolve_tool_choice, resolve_tools};
use crate::types::request_response::RequestPayload;
use crate::utils::uuid7_str;

/// Step 1 — Build [`RequestContext`] by rehydrating conversation history.
///
/// `request` is moved into the context as `enriched_request`; one clone is taken
/// for `original_request` so the engine retains an unmodified copy for persistence
/// and ID resolution.
///
/// Dispatches based on `store` flag and which ID is present:
/// - `previous_response_id`: rehydrate from the prior response checkpoint
/// - `conversation_id`:      rehydrate from the conversation
/// - no ids:                 forward only the new input
///
/// # Errors
/// Returns [`ExecutorError`] if storage is unavailable or a referenced ID does not exist.
pub async fn rehydrate_conversation(
    request: RequestPayload,
    exec_ctx: &ExecutionContext,
) -> ExecutorResult<RequestContext> {
    let response_id = uuid7_str("resp_");
    let new_input_items: Vec<InputItem> = Vec::from(&request.input);

    // One clone for the unmodified original; `request` is moved as enriched_request.
    let original_request = request.clone();
    let mut ctx = RequestContext {
        enriched_request: request,
        original_request,
        new_input_items,
        response_id,
        conversation_id: None,
        conversation_version: None,
    };

    if ctx.original_request.conversation_id.is_some() && ctx.original_request.previous_response_id.is_some() {
        return Err(ExecutorError::InvalidRequest(
            "provide only one of conversation_id or previous_response_id".into(),
        ));
    }

    if ctx.original_request.conversation_id.is_some() {
        from_conversation(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    if ctx.original_request.previous_response_id.is_some() {
        from_response(&mut ctx, exec_ctx).await?;
        return Ok(ctx);
    }

    ctx.enriched_request.input = ResponsesInput::Items(ctx.new_input_items.clone());
    Ok(ctx)
}

/// Hydrates `ctx` from the previous response chain.
///
/// Loads the stored response, rehydrates its history items, resolves effective
/// tools and tool choice from the stored metadata, and prepends the history to
/// the enriched request input.
async fn from_response(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let stored = exec_ctx.resp_handler.get(ctx).await?;
    let history = exec_ctx.resp_handler.rehydrate(ctx).await?;

    let mut items = InOutItem::into_input_items(history);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.previous_response_id = None;
    ctx.enriched_request.input = ResponsesInput::Items(items);
    apply_effective_settings(ctx, &stored.metadata);
    ctx.conversation_id = stored.conversation_id;
    Ok(())
}

/// Hydrates `ctx` from the conversation store.
///
/// Gets or creates the conversation (depending on `store`) and rehydrates its
/// history in parallel, then prepends the history items to the enriched request input.
async fn from_conversation(ctx: &mut RequestContext, exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    let (conv_data, snapshot) = tokio::try_join!(
        async {
            if ctx.original_request.store {
                exec_ctx.conv_handler.get_or_create(ctx).await
            } else {
                exec_ctx.conv_handler.get(ctx).await
            }
        },
        exec_ctx.conv_handler.rehydrate_snapshot(ctx),
    )?;

    let mut items = InOutItem::into_input_items(snapshot.items);
    items.reserve(ctx.new_input_items.len());
    items.extend(ctx.new_input_items.iter().cloned());

    ctx.enriched_request.input = ResponsesInput::Items(items);
    ctx.conversation_id = Some(conv_data.conversation_id);
    ctx.conversation_version = Some(snapshot.version);
    Ok(())
}

pub(crate) fn apply_effective_settings(ctx: &mut RequestContext, stored: &crate::storage::ResponseMetadata) {
    let tools_explicitly_set = ctx.original_request.tools.is_some();
    ctx.enriched_request.tools = resolve_tools(
        ctx.original_request.tools.as_deref(),
        stored.effective_tools.as_deref(),
        tools_explicitly_set,
    );
    ctx.enriched_request.tool_choice = Some(resolve_tool_choice(
        ctx.original_request.tool_choice.as_ref(),
        &stored.effective_tool_choice,
        ctx.original_request.tool_choice.is_some(),
    ));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::storage::{
        ConversationStore, ConversationVersion, InOutItem, ResponseMetadata, ResponseStore, create_pool_with_schema,
    };
    use crate::tool::ToolError;
    use crate::types::request_response::RequestPayload;

    fn request(conversation_id: Option<&str>, previous_response_id: Option<&str>) -> RequestPayload {
        RequestPayload {
            model: "test".into(),
            input: ResponsesInput::Text("new input".into()),
            instructions: None,
            previous_response_id: previous_response_id.map(str::to_owned),
            conversation_id: conversation_id.map(str::to_owned),
            tools: None,
            tool_choice: None,
            stream: false,
            store: true,
            include: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            truncation: None,
            metadata: None,
            parallel_tool_calls: None,
            cache_salt: None,
            context_management: None,
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
    async fn new_conversation_rehydration_captures_empty_version() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store.create().await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, Some(ConversationVersion::Empty));
        Ok(())
    }

    #[tokio::test]
    async fn existing_conversation_rehydration_captures_last_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let conversation_store = ConversationStore::new(pool);
        let conversation = conversation_store.create().await?;
        let prior_items = Vec::<InputItem>::from(&ResponsesInput::Text("prior input".into()))
            .into_iter()
            .map(InOutItem::Input)
            .collect();
        conversation_store
            .persist(
                &conversation.conversation_id,
                "resp_prior",
                None,
                prior_items,
                &ResponseMetadata::default(),
            )
            .await?;
        let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, Some(ConversationVersion::LastSequence(0)));
        Ok(())
    }

    #[tokio::test]
    async fn request_without_continuation_has_no_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());

        let ctx = rehydrate_conversation(request(None, None), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, None);
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

        let (ctx, registry) =
            crate::executor::prepare::prepare_request_tools(ctx, &exec_ctx.conv_handler, &exec_ctx.resp_handler)
                .await
                .expect("explicit handler preparation accepts the rehydrated request");

        assert!(
            registry
                .tool_search_state()
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
    async fn execution_preparation_validates_tool_search_after_full_rehydration() {
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
                &ResponseMetadata::default(),
            )
            .await
            .expect("seed prior response");
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let ctx = rehydrate_conversation(request(None, Some("resp_search")), &exec_ctx)
            .await
            .expect("orphan history remains a valid rehydrated public shape");
        let error =
            crate::executor::prepare::prepare_request_tools(ctx, &exec_ctx.conv_handler, &exec_ctx.resp_handler)
                .await
                .expect_err("explicit preparation rejects orphan stored public history");

        assert!(
            matches!(error, ExecutorError::Tool(ToolError::Config(ref message)) if message.contains("orphan")),
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
        let stored_call: crate::types::io::OutputItem = serde_json::from_value(serde_json::json!({
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
            effective_tools: Some(effective_tools),
            ..ResponseMetadata::default()
        };
        response_store
            .persist(
                "resp_stored_search",
                None,
                vec![InOutItem::Output(stored_call)],
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
        let (ctx, registry) =
            crate::executor::prepare::prepare_request_tools(ctx, &exec_ctx.conv_handler, &exec_ctx.resp_handler)
                .await
                .expect("stored continuation derives valid tool-search state");

        let state = registry
            .tool_search_state()
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
    async fn previous_response_rehydration_has_no_conversation_version() -> Result<(), Box<dyn std::error::Error>> {
        let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
        let response_store = ResponseStore::new(pool);
        response_store
            .persist("resp_prior", None, Vec::new(), &ResponseMetadata::default())
            .await?;
        let exec_ctx = execution_context(ConversationStore::disabled(), response_store);

        let ctx = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx).await?;

        assert_eq!(ctx.conversation_version, None);
        Ok(())
    }
}
