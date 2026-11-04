use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::pending_calls::pending_calls;
use crate::storage::{
    ConversationStore, ConversationVersion, ResponseMetadata, ResponseStore, create_pool_with_schema,
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

fn reasoning_item(content: &[&str], encrypted_content: Option<&str>) -> InputItem {
    InputItem::Reasoning(ReasoningOutput {
        agent: None,
        id: "rs_prior".to_owned(),
        content: content.iter().map(|text| ReasoningTextContent::new(*text)).collect(),
        summary: vec![crate::types::ReasoningSummaryContent::new("public summary")],
        encrypted_content: encrypted_content
            .map(|text| crate::types::OpaqueReasoning::try_from(text.to_owned()).unwrap()),
        status: Some(crate::types::ReasoningStatus::Completed),
    })
}

#[test]
fn plaintext_reasoning_is_normalized_for_both_vllm_paths() {
    let mut input = ResponsesInput::from(vec![reasoning_item(
        &["first continuation part", "second continuation part"],
        Some("opaque-provider-state"),
    )]);

    prepare_reasoning_for_vllm(&mut input).expect("plaintext reasoning is replayable");

    let ResponsesInput::Items(items) = input else {
        panic!("expected structured input");
    };
    let InOutItem::Input(InputItem::Reasoning(reasoning)) = &items[0] else {
        panic!("expected reasoning item");
    };
    assert_eq!(reasoning.id, "rs_prior");
    assert_eq!(reasoning.content.len(), 1);
    assert_eq!(
        reasoning.content[0].text,
        "first continuation part\nsecond continuation part"
    );
    assert!(reasoning.summary.is_empty());
    assert_eq!(reasoning.status, Some(crate::types::ReasoningStatus::Completed));
    assert_eq!(reasoning.encrypted_content, None);
}

#[test]
fn encrypted_reasoning_requires_nonempty_plaintext_content() {
    for content in [Vec::new(), vec![""], vec!["", ""]] {
        let mut input = ResponsesInput::from(vec![reasoning_item(&content, Some("opaque-provider-state"))]);

        let error = prepare_reasoning_for_vllm(&mut input).expect_err("encrypted-only reasoning must not reach vLLM");

        assert_eq!(error.http_status(), http::StatusCode::BAD_REQUEST);
        assert!(
            error
                .to_string()
                .contains("encrypted state without plaintext reasoning content")
        );
        assert!(!error.to_string().contains("opaque-provider-state"));
    }
}

#[test]
fn plaintext_reasoning_with_null_encrypted_state_is_normalized_without_summary() {
    let item = reasoning_item(&["plaintext continuation"], None);
    let mut input = ResponsesInput::from(vec![item]);

    prepare_reasoning_for_vllm(&mut input).expect("null encrypted state is valid");

    let ResponsesInput::Items(items) = input else {
        panic!("expected structured input");
    };
    let InOutItem::Input(InputItem::Reasoning(reasoning)) = &items[0] else {
        panic!("expected reasoning item");
    };
    assert_eq!(
        reasoning.content[0].type_,
        crate::types::ReasoningTextKind::ReasoningText
    );
    assert_eq!(reasoning.content[0].text, "plaintext continuation");
    assert!(reasoning.summary.is_empty());
    assert_eq!(reasoning.encrypted_content, None);
}

#[test]
fn summary_only_reasoning_without_opaque_state_is_removed_from_vllm_copy() {
    for input in [
        r#"[{"type":"reasoning","summary":[{"type":"summary_text","text":"public summary"}]}]"#,
        r#"[{"type":"reasoning","summary":[{"type":"summary_text","text":"public summary"}],"encrypted_content":null}]"#,
    ] {
        let mut input: ResponsesInput = serde_json::from_str(input).unwrap();

        prepare_reasoning_for_vllm(&mut input).expect("summary-only reasoning has no usable vLLM state");

        let ResponsesInput::Items(items) = input else {
            panic!("expected structured input");
        };
        assert!(
            items.is_empty(),
            "a reasoning summary must never be promoted to reasoning text"
        );
    }
}

#[test]
fn validation_failure_does_not_partially_mutate_input() {
    let valid = reasoning_item(&["plaintext continuation"], Some("first-opaque-state"));
    let invalid = reasoning_item(&[], Some("second-opaque-state"));
    let mut input = ResponsesInput::from(vec![valid.clone(), invalid]);

    prepare_reasoning_for_vllm(&mut input).expect_err("the complete input must validate before normalization");

    let ResponsesInput::Items(items) = input else {
        panic!("expected structured input");
    };
    let (InOutItem::Input(InputItem::Reasoning(actual)), InputItem::Reasoning(expected)) = (&items[0], &valid) else {
        panic!("expected reasoning items");
    };
    assert_eq!(actual.content[0].text, expected.content[0].text);
    assert_eq!(actual.summary, expected.summary);
    assert_eq!(actual.encrypted_content, expected.encrypted_content);
}

#[test]
fn text_input_is_unchanged() {
    let mut input = ResponsesInput::Text("plain user input".to_owned());

    prepare_reasoning_for_vllm(&mut input).expect("text input contains no reasoning item");

    assert!(matches!(input, ResponsesInput::Text(ref text) if text == "plain user input"));
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
        .persist("resp_tree", None, Vec::new(), &metadata)
        .await
        .unwrap();
    conversation_store
        .persist(
            &conversation.conversation_id,
            ConversationVersion::default(),
            "resp_conversation_tree",
            Vec::new(),
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
async fn new_conversation_rehydration_captures_empty_version() -> Result<(), Box<dyn std::error::Error>> {
    let pool = create_pool_with_schema(Some("sqlite://?mode=memory")).await?;
    let conversation_store = ConversationStore::new(pool);
    let conversation = conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await?;
    let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

    let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

    assert_eq!(ctx.conversation_version, Some(ConversationVersion::default()));
    Ok(())
}

#[tokio::test]
async fn existing_conversation_rehydration_captures_last_response() -> Result<(), Box<dyn std::error::Error>> {
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
            ConversationVersion::default(),
            "resp_prior",
            prior_items,
            &ResponseMetadata::default(),
        )
        .await?;
    let exec_ctx = execution_context(conversation_store, ResponseStore::disabled());

    let ctx = rehydrate_conversation(request(Some(&conversation.conversation_id), None), &exec_ctx).await?;

    assert_eq!(
        ctx.conversation_version,
        Some(ConversationVersion {
            response_id: Some("resp_prior".to_owned()),
            revision: 1,
            last_sequence: Some(0),
        })
    );
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
            ConversationVersion::default(),
            "resp_prior",
            vec![InOutItem::Output(OutputItem::McpListTools(McpListTools::new(
                "mcpl_prior",
                "counter",
                Vec::new(),
            )))],
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
async fn native_code_interpreter_call_rehydrates_for_previous_response_and_conversation()
-> Result<(), Box<dyn std::error::Error>> {
    let native_call: OutputItem = serde_json::from_value(serde_json::json!({
        "type": "code_interpreter_call",
        "id": "ci_native",
        "container_id": "cntr_native",
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
            "resp_native",
            None,
            vec![InOutItem::Output(native_call.clone())],
            &ResponseMetadata::default(),
        )
        .await?;
    let conversation = conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await?;
    conversation_store
        .persist(
            &conversation.conversation_id,
            ConversationVersion::default(),
            "resp_conversation_native",
            vec![InOutItem::Output(native_call)],
            &ResponseMetadata::default(),
        )
        .await?;
    let exec_ctx = execution_context(conversation_store, response_store);

    for request in [
        request(None, Some("resp_native")),
        request(Some(&conversation.conversation_id), None),
    ] {
        let ctx = rehydrate_conversation(request, &exec_ctx).await?;
        let model_input = serde_json::to_value(ctx.enriched_request.input.model_input())?;
        assert_eq!(model_input[0]["type"], "code_interpreter_call");
        assert_eq!(model_input[0]["id"], "ci_native");
        assert_eq!(model_input[0]["outputs"][0]["logs"], "7\n");
        assert_eq!(model_input[1]["role"], "user");
    }
    Ok(())
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
