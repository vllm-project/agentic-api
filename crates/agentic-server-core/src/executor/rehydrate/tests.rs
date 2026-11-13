use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::pending_calls::{CallLedger, PendingCall};
use crate::storage::{ConversationStore, ResponseCheckpoint, ResponseMetadata, ResponseStore, create_pool_with_schema};
use crate::tool::ToolError;
use crate::tool::file_search::FileSearchService;
use crate::types::agent::{AgentIdentity, AgentTurnId, AgentTurnKey};
use crate::types::agent_tree::{AgentPhase, AgentState, StoredAgent, StoredClientCall, StoredTreeSnapshot};
use crate::types::client_calls::{ClientCallId, ClientCallKind, ClientCallOwner};
use crate::types::conversations::ConversationMetadata;
use crate::types::file_search::FileSearchConfig;
use crate::types::io::InOutItem;
use crate::types::io::MultiAgentConfig;
use crate::types::io::output::{McpListTools, OutputItem};
use crate::types::request_response::{MaxToolCalls, RequestPayload};
use std::num::NonZeroU64;

fn pending_calls(items: &[InputItem]) -> ExecutorResult<Vec<PendingCall>> {
    CallLedger::scan(items).map(|ledger| ledger.pending().collect())
}

#[test]
fn session_history_without_compaction_is_unchanged() {
    let history: Vec<InputItem> = serde_json::from_value(serde_json::json!([
        {"type":"message", "role":"user", "content":"question"},
        {"type":"mcp_list_tools", "id":"mcpl_1", "server_label":"counter", "tools":[]},
        {"type":"function_call", "call_id":"pending", "name":"lookup", "arguments":"{}"}
    ]))
    .unwrap();
    let expected = serde_json::to_value(&history).unwrap();
    assert_eq!(serde_json::to_value(retain_window(history)).unwrap(), expected);
}

