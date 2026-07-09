//! Cassette-based integration tests for the Responses API (cases 1–5).
//!
//! Mirrors `test_responses_api.py`. Each test replays a YAML cassette
//! against a mock HTTP server and verifies `execute()` output.

mod support;

use agentic_core::executor::execute;
use agentic_core::executor::request::RequestContext;
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::tools::{FunctionToolParam, NonEmptyToolName};
use agentic_core::{FunctionToolResultMessage, InputItem, ResponsesInput, ResponsesTool, ToolChoice};
use either::Either;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use support::{
    MockResponse, TestFixture, collect_stream, expected_text, load_cassette, make_request, output_text,
    request_input_texts, text_response, unwrap_blocking,
};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/text_only/responses");

/// Case 1 — single turn, non-streaming.
#[tokio::test]
async fn test_single_turn_nonstreaming() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/resp-single-gpt-4o-nonstreaming.yaml"));
    let t1 = &cassette.turns[0];
    let fixture = TestFixture::new(&[t1]).await;

    // Act
    let payload = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, t1.request.body.store, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("execute"),
    );

    // Assert
    assert!(payload.id.starts_with("resp_"), "id={}", payload.id);
    assert_eq!(payload.status, "completed");
    assert_eq!(output_text(&payload), expected_text(t1));
}

/// Case 2 — single turn, streaming.
#[tokio::test]
async fn test_single_turn_streaming() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/resp-single-gpt-4o-streaming.yaml"));
    let t1 = &cassette.turns[0];
    let fixture = TestFixture::new(&[t1]).await;

    // Act
    let payload = collect_stream(
        execute(
            make_request(&t1.request.body.input, t1.request.body.store, true, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("execute"),
    )
    .await;

    // Assert
    assert!(payload.id.starts_with("resp_"), "id={}", payload.id);
    assert_eq!(payload.status, "completed");
    assert_eq!(output_text(&payload), expected_text(t1));
}

#[tokio::test]
async fn test_single_turn_streaming_emits_response_completed_event() {
    let cassette = load_cassette(&format!("{DIR}/resp-single-gpt-4o-streaming.yaml"));
    let t1 = &cassette.turns[0];
    let fixture = TestFixture::new(&[t1]).await;

    let result = execute(
        make_request(&t1.request.body.input, t1.request.body.store, true, None, None),
        Arc::clone(&fixture.exec_ctx),
    )
    .await
    .expect("execute");
    let Either::Right(stream) = result else {
        panic!("expected streaming response");
    };
    let chunks = stream.collect::<Vec<_>>().await;
    let events = chunks
        .iter()
        .filter_map(|chunk| {
            let data = chunk.trim_end_matches('\n').strip_prefix("data: ")?;
            (data != "[DONE]").then(|| serde_json::from_str::<Value>(data).expect("stream event JSON"))
        })
        .collect::<Vec<_>>();

    assert!(
        !events
            .iter()
            .any(|event| event.get("object").and_then(Value::as_str) == Some("response")),
        "executor stream should not emit a bare ResponsePayload"
    );
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"response.created"));
    assert!(event_types.contains(&"response.output_text.delta"));
    let completed = events.last().expect("stream should include events");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        expected_text(t1)
    );
}

