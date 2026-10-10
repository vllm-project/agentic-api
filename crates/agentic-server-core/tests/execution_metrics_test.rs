//! Execution metrics through the real executor against mock and recorded
//! upstreams, captured by a per-test in-memory meter provider.
//!
//! No `tracing` subscriber is installed in this binary: every assertion
//! here holds without a tracer, which is what makes the metrics independent
//! of the trace sampling decision.

use std::fmt::Write as _;
use std::sync::Arc;

use agentic_core::executor::{
    BoxStream, ExecuteRequest, ExecutionContext, ExecutorResult, MessagesRequestContext, MessagesUpstream,
    run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::ToolRegistry;
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use either::Either;
use futures::StreamExt as _;
use serde_json::json;

#[path = "execution_metrics/harness.rs"]
mod harness;
mod support;
use harness::{Metrics, histogram_sum, int_histogram_sum, recorded, total};
use support::{MockResponse, MockServer, TestFixture, text_response};

const RESPONSES_EXECUTOR: &[(&str, &str)] = &[("agentic.api", "responses"), ("agentic.route", "executor")];

fn request(stream: bool) -> RequestPayload {
    support::make_request("say one word", true, stream, None, None)
}

/// The fixture's context, recording into `metrics`.
fn measured(exec_ctx: &Arc<ExecutionContext>, metrics: &Metrics) -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::clone(exec_ctx).with_metrics(metrics.executor()))
}

#[allow(clippy::result_large_err)]
async fn execute(
    exec_ctx: Arc<ExecutionContext>,
    payload: RequestPayload,
) -> ExecutorResult<Either<ResponsePayload, BoxStream>> {
    Box::pin(ExecuteRequest::new(payload, exec_ctx).run()).await
}

async fn stream_of(exec_ctx: Arc<ExecutionContext>, payload: RequestPayload) -> BoxStream {
    match execute(exec_ctx, payload).await.unwrap() {
        Either::Right(stream) => stream,
        Either::Left(_) => panic!("streaming request returns a stream"),
    }
}