#[test]
fn restored_session_keeps_canonical_compaction_and_drops_old_mcp_discovery() {
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
    let history = retain_window(history);
    assert_eq!(history.len(), 4);
    assert!(matches!(&history[1], InputItem::Compaction(item)
        if item.id.as_deref() == Some("cmp_latest") && item.encrypted_content == "latest summary"));
    assert!(pending_calls(&history).unwrap().is_empty());
    let serialized = serde_json::to_value(&history).unwrap();
    assert_eq!(serialized[0]["id"], "msg_keep");
    assert_eq!(serialized[2]["call_id"], "live");
    assert_eq!(serialized[3]["call_id"], "live");
    assert!(!serialized.to_string().contains("mcp_list_tools"));
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
    let history = retain_window(history);
    let serialized = serde_json::to_value(&history).unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(serialized[0]["id"], "msg_keep");
    assert_eq!(serialized[1]["id"], "cmp_new");
    assert!(!serialized.to_string().contains("mcp_list_tools"));
    let pending = pending_calls(&history).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].call_id.as_str(), "live");
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
        } if param.as_deref() == Some("multi_agent") && code == Some("unsupported_value") && message == "Async parallel tool calls are not yet supported in multi-agent mode."
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
async fn inherited_multi_agent_preserves_checkpoint_and_validates_limits() {
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
    let items: Vec<InOutItem> = serde_json::from_value(serde_json::json!([
        {"role": "user", "content": "retained public history"},
        {"type": "compaction", "id": "cmp_tree", "encrypted_content": "summary"}
    ]))
    .unwrap();
    response_store
        .persist("resp_tree", None, items.clone(), None, &metadata)
        .await
        .unwrap();
    conversation_store
        .persist(
            &conversation.conversation_id,
            &ResponseCheckpoint::default(),
            "resp_conversation_tree",
            items,
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
            let mut payload = request(conversation_id, previous);
            payload.stream = stream;
            payload.max_tool_calls = Some(MaxToolCalls::new(NonZeroU64::new(5).unwrap()));
            let error = rehydrate_conversation(payload, &exec_ctx).await.unwrap_err();
            assert!(matches!(error, ExecutorError::InvalidRequest(message)
                if message == "max_tool_calls is not supported with multi_agent"));
            let ctx = rehydrate_conversation(request(conversation_id, previous), &exec_ctx)
                .await
                .unwrap();
            assert!(ctx.enriched_request.multi_agent.unwrap().enabled);
            assert_eq!(ctx.checkpoint.unwrap().history_item_ids.len(), 2);
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
    assert!(ctx.checkpoint.as_ref().is_some_and(|turn| turn.response_id.is_none()));
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

    assert_eq!(
        ctx.checkpoint.as_ref().and_then(|turn| turn.response_id.as_deref()),
        Some("resp_prior")
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
    let private_input = serde_json::to_value(&ctx.enriched_request.input).expect("prepared private history serializes");
    assert_eq!(private_input[0]["call_id"], "call_search_stored");
    assert_eq!(private_input[1]["call_id"], "call_search_stored");
}

#[tokio::test]
async fn stored_code_interpreter_call_reaches_the_model_as_its_function_call() -> Result<(), Box<dyn std::error::Error>>
{
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

/// Turns that rehydrated the same pending call and both answer it: after the first commits, the second is refused,
/// so the conversation holds one output and continues. A call the client resubmits with its output is its own.
#[tokio::test]
async fn competing_outputs_for_one_pending_call_keep_the_conversation_usable() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
    let conversation_store = ConversationStore::new(pool);
    let conversation = conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await?;
    let id = conversation.conversation_id.as_str();
    let call: OutputItem = serde_json::from_value(serde_json::json!({
        "type":"function_call", "id":"fc_weather", "call_id":"call_weather", "name":"get_weather",
        "arguments":"{}", "status":"completed"
    }))?;
    conversation_store
        .persist(
            id,
            &ResponseCheckpoint::default(),
            "resp_call",
            vec![InOutItem::Output(call)],
            None,
            &ResponseMetadata::default(),
        )
        .await?;
    let exec_ctx = execution_context(conversation_store.clone(), ResponseStore::disabled());
    let answer = |input: serde_json::Value| {
        let mut request = request(Some(id), None);
        request.input = serde_json::from_value(input).unwrap();
        request
    };
    let output = |temperature: u32| serde_json::json!([{"type":"function_call_output", "call_id":"call_weather", "output":temperature.to_string()}]);

    let first = rehydrate_conversation(answer(output(21)), &exec_ctx).await?;
    let second = rehydrate_conversation(answer(output(5)), &exec_ctx).await?;
    exec_ctx
        .conv_handler
        .persist(
            &first,
            first.new_input_items.clone(),
            None,
            &ResponseMetadata::default(),
        )
        .await?;
    let error = exec_ctx
        .conv_handler
        .persist(
            &second,
            second.new_input_items.clone(),
            None,
            &ResponseMetadata::default(),
        )
        .await
        .expect_err("the call is already answered");
    assert!(matches!(error, ExecutorError::ConversationLocked { .. }), "{error}");

    // A turn stored meanwhile does not refuse a call and output resubmitted together.
    let resubmitted = rehydrate_conversation(
        answer(serde_json::json!([
            {"type":"function_call", "call_id":"call_own", "name":"get_weather", "arguments":"{}"},
            {"type":"function_call_output", "call_id":"call_own", "output":"18"}
        ])),
        &exec_ctx,
    )
    .await?;
    let plain = rehydrate_conversation(request(Some(id), None), &exec_ctx).await?;
    for ctx in [plain, resubmitted] {
        exec_ctx
            .conv_handler
            .persist(&ctx, ctx.new_input_items.clone(), None, &ResponseMetadata::default())
            .await?;
    }

    let answers = conversation_store
        .rehydrate(id)
        .await?
        .items
        .iter()
        .filter_map(InOutItem::as_input)
        .filter(|item| item.output_call_id() == Some("call_weather"))
        .count();
    assert_eq!(answers, 1);
    rehydrate_conversation(request(Some(id), None), &exec_ctx).await?;
    Ok(())
}

/// A root-only agent tree, waiting for the client's output to `pending_call` when there is one.
fn agent_tree(pending_call: Option<&str>) -> ResponseMetadata {
    let turn = AgentTurnId::new();
    let history = pending_call.map_or_else(Vec::new, |call_id| {
        serde_json::from_value(serde_json::json!([
            {"type":"function_call", "call_id":call_id, "name":"lookup", "arguments":"{}"}
        ]))
        .unwrap()
    });
    let client_calls = pending_call
        .map(|call_id| StoredClientCall {
            call_id: ClientCallId::try_from(call_id.to_owned()).unwrap(),
            owner: ClientCallOwner {
                agent_turn: AgentTurnKey {
                    agent: AgentIdentity::root(),
                    turn,
                },
                kind: ClientCallKind::Function,
                async_execution: false,
            },
            resolved: false,
        })
        .into_iter()
        .collect();
    ResponseMetadata {
        multi_agent_tree: Some(StoredTreeSnapshot {
            version: 1,
            config: MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(3),
            },
            agents: vec![StoredAgent {
                identity: AgentIdentity::root(),
                parent: None,
                turn,
                state: if pending_call.is_some() {
                    AgentState::Active(AgentPhase::WaitingForClientOutputs)
                } else {
                    AgentState::Idle
                },
                mailbox: Vec::new(),
                history,
                loaded_tools: Vec::new(),
                last_task: String::new(),
                final_answer: None,
                rounds: 0,
                wait: None,
            }],
            client_calls,
        }),
        ..Default::default()
    }
}

/// Turns that rehydrated the same multi-agent tree: after the first commits, a later turn would replace its tree, so
/// it is refused, and the first turn's pending client call still continues.
#[tokio::test]
async fn overlapping_multi_agent_turns_keep_the_first_turns_pending_call() -> Result<(), Box<dyn std::error::Error>> {
    let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
    let conversation_store = ConversationStore::new(pool);
    let conversation = conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await?;
    let id = conversation.conversation_id.as_str();
    conversation_store
        .persist(
            id,
            &ResponseCheckpoint::default(),
            "resp_tree",
            Vec::new(),
            None,
            &agent_tree(None),
        )
        .await?;
    let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

    let first = rehydrate_conversation(request(Some(id), None), &exec_ctx).await?;
    let stale_tree = rehydrate_conversation(request(Some(id), None), &exec_ctx).await?;
    let plain_turn = rehydrate_conversation(request(Some(id), None), &exec_ctx).await?;
    let first_response_id = first.response_id.clone();
    exec_ctx
        .conv_handler
        .persist(
            &first,
            first.new_input_items.clone(),
            None,
            &agent_tree(Some("call_first")),
        )
        .await?;
    for (ctx, metadata) in [
        (stale_tree, agent_tree(None)),
        (plain_turn, ResponseMetadata::default()),
    ] {
        let error = exec_ctx
            .conv_handler
            .persist(&ctx, ctx.new_input_items.clone(), None, &metadata)
            .await
            .expect_err("a turn stored after the first would replace its tree");
        assert!(matches!(error, ExecutorError::ConversationLocked { .. }), "{error}");
    }

    // The client answers the first turn's call.
    let mut continuation = request(Some(id), None);
    continuation.input = serde_json::from_value(serde_json::json!([
        {"type":"function_call_output", "call_id":"call_first", "output":"sunny"}
    ]))?;
    let ctx = rehydrate_conversation(continuation, &exec_ctx).await?;
    assert_eq!(
        ctx.checkpoint.as_ref().and_then(|turn| turn.response_id.as_deref()),
        Some(first_response_id.as_str())
    );
    let tree = ctx.multi_agent_tree.as_ref().expect("the first turn's tree").snapshot();
    assert!(
        tree.client_calls
            .iter()
            .any(|call| call.call_id.as_str() == "call_first" && !call.resolved)
    );
    Ok(())
}

async fn file_execution_context() -> (ExecutionContext, FileSearchService, tempfile::TempDir) {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let config = FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        ..FileSearchConfig::default()
    };
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).unwrap();
    let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled())
        .with_file_search_service(service.clone());
    (exec_ctx, service, files)
}

