use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::StreamEvent;
use crate::executor::inference::{call_inference, fetch_response_json};
use crate::executor::pipeline::{AgentPipeline, StreamPayload};
use crate::executor::rehydrate::validate_message_files;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::translate::TranslationContext;
use crate::tool::{ToolRegistry, ToolSearchState};
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;
use futures::StreamExt;
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

pub(super) async fn fetch_blocking_payload(
    agent: &mut AgentPipeline,
    exec_ctx: &ExecutionContext,
    auth: Option<&str>,
    registry: &ToolRegistry,
    response_budget: Option<&ExecutorResponseBudget>,
) -> ExecutorResult<ResponsePayload> {
    agent.ensure_request_prepared()?;
    let upstream_json = upstream_request(&agent.request, false)?;
    let body = fetch_response_json(upstream_json, &exec_ctx.responses_url(), &exec_ctx.client, auth).await?;
    if let Some(response_budget) = response_budget {
        response_budget.consume(body.len())?;
    }
    agent.run_with_json_body(&body, Validation::Lenient, translation_context(registry, agent))
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
            agent.run_with_json_body(body, Validation::Strict, TranslationContext::default())?
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
    let lines = call_inference(
        upstream_json,
        exec_ctx.responses_url(),
        Arc::clone(&exec_ctx.client),
        auth.map(str::to_owned),
        exec_ctx.streaming_timeout,
    )
    .map(|line| {
        let line = line?;
        response_budget.consume(line.len())?;
        Ok(line)
    });
    agent
        .run_with_stream_body(
            lines,
            Validation::Lenient,
            translation_context(registry, agent),
            registry,
            output_offset,
        )
        .await
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::events::SseLine;
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
        let mut pipeline = RoundIngestion::new("resp_1".to_owned(), None, Validation::Lenient, context);
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

        let mut ingestion = RoundIngestion::new("resp_1".to_owned(), None, Validation::Lenient, stream_context);
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
            .run_with_json_body(&body, Validation::Lenient, json_context)
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
        assert!(error.to_string().contains("executor response budget exceeded"));
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
            Arc::new(reqwest::Client::new()),
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
                .err()
                .expect("translation must reject the stream");
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

        let data = "x".repeat(200 * 1024);
        let mut response = String::new();
        for sequence_number in 0..6 {
            write!(
                &mut response,
                "data: {{\"type\":\"test.event\",\"sequence_number\":{sequence_number},\"delta\":\"{data}\"}}\n\n"
            )
            .expect("writing to a String cannot fail");
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
            Arc::new(reqwest::Client::new()),
            format!("http://{address}"),
        );
        let budget = ExecutorResponseBudget::new();

        let mut agent = agent_pipeline(request_context(), None, None);
        let error = fetch_stream_payload(&mut agent, &exec_ctx, None, &ToolRegistry::default(), 0, &budget)
            .await
            .err()
            .expect("cumulative streamed response must be bounded");
        assert!(error.to_string().contains("executor response budget exceeded"));
        server.abort();
    }
}