/// A minimal upstream stream: created, one text delta, completed without usage.
fn streaming_text_response(text: &str) -> MockResponse {
    let message = json!({
        "id": "msg_upstream", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    });
    let events = [
        json!({"type": "response.created", "sequence_number": 0,
               "response": {"id": "resp_upstream", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
               "item": {"id": "msg_upstream", "type": "message", "role": "assistant",
                        "status": "in_progress", "content": []}}),
        json!({"type": "response.output_text.delta", "sequence_number": 2, "output_index": 0,
               "content_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0, "item": message}),
        json!({"type": "response.completed", "sequence_number": 4,
               "response": {"id": "resp_upstream", "status": "completed", "usage": null, "output": [message]}}),
    ];
    let mut body = String::new();
    for event in events {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse(body)
}

fn upstream_502() -> MockResponse {
    MockResponse::Status(
        502,
        r#"{"error":{"message":"upstream secret detail","type":"server_error"}}"#.to_owned(),
    )
}

/// The execution was counted exactly once with `outcome`, and its active
/// count was released.
fn assert_finalized_once(points: &[harness::Point], stream: &str, outcome: &str, delivery: &str) {
    let filters = [RESPONSES_EXECUTOR, &[("agentic.stream", stream)]].concat();
    assert_eq!(
        total(points, "agentic.execution.count", &[]),
        1,
        "one execution in total"
    );
    let outcome_filter = [filters.as_slice(), &[("agentic.execution.outcome", outcome)]].concat();
    assert_eq!(total(points, "agentic.execution.count", &outcome_filter), 1);
    assert_eq!(total(points, "agentic.execution.duration", &outcome_filter), 1);
    let delivery_filter = [filters.as_slice(), &[("agentic.delivery.outcome", delivery)]].concat();
    assert_eq!(total(points, "agentic.delivery.count", &[]), 1);
    assert_eq!(total(points, "agentic.delivery.count", &delivery_filter), 1);
    assert!(
        recorded(points, "agentic.execution.active"),
        "the active count was taken"
    );
    assert_eq!(total(points, "agentic.execution.active", &[]), 0, "and released");
}

fn finalized(points: &[harness::Point]) -> bool {
    total(points, "agentic.execution.count", &[]) >= 1
}

#[tokio::test]
async fn blocking_completion_records_each_instrument_once() {
    let metrics = Metrics::new();
    let fixture = TestFixture::new_with_responses(vec![text_response("hello")]).await;

    let result = execute(measured(&fixture.exec_ctx, &metrics), request(false)).await;
    assert!(matches!(result, Ok(Either::Left(_))));

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "false", "completed", "delivered");
    assert_eq!(
        total(&points, "agentic.execution.count", &[("error.type", "storage")]),
        0
    );
    for stage in ["rehydrate", "inference", "persist"] {
        assert_eq!(
            total(&points, "agentic.stage.duration", &[("agentic.stage", stage)]),
            1,
            "{stage}"
        );
    }
    assert!(
        points
            .iter()
            .filter(|point| point.name == "agentic.stage.duration")
            .all(|point| !point.attributes.contains_key("error.type")),
        "no stage failed"
    );
    assert_eq!(total(&points, "agentic.inference.rounds", &[]), 1);
    assert_eq!(int_histogram_sum(&points, "agentic.inference.rounds", &[]), 1);
    assert!(
        !recorded(&points, "gen_ai.client.token.usage"),
        "the upstream sent `usage: null`; nothing is fabricated"
    );
    for timing in [
        "agentic.time_to_first_upstream_data",
        "agentic.time_to_first_client_event",
        "agentic.time_to_first_text",
        "agentic.delivery.wait.duration",
    ] {
        assert!(!recorded(&points, timing), "{timing} is only for streamed executions");
    }
}

#[tokio::test]
async fn recorded_stream_orders_first_timings_and_reports_upstream_usage() {
    let metrics = Metrics::new();
    let cassette = support::load_cassette(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/text_only/responses/resp-single-gpt-4o-streaming.yaml"
    ));
    let fixture = TestFixture::new(&support::responses_turns(&cassette)).await;

    let frames: Vec<String> = stream_of(measured(&fixture.exec_ctx, &metrics), request(true))
        .await
        .collect()
        .await;
    assert!(frames.iter().any(|frame| frame.contains("response.output_text.delta")));
    assert!(frames.iter().any(|frame| frame.contains("response.completed")));

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "true", "completed", "delivered");
    let api = [("agentic.api", "responses")];
    let first_upstream = histogram_sum(&points, "agentic.time_to_first_upstream_data", &api);
    let first_event = histogram_sum(&points, "agentic.time_to_first_client_event", &api);
    let first_text = histogram_sum(&points, "agentic.time_to_first_text", &api);
    for timing in [
        "agentic.time_to_first_upstream_data",
        "agentic.time_to_first_client_event",
        "agentic.time_to_first_text",
    ] {
        assert_eq!(total(&points, timing, &api), 1, "{timing} is recorded once");
    }
    assert!(
        0.0 < first_upstream && first_upstream <= first_event && first_event <= first_text,
        "upstream {first_upstream} <= client event {first_event} <= text {first_text}"
    );
    assert_eq!(total(&points, "agentic.delivery.wait.duration", &api), 1);

    // The cassette's `response.completed` reports 14 input and 4 output tokens.
    let input = [("gen_ai.token.type", "input"), ("gen_ai.operation.name", "chat")];
    let output = [("gen_ai.token.type", "output"), ("gen_ai.operation.name", "chat")];
    assert_eq!(total(&points, "gen_ai.client.token.usage", &input), 1);
    assert_eq!(int_histogram_sum(&points, "gen_ai.client.token.usage", &input), 14);
    assert_eq!(int_histogram_sum(&points, "gen_ai.client.token.usage", &output), 4);
}

#[tokio::test]
async fn upstream_failure_in_a_stream_is_one_delivered_failure() {
    let metrics = Metrics::new();
    let fixture = TestFixture::new_with_responses(vec![upstream_502()]).await;

    let frames: Vec<String> = stream_of(measured(&fixture.exec_ctx, &metrics), request(true))
        .await
        .collect()
        .await;
    assert!(
        frames.iter().any(|frame| frame.starts_with("event: error")),
        "{frames:?}"
    );

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "true", "failed", "delivered");
    assert_eq!(
        total(&points, "agentic.execution.count", &[("error.type", "upstream_status")]),
        1
    );
    assert_eq!(
        total(
            &points,
            "agentic.stage.duration",
            &[("agentic.stage", "inference"), ("error.type", "upstream_status")]
        ),
        1
    );
    assert!(!recorded(&points, "gen_ai.client.token.usage"));
    // No upstream line arrived and an executor error frame is not a response
    // event, so no first-* timing describes this execution.
    for timing in [
        "agentic.time_to_first_upstream_data",
        "agentic.time_to_first_client_event",
        "agentic.time_to_first_text",
    ] {
        assert!(!recorded(&points, timing), "{timing}");
    }
}