#[tokio::test]
async fn rehydration_hydrates_file_references_without_changing_canonical_input() {
    let (exec_ctx, service, _files) = file_execution_context().await;
    let text = service
        .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
        .await
        .unwrap();
    let image = service
        .upload_file("plot.png", "image/png", "vision", b"png".to_vec())
        .await
        .unwrap();
    let (text_id, image_id) = (text.id.clone(), image.id.clone());
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": "test",
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "read these"},
            {"type": "input_file", "file_id": text_id},
            {"type": "input_image", "file_id": image_id}
        ]}]
    }))
    .unwrap();

    let ctx = rehydrate_conversation(request, &exec_ctx).await.unwrap();

    let canonical = serde_json::to_value(&ctx.new_input_items).unwrap();
    assert_eq!(canonical[0]["content"][1]["file_id"], text.id);
    assert!(canonical[0]["content"][1].get("file_data").is_none());
    assert_eq!(canonical[0]["content"][2]["file_id"], image.id);
    let original = serde_json::to_value(&ctx.original_request.input).unwrap();
    assert_eq!(original[0]["content"][1]["file_id"], text.id);
    assert!(original[0]["content"][1].get("file_data").is_none());

    let model = serde_json::to_value(&ctx.enriched_request.input).unwrap();
    assert!(
        model[0]["content"][1]["file_data"]
            .as_str()
            .unwrap()
            .starts_with("data:text/plain;base64,")
    );
    assert!(model[0]["content"][1].get("file_id").is_none());
    assert!(
        model[0]["content"][2]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert!(model[0]["content"][2].get("file_id").is_none());

    let upstream = serde_json::to_value(ctx.enriched_request.to_upstream_request(false).unwrap()).unwrap();
    assert_eq!(
        upstream["input"][0]["content"][1],
        serde_json::json!({"type": "input_text", "text": "File: notes.txt\nhello"})
    );
    assert!(
        upstream["input"][0]["content"][2]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(upstream["input"][0]["content"][2]["detail"], "auto");
}

#[tokio::test]
async fn rehydration_rejects_file_references_without_a_file_store() {
    let exec_ctx = execution_context(ConversationStore::disabled(), ResponseStore::disabled());
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": "test",
        "input": [{"role": "user", "content": [
            {"type": "input_file", "file_id": "file-01a15d85-a1db-76f3-918e-b31d6819a316"}
        ]}]
    }))
    .unwrap();
    let error = rehydrate_conversation(request, &exec_ctx).await.unwrap_err();
    assert!(
        matches!(error, ExecutorError::InvalidRequest(ref message) if message.contains("gateway file store")),
        "{error}"
    );
}

