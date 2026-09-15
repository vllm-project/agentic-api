use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::StreamEvent;
use crate::executor::inference::{call_inference_limited, fetch_response_json_limited};
use crate::executor::pipeline::{AgentPipeline, StreamPayload};
use crate::executor::rehydrate::validate_message_files;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::translate::TranslationContext;
use crate::tool::{ToolRegistry, ToolSearchState};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;
use std::sync::Arc;

/// Snapshot tool facts at the orchestration boundary, excluding all execution bindings.
fn translation_context(registry: &ToolRegistry, agent: &AgentPipeline) -> TranslationContext {
    let state = agent.tool_search_state();
    TranslationContext::new(
        registry
            .tool_classifications()
            .map(|(name, kind)| (name.to_owned(), kind))
            .collect(),
        state
            .map(|state| state.withheld_function_names().clone())
            .unwrap_or_default(),
        state.is_some_and(ToolSearchState::is_active),
    )
    .with_gateway_owned_names(
        registry
            .tool_classifications()
            .filter(|(name, _)| registry.is_gateway_owned_name(name))
            .map(|(name, _)| name.to_owned())
            .collect(),
    )
    .with_response_metadata(
        registry.namespace_map().cloned(),
        registry.custom_tool_map().cloned(),
        state
            .filter(|state| state.is_active())
            .map(crate::tool::ToolSearchState::public_response_tools)
            .or_else(|| {
                agent
                    .request
                    .enriched_request
                    .tools
                    .as_ref()
                    .filter(|tools| {
                        tools
                            .iter()
                            .any(|tool| matches!(tool, crate::types::tools::ResponsesTool::Shell(_)))
                    })
                    .cloned()
            }),
        agent.request.enriched_request.tool_choice.clone(),
    )
}

/// Builds the JSON body sent upstream: history inlined, continuation and storage
/// fields removed.
///
/// # Errors
/// Unsupported message files, a tool-configuration error, or a serialization failure.
pub fn upstream_request(ctx: &RequestContext, stream: bool) -> ExecutorResult<String> {
    // Composable callers may supply RequestContext without the rehydration step.
    validate_message_files(&ctx.enriched_request.input)?;
    let request = ctx.enriched_request.to_upstream_request(stream)?;
    serialize_to_string(&request).map_err(ExecutorError::JsonError)
}

/// One pipeline per response sender; collect-only and JSON requests have no sender.
pub(super) fn agent_pipeline(
    ctx: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    sender: Option<tokio::sync::mpsc::Sender<StreamEvent>>,
) -> AgentPipeline {
    AgentPipeline::new(ctx, tool_search_state, sender)
}

pub(super) fn agent_pipeline_with_limits(
    ctx: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    sender: Option<tokio::sync::mpsc::Sender<StreamEvent>>,
    max_stream_event_bytes: usize,
) -> AgentPipeline {
    AgentPipeline::with_limits(ctx, tool_search_state, sender, max_stream_event_bytes)
}

pub(super) async fn fetch_blocking_payload(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &ToolRegistry,
    response_budget: Option<&ExecutorResponseBudget>,
) -> ExecutorResult<ResponsePayload> {
    agent.ensure_request_prepared()?;
    let upstream_json = upstream_request(&agent.request, false)?;
    let body = fetch_response_json_limited(
        upstream_json,
        &exec_ctx.responses_url(),
        &exec_ctx.client,
        auth,
        exec_ctx.responses_config.max_upstream_json_bytes,
    )
    .await?;
    agent.run_with_json_body(
        &body,
        Validation::Lenient,
        translation_context(registry, agent),
        response_budget.cloned(),
    )
}

