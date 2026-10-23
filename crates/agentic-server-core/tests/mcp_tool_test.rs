use agentic_core::executor::ExecuteRequest;
use agentic_core::executor::accumulator::ResponseAccumulator;
use agentic_core::tool::ToolDeclaration;
use agentic_core::tool::{GatewayExecutors, ToolOwnership, ToolRegistry, ToolType};
use agentic_core::types::io::output::McpListTools;
use agentic_core::types::io::{ItemPage, McpCall, OutputItem, ToolChoice};
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::tools::ResponsesTool;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

mod support;

const MODEL: &str = "Qwen/Qwen3.5-35B-A3B-FP8";
const MCP_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/mcp");
const GATEWAY_MODEL_SLUG: &str = "Qwen-Qwen3.5-35B-A3B-FP8";
const OPENAI_MODEL_SLUG: &str = "gpt-4o";

fn load_mcp_cassette(filename: &str) -> support::Cassette {
    support::load_cassette(&format!("{MCP_DIR}/{filename}"))
}

fn load_scenario_pair(scenario: &str, streaming: bool) -> (support::Cassette, support::Cassette) {
    let mode = if streaming { "streaming" } else { "nonstreaming" };
    let openai = load_mcp_cassette(&format!(
        "mcp-openai-reference-counter-{scenario}-{OPENAI_MODEL_SLUG}-{mode}.yaml"
    ));
    let gateway = load_mcp_cassette(&format!(
        "mcp-gateway-counter-{scenario}-{GATEWAY_MODEL_SLUG}-{mode}.yaml"
    ));
    (openai, gateway)
}

fn native_mcp_declaration() -> ResponsesTool {
    serde_json::from_value(serde_json::json!({
        "type": "mcp",
        "server_label": "counter",
        "server_url": "http://127.0.0.1:8000/mcp",
        "allowed_tools": ["increment"],
        "require_approval": "never"
    }))
    .expect("native MCP declaration")
}

/// Minimal stateless MCP server over streamable HTTP exposing one `forecast` tool.
async fn spawn_forecast_mcp() -> String {
    let app = Router::new().route(
        "/mcp",
        post(|axum::Json(request): axum::Json<Value>| async move {
            let id = request["id"].clone();
            if id.is_null() {
                // A notification such as `notifications/initialized`.
                return StatusCode::ACCEPTED.into_response();
            }
            let reply = match request["method"].as_str() {
                Some("initialize") => json!({"result": {
                    "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub", "version": "0"}
                }}),
                Some("tools/list") => json!({"result": {"tools": [{
                    "name": "forecast", "description": "Forecast a city.",
                    "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}}
                }]}}),
                Some("tools/call") => json!({"result": {
                    "content": [{"type": "text", "text": "forecast result"}],
                    "isError": request["params"]["arguments"]["city"] == "Unavailable"
                }}),
                _ => json!({"error": {"code": -32601, "message": "method not found"}}),
            };
            let mut body = json!({"jsonrpc": "2.0", "id": id});
            body.as_object_mut().unwrap().extend(reply.as_object().unwrap().clone());
            axum::Json(body).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock MCP socket");
    let address = listener.local_addr().expect("mock MCP address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock MCP server") });
    format!("http://{address}/mcp")
}

/// The declaration a client submits, including the request-scoped credentials that the
/// gateway strips from every client-facing tools value.
fn forecast_declaration(server_url: &str) -> Value {
    json!([{
        "type": "mcp",
        "server_label": "stub",
        "server_url": server_url,
        "headers": {"Authorization": "Bearer header-secret"},
        "authorization": "field-secret",
        "allowed_tools": ["forecast"],
        "require_approval": "never"
    }])
}

fn forecast_request(stream: bool, server_url: &str) -> RequestPayload {
    serde_json::from_value(json!({
        "model": "test-model",
        "input": "forecast Tokyo",
        "stream": stream,
        "store": true,
        "tool_choice": "required",
        "tools": forecast_declaration(server_url)
    }))
    .expect("MCP request")
}