#[tokio::test]
async fn continuation_hydrates_file_references_from_stored_history() -> Result<(), Box<dyn std::error::Error>> {
    let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
    let files = tempfile::tempdir()?;
    let config = FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        ..FileSearchConfig::default()
    };
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config)?;
    let text = service
        .upload_file("notes.txt", "text/plain", "assistants", b"stored text".to_vec())
        .await?;
    let stored: InOutItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user", "content": [
            {"type": "input_file", "file_id": text.id}
        ]
    }))?;
    let response_store = ResponseStore::new(pool);
    response_store
        .persist("resp_prior", None, vec![stored], None, &ResponseMetadata::default())
        .await?;
    let exec_ctx = execution_context(ConversationStore::disabled(), response_store).with_file_search_service(service);

    let ctx = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx).await?;

    let model = serde_json::to_value(&ctx.enriched_request.input)?;
    assert!(
        model[0]["content"][0]["file_data"]
            .as_str()
            .unwrap()
            .starts_with("data:text/plain;base64,")
    );
    assert!(model[0]["content"][0].get("file_id").is_none());
    Ok(())
}

#[tokio::test]
async fn continuation_reports_a_deleted_file_reference() -> Result<(), Box<dyn std::error::Error>> {
    let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
    let files = tempfile::tempdir()?;
    let config = FileSearchConfig {
        files_storage_dir: Some(files.path().to_owned()),
        ..FileSearchConfig::default()
    };
    let service = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config)?;
    let text = service
        .upload_file("notes.txt", "text/plain", "assistants", b"stored text".to_vec())
        .await?;
    let stored: InOutItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user", "content": [
            {"type": "input_file", "file_id": text.id}
        ]
    }))?;
    let response_store = ResponseStore::new(pool);
    response_store
        .persist("resp_prior", None, vec![stored], None, &ResponseMetadata::default())
        .await?;
    service.delete_file(&text.id).await?;
    let exec_ctx = execution_context(ConversationStore::disabled(), response_store).with_file_search_service(service);

    let error = rehydrate_conversation(request(None, Some("resp_prior")), &exec_ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ExecutorError::FileSearch(crate::types::file_search::FileSearchError::NotFound(ref message))
                if message.contains("input[0].content[0]: ")
        ),
        "{error}"
    );
    Ok(())
}