/// A complete upstream response, in whichever form the caller received it.
#[derive(Debug, Clone, Copy)]
pub enum UpstreamBody<'a> {
    Json(&'a str),
    /// Frames of a streamed response, already relayed by the caller.
    Sse(&'a str),
}

/// Decodes a complete public body and returns its request context for persistence.
///
/// # Errors
/// Invalid JSON, stream lifecycle, output identity, or tool-call data.
pub async fn decode_upstream(
    ctx: RequestContext,
    body: UpstreamBody<'_>,
) -> ExecutorResult<(ResponsePayload, RequestContext)> {
    let mut agent = agent_pipeline(ctx, None, None);
    let payload = match body {
        UpstreamBody::Json(body) => {
            agent.run_with_json_body(body, Validation::Strict, TranslationContext::default(), None)?
        }
        UpstreamBody::Sse(body) => {
            let lines = futures::stream::iter(body.lines().map(|line| Ok(line.to_owned())));
            agent
                .run_with_stream_body(
                    lines,
                    Validation::Strict,
                    TranslationContext::default(),
                    &ToolRegistry::default(),
                    0,
                    None,
                )
                .await?
                .payload
        }
    };
    let (ctx, _) = agent.into_parts();
    Ok((payload, ctx))
}

pub(super) async fn fetch_stream_payload(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &ToolRegistry,
    output_offset: usize,
    response_budget: &ExecutorResponseBudget,
) -> ExecutorResult<StreamPayload> {
    agent.ensure_request_prepared()?;
    let upstream_json = upstream_request(&agent.request, true)?;
    let lines = call_inference_limited(
        upstream_json,
        exec_ctx.responses_url(),
        Arc::clone(&exec_ctx.client),
        auth.map(str::to_owned),
        exec_ctx.streaming_timeout,
        exec_ctx.responses_config.max_upstream_sse_line_bytes,
    );
    agent
        .run_with_stream_body(
            lines,
            Validation::Lenient,
            translation_context(registry, agent),
            registry,
            output_offset,
            Some(response_budget.clone()),
        )
        .await
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::events::SseLine;
    use crate::executor::error::ResourceLimit;
    use crate::executor::modes::{ConversationHandler, ResponseHandler};
    use crate::executor::pipeline::RoundIngestion;
    use crate::storage::{ConversationStore, ResponseStore};
    use crate::types::io::ResponsesInput;
    use crate::types::request_response::RequestPayload;
    use serde_json::Value;

    #[test]
    fn translation_snapshot_owns_prepared_availability_after_registry_is_dropped() {
        use crate::tool::{ToolSearchState, ToolType};
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "model":"test", "input":"find a tool", "parallel_tool_calls":false,
            "tools":[
                {"type":"tool_search","execution":"client"},
                {"type":"function","name":"hidden","defer_loading":true},
                {"type":"custom","name":"raw_echo"}
            ]
        }))
        .expect("request");
        let registry = ToolRegistry::from_tool_types(std::collections::HashMap::from([
            ("tool_search".to_owned(), ToolType::ToolSearch),
            ("raw_echo".to_owned(), ToolType::Custom),
        ]));
        let state = ToolSearchState::build(&request).expect("prepared state");
        let agent = agent_pipeline(request_context(), Some(state), None);
        let context = translation_context(&registry, &agent);
        drop(registry);
        assert_eq!(context.tool_type("raw_echo"), ToolType::Custom);
        assert_eq!(context.tool_type("tool_search"), ToolType::ToolSearch);
        assert_eq!(context.tool_type("unknown"), ToolType::Function);
        let mut pipeline = RoundIngestion::new("resp_1".to_owned(), None, Validation::Lenient, context, None);
        let line = format!(
            "data: {}",
            serde_json::json!({
                "type":"response.output_item.added", "output_index":0,
                "item":{"id":"fc_hidden","type":"function_call","call_id":"call_hidden","name":"hidden","arguments":"","status":"in_progress"}
            })
        );
        assert!(matches!(
            pipeline.push(SseLine::parse(&line)),
            Err(ExecutorError::Tool(_))
        ));
    }

    #[tokio::test]
    async fn public_metadata_and_namespace_output_translate_without_the_registry() {
        use crate::tool::{GatewayExecutors, ToolSearchHandler};
        let mut request = request_context();
        request.enriched_request.tools = Some(
            serde_json::from_value(serde_json::json!([
                {"type":"tool_search","execution":"client"},
                {"type":"custom","name":"raw_echo","description":"Original description"},
                {"type":"namespace","name":"travel","tools":[{"type":"function","name":"timezone"}]}
            ]))
            .unwrap(),
        );
        request.enriched_request.parallel_tool_calls = Some(false);
        let state = ToolSearchHandler::prepare_request(&mut request.enriched_request, &[], false).unwrap();
        let registry = ToolRegistry::build_with_handlers(
            request.enriched_request.tools.as_mut().unwrap(),
            &mut GatewayExecutors::default(),
        )
        .await
        .unwrap();
        let mut agent = agent_pipeline(request, state, None);
        let stream_context = translation_context(&registry, &agent);
        let json_context = translation_context(&registry, &agent);
        drop(registry);

        let mut ingestion = RoundIngestion::new("resp_1".to_owned(), None, Validation::Lenient, stream_context, None);
        let created = serde_json::json!({"type":"response.created","response":{
            "id":"upstream","status":"in_progress","tools":[],
            "tool_choice":{"type":"function","name":"raw_echo"}
        }});
        let translated = ingestion.push(SseLine::parse(&format!("data: {created}"))).unwrap();
        let response = &translated.frames[0].wire.rest["response"];
        let public_tools = response["tools"].clone();
        assert!(
            public_tools
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "custom" && tool["description"] == "Original description")
        );
        assert!(
            public_tools
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "namespace" && tool["name"] == "travel")
        );
        assert_eq!(response["tool_choice"]["type"], "custom");
        let item = serde_json::json!({"id":"fc_1","type":"function_call","call_id":"call_1","name":"agentic_ns__travel__timezone","arguments":"{}","status":"completed"});
        let done = serde_json::json!({"type":"response.output_item.done","output_index":0,"item":item});
        let translated = ingestion.push(SseLine::parse(&format!("data: {done}"))).unwrap();
        assert_eq!(translated.frames[0].wire.rest["item"]["namespace"], "travel");
        assert_eq!(translated.frames[0].wire.rest["item"]["name"], "timezone");
        let stream_payload = ingestion.finish("test", None, None).unwrap();
        let body = serde_json::json!({"id":"upstream","status":"completed","output":[item]}).to_string();
        let json_payload = agent
            .run_with_json_body(&body, Validation::Lenient, json_context, None)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&stream_payload.output).unwrap(),
            serde_json::to_value(&json_payload.output).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&json_payload.output).unwrap()[0]["namespace"],
            "travel"
        );
        assert_eq!(serde_json::to_value(&stream_payload.tools).unwrap(), public_tools);
        assert_eq!(serde_json::to_value(&json_payload.tools).unwrap(), public_tools);
        assert_eq!(
            serde_json::to_value(&stream_payload.tool_choice).unwrap(),
            serde_json::to_value(&json_payload.tool_choice).unwrap()
        );
    }

    pub(in crate::executor) fn request_context() -> RequestContext {
        let request = RequestPayload {
            model: "test".to_owned(),
            input: ResponsesInput::Text("hi".to_owned()),
            instructions: None,
            previous_response_id: None,
            conversation_id: None,
            tools: None,
            tool_choice: None,
            stream: true,
            store: false,
            include: None,
            reasoning: None,
            text: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            ignore_eos: None,
            truncation: None,
            metadata: None,
            parallel_tool_calls: None,
            cache_salt: None,
            context_management: None,
        };
        RequestContext {
            original_request: request.clone(),
            enriched_request: request,
            new_input_items: Vec::new(),
            response_id: "resp_test".to_owned(),
            conversation_id: None,
            conversation_version: None,
            continuation: None,
        }
    }

    #[test]
    fn executor_response_budget_is_shared_across_rounds() {
        let budget = ExecutorResponseBudget::new();
        budget
            .consume(crate::executor::response_budget::MAX_EXECUTOR_RESPONSE_BYTES / 2)
            .expect("first round should fit");
        budget
            .consume(crate::executor::response_budget::MAX_EXECUTOR_RESPONSE_BYTES / 2)
            .expect("second round should consume the budget");
        let error = budget.consume(1).expect_err("next round must exceed shared budget");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                ..
            }
        ));
    }

    async fn streaming_test_upstream(events: &[Value]) -> (ExecutionContext, tokio::task::JoinHandle<()>) {
        use std::fmt::Write as _;

        let mut body = String::new();
        for event in events {
            write!(&mut body, "data: {event}\n\n").unwrap();
        }
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move || {
                let body = body.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let context = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::builder().no_proxy().build().unwrap()),
            format!("http://{address}"),
        );
        (context, server)
    }

    #[tokio::test]
    async fn live_and_collect_only_streams_finalize_the_same_payload() {
        let item = serde_json::json!({"id":"fc_1","type":"function_call","call_id":"call_1","name":"raw_echo","arguments":"{\"input\":\"hello\"}","status":"completed"});
        let events = [
            serde_json::json!({"type":"response.created","response":{"id":"resp_upstream","status":"in_progress"}}),
            serde_json::json!({"type":"response.in_progress","response":{"id":"resp_upstream","status":"in_progress"}}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":item}),
            serde_json::json!({"type":"response.completed","response":{"id":"resp_upstream","status":"completed","output":[item],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}),
        ];
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let registry = ToolRegistry::from_tool_types(std::collections::HashMap::from([(
            "raw_echo".to_owned(),
            crate::tool::ToolType::Custom,
        )]));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut agent = agent_pipeline(request_context(), None, Some(sender));
        let live = fetch_stream_payload(
            &mut agent,
            &exec_ctx,
            None,
            &registry,
            0,
            &ExecutorResponseBudget::new(),
        )
        .await
        .unwrap();
        let mut agent = agent_pipeline(request_context(), None, None);
        let collected = fetch_stream_payload(
            &mut agent,
            &exec_ctx,
            None,
            &registry,
            0,
            &ExecutorResponseBudget::new(),
        )
        .await
        .unwrap();
        server.abort();
        let mut live_payload = serde_json::to_value(live.payload).unwrap();
        let mut collected_payload = serde_json::to_value(collected.payload).unwrap();
        live_payload.as_object_mut().unwrap().remove("created_at");
        collected_payload.as_object_mut().unwrap().remove("created_at");
        assert_eq!(live_payload, collected_payload);
        assert_eq!(live_payload["id"], agent.request.response_id);
        assert_eq!(live_payload["usage"]["total_tokens"], 5);
        assert!(live.deferred_events.is_empty());
        assert!(collected.deferred_events.is_empty());
        let mut emitted = String::new();
        while let Ok(event) = receiver.try_recv() {
            emitted.push_str(&event.content);
        }
        assert!(emitted.contains("custom_tool_call"));
        assert!(!emitted.contains("function_call_arguments"));
    }

    #[tokio::test]
    async fn collect_only_streams_enforce_live_translation_errors_and_limits() {
        let added = serde_json::json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"raw_echo","arguments":"","status":"in_progress"}});
        let contradiction = vec![
            added.clone(),
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"input\":\"first"}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,
                "item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"raw_echo","arguments":"{\"input\":\"different\"}","status":"completed"}}),
        ];
        let mut unnamed = added;
        unnamed["item"].as_object_mut().unwrap().remove("name");
        let oversized = vec![
            unnamed,
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"x".repeat(140 * 1024)}),
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"x".repeat(140 * 1024)}),
        ];
        let registry = ToolRegistry::from_tool_types(std::collections::HashMap::from([(
            "raw_echo".to_owned(),
            crate::tool::ToolType::Custom,
        )]));
        for (events, expected_error) in [
            (contradiction, "contradicts streamed custom tool input"),
            (oversized, "unnamed function-call SSE exceeded"),
        ] {
            let (exec_ctx, server) = streaming_test_upstream(&events).await;
            let mut errors = Vec::new();
            for emit in [false, true] {
                let (sender, _receiver) = tokio::sync::mpsc::channel(16);
                let mut agent = agent_pipeline(request_context(), None, emit.then_some(sender));
                let error = fetch_stream_payload(
                    &mut agent,
                    &exec_ctx,
                    None,
                    &registry,
                    0,
                    &ExecutorResponseBudget::new(),
                )
                .await
                .expect_err("translation must reject the stream");
                assert!(error.to_string().contains(expected_error), "{error}");
                errors.push(error.to_string());
            }
            server.abort();
            assert_eq!(errors[0], errors[1]);
        }
    }

    #[tokio::test]
    async fn streamed_response_reader_enforces_the_cumulative_budget() {
        use std::fmt::Write as _;

        let item_id = "msg_1";
        let delta = "x".repeat(300 * 1024);
        let mut response = String::new();
        let events = [
            serde_json::json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "status": "in_progress"
                }
            }),
            serde_json::json!({
                "type": "response.content_part.added",
                "output_index": 0,
                "item_id": item_id,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": item_id,
                "content_index": 0,
                "delta": delta
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": item_id,
                "content_index": 0,
                "delta": delta
            }),
        ];
        for event in &events {
            write!(&mut response, "data: {event}\n\n").expect("writing to a String cannot fail");
        }
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move || {
                let response = response.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], response) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind streaming response limit server");
        let address = listener.local_addr().expect("streaming response limit server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::builder().no_proxy().build().unwrap()),
            format!("http://{address}"),
        );
        let budget = ExecutorResponseBudget::with_limit(500 * 1024);

        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect_err("cumulative streamed response must be bounded");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                ..
            }
        ));
        server.abort();
    }

    fn synthetic_events(size: usize, chunk: usize) -> (Value, Vec<Value>) {
        let text = "x".repeat(size);
        let part = serde_json::json!({"type": "output_text", "text": text, "annotations": []});
        let item = serde_json::json!({
            "id": "msg_fixture",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [part]
        });
        let response = serde_json::json!({
            "id": "resp_fixture",
            "object": "response",
            "created_at": 1_789_151_000,
            "model": "fixture",
            "status": "completed",
            "output": [item],
            "error": null,
            "incomplete_details": null
        });
        let initial = serde_json::json!({
            "id": "resp_fixture",
            "object": "response",
            "created_at": 1_789_151_000,
            "model": "fixture",
            "status": "in_progress",
            "output": [],
            "error": null,
            "incomplete_details": null
        });
        let mut events = Vec::new();
        events.push(serde_json::json!({"type": "response.created", "response": initial}));
        events.push(serde_json::json!({"type": "response.in_progress", "response": initial}));
        events.push(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": "msg_fixture",
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }));
        events.push(serde_json::json!({
            "type": "response.content_part.added",
            "output_index": 0,
            "item_id": "msg_fixture",
            "content_index": 0,
            "part": {"type": "output_text", "text": ""}
        }));
        let mut at = 0;
        while at < size {
            let end = (at + chunk).min(size);
            events.push(serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": "msg_fixture",
                "content_index": 0,
                "delta": &text[at..end]
            }));
            at = end;
        }
        events.push(serde_json::json!({
            "type": "response.output_text.done",
            "output_index": 0,
            "item_id": "msg_fixture",
            "content_index": 0,
            "text": text
        }));
        events.push(serde_json::json!({
            "type": "response.content_part.done",
            "output_index": 0,
            "item_id": "msg_fixture",
            "content_index": 0,
            "part": {"type": "output_text", "text": text, "annotations": []}
        }));
        events.push(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": item
        }));
        events.push(serde_json::json!({
            "type": "response.completed",
            "response": response
        }));
        (response, events)
    }

    #[tokio::test]
    async fn small_and_large_sse_chunking_and_json_produce_identical_retained_bytes() {
        let (json_response, events_1byte) = synthetic_events(8000, 1);
        let (_, events_1024byte) = synthetic_events(8000, 1024);

        // 1. Stream with 1-byte chunks and 1 MiB budget
        // In the original bug (Issue #288), 8000 1-byte chunks failed after 7,655 bytes with a 1 MiB budget
        // because wire bytes charged 1,048,582 bytes. Now it succeeds!
        let budget_1byte = ExecutorResponseBudget::with_limit(1024 * 1024);
        let (exec_ctx, server) = streaming_test_upstream(&events_1byte).await;
        let mut agent = agent_pipeline(request_context(), None, None);
        let result_1byte =
            fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget_1byte)
                .await
                .expect("1-byte chunks must succeed under 1 MiB budget");
        server.abort();

        // 2. Stream with 1024-byte chunks
        let budget_1024byte = ExecutorResponseBudget::with_limit(1024 * 1024);
        let (exec_ctx, server) = streaming_test_upstream(&events_1024byte).await;
        let mut agent = agent_pipeline(request_context(), None, None);
        let result_1024byte = fetch_stream_payload(
            &mut agent,
            &exec_ctx,
            None,
            &ToolRegistry::default(),
            0,
            &budget_1024byte,
        )
        .await
        .expect("1024-byte chunks must succeed");
        server.abort();

        // 3. JSON body
        let budget_json = ExecutorResponseBudget::with_limit(1024 * 1024);
        let mut agent = agent_pipeline(request_context(), None, None);
        let result_json = agent
            .run_with_json_body(
                &json_response.to_string(),
                Validation::Strict,
                TranslationContext::default(),
                Some(budget_json.clone()),
            )
            .expect("JSON body must succeed");

        // Retained bytes charged MUST be identical across 1-byte chunks, 1024-byte chunks, and JSON!
        assert_eq!(budget_1byte.used(), budget_1024byte.used());
        assert_eq!(budget_1byte.used(), budget_json.used());
        // Verify payload equivalence
        assert_eq!(
            serde_json::to_value(&result_1byte.payload.output).unwrap(),
            serde_json::to_value(&result_1024byte.payload.output).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&result_1byte.payload.output).unwrap(),
            serde_json::to_value(&result_json.output).unwrap()
        );
    }

    #[tokio::test]
    async fn repeated_completion_snapshots_do_not_double_count_retained_bytes() {
        let (json_response, events) = synthetic_events(1000, 100);
        let budget = ExecutorResponseBudget::with_limit(1024 * 1024);
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let mut agent = agent_pipeline(request_context(), None, None);
        let result = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .unwrap();
        server.abort();

        let budget_json = ExecutorResponseBudget::with_limit(1024 * 1024);
        let mut agent = agent_pipeline(request_context(), None, None);
        let _ = agent
            .run_with_json_body(
                &json_response.to_string(),
                Validation::Strict,
                TranslationContext::default(),
                Some(budget_json.clone()),
            )
            .unwrap();

        assert_eq!(budget.used(), budget_json.used());
        assert_eq!(result.payload.output.len(), 1);
    }

    #[tokio::test]
    async fn streaming_response_larger_than_old_stream_event_cap_succeeds_under_default_config() {
        let (_response, events) = synthetic_events(2 * 1024 * 1024, 64 * 1024);
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
        let mut agent = agent_pipeline_with_limits(
            request_context(),
            None,
            Some(sender),
            exec_ctx.responses_config.max_stream_event_bytes,
        );
        let result = fetch_stream_payload(
            &mut agent,
            &exec_ctx,
            None,
            &ToolRegistry::default(),
            0,
            &ExecutorResponseBudget::new(),
        )
        .await
        .expect("2 MiB stream should succeed under default config");
        server.abort();
        assert_eq!(result.payload.output.len(), 1);
        let mut saw_output_item_done = false;
        while let Ok(event) = receiver.try_recv() {
            if event.content.contains("response.output_item.done") {
                saw_output_item_done = true;
            }
        }
        assert!(saw_output_item_done, "stream must emit response.output_item.done");
        let (_, mut accumulator) = agent.into_parts();
        let terminal_chunk = accumulator
            .terminal_response_chunk(&result.payload)
            .expect("terminal response chunk of 2 MiB succeeds under default limit");
        assert!(terminal_chunk.contains("response.completed"));
    }

    #[tokio::test]
    async fn lenient_done_without_output_item_done_reconciles_budget() {
        let text = "x".repeat(5000);
        let events = vec![
            serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_lenient", "status": "in_progress"}
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "msg_lenient",
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
            serde_json::json!({
                "type": "response.output_text.done",
                "output_index": 0,
                "item_id": "msg_lenient",
                "content_index": 0,
                "text": &text
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_lenient", "status": "completed", "output": []}
            }),
        ];
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let budget = ExecutorResponseBudget::with_limit(1024 * 1024);
        let mut agent = agent_pipeline(request_context(), None, None);
        let result = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect("lenient stream with done-only text should reconcile and succeed");
        server.abort();
        assert_eq!(result.payload.output.len(), 1);
        assert!(budget.used() >= 5000);
    }

    #[tokio::test]
    async fn lenient_done_without_output_item_done_exceeding_budget_fails_promptly() {
        use crate::executor::error::ResourceLimit;

        let text = "x".repeat(5000);
        // Note: No terminal response.completed. The stream must fail promptly on output_text.done
        // rather than at terminal stream completion.
        let events = vec![
            serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_lenient", "status": "in_progress"}
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "msg_lenient",
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
            serde_json::json!({
                "type": "response.output_text.done",
                "output_index": 0,
                "item_id": "msg_lenient",
                "content_index": 0,
                "text": &text
            }),
        ];
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let budget = ExecutorResponseBudget::with_limit(4000);
        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect_err("stream exceeding budget on done payload must fail promptly");
        server.abort();
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                max_bytes: 4000,
            }
        ));
    }

    fn two_part_stream_events(deltas: bool) -> Vec<serde_json::Value> {
        let (t0, k0, v0) = if deltas {
            ("response.output_text.delta", "delta", "part 0 text ")
        } else {
            ("response.output_text.done", "text", "part 0 text ")
        };
        let (t1, k1, v1) = if deltas {
            ("response.output_text.delta", "delta", "part 1 text")
        } else {
            ("response.output_text.done", "text", "part 1 text")
        };
        vec![
            serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_2parts", "status": "in_progress"}
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "msg_2parts",
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
            serde_json::json!({
                "type": t0,
                "output_index": 0,
                "item_id": "msg_2parts",
                "content_index": 0,
                k0: v0
            }),
            serde_json::json!({
                "type": t1,
                "output_index": 0,
                "item_id": "msg_2parts",
                "content_index": 1,
                k1: v1
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_2parts", "status": "completed", "output": []}
            }),
        ]
    }

    #[tokio::test]
    async fn two_part_done_only_stream_preserves_both_parts() {
        use crate::types::io::OutputItem;

        let events = two_part_stream_events(false);
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let budget = ExecutorResponseBudget::with_limit(1024 * 1024);
        let mut agent = agent_pipeline(request_context(), None, None);
        let result = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect("two-part done-only stream must succeed");
        server.abort();
        assert_eq!(result.payload.output.len(), 1);
        if let OutputItem::Message(msg) = &result.payload.output[0] {
            assert_eq!(msg.content.len(), 2);
            assert_eq!(msg.content[0].text, "part 0 text ");
            assert_eq!(msg.content[1].text, "part 1 text");
        } else {
            panic!("expected OutputItem::Message");
        }

        // Equivalent multi-part stream with deltas to assert budget invariance
        let delta_events = two_part_stream_events(true);
        let (exec_ctx_delta, server_delta) = streaming_test_upstream(&delta_events).await;
        let delta_budget = ExecutorResponseBudget::with_limit(1024 * 1024);
        let mut agent_delta = agent_pipeline(request_context(), None, None);
        let result_delta = fetch_stream_payload(
            &mut agent_delta,
            &exec_ctx_delta,
            None,
            &ToolRegistry::default(),
            0,
            &delta_budget,
        )
        .await
        .expect("two-part delta stream must succeed");
        server_delta.abort();

        assert_eq!(budget.used(), delta_budget.used());
        assert_eq!(
            serde_json::to_value(&result.payload.output).unwrap(),
            serde_json::to_value(&result_delta.payload.output).unwrap()
        );
    }

    #[tokio::test]
    async fn shared_budget_draws_down_across_stream_rounds() {
        let (_, events) = synthetic_events(400 * 1024, 64 * 1024);
        let budget = ExecutorResponseBudget::with_limit(500 * 1024);

        // Round 1 consumes ~400 KiB
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let mut agent = agent_pipeline(request_context(), None, None);
        let _ = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect("round 1 must fit within budget");
        server.abort();

        // Round 2 tries to consume another ~400 KiB and must exceed the 500 KiB budget
        let (exec_ctx, server) = streaming_test_upstream(&events).await;
        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .expect_err("round 2 must exceed shared budget");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                ..
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn raw_sse_line_limit_is_enforced() {
        use std::fmt::Write as _;
        let mut response = String::new();
        let huge_line = "x".repeat(1000);
        write!(
            &mut response,
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{huge_line}\"}}\n\n"
        )
        .unwrap();

        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move || {
                let response = response.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], response) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let mut exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::builder().no_proxy().build().unwrap()),
            format!("http://{address}"),
        );
        exec_ctx.responses_config.max_upstream_sse_line_bytes = 500;

        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_stream_payload(
            &mut agent,
            &exec_ctx,
            None,
            &ToolRegistry::default(),
            0,
            &ExecutorResponseBudget::new(),
        )
        .await
        .expect_err("line exceeding max_upstream_sse_line_bytes must fail");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::UpstreamSseLine,
                ..
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn raw_json_body_limit_is_enforced() {
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "x".repeat(2000),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let mut exec_ctx = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::builder().no_proxy().build().unwrap()),
            format!("http://{address}"),
        );
        exec_ctx.responses_config.max_upstream_json_bytes = 1000;

        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_blocking_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), None)
            .await
            .expect_err("body exceeding max_upstream_json_bytes must fail");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::UpstreamJsonBody,
                ..
            }
        ));
        server.abort();
    }
}