#[tokio::test]
async fn upstream_failure_without_a_stream_is_never_delivered() {
    let metrics = Metrics::new();
    let fixture = TestFixture::new_with_responses(vec![upstream_502()]).await;

    assert!(
        execute(measured(&fixture.exec_ctx, &metrics), request(false))
            .await
            .is_err()
    );

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "false", "failed", "not_started");
    assert_eq!(total(&points, "agentic.inference.rounds", &[]), 1);
}

#[tokio::test]
async fn dropped_stream_is_one_cancelled_and_disconnected_execution() {
    let metrics = Metrics::new();
    let fixture = TestFixture::new_with_responses(vec![streaming_text_response("hello")]).await;

    let mut stream = stream_of(measured(&fixture.exec_ctx, &metrics), request(true)).await;
    let first = stream.next().await.expect("at least one frame");
    assert!(first.contains("response.created"), "{first}");
    // The client goes away before the terminal frame.
    drop(stream);

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "true", "cancelled", "disconnected");
    assert!(
        points
            .iter()
            .filter(|point| point.name == "agentic.execution.count")
            .all(|point| !point.attributes.contains_key("error.type")),
        "cancellation is not a failure"
    );
    assert_eq!(
        total(&points, "agentic.time_to_first_client_event", &[]),
        1,
        "the first event reached the transport"
    );
}

#[tokio::test]
async fn gateway_tool_round_counts_rounds_and_times_the_tool() {
    let metrics = Metrics::new();
    let cassette = support::load_cassette(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/codex/codex-openai-web-search-then-namespace-gpt-4o-streaming.yaml"
    ));
    let fixture = TestFixture::new(&support::responses_turns(&cassette)).await;
    let search = search_server().await;
    let exec_ctx = ExecutionContext::clone(&fixture.exec_ctx)
        .with_gateway_executor(Arc::new(agentic_core::tool::WebSearchHandler::with_api_key(
            Arc::new(reqwest::Client::new()),
            "private-search-key".into(),
            &search.0,
        )))
        .with_metrics(metrics.executor());
    let mut payload = request(true);
    payload.tools = Some(
        serde_json::from_value(json!([
            {"type":"web_search_preview"},
            {"type":"namespace","name":"mcp__shell","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]}
        ]))
        .unwrap(),
    );

    let frames: Vec<String> = stream_of(Arc::new(exec_ctx), payload).await.collect().await;
    search.1.abort();
    assert!(frames.iter().any(|frame| frame.contains("response.completed")));

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_finalized_once(&points, "true", "completed", "delivered");
    assert_eq!(total(&points, "agentic.inference.rounds", &[]), 1);
    assert_eq!(int_histogram_sum(&points, "agentic.inference.rounds", &[]), 2);
    assert_eq!(
        total(&points, "agentic.stage.duration", &[("agentic.stage", "inference")]),
        2
    );
    assert_eq!(
        total(
            &points,
            "agentic.stage.duration",
            &[("agentic.stage", "tool"), ("agentic.tool.type", "web_search")]
        ),
        1
    );
    assert_eq!(
        total(&points, "gen_ai.client.token.usage", &[("gen_ai.token.type", "input")]),
        2,
        "one sample per upstream call, not per turn"
    );
}

