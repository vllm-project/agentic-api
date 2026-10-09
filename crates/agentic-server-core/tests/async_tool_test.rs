//! Gateway support for async client tools (issue #332), with a constructed upstream.
//!
//! The expected public behavior comes from the `OpenAI` reference recordings and the upstream request
//! shape from the `vLLM` recordings in `tests/cassettes/async_tools`; `async_tool_cassette_test.rs`
//! checks those recordings. Here a mock model server plays the `vLLM` side: it never marks calls
//! async and stops at each call, so the gateway must run the continuation round itself.

use std::fmt::Write as _;
use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, ExecutionContext, ExecutorError};
use agentic_core::tool::ToolError;
use agentic_core::tool::async_execution::{DESCRIPTION_SUFFIX, pending_call_note};
use agentic_core::types::io::OutputItem;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

const WEATHER_TOOL: &str = "get_weather";

fn weather_tool(async_execution: bool) -> Value {
    let mut tool = json!({
        "type": "function", "name": WEATHER_TOOL, "description": "Read a demo weather snapshot for a city.",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
    });
    if async_execution {
        tool["async"] = json!(true);
    }
    tool
}

fn request(stream: bool, input: Value, tools: Value, previous_response_id: Option<&str>) -> RequestPayload {
    let mut body = json!({
        "model": "test-model", "store": true, "stream": stream, "previous_response_id": previous_response_id
    });
    body["input"] = input;
    body["tools"] = tools;
    serde_json::from_value(body).unwrap()
}

fn call_item(name: &str, call_id: &str, arguments: &str) -> Value {
    json!({"type": "function_call", "id": format!("fc_{call_id}"), "call_id": call_id, "name": name,
        "arguments": arguments, "status": "completed"})
}

fn message_item(id: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

/// One upstream response with these output items, as JSON or as an SSE item lifecycle.
fn model_response(stream: bool, items: &[Value]) -> support::MockResponse {
    if !stream {
        return support::MockResponse::Json(
            json!({"id": "resp_upstream", "object": "response", "model": "test-model",
                "status": "completed", "output": items})
            .to_string(),
        );
    }
    // Like vLLM, echo the model-visible declaration: the hint in its description, no `async`.
    let echoed_tools = json!([{"type": "function", "name": WEATHER_TOOL,
        "description": format!("Read a demo weather snapshot for a city.{DESCRIPTION_SUFFIX}")}]);
    let mut events = vec![
        json!({"type": "response.created",
            "response": {"id": "resp_upstream", "status": "in_progress", "tools": echoed_tools}}),
        json!({"type": "response.in_progress",
            "response": {"id": "resp_upstream", "status": "in_progress", "tools": echoed_tools}}),
    ];
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        events.push(json!({"type": "response.output_item.added", "output_index": index, "item": added}));
        events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
    }
    events.push(json!({"type": "response.completed",
        "response": {"id": "resp_upstream", "status": "completed", "output": items}}));
    let mut body = String::new();
    for event in events {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    support::MockResponse::Sse(body)
}

/// Runs one request; streaming responses also return their public events.
async fn run(request: RequestPayload, ctx: Arc<ExecutionContext>) -> (ResponsePayload, Vec<Value>) {
    match ExecuteRequest::new(request, ctx).run().await.unwrap() {
        Either::Left(response) => (response, Vec::new()),
        Either::Right(stream) => {
            let chunks = stream.collect::<Vec<_>>().await;
            let events = support::streamed_sse_events(&chunks);
            let response = events
                .iter()
                .find(|event| event["type"] == "response.completed")
                .expect("completed response")["response"]
                .clone();
            (serde_json::from_value(response).unwrap(), events)
        }
    }
}

async fn run_error(request: RequestPayload, ctx: Arc<ExecutionContext>) -> ExecutorError {
    match ExecuteRequest::new(request, ctx).run().await {
        Err(error) => error,
        Ok(_) => panic!("request should be rejected"),
    }
}

fn items(body: &Value) -> &[Value] {
    body["input"].as_array().map_or(&[], Vec::as_slice)
}

fn developer_notes(body: &Value) -> Vec<&str> {
    items(body)
        .iter()
        .filter(|item| item["role"] == "developer")
        .map(|item| item["content"].as_str().expect("text note"))
        .collect()
}

fn function_output(call_id: &str, output: &str) -> Value {
    json!({"type": "function_call_output", "call_id": call_id, "output": output})
}

/// The client sees its own declaration: `async: true` and no upstream hint in the description.
fn assert_public_tools(tools: &Value) {
    let tool = &tools[0];
    assert_eq!(tool["async"], true, "public declaration: {tools}");
    assert!(
        !tool["description"]
            .as_str()
            .unwrap_or_default()
            .contains(DESCRIPTION_SUFFIX.trim()),
        "the upstream hint is not returned: {tools}"
    );
}

/// The public stream completes the async call, marked `async: true`, before the answer starts.
fn assert_async_call_streams_before_the_answer(events: &[Value]) {
    let lifecycle: Vec<&Value> = events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            )
        })
        .collect();
    let call_done = lifecycle
        .iter()
        .position(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call")
        .expect("call completes in the stream");
    assert_eq!(lifecycle[call_done]["item"]["async"], true);
    let call_added = lifecycle
        .iter()
        .position(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "function_call")
        .expect("call starts in the stream");
    assert_eq!(
        lifecycle[call_added]["item"]["async"], true,
        "the call is marked async from its first event"
    );
    let message_added = lifecycle
        .iter()
        .position(|event| event["item"]["type"] == "message")
        .expect("answer streams");
    assert!(
        call_done < message_added,
        "the call is complete before the answer starts"
    );
}