fn assert_public_forecast_metadata(value: &Value, server_url: &str, context: &str) {
    assert_eq!(
        value["tools"],
        json!([{
            "type": "mcp",
            "server_label": "stub",
            "server_url": server_url,
            "allowed_tools": ["forecast"],
            "require_approval": "never"
        }]),
        "{context} tools"
    );
    let client_tools = value["tools"].to_string();
    for internal in [
        "header-secret",
        "field-secret",
        "_agentic_discovered_tools",
        "mcp__stub__forecast",
    ] {
        assert!(
            !client_tools.contains(internal),
            "{internal} must never reach a client tools value: {context}"
        );
    }
}

/// Asserts the upstream receives exactly the lowered schema the discovered tool normalizes
/// to, without the credentials or the internal discovery result.
fn assert_lowered_forecast_upstream(body: &Value, context: &str) {
    let tools = body["tools"].as_array().expect("upstream tools");
    assert_eq!(tools.len(), 1, "{context} upstream tool count");
    assert_eq!(tools[0]["type"], "function", "{context} upstream tool kind");
    assert_eq!(tools[0]["name"], "mcp__stub__forecast", "{context} upstream tool name");
    assert_eq!(
        tools[0]["parameters"],
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        "{context} upstream tool parameters"
    );
    let serialized = body.to_string();
    for internal in ["header-secret", "field-secret", "_agentic_discovered_tools"] {
        assert!(
            !serialized.contains(internal),
            "{internal} must stay out of the upstream request: {context}"
        );
    }
}