/// Case 3 — two turns, non-streaming, chained via `previous_response_id`.
#[tokio::test]
async fn test_two_turn_nonstreaming_previous_response_id() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/resp-two-turn-gpt-4o-nonstreaming.yaml"));
    let (t1, t2) = (&cassette.turns[0], &cassette.turns[1]);
    let fixture = TestFixture::new(&[t1, t2]).await;

    // Act
    let p1 = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, true, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t1"),
    );
    let p2 = unwrap_blocking(
        execute(
            make_request(&t2.request.body.input, true, false, Some(p1.id.clone()), None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t2"),
    );

    // Assert
    assert!(p1.id.starts_with("resp_"));
    assert_eq!(p1.status, "completed");
    assert_eq!(output_text(&p1), expected_text(t1));
    assert_ne!(p2.id, p1.id);
    assert_eq!(p2.status, "completed");
    assert_eq!(p2.previous_response_id.as_deref(), Some(p1.id.as_str()));
    assert_eq!(output_text(&p2), expected_text(t2));
}

/// Case 4 — two turns, streaming, chained via `previous_response_id`.
#[tokio::test]
async fn test_two_turn_streaming_previous_response_id() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/resp-two-turn-gpt-4o-streaming.yaml"));
    let (t1, t2) = (&cassette.turns[0], &cassette.turns[1]);
    let fixture = TestFixture::new(&[t1, t2]).await;

    // Act
    let p1 = collect_stream(
        execute(
            make_request(&t1.request.body.input, true, true, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t1"),
    )
    .await;
    let p2 = collect_stream(
        execute(
            make_request(&t2.request.body.input, true, true, Some(p1.id.clone()), None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t2"),
    )
    .await;

    // Assert
    assert!(p1.id.starts_with("resp_"));
    assert_eq!(p1.status, "completed");
    assert_eq!(output_text(&p1), expected_text(t1));
    assert_ne!(p2.id, p1.id);
    assert_eq!(p2.status, "completed");
    assert_eq!(output_text(&p2), expected_text(t2));
}

/// Case 5 — `store=false` response cannot be used as `previous_response_id`.
#[tokio::test]
async fn test_store_disabled_not_reusable_as_previous_response_id() {
    // Arrange — only one mock needed; follow-up errors before hitting the LLM
    let cassette = load_cassette(&format!("{DIR}/resp-no-store-gpt-4o-nonstreaming.yaml"));
    let t1 = &cassette.turns[0];
    let fixture = TestFixture::new(&[t1]).await;

    // Act — turn 1, store=false
    let p1 = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, false, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t1"),
    );
    assert_eq!(p1.status, "completed");

    // Act — follow-up with the unstored id
    let result = execute(
        make_request("follow up", false, false, Some(p1.id.clone()), None),
        Arc::clone(&fixture.exec_ctx),
    )
    .await;

    // Assert — executor errors at rehydrate, before calling the LLM
    assert!(result.is_err(), "expected error for unstored previous_response_id");
}

#[tokio::test]
async fn test_previous_response_id_rehydrates_full_checkpoint_history() {
    let fixture = TestFixture::new_with_responses(vec![
        text_response("first answer"),
        text_response("second answer"),
        text_response("third answer"),
    ])
    .await;

    let p1 = unwrap_blocking(
        execute(
            make_request("turn 1", true, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t1"),
    );
    let p2 = unwrap_blocking(
        execute(
            make_request("turn 2", true, false, Some(p1.id.clone()), None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t2"),
    );
    let p3 = unwrap_blocking(
        execute(
            make_request("turn 3", true, false, Some(p2.id), None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("t3"),
    );

    assert_eq!(output_text(&p3), "third answer");
    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        request_input_texts(&requests[2]),
        vec!["turn 1", "first answer", "turn 2", "second answer", "turn 3"]
    );
}

#[tokio::test]
async fn test_codex_namespace_tool_shape_rehydrates_from_previous_response_metadata() {
    let fixture = TestFixture::new_with_responses(vec![
        text_response("seed answer"),
        text_response("next answer"),
        text_response("third answer"),
    ])
    .await;
    let tool_json = serde_json::json!([
        {
            "type": "namespace",
            "name": "mcp__shell",
            "tools": [{"type": "function", "name": "run", "parameters": {"type": "object"}}]
        }
    ]);
    let tools: Vec<ResponsesTool> = serde_json::from_value(tool_json.clone()).unwrap();

    let mut first = make_request("seed", true, false, None, None);
    first.tools = Some(tools);
    let p1 = unwrap_blocking(execute(first, Arc::clone(&fixture.exec_ctx)).await.expect("first turn"));

    let second = make_request("next", true, false, Some(p1.id), None);
    let p2 = unwrap_blocking(
        execute(second, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("second turn"),
    );
    let third = make_request("third", true, false, Some(p2.id), None);
    let _p3 = unwrap_blocking(execute(third, Arc::clone(&fixture.exec_ctx)).await.expect("third turn"));

    let requests = fixture.request_bodies().await;
    for request in &requests {
        let tools = request["tools"].as_array().expect("typed upstream tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "agentic_ns__mcp__shell__run");
        assert_eq!(tools[0]["parameters"], tool_json[0]["tools"][0]["parameters"]);
    }
}

#[tokio::test]
async fn test_codex_namespace_collision_with_top_level_function_is_rejected() {
    let fixture = TestFixture::new_with_responses(vec![text_response("should not be called")]).await;
    let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
        {"type": "function", "name": "agentic_ns__mcp__shell__run"},
        {
            "type": "namespace",
            "name": "mcp__shell",
            "tools": [{"type": "function", "name": "run"}]
        }
    ]))
    .unwrap();

    let mut request = make_request("run pwd", true, false, None, None);
    request.tools = Some(tools);

    let Err(err) = execute(request, Arc::clone(&fixture.exec_ctx)).await else {
        panic!("colliding namespace member should be rejected");
    };

    assert!(
        err.to_string().contains("collides with top-level function"),
        "unexpected error: {err}"
    );
    assert!(
        fixture.request_bodies().await.is_empty(),
        "invalid request must fail before calling upstream"
    );
}

#[tokio::test]
async fn test_previous_response_id_explicit_tool_choice_overrides_stored_choice() {
    let fixture =
        TestFixture::new_with_responses(vec![text_response("seed answer"), text_response("next answer")]).await;

    let mut first = make_request("seed", true, false, None, None);
    first.tool_choice = Some(ToolChoice::Required);
    let p1 = unwrap_blocking(execute(first, Arc::clone(&fixture.exec_ctx)).await.expect("first turn"));

    let mut second = make_request("next", true, false, Some(p1.id), None);
    second.tool_choice = Some(ToolChoice::None);
    let _p2 = unwrap_blocking(
        execute(second, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("second turn"),
    );

    let requests = fixture.request_bodies().await;
    assert_eq!(requests[0]["tool_choice"], "required");
    assert_eq!(requests[1]["tool_choice"], "none");
}

#[tokio::test]
async fn test_previous_response_id_rehydrates_function_call_before_tool_output() {
    let tool_call_response = MockResponse::Json(
        serde_json::json!({
            "id": "resp_tool",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [{
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "run",
                "namespace": "mcp__shell",
                "arguments": "{\"cmd\":\"pwd\"}",
                "status": "completed"
            }],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    );
    let fixture = TestFixture::new_with_responses(vec![tool_call_response, text_response("tool result handled")]).await;

    let first = make_request("run pwd", true, false, None, None);
    let p1 = unwrap_blocking(execute(first, Arc::clone(&fixture.exec_ctx)).await.expect("first turn"));

    let mut second = make_request("ignored", true, false, Some(p1.id), None);
    second.input = ResponsesInput::Items(vec![InputItem::FunctionCallOutput(FunctionToolResultMessage {
        call_id: "call_1".to_string(),
        output: "{\"stdout\":\"/workspace\"}".to_string(),
    })]);
    let _p2 = unwrap_blocking(
        execute(second, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("second turn"),
    );

    let requests = fixture.request_bodies().await;
    let input = requests[1]["input"].as_array().expect("input array");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["namespace"], "mcp__shell");
    assert_eq!(input[1]["name"], "run");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_1");
}

#[tokio::test]
async fn test_mcp_namespace_showcase_round_trip_rehydrates_calls_tools_and_outputs() {
    let tool_json = mcp_showcase_tools_json();
    let tools: Vec<ResponsesTool> = serde_json::from_value(tool_json.clone()).expect("tool fixture parses");
    let tool_call_response = MockResponse::Json(
        serde_json::json!({
            "id": "resp_mcp_showcase",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [
                upstream_mcp_fixture_call("fc_echo", "call_echo", "echo_text", r#"{"text":"namespace showcase","uppercase":true}"#),
                upstream_mcp_fixture_call("fc_sum", "call_sum", "add_numbers", r#"{"numbers":[2,3,5]}"#),
                upstream_mcp_fixture_call("fc_slug", "call_slug", "make_slug", r#"{"text":"Codex MCP Showcase"}"#),
                upstream_mcp_fixture_call("fc_head", "call_head", "repo_file_head", r#"{"path":"README.md","lines":2}"#),
                upstream_mcp_fixture_call("fc_search", "call_search", "search_repo", r#"{"query":"codex","path_prefix":"scripts","max_results":3}"#)
            ],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": null,
            "instructions": null
        })
        .to_string(),
    );
    let fixture = TestFixture::new_with_responses(vec![tool_call_response, text_response("showcase complete")]).await;

    let mut first = make_request("use the agentic_fixture MCP toolbox", true, false, None, None);
    first.tools = Some(tools);
    let p1 = unwrap_blocking(execute(first, Arc::clone(&fixture.exec_ctx)).await.expect("first turn"));

    let output = serde_json::to_value(&p1.output).expect("output serializes");
    assert_namespaced_calls(
        output.as_array().expect("output array"),
        &["echo_text", "add_numbers", "make_slug", "repo_file_head", "search_repo"],
    );

    let mut second = make_request("ignored", true, false, Some(p1.id), None);
    second.input = ResponsesInput::Items(vec![
        tool_output(
            "call_echo",
            r#"{"echo":"NAMESPACE SHOWCASE","characters":18,"words":2}"#,
        ),
        tool_output("call_sum", r#"{"count":3,"sum":10}"#),
        tool_output("call_slug", r#"{"slug":"codex-mcp-showcase"}"#),
        tool_output(
            "call_head",
            "README.md first 2 lines:\n1: # agentic-api\n2: Stateful API logic",
        ),
        tool_output(
            "call_search",
            r#"{"query":"codex","matches":[{"path":"scripts/codex-run.sh","line":16}]}"#,
        ),
    ]);
    let p2 = unwrap_blocking(
        execute(second, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("second turn"),
    );

    assert_eq!(output_text(&p2), "showcase complete");
    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert_flat_mcp_showcase_tools(&requests[0]["tools"]);
    assert_flat_mcp_showcase_tools(&requests[1]["tools"]);

    let input = requests[1]["input"].as_array().expect("rehydrated input array");
    assert_namespaced_calls(
        input,
        &["echo_text", "add_numbers", "make_slug", "repo_file_head", "search_repo"],
    );
    assert_tool_outputs(
        input,
        &["call_echo", "call_sum", "call_slug", "call_head", "call_search"],
    );
    assert!(
        !contains_key(&requests[1], "_agentic_item_kind"),
        "storage marker must not leak into rehydrated upstream request"
    );
}

#[tokio::test]
async fn test_store_false_with_previous_response_id_hydrates_but_does_not_persist() {
    let fixture =
        TestFixture::new_with_responses(vec![text_response("stored answer"), text_response("stateless answer")]).await;

    let p1 = unwrap_blocking(
        execute(
            make_request("seed", true, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("stored turn"),
    );
    let p2 = unwrap_blocking(
        execute(
            make_request("follow up", false, false, Some(p1.id), None),
            Arc::clone(&fixture.exec_ctx),
        )
        .await
        .expect("store=false follow-up"),
    );

    assert_eq!(output_text(&p2), "stateless answer");
    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_input_texts(&requests[1]),
        vec!["seed", "stored answer", "follow up"]
    );

    let result = execute(
        make_request("should not find stateless response", true, false, Some(p2.id), None),
        Arc::clone(&fixture.exec_ctx),
    )
    .await;
    assert!(result.is_err(), "store=false response should not be persisted");
}

#[tokio::test]
async fn test_previous_response_id_persists_inherited_tools_and_choice() {
    let fixture =
        TestFixture::new_with_responses(vec![text_response("seed answer"), text_response("follow up answer")]).await;

    let tool = ResponsesTool::Function(FunctionToolParam {
        name: NonEmptyToolName::try_from("lookup_weather").expect("valid tool name"),
        description: Some("Look up weather".to_string()),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"}
            }
        })),
        strict: Some(true),
        defer_loading: None,
        extra: std::collections::HashMap::new(),
    });

    let mut first_request = make_request("seed", true, false, None, None);
    first_request.tools = Some(vec![tool]);
    first_request.tool_choice = Some(ToolChoice::Required);

    let p1 = unwrap_blocking(
        execute(first_request, Arc::clone(&fixture.exec_ctx))
            .await
            .expect("seed turn"),
    );

    let mut second_request = make_request("follow up", true, false, Some(p1.id.clone()), None);
    second_request.tools = None;
    second_request.tool_choice = None;

    let p2 = unwrap_blocking(
        execute(second_request.clone(), Arc::clone(&fixture.exec_ctx))
            .await
            .expect("follow-up turn"),
    );

    assert_eq!(output_text(&p2), "follow up answer");

    let lookup_ctx = RequestContext {
        original_request: RequestPayload {
            previous_response_id: Some(p2.id.clone()),
            ..second_request
        },
        enriched_request: RequestPayload {
            previous_response_id: Some(p2.id.clone()),
            ..make_request("lookup", true, false, None, None)
        },
        new_input_items: vec![],
        response_id: "resp_lookup".into(),
        conversation_id: None,
    };

    let stored = fixture
        .exec_ctx
        .resp_handler
        .get(&lookup_ctx)
        .await
        .expect("fetch persisted response");

    assert_eq!(stored.metadata.model, "test-model");
    assert!(matches!(stored.metadata.effective_tool_choice, ToolChoice::Required));

    let tools = stored.metadata.effective_tools.expect("expected persisted tools");
    assert_eq!(tools.len(), 1);
    match &tools[0] {
        ResponsesTool::Function(p) => {
            assert_eq!(p.name.as_str(), "lookup_weather");
            assert_eq!(p.description.as_deref(), Some("Look up weather"));
            assert_eq!(p.strict, Some(true));
            assert_eq!(p.parameters.as_ref().and_then(|v| v["type"].as_str()), Some("object"));
        }
        _ => panic!("expected function tool"),
    }
}

#[tokio::test]
async fn test_conversation_id_and_previous_response_id_are_rejected_together() {
    let fixture = TestFixture::new_with_responses(vec![]).await;

    let result = execute(
        make_request(
            "ambiguous",
            true,
            false,
            Some("resp_ambiguous".to_string()),
            Some("conv_ambiguous".to_string()),
        ),
        Arc::clone(&fixture.exec_ctx),
    )
    .await;

    assert!(result.is_err(), "expected ambiguous state IDs to be rejected");
    assert!(fixture.request_bodies().await.is_empty());
}

fn mcp_showcase_tools_json() -> Value {
    serde_json::json!([
        {
            "type": "namespace",
            "name": "mcp__agentic_fixture",
            "description": "Fixture namespace tool for Codex MCP round-trip tests.",
            "tools": [
                {
                    "type": "function",
                    "name": "run",
                    "description": "Echo a command string for namespace round-trip validation.",
                    "parameters": {
                        "type": "object",
                        "properties": {"cmd": {"type": "string"}},
                        "required": ["cmd"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "type": "function",
                    "name": "echo_text",
                    "description": "Echo text with basic metadata.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "uppercase": {"type": "boolean"}
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "type": "function",
                    "name": "add_numbers",
                    "description": "Add a list of numbers and return the total.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "numbers": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 1
                            }
                        },
                        "required": ["numbers"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "type": "function",
                    "name": "make_slug",
                    "description": "Turn text into a lowercase URL/file-name friendly slug.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "separator": {"type": "string"}
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "type": "function",
                    "name": "repo_file_head",
                    "description": "Read the first lines of a repository file.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "lines": {"type": "integer", "minimum": 1, "maximum": 80}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    },
                    "strict": true
                },
                {
                    "type": "function",
                    "name": "search_repo",
                    "description": "Literal text search across repository files.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "path_prefix": {"type": "string"},
                            "max_results": {"type": "integer", "minimum": 1, "maximum": 30}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    },
                    "strict": true
                }
            ]
        }
    ])
}

fn upstream_mcp_fixture_call(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    serde_json::json!({
        "id": id,
        "type": "function_call",
        "call_id": call_id,
        "name": format!("agentic_ns__mcp__agentic_fixture__{name}"),
        "arguments": arguments,
        "status": "completed"
    })
}

fn tool_output(call_id: &str, output: &str) -> InputItem {
    InputItem::FunctionCallOutput(FunctionToolResultMessage {
        call_id: call_id.to_string(),
        output: output.to_string(),
    })
}

fn assert_namespaced_calls(items: &[Value], expected_names: &[&str]) {
    for expected_name in expected_names {
        assert!(
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item.get("namespace").and_then(Value::as_str) == Some("mcp__agentic_fixture")
                    && item.get("name").and_then(Value::as_str) == Some(expected_name)
            }),
            "missing namespaced function call mcp__agentic_fixture.{expected_name}"
        );
    }
}

fn assert_tool_outputs(items: &[Value], expected_call_ids: &[&str]) {
    for expected_call_id in expected_call_ids {
        assert!(
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(expected_call_id)
            }),
            "missing function_call_output for {expected_call_id}"
        );
    }
}

fn assert_flat_mcp_showcase_tools(tools: &Value) {
    let tools = tools.as_array().expect("tools array");
    assert_eq!(tools.len(), 6);
    for name in [
        "run",
        "echo_text",
        "add_numbers",
        "make_slug",
        "repo_file_head",
        "search_repo",
    ] {
        let flat_name = format!("agentic_ns__mcp__agentic_fixture__{name}");
        assert!(
            tools.iter().any(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("function")
                    && tool.get("name").and_then(Value::as_str) == Some(flat_name.as_str())
            }),
            "missing flat upstream tool {flat_name}"
        );
    }
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => object.contains_key(key) || object.values().any(|nested| contains_key(nested, key)),
        Value::Array(values) => values.iter().any(|nested| contains_key(nested, key)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}