/// The recorded `OpenAI` flow, served by a model that stops at the call: the gateway continues the
/// response with the call pending and the hints applied, accepts follow-ups without the output,
/// and accepts the late output on the original `call_id`.
#[tokio::test]
async fn async_call_continues_the_response_and_accepts_a_late_output() {
    for stream in [false, true] {
        let fixture = support::TestFixture::new_with_responses(vec![
            model_response(stream, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
            model_response(stream, &[message_item("msg_1", "Essentials: shoes, bag, charger.")]),
            model_response(stream, &[message_item("msg_2", "A compact folding umbrella.")]),
            model_response(stream, &[message_item("msg_3", "Paris is clear and 22 C.")]),
        ])
        .await;
        let tools = json!([weather_tool(true)]);

        let (first, events) = run(
            request(
                stream,
                json!("Check Paris; meanwhile list essentials."),
                tools.clone(),
                None,
            ),
            fixture.exec_ctx.clone(),
        )
        .await;
        assert_eq!(first.status, "completed", "stream={stream}");
        let [OutputItem::FunctionCall(call), OutputItem::Message(_)] = first.output.as_slice() else {
            panic!(
                "stream={stream}: expected the async call and an answer, got {:?}",
                first.output
            );
        };
        assert!(call.async_execution, "stream={stream}: the public call is marked async");
        assert_eq!(call.call_id, "call_1");
        assert_public_tools(&serde_json::to_value(&first.tools).unwrap());
        for event in events.iter().filter(|event| event["response"]["tools"].is_array()) {
            assert_public_tools(&event["response"]["tools"]);
        }

        let bodies = fixture.request_bodies().await;
        assert_eq!(
            bodies.len(),
            2,
            "stream={stream}: the gateway ran the continuation round"
        );
        for body in &bodies {
            let tool = &body["tools"][0];
            assert!(tool.get("async").is_none(), "async is not sent upstream");
            assert!(tool["description"].as_str().unwrap().ends_with(DESCRIPTION_SUFFIX));
        }
        assert!(developer_notes(&bodies[0]).is_empty(), "no call is pending yet");
        let continuation = items(&bodies[1]);
        let upstream_call = continuation
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("pending call replayed");
        assert!(upstream_call.get("async").is_none(), "the marker is not sent upstream");
        assert_eq!(
            developer_notes(&bodies[1]),
            [pending_call_note(WEATHER_TOOL, "call_1").as_str()],
            "the note follows the pending call"
        );
        assert_eq!(continuation.last().unwrap()["role"], "developer");

        if stream {
            assert_async_call_streams_before_the_answer(&events);
        }

        // A follow-up without the output is accepted; the call stays pending upstream.
        let (second, _) = run(
            request(
                stream,
                json!("Also, which umbrella size?"),
                tools.clone(),
                Some(&first.id),
            ),
            fixture.exec_ctx.clone(),
        )
        .await;
        assert_eq!(second.status, "completed");
        let bodies = fixture.request_bodies().await;
        assert_eq!(
            developer_notes(&bodies[2]),
            [pending_call_note(WEATHER_TOOL, "call_1").as_str()]
        );

        // The late output resolves the original call; no note remains.
        let (third, _) = run(
            request(
                stream,
                json!([function_output("call_1", r#"{"temperature_c":22}"#),
                    {"type": "message", "role": "user", "content": "What is the weather?"}]),
                tools.clone(),
                Some(&second.id),
            ),
            fixture.exec_ctx.clone(),
        )
        .await;
        assert_eq!(support::output_text(&third), "Paris is clear and 22 C.");
        let bodies = fixture.request_bodies().await;
        assert!(developer_notes(&bodies[3]).is_empty());
        assert!(
            items(&bodies[3])
                .iter()
                .any(|item| item["type"] == "function_call_output")
        );
    }
}

/// Recorded `OpenAI` behavior: repeated outputs for an async call are accepted, and every output
/// reaches the model; an output for an unknown `call_id` is rejected.
#[tokio::test]
async fn repeated_outputs_are_accepted_and_unknown_outputs_rejected() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(false, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
        model_response(false, &[message_item("msg_1", "Started.")]),
        model_response(false, &[message_item("msg_2", "Two snapshots arrived.")]),
        model_response(false, &[message_item("msg_3", "Same again.")]),
    ])
    .await;
    let tools = json!([weather_tool(true)]);
    let (first, _) = run(
        request(false, json!("Check Paris."), tools.clone(), None),
        fixture.exec_ctx.clone(),
    )
    .await;

    let (second, _) = run(
        request(
            false,
            json!([function_output("call_1", "22 C"), function_output("call_1", "9 C")]),
            tools.clone(),
            Some(&first.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    let (third, _) = run(
        request(
            false,
            json!([function_output("call_1", "22 C")]),
            tools.clone(),
            Some(&second.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(support::output_text(&third), "Same again.");
    let bodies = fixture.request_bodies().await;
    let outputs = |body: &Value| {
        items(body)
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count()
    };
    assert_eq!(outputs(&bodies[2]), 2, "both outputs reach the model");
    assert_eq!(outputs(&bodies[3]), 3, "a repeat in a later request is kept as well");

    let error = run_error(
        request(
            false,
            json!([function_output("call_unknown", "{}")]),
            tools,
            Some(&third.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert!(
        matches!(&error, ExecutorError::Tool(ToolError::UnknownCallOutput { call_id }) if call_id == "call_unknown"),
        "{error}"
    );
}

/// A synchronous call next to an async one ends the response, as in the recorded `OpenAI`
/// responses; the synchronous call must be answered before continuing, the async one need not be.
#[tokio::test]
async fn a_synchronous_call_still_ends_the_response() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(
            false,
            &[
                call_item(WEATHER_TOOL, "call_async", r#"{"city":"Paris"}"#),
                call_item("get_local_time", "call_sync", r#"{"city":"Tokyo"}"#),
            ],
        ),
        model_response(false, &[message_item("msg_1", "Tokyo is 18:30.")]),
    ])
    .await;
    let tools = json!([weather_tool(true), {"type": "function", "name": "get_local_time"}]);
    let (first, _) = run(
        request(false, json!("Weather and time."), tools.clone(), None),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(fixture.request_bodies().await.len(), 1, "no continuation round");
    let marks: Vec<bool> = first
        .output
        .iter()
        .map(|item| matches!(item, OutputItem::FunctionCall(call) if call.async_execution))
        .collect();
    assert_eq!(marks, [true, false]);

    let error = run_error(
        request(false, json!("Anything else?"), tools.clone(), Some(&first.id)),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert!(matches!(&error, ExecutorError::Tool(ToolError::MissingOutput { call_id }) if call_id == "call_sync"));

    let (second, _) = run(
        request(
            false,
            json!([function_output("call_sync", "18:30")]),
            tools,
            Some(&first.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(support::output_text(&second), "Tokyo is 18:30.");
}

#[tokio::test]
async fn async_custom_tool_calls_keep_their_public_marker() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(
            false,
            &[call_item("echo", "call_custom", r#"{"input":"ASYNC_CUSTOM_OK"}"#)],
        ),
        model_response(false, &[message_item("msg_1", "Paris is the capital of France.")]),
    ])
    .await;
    let (response, _) = run(
        request(
            false,
            json!("Start the echo job; meanwhile, name the capital of France."),
            json!([{"type": "custom", "name": "echo", "async": true}]),
            None,
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    let [OutputItem::CustomToolCall(call), OutputItem::Message(_)] = response.output.as_slice() else {
        panic!(
            "expected the async custom call and an answer, got {:?}",
            response.output
        );
    };
    assert!(call.async_execution);
    assert_eq!(call.input, "ASYNC_CUSTOM_OK");
}

/// Recorded `OpenAI` behavior: `async` on a hosted tool is an unknown parameter, rejected before
/// any inference.
#[tokio::test]
async fn async_on_a_hosted_tool_is_rejected_before_inference() {
    let fixture = support::TestFixture::new_with_responses(vec![]).await;
    let error = run_error(
        request(
            false,
            json!("Search for Lisbon."),
            json!([{"type": "web_search", "async": true}]),
            None,
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert!(
        matches!(&error, ExecutorError::Tool(ToolError::UnknownParameter { param }) if param == "tools[0].async"),
        "{error}"
    );
    assert!(fixture.request_bodies().await.is_empty());
}

// ── compaction keeps pending async calls without duplicating them ────────────

const COMPACT_EVERY_TURN: &str = r#"[{"type": "compaction", "compact_threshold": 1}]"#;

fn compacting(mut request: RequestPayload) -> RequestPayload {
    request.context_management = serde_json::from_str(COMPACT_EVERY_TURN).unwrap();
    request
}

/// Upstream requests that run inference, not the compaction summary calls.
fn inference_bodies(bodies: &[Value]) -> Vec<&Value> {
    bodies.iter().filter(|body| body.get("tools").is_some()).collect()
}

/// The model input carries the pending call exactly once, before its output when one is present.
fn assert_call_once_before_output(body: &Value, call_id: &str) {
    let positions: Vec<usize> = items(body)
        .iter()
        .enumerate()
        .filter(|(_, item)| item["type"] == "function_call" && item["call_id"] == call_id)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "the call appears once: {body}");
    if let Some(output) = items(body)
        .iter()
        .position(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
    {
        assert!(positions[0] < output, "the call precedes its output");
    }
}

/// An async call made in one response survives compaction in a later response: the stored history
/// keeps one copy of the call, and the late output in a third response finds it.
#[tokio::test]
async fn compaction_in_a_later_response_keeps_the_pending_async_call() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(false, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
        model_response(false, &[message_item("msg_1", "Started the lookup.")]),
        support::text_response("Summary: a Paris weather lookup is pending."),
        model_response(false, &[message_item("msg_2", "A compact umbrella.")]),
        model_response(false, &[message_item("msg_3", "Paris is clear and 22 C.")]),
    ])
    .await;
    let tools = json!([weather_tool(true)]);
    let (first, _) = run(
        request(false, json!("Check Paris."), tools.clone(), None),
        fixture.exec_ctx.clone(),
    )
    .await;
    let (second, _) = run(
        compacting(request(false, json!("Which umbrella?"), tools.clone(), Some(&first.id))),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(second.status, "completed");

    let (third, _) = run(
        request(
            false,
            json!([function_output("call_1", r#"{"temperature_c":22}"#)]),
            tools,
            Some(&second.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(support::output_text(&third), "Paris is clear and 22 C.");
    let bodies = fixture.request_bodies().await;
    let inference = inference_bodies(&bodies);
    assert_call_once_before_output(inference.last().unwrap(), "call_1");
}

/// Compaction between the async round and the continuation round of the same response keeps the
/// call in the model input once, and the stored response does not repeat it.
#[tokio::test]
async fn compaction_inside_the_response_keeps_the_pending_async_call() {
    for stream in [false, true] {
        let fixture = support::TestFixture::new_with_responses(vec![
            support::text_response("Summary: the user asked about Paris."),
            model_response(stream, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
            support::text_response("Summary: a Paris weather lookup is pending."),
            model_response(stream, &[message_item("msg_1", "Started the lookup.")]),
            model_response(stream, &[message_item("msg_2", "Paris is clear and 22 C.")]),
        ])
        .await;
        let tools = json!([weather_tool(true)]);
        let (first, _) = run(
            compacting(request(stream, json!("Check Paris."), tools.clone(), None)),
            fixture.exec_ctx.clone(),
        )
        .await;
        assert_eq!(first.status, "completed", "stream={stream}");
        let bodies = fixture.request_bodies().await;
        let inference = inference_bodies(&bodies);
        assert_eq!(inference.len(), 2, "stream={stream}: async round and continuation");
        assert_call_once_before_output(inference[1], "call_1");
        assert_eq!(
            developer_notes(inference[1]),
            [pending_call_note(WEATHER_TOOL, "call_1").as_str()]
        );

        let (second, _) = run(
            request(
                stream,
                json!([function_output("call_1", r#"{"temperature_c":22}"#)]),
                tools,
                Some(&first.id),
            ),
            fixture.exec_ctx.clone(),
        )
        .await;
        assert_eq!(support::output_text(&second), "Paris is clear and 22 C.");
        let bodies = fixture.request_bodies().await;
        assert_call_once_before_output(inference_bodies(&bodies).last().unwrap(), "call_1");
    }
}

/// The explicit compact endpoint returns a window that keeps the pending async call in its public
/// form, and a continuation of the stored compaction accepts the late output.
#[tokio::test]
async fn explicit_compaction_keeps_the_pending_async_call() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(false, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
        model_response(false, &[message_item("msg_1", "Started the lookup.")]),
        support::text_response("Summary: a Paris weather lookup is pending."),
        model_response(false, &[message_item("msg_2", "Paris is clear and 22 C.")]),
    ])
    .await;
    let tools = json!([weather_tool(true)]);
    let (first, _) = run(
        request(false, json!("Check Paris."), tools.clone(), None),
        fixture.exec_ctx.clone(),
    )
    .await;

    let compacted = agentic_core::executor::compact_response(
        serde_json::from_value(json!({"model": "test-model", "previous_response_id": first.id})).unwrap(),
        &fixture.exec_ctx,
        None,
    )
    .await
    .expect("compaction succeeds");
    let window = serde_json::to_value(&compacted.output).unwrap();
    let kept: Vec<&Value> = window
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    assert_eq!(kept.len(), 1, "the window keeps the pending call: {window}");
    assert_eq!(kept[0]["async"], true);

    let (second, _) = run(
        request(
            false,
            json!([function_output("call_1", r#"{"temperature_c":22}"#)]),
            tools,
            Some(&compacted.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(support::output_text(&second), "Paris is clear and 22 C.");
    let bodies = fixture.request_bodies().await;
    assert_call_once_before_output(inference_bodies(&bodies).last().unwrap(), "call_1");
}

/// A `compaction_trigger` turn (Codex remote compaction) keeps the pending async call too.
#[tokio::test]
async fn compaction_trigger_keeps_the_pending_async_call() {
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(false, &[call_item(WEATHER_TOOL, "call_1", r#"{"city":"Paris"}"#)]),
        model_response(false, &[message_item("msg_1", "Started the lookup.")]),
        support::text_response("Summary: a Paris weather lookup is pending."),
        model_response(false, &[message_item("msg_2", "Paris is clear and 22 C.")]),
    ])
    .await;
    let tools = json!([weather_tool(true)]);
    let (first, _) = run(
        request(false, json!("Check Paris."), tools.clone(), None),
        fixture.exec_ctx.clone(),
    )
    .await;
    let (compacted, _) = run(
        request(
            false,
            json!([{"type": "compaction_trigger"}]),
            tools.clone(),
            Some(&first.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert!(matches!(compacted.output.as_slice(), [OutputItem::Compaction(_)]));

    let (second, _) = run(
        request(
            false,
            json!([function_output("call_1", r#"{"temperature_c":22}"#)]),
            tools,
            Some(&compacted.id),
        ),
        fixture.exec_ctx.clone(),
    )
    .await;
    assert_eq!(support::output_text(&second), "Paris is clear and 22 C.");
    let bodies = fixture.request_bodies().await;
    assert_call_once_before_output(inference_bodies(&bodies).last().unwrap(), "call_1");
}

/// An async namespace member is called by its flattened model-visible name; the public call keeps
/// its member name and namespace, and the upstream note names the call as the model sees it.
#[tokio::test]
async fn async_namespace_member_calls_are_marked_and_noted() {
    let flat_name = agentic_core::tool::codex::model_visible_namespace_member_name("travel", "book");
    let fixture = support::TestFixture::new_with_responses(vec![
        model_response(false, &[call_item(&flat_name, "call_book", r#"{"city":"Paris"}"#)]),
        model_response(false, &[message_item("msg_1", "Booking started.")]),
    ])
    .await;
    let tools = json!([{
        "type": "namespace", "name": "travel", "description": "Travel tools.",
        "tools": [{"type": "function", "name": "book", "async": true,
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}]
    }]);
    let (response, _) = run(
        request(false, json!("Book Paris."), tools, None),
        fixture.exec_ctx.clone(),
    )
    .await;
    let [OutputItem::FunctionCall(call), OutputItem::Message(_)] = response.output.as_slice() else {
        panic!(
            "expected the async member call and an answer, got {:?}",
            response.output
        );
    };
    assert_eq!(call.name, "book");
    assert_eq!(call.namespace.as_deref(), Some("travel"));
    assert!(call.async_execution);

    let bodies = fixture.request_bodies().await;
    assert_eq!(
        developer_notes(&bodies[1]),
        [pending_call_note(&flat_name, "call_book").as_str()]
    );
    assert!(
        bodies[1]["tools"][0]["description"]
            .as_str()
            .unwrap()
            .ends_with(DESCRIPTION_SUFFIX.trim_start()),
        "a member without a description gets the hint alone"
    );
}