#[tokio::test]
async fn explicit_compaction_times_its_stages_and_usage_without_an_execution() {
    let metrics = Metrics::new();
    let summary = MockResponse::Json(
        json!({
            "id": "resp_upstream", "object": "response", "created_at": 0, "model": "test-model",
            "status": "completed",
            "output": [{"id": "msg_upstream", "type": "message", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "summary", "annotations": []}]}],
            "usage": {"input_tokens": 11, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 3,
                      "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 14}
        })
        .to_string(),
    );
    let fixture = TestFixture::new_with_responses(vec![summary]).await;
    let payload = serde_json::from_value(json!({"model": "test-model", "input": "compact me"})).unwrap();

    Box::pin(agentic_core::executor::compact_response(
        payload,
        &measured(&fixture.exec_ctx, &metrics),
        None,
    ))
    .await
    .unwrap();

    let points = metrics
        .wait_for("persist stage", |points| {
            total(points, "agentic.stage.duration", &[("agentic.stage", "persist")]) == 1
        })
        .await;
    for stage in ["rehydrate", "compaction", "persist"] {
        assert_eq!(
            total(&points, "agentic.stage.duration", &[("agentic.stage", stage)]),
            1,
            "{stage}"
        );
    }
    assert_eq!(
        int_histogram_sum(&points, "gen_ai.client.token.usage", &[("gen_ai.token.type", "input")]),
        11
    );
    assert!(
        !recorded(&points, "agentic.execution.count"),
        "compaction has no agentic.execute span"
    );
}

async fn search_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/v1/search",
        axum::routing::get(|| async {
            axum::Json(json!({"results":{"web":[{"url":"https://example.com","title":"result"}],"news":[]}}))
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

struct MessagesFixture {
    exec_ctx: Arc<ExecutionContext>,
    upstream: MessagesUpstream,
    _server: MockServer,
}

async fn messages_fixture(metrics: &Metrics, responses: Vec<MockResponse>) -> MessagesFixture {
    let server = MockServer::start_deque_on("/v1/messages", responses).await;
    let pool = support::setup_pool().await;
    let exec_ctx = ExecutionContext::new(
        agentic_core::executor::ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        agentic_core::executor::ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        server.url().to_owned(),
    )
    .with_metrics(metrics.executor());
    let upstream = MessagesUpstream::new(server.url(), None, reqwest::header::HeaderMap::new());
    MessagesFixture {
        exec_ctx: Arc::new(exec_ctx),
        upstream,
        _server: server,
    }
}

fn messages_request(stream: bool) -> MessagesRequestContext {
    MessagesRequestContext::from_value(json!({
        "model": "test-model", "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": "say one word"}]
    }))
    .unwrap()
}