#[tokio::test]
async fn mcp_declarations_stay_public_on_streaming_lifecycle_snapshots() {
    let mcp_url = spawn_forecast_mcp().await;
    let fixture = support::TestFixture::new_with_responses(vec![support::sse_omitting_tool_declarations(
        support::text_sse_response("Tokyo is mild."),
    )])
    .await;

    let payload = forecast_request(true, &mcp_url);
    let result = ExecuteRequest::new(payload, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .expect("MCP turn executes");
    let Either::Right(stream) = result else {
        panic!("expected a streaming response");
    };
    let chunks: Vec<String> = stream.collect().await;

    let mut snapshots = 0;
    for event in support::streamed_sse_events(&chunks) {
        let Some(response) = event.get("response") else {
            continue;
        };
        snapshots += 1;
        let context = event["type"].as_str().unwrap_or_default().to_owned();
        assert_public_forecast_metadata(response, &mcp_url, &context);
        assert_eq!(
            response["tool_choice"],
            json!("required"),
            "the submitted selector must be reported: {context}"
        );
    }
    assert!(snapshots >= 2, "expected creation and terminal snapshots");

    // Inference keeps the lowered schema the discovered tool normalizes to.
    let bodies = fixture.request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_lowered_forecast_upstream(&bodies[0], "streaming turn");
}

#[tokio::test]
async fn mcp_declarations_stay_public_in_json_and_stored_responses() {
    let mcp_url = spawn_forecast_mcp().await;
    let fixture = support::TestFixture::new_with_responses(vec![support::text_response("Tokyo is mild.")]).await;

    let payload = forecast_request(false, &mcp_url);
    let result = ExecuteRequest::new(payload, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .expect("MCP turn executes");
    let Either::Left(response) = result else {
        panic!("expected a non-streaming response");
    };

    assert_public_forecast_metadata(
        &serde_json::to_value(&response).expect("response serializes"),
        &mcp_url,
        "terminal payload",
    );
    assert_eq!(
        serde_json::to_value(&response.tool_choice).expect("selector serializes"),
        json!("required")
    );
    let stored = fixture
        .exec_ctx
        .resp_handler
        .retrieve(&response.id)
        .await
        .expect("stored response");
    assert_public_forecast_metadata(
        &serde_json::to_value(&stored).expect("stored response serializes"),
        &mcp_url,
        "stored response",
    );

    let bodies = fixture.request_bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_lowered_forecast_upstream(&bodies[0], "blocking turn");
}

fn forecast_inference_responses(stream: bool, city: &str) -> Vec<support::MockResponse> {
    let call = support::function_call_response(
        "fc_forecast",
        "call_forecast",
        "mcp__stub__forecast",
        &json!({"city": city}).to_string(),
    );
    if !stream {
        return vec![call, support::text_response("Forecast handled.")];
    }
    let support::MockResponse::Json(body) = call else {
        panic!("function call fixture is JSON");
    };
    let response: Value = serde_json::from_str(&body).unwrap();
    let item = &response["output"][0];
    let mut added = item.clone();
    added["status"] = json!("in_progress");
    added["arguments"] = json!("");
    let events = [
        json!({"type":"response.created", "response":{"id":response["id"], "status":"in_progress"}}),
        json!({"type":"response.in_progress", "response":{"id":response["id"], "status":"in_progress"}}),
        json!({"type":"response.output_item.added", "output_index":0, "item":added}),
        json!({"type":"response.function_call_arguments.delta", "output_index":0,
            "item_id":item["id"], "delta":item["arguments"]}),
        json!({"type":"response.function_call_arguments.done", "output_index":0,
            "item_id":item["id"], "arguments":item["arguments"]}),
        json!({"type":"response.output_item.done", "output_index":0, "item":item}),
        json!({"type":"response.completed", "response":response}),
    ];
    let mut sse = String::new();
    for event in events {
        use std::fmt::Write as _;
        write!(sse, "data: {event}\n\n").unwrap();
    }
    sse.push_str("data: [DONE]\n\n");
    vec![
        support::MockResponse::Sse(sse),
        support::text_sse_response("Forecast handled."),
    ]
}

async fn assert_continuation_tool_choice(fixture: &support::TestFixture, parent: &str) {
    for choice in [None, Some(ToolChoice::Auto), Some(ToolChoice::None)] {
        let mut request: RequestPayload = serde_json::from_value(json!({
            "model": "test-model", "input": "Continue", "previous_response_id": parent
        }))
        .unwrap();
        request.tool_choice = choice.clone();
        let ctx = agentic_core::executor::rehydrate_conversation(request, &fixture.exec_ctx)
            .await
            .unwrap();
        assert_eq!(
            ctx.enriched_request.tool_choice,
            Some(choice.unwrap_or(ToolChoice::Required)),
            "a new request must inherit the public selector or use its explicit override"
        );
        assert!(ctx.enriched_request.tools.is_some(), "omitted tools must still inherit");
    }
}

async fn assert_mcp_tool_choice_after_execution(stream: bool, city: &str, inherit_selector: bool) {
    let mcp_url = spawn_forecast_mcp().await;
    let mut responses = forecast_inference_responses(stream, city);
    if inherit_selector {
        responses.insert(0, support::text_response("Seed inherited tool settings."));
    }
    let fixture = support::TestFixture::new_with_responses(responses).await;
    let mut request = forecast_request(stream, &mcp_url);
    if inherit_selector {
        let seed = ExecuteRequest::new(forecast_request(false, &mcp_url), Arc::clone(&fixture.exec_ctx))
            .run()
            .await
            .unwrap();
        let Either::Left(seed) = seed else {
            panic!("seed request is non-streaming");
        };
        request.previous_response_id = Some(seed.id);
        request.tools = None;
        request.tool_choice = None;
    }
    let result = ExecuteRequest::new(request, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .expect("MCP execution completes");
    let response = match result {
        Either::Left(response) => serde_json::to_value(response).unwrap(),
        Either::Right(stream) => {
            let chunks: Vec<String> = stream.collect().await;
            let events = support::streamed_sse_events(&chunks);
            let lifecycle: Vec<&Value> = events.iter().filter(|event| event.get("response").is_some()).collect();
            assert_eq!(
                lifecycle
                    .iter()
                    .map(|event| event["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["response.created", "response.in_progress", "response.completed"]
            );
            for event in &lifecycle {
                assert_public_forecast_metadata(&event["response"], &mcp_url, event["type"].as_str().unwrap());
                assert_eq!(
                    event["response"]["tool_choice"],
                    json!("required"),
                    "{} selector",
                    event["type"]
                );
            }
            lifecycle.last().unwrap()["response"].clone()
        }
    };
    assert_public_forecast_metadata(&response, &mcp_url, "terminal response");
    assert_eq!(response["status"], "completed");
    assert_eq!(response["tool_choice"], json!("required"));
    let call = response["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "mcp_call")
        .unwrap();
    assert_eq!(
        call["status"],
        if city == "Unavailable" { "failed" } else { "completed" }
    );
    let stored = fixture
        .exec_ctx
        .resp_handler
        .retrieve(response["id"].as_str().unwrap())
        .await
        .unwrap();
    let stored = serde_json::to_value(stored).unwrap();
    assert_public_forecast_metadata(&stored, &mcp_url, "stored response");
    assert_eq!(stored["tool_choice"], json!("required"));
    assert_continuation_tool_choice(&fixture, response["id"].as_str().unwrap()).await;

    let bodies = fixture.request_bodies().await;
    let bodies = &bodies[usize::from(inherit_selector)..];
    assert_eq!(bodies.len(), 2, "the gateway must execute the tool and infer again");
    assert_eq!(bodies[0]["tool_choice"], json!("required"));
    assert!(
        bodies[1].get("tool_choice").is_none(),
        "the second inference must still relax the selector"
    );
    assert!(bodies[1]["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "function_call_output"
            && item["call_id"] == "call_forecast"
            && item["output"]
                .as_str()
                .is_some_and(|output| output.contains("forecast result"))
    }));
}

#[tokio::test]
async fn mcp_tool_choice_survives_execution_in_streaming_and_storage() {
    for city in ["Tokyo", "Unavailable"] {
        for inherit_selector in [false, true] {
            assert_mcp_tool_choice_after_execution(true, city, inherit_selector).await;
        }
    }
}

#[tokio::test]
async fn mcp_tool_choice_survives_execution_in_json_and_storage() {
    for city in ["Tokyo", "Unavailable"] {
        for inherit_selector in [false, true] {
            assert_mcp_tool_choice_after_execution(false, city, inherit_selector).await;
        }
    }
}

#[test]
fn native_mcp_declaration_uses_server_identity_without_a_tool_name() {
    let ResponsesTool::Mcp(param) = native_mcp_declaration() else {
        panic!("expected MCP declaration");
    };

    assert_eq!(param.server_label, "counter");
    assert_eq!(param.server_url.as_deref(), Some("http://127.0.0.1:8000/mcp"));
    assert_eq!(
        param.allowed_tools.as_deref(),
        Some(["increment".to_owned()].as_slice())
    );
    assert_eq!(param.require_approval.as_deref(), Some("never"));
}

#[test]
fn native_mcp_declaration_ignores_a_client_supplied_tool_name() {
    let tool = serde_json::from_value::<ResponsesTool>(serde_json::json!({
        "type": "mcp",
        "name": "increment",
        "server_label": "counter",
        "server_url": "http://127.0.0.1:8000/mcp"
    }))
    .expect("MCP declaration with an unknown field");

    let serialized = serde_json::to_value(tool).expect("serialized MCP declaration");
    assert_eq!(serialized["server_label"], "counter");
    assert!(serialized.get("name").is_none());
}

#[tokio::test]
async fn read_mcp_resource_function_is_client_owned() {
    let mut tools = vec![ToolDeclaration::from(
        serde_json::from_value::<ResponsesTool>(serde_json::json!({
            "type": "function",
            "name": "read_mcp_resource",
            "description": "A client-owned function with no gateway MCP semantics",
            "parameters": {"type": "object"},
            "metadata": {
                "server_label": "repo",
                "server_url": "http://127.0.0.1:8000/mcp"
            }
        }))
        .expect("function declaration"),
    )];
    let mut executors = GatewayExecutors::default();

    let registry = ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .expect("function registry");
    let entry = registry.lookup("read_mcp_resource").expect("function registry entry");

    assert_eq!(entry.tool_type, ToolType::Function);
    assert!(matches!(entry.ownership, ToolOwnership::Client));
}

fn assert_matching_native_mcp_requests(
    openai: &support::Cassette,
    gateway: &support::Cassette,
    streaming: bool,
    allowed_tools: Option<&[&str]>,
) {
    assert_eq!(openai.turns.len(), 1);
    assert_eq!(gateway.turns.len(), 1);

    let openai_request = &openai.turns[0].request;
    let gateway_request = &gateway.turns[0].request;
    let openai_server_url = openai_request.body.tools[0]["server_url"]
        .as_str()
        .expect("OpenAI MCP server_url");
    let gateway_server_url = gateway_request.body.tools[0]["server_url"]
        .as_str()
        .expect("gateway MCP server_url");
    assert!(
        openai_server_url.starts_with("https://"),
        "OpenAI MCP cassette requires a public HTTPS server_url"
    );
    assert!(
        gateway_server_url.starts_with("http://") || gateway_server_url.starts_with("https://"),
        "gateway MCP cassette requires an HTTP(S) server_url"
    );

    for request in [openai_request, gateway_request] {
        assert_eq!(request.path, "/v1/responses");
        assert_eq!(request.body.stream, streaming);
        assert_eq!(request.body.tools.len(), 1);

        let declaration = &request.body.tools[0];
        assert_eq!(declaration["type"], "mcp");
        assert_eq!(declaration["server_label"], "counter");
        assert_eq!(declaration["require_approval"], "never");
        assert!(declaration.get("name").is_none());

        match allowed_tools {
            Some(expected) => {
                let actual = declaration["allowed_tools"]
                    .as_array()
                    .expect("allowed_tools array")
                    .iter()
                    .map(|name| name.as_str().expect("allowed tool name"))
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected);
            }
            None => assert!(declaration.get("allowed_tools").is_none()),
        }
    }

    // OpenAI requires a public HTTPS endpoint, while the gateway can use a local
    // or otherwise separately reachable endpoint. Compare the MCP declarations
    // without coupling the reference cassettes to the same recorded URL.
    let mut openai_declaration = openai_request.body.tools[0].clone();
    let mut gateway_declaration = gateway_request.body.tools[0].clone();
    openai_declaration
        .as_object_mut()
        .expect("OpenAI MCP declaration")
        .remove("server_url");
    gateway_declaration
        .as_object_mut()
        .expect("gateway MCP declaration")
        .remove("server_url");
    assert_eq!(openai_declaration, gateway_declaration);
    assert_eq!(openai_request.body.tool_choice, gateway_request.body.tool_choice);

    let turn = &gateway.turns[0];
    let snapshots = if let Some(body) = &turn.response.body {
        vec![body.clone()]
    } else {
        streaming_events(turn)
            .into_iter()
            .filter_map(|event| event.get("response").cloned())
            .collect()
    };
    assert_eq!(snapshots.len(), if streaming { 3 } else { 1 });
    for snapshot in snapshots {
        assert_eq!(snapshot["tools"], json!(gateway_request.body.tools));
        assert_eq!(snapshot["tool_choice"], json!(gateway_request.body.tool_choice));
    }
}

fn streaming_events(turn: &support::Turn) -> Vec<Value> {
    support::recorded_named_sse_events(turn)
}

fn response_output(turn: &support::Turn) -> Vec<OutputItem> {
    let response = if let Some(body) = &turn.response.body {
        body.clone()
    } else {
        streaming_events(turn)
            .into_iter()
            .rev()
            .filter_map(|event| event.get("response").cloned())
            .find(|response| response["status"] == "completed" && response["output"].is_array())
            .expect("completed streaming response payload")
    };
    let accumulator = ResponseAccumulator::from_json(&response.to_string(), None).expect("valid completed response");
    let payload = accumulator.finalize(MODEL, None, None);
    assert_eq!(payload.status, "completed");
    payload.output
}

fn output_text(output: &[OutputItem]) -> String {
    output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Message(message) => Some(
                message
                    .content
                    .iter()
                    .map(agentic_core::types::io::OutputMessageContent::text)
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn mcp_calls(output: &[OutputItem]) -> Vec<&McpCall> {
    assert!(
        !output.iter().any(|item| matches!(item, OutputItem::FunctionCall(_))),
        "internal function calls must not leak from the gateway"
    );
    output
        .iter()
        .filter_map(|item| match item {
            OutputItem::McpCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn mcp_list_tools(output: &[OutputItem]) -> &McpListTools {
    let items = output
        .iter()
        .filter_map(|item| match item {
            OutputItem::McpListTools(list_tools) => Some(list_tools),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(items.len(), 1, "response should contain one mcp_list_tools item");
    items[0]
}

fn normalized_mcp_list_tools_item(item: &Value) -> Value {
    json!({
        "server_label": item["server_label"],
        "tools": item["tools"],
        "error": item.get("error").cloned().unwrap_or(Value::Null),
    })
}

fn assert_list_tools_output_matches_openai(openai: &[OutputItem], gateway: &[OutputItem]) {
    let expected = mcp_list_tools(openai);
    let actual = mcp_list_tools(gateway);
    assert!(expected.id.starts_with("mcpl_"));
    assert!(actual.id.starts_with("mcpl_"));

    let expected = serde_json::to_value(expected).expect("OpenAI mcp_list_tools JSON");
    let actual = serde_json::to_value(actual).expect("gateway mcp_list_tools JSON");
    assert_eq!(
        normalized_mcp_list_tools_item(&actual),
        normalized_mcp_list_tools_item(&expected)
    );
}

fn normalized_json_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

fn normalized_optional_output(output: Option<&str>) -> Value {
    output.map_or(Value::Null, normalized_json_string)
}

fn assert_calls_match_openai(openai: &[OutputItem], gateway: &[OutputItem], compare_arguments: bool) {
    assert_list_tools_output_matches_openai(openai, gateway);

    let expected = mcp_calls(openai);
    let actual = mcp_calls(gateway);
    assert_eq!(actual.len(), expected.len());

    for (expected, actual) in expected.into_iter().zip(actual) {
        assert_eq!(actual.server_label, expected.server_label);
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.status, expected.status);
        assert_eq!(
            normalized_optional_output(actual.output.as_deref()),
            normalized_optional_output(expected.output.as_deref())
        );
        assert_eq!(
            serde_json::to_value(&actual.error).expect("gateway MCP error JSON"),
            serde_json::to_value(&expected.error).expect("OpenAI MCP error JSON")
        );
        if compare_arguments {
            assert_eq!(
                normalized_json_string(&actual.arguments),
                normalized_json_string(&expected.arguments)
            );
        } else {
            assert!(normalized_json_string(&actual.arguments).is_object());
            assert!(normalized_json_string(&expected.arguments).is_object());
        }
    }
}

fn assert_mcp_list_tools_lifecycle(events: &[Value]) -> Value {
    let sequence_numbers = events
        .iter()
        .map(|event| {
            event["sequence_number"]
                .as_u64()
                .unwrap_or_else(|| panic!("event missing sequence_number: {event}"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequence_numbers,
        (0..u64::try_from(events.len()).expect("event count fits in u64")).collect::<Vec<_>>()
    );

    let lifecycle = events
        .iter()
        .filter(|event| {
            event["type"]
                .as_str()
                .is_some_and(|event_type| event_type.starts_with("response.mcp_list_tools."))
                || matches!(
                    event["type"].as_str(),
                    Some("response.output_item.added" | "response.output_item.done")
                ) && event["item"]["type"] == "mcp_list_tools"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect::<Vec<_>>(),
        [
            "response.output_item.added",
            "response.mcp_list_tools.in_progress",
            "response.mcp_list_tools.completed",
            "response.output_item.done",
        ]
    );

    let item_id = lifecycle[0]["item"]["id"]
        .as_str()
        .expect("mcp_list_tools added item id");
    assert!(item_id.starts_with("mcpl_"));
    assert!(
        lifecycle
            .iter()
            .all(|event| { event["item"]["id"].as_str().or_else(|| event["item_id"].as_str()) == Some(item_id) })
    );

    let output_index = &lifecycle[0]["output_index"];
    assert!(lifecycle.iter().all(|event| &event["output_index"] == output_index));
    assert_eq!(lifecycle[0]["item"]["tools"], json!([]));
    assert_eq!(lifecycle[0]["item"]["server_label"], "counter");

    let done_item = &lifecycle[3]["item"];
    assert_eq!(done_item["server_label"], "counter");
    assert!(!done_item["tools"].as_array().expect("discovered tools").is_empty());

    let terminal_item = events
        .iter()
        .rev()
        .find(|event| event["type"] == "response.completed")
        .and_then(|event| event["response"]["output"].as_array())
        .and_then(|output| output.iter().find(|item| item["type"] == "mcp_list_tools"))
        .expect("terminal response mcp_list_tools item");
    assert_eq!(terminal_item, done_item);

    done_item.clone()
}

fn mcp_call_event_traces(events: &[Value]) -> Vec<(String, Vec<String>)> {
    let mut traces: HashMap<String, (String, Vec<String>)> = HashMap::new();

    for event in events {
        let Some(event_type) = event["type"].as_str() else {
            continue;
        };
        let item = event.get("item");
        let mcp_item = item.filter(|item| item["type"] == "mcp_call");
        let item_id = mcp_item
            .and_then(|item| item["id"].as_str())
            .or_else(|| event["item_id"].as_str());
        let Some(item_id) = item_id else {
            continue;
        };

        if let Some(item) = mcp_item {
            let name = item["name"].as_str().expect("mcp_call name").to_owned();
            traces.entry(item_id.to_owned()).or_insert_with(|| (name, Vec::new()));
        }
        if event_type.starts_with("response.mcp_call")
            || matches!(event_type, "response.output_item.added" | "response.output_item.done") && mcp_item.is_some()
        {
            traces
                .get_mut(item_id)
                .expect("mcp_call added before lifecycle events")
                .1
                .push(event_type.to_owned());
        }
    }

    let mut traces = traces.into_values().collect::<Vec<_>>();
    traces.sort_by(|left, right| left.0.cmp(&right.0));
    traces
}

fn normalized_mcp_item_transitions(events: &[Value]) -> HashMap<String, Vec<Value>> {
    let mut transitions: HashMap<String, Vec<Value>> = HashMap::new();
    for event in events {
        let Some(event_type) = event["type"].as_str() else {
            continue;
        };
        if !matches!(event_type, "response.output_item.added" | "response.output_item.done") {
            continue;
        }
        let Some(item) = event.get("item") else {
            continue;
        };
        if item["type"] != "mcp_call" {
            continue;
        }
        let name = item["name"].as_str().expect("mcp_call name").to_owned();
        transitions.entry(name).or_default().push(json!({
            "event_type": event_type,
            "type": item["type"],
            "server_label": item["server_label"],
            "name": item["name"],
            "status": item["status"],
            "arguments": item["arguments"].as_str().map(normalized_json_string),
            "output": item["output"].as_str().map(normalized_json_string),
            "error": item["error"],
            "approval_request_id": item["approval_request_id"],
        }));
    }
    transitions
}

fn assert_streaming_contract_matches_openai(openai: &support::Turn, gateway: &support::Turn) {
    let expected = streaming_events(openai);
    let actual = streaming_events(gateway);
    let expected_list_tools = assert_mcp_list_tools_lifecycle(&expected);
    let actual_list_tools = assert_mcp_list_tools_lifecycle(&actual);
    assert_eq!(
        normalized_mcp_list_tools_item(&actual_list_tools),
        normalized_mcp_list_tools_item(&expected_list_tools)
    );
    assert_eq!(mcp_call_event_traces(&actual), mcp_call_event_traces(&expected));
    assert_eq!(
        normalized_mcp_item_transitions(&actual),
        normalized_mcp_item_transitions(&expected)
    );
    assert!(actual.iter().all(|event| {
        !event["type"]
            .as_str()
            .is_some_and(|kind| kind.contains("mcp_tool_call"))
    }));
}

#[test]
fn mcp_tool_listing_matches_openai_without_calls() {
    let (openai, gateway) = load_scenario_pair("list-tools", true);
    assert_matching_native_mcp_requests(&openai, &gateway, true, None);

    let openai_output = response_output(&openai.turns[0]);
    let gateway_output = response_output(&gateway.turns[0]);
    assert_calls_match_openai(&openai_output, &gateway_output, true);
    assert_streaming_contract_matches_openai(&openai.turns[0], &gateway.turns[0]);

    for text in [output_text(&openai_output), output_text(&gateway_output)] {
        for tool_name in ["increment", "get_value", "sum"] {
            assert!(
                text.contains(tool_name),
                "tool listing should contain {tool_name}: {text}"
            );
        }
    }
}

#[test]
fn successful_streaming_mcp_calls_match_openai() {
    let (openai, gateway) = load_scenario_pair("call-sum-and-echo", true);
    assert_matching_native_mcp_requests(&openai, &gateway, true, Some(&["sum", "echo"]));

    let openai_output = response_output(&openai.turns[0]);
    let gateway_output = response_output(&gateway.turns[0]);
    assert_calls_match_openai(&openai_output, &gateway_output, true);
    assert_streaming_contract_matches_openai(&openai.turns[0], &gateway.turns[0]);
    assert_eq!(output_text(&gateway_output), output_text(&openai_output));
}

#[test]
fn missing_argument_mcp_failure_matches_openai() {
    let (openai, gateway) = load_scenario_pair("sum-missing-argument", true);
    assert_matching_native_mcp_requests(&openai, &gateway, true, Some(&["sum"]));

    let openai_output = response_output(&openai.turns[0]);
    let gateway_output = response_output(&gateway.turns[0]);
    assert_calls_match_openai(&openai_output, &gateway_output, true);
    assert_streaming_contract_matches_openai(&openai.turns[0], &gateway.turns[0]);
}

#[test]
fn invalid_argument_type_mcp_failure_matches_openai() {
    let (openai, gateway) = load_scenario_pair("sum-invalid-argument-type", true);
    assert_matching_native_mcp_requests(&openai, &gateway, true, Some(&["sum"]));

    let openai_output = response_output(&openai.turns[0]);
    let gateway_output = response_output(&gateway.turns[0]);
    assert_calls_match_openai(&openai_output, &gateway_output, true);
    assert_streaming_contract_matches_openai(&openai.turns[0], &gateway.turns[0]);
}

#[test]
fn successful_blocking_mcp_call_matches_openai() {
    let (openai, gateway) = load_scenario_pair("say-hello", false);
    assert_matching_native_mcp_requests(&openai, &gateway, false, Some(&["say_hello"]));

    let openai_output = response_output(&openai.turns[0]);
    let gateway_output = response_output(&gateway.turns[0]);
    // Arguments are model-generated. Qwen adds an ignored placeholder field
    // while GPT-4o sends `{}`; both execute the same zero-argument MCP tool.
    assert_calls_match_openai(&openai_output, &gateway_output, false);
    assert_eq!(output_text(&gateway_output), output_text(&openai_output));
}

#[tokio::test]
async fn mcp_input_items_return_stored_call_and_result() {
    let mcp_url = spawn_forecast_mcp().await;
    let fixture = support::TestFixture::new_with_responses(forecast_inference_responses(false, "Paris")).await;
    let Either::Left(response) = ExecuteRequest::new(forecast_request(false, &mcp_url), Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .unwrap()
    else {
        panic!("expected JSON response")
    };
    let page = fixture
        .exec_ctx
        .resp_handler
        .list_input_items(&response.id, ItemPage::default())
        .await
        .unwrap();
    let page = serde_json::to_value(page).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 3);
    assert_eq!(page["data"][0]["type"], "function_call_output");
    assert_eq!(page["data"][1]["type"], "function_call");
    assert_eq!(page["data"][2]["type"], "message");
    let bodies = fixture.request_bodies().await;
    for kind in ["function_call", "function_call_output"] {
        assert!(
            bodies[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == kind && item["call_id"] == "call_forecast")
        );
    }
}