fn messages_sse_response() -> MockResponse {
    let events = [
        json!({"type": "message_start", "message": {
            "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0, "cache_read_input_tokens": 5}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ];
    let mut body = String::new();
    for event in events {
        writeln!(body, "event: {}\ndata: {event}\n", event["type"].as_str().unwrap()).unwrap();
    }
    MockResponse::Sse(body)
}

fn assert_messages_finalized_once(points: &[harness::Point], stream: &str, outcome: &str, delivery: &str) {
    let filters = [
        ("agentic.api", "messages"),
        ("agentic.route", "executor"),
        ("agentic.stream", stream),
    ];
    let outcome_filter = [filters.as_slice(), &[("agentic.execution.outcome", outcome)]].concat();
    let delivery_filter = [filters.as_slice(), &[("agentic.delivery.outcome", delivery)]].concat();
    assert_eq!(total(points, "agentic.execution.count", &[]), 1);
    assert_eq!(total(points, "agentic.execution.count", &outcome_filter), 1);
    assert_eq!(total(points, "agentic.delivery.count", &delivery_filter), 1);
    assert_eq!(total(points, "agentic.execution.active", &[]), 0);
}

#[tokio::test]
async fn messages_loop_reports_prompt_tokens_including_cache_reads() {
    let metrics = Metrics::new();
    let response = MockResponse::Json(
        json!({
            "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
            "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 1, "cache_read_input_tokens": 5}
        })
        .to_string(),
    );
    let fixture = messages_fixture(&metrics, vec![response]).await;

    run_messages_loop(
        messages_request(false),
        &ToolRegistry::default(),
        &fixture.exec_ctx,
        &fixture.upstream,
    )
    .await
    .unwrap();

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_messages_finalized_once(&points, "false", "completed", "delivered");
    let api = ("agentic.api", "messages");
    assert_eq!(
        int_histogram_sum(
            &points,
            "gen_ai.client.token.usage",
            &[api, ("gen_ai.token.type", "input")]
        ),
        8,
        "input_tokens plus cache reads"
    );
    assert_eq!(
        int_histogram_sum(
            &points,
            "gen_ai.client.token.usage",
            &[api, ("gen_ai.token.type", "output")]
        ),
        1
    );
    assert_eq!(
        total(&points, "agentic.stage.duration", &[("agentic.stage", "inference")]),
        1
    );
    assert_eq!(total(&points, "agentic.inference.rounds", &[api]), 1);
}

#[tokio::test]
async fn messages_stream_orders_first_timings_and_reports_final_round_usage() {
    let metrics = Metrics::new();
    let fixture = messages_fixture(&metrics, vec![messages_sse_response()]).await;

    let response = run_messages_stream(
        messages_request(true),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&fixture.exec_ctx),
        fixture.upstream.clone(),
    )
    .await
    .unwrap();
    let frames: Vec<String> = response.body.collect().await;
    assert!(frames.iter().any(|frame| frame.contains("text_delta")), "{frames:?}");

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_messages_finalized_once(&points, "true", "completed", "delivered");
    let api = [("agentic.api", "messages")];
    let first_upstream = histogram_sum(&points, "agentic.time_to_first_upstream_data", &api);
    let first_event = histogram_sum(&points, "agentic.time_to_first_client_event", &api);
    let first_text = histogram_sum(&points, "agentic.time_to_first_text", &api);
    assert!(
        0.0 < first_upstream && first_upstream <= first_event && first_event <= first_text,
        "upstream {first_upstream} <= client event {first_event} <= text {first_text}"
    );
    // `message_start.usage` overlaid by the final `message_delta.usage`.
    assert_eq!(
        int_histogram_sum(
            &points,
            "gen_ai.client.token.usage",
            &[api[0], ("gen_ai.token.type", "input")]
        ),
        8
    );
    assert_eq!(
        int_histogram_sum(
            &points,
            "gen_ai.client.token.usage",
            &[api[0], ("gen_ai.token.type", "output")]
        ),
        2
    );
    assert_eq!(total(&points, "agentic.delivery.wait.duration", &api), 1);
}

#[tokio::test]
async fn messages_stream_rejected_before_headers_is_never_delivered() {
    let metrics = Metrics::new();
    let fixture = messages_fixture(&metrics, vec![upstream_502()]).await;

    let rejected = run_messages_stream(
        messages_request(true),
        Arc::new(ToolRegistry::default()),
        Arc::clone(&fixture.exec_ctx),
        fixture.upstream.clone(),
    )
    .await;
    assert!(rejected.is_err());

    let points = metrics.wait_for("finalized execution", finalized).await;
    assert_messages_finalized_once(&points, "true", "failed", "not_started");
    assert_eq!(
        total(
            &points,
            "agentic.stage.duration",
            &[("agentic.stage", "inference"), ("error.type", "upstream_status")]
        ),
        1
    );
}

/// The cardinality check is what fails the build on a new dimension, so it
/// must reject each kind of violation.
#[test]
fn cardinality_contract_rejects_unlisted_instruments_keys_and_values() {
    fn violation(name: &str, key: &str, value: &str) -> bool {
        let point = harness::Point {
            name: name.to_owned(),
            attributes: [(key.to_owned(), value.to_owned())].into(),
            value: harness::PointValue::Sum(1),
        };
        std::panic::catch_unwind(|| harness::assert_allowed(&[point])).is_err()
    }
    assert!(!violation("agentic.execution.count", "agentic.api", "responses"));
    assert!(violation("agentic.execution.count", "response.id", "resp_123"));
    assert!(violation("agentic.execution.count", "agentic.api", "gpt-4o"));
    assert!(violation("agentic.stage.duration", "agentic.tool.name", "my_tool"));
    assert!(violation("gen_ai.client.token.usage", "gen_ai.request.model", "gpt-4o"));
    assert!(violation("agentic.unlisted.metric", "agentic.api", "responses"));
    assert!(violation(
        "http.server.request.duration",
        "http.route",
        "/v1/responses?x=1"
    ));
}
