//! The `agentic.execute` span through the real executor against a mock
//! upstream, captured by an in-memory exporter through the same
//! `tracing` → OpenTelemetry bridge the server installs.
//!
//! One global subscriber serves the whole binary, as in production, and each
//! test isolates its spans by the trace id of its own root span. A scoped
//! `with_subscriber` would not do: the registry releases a closed span's
//! parent through the *closing thread's* default dispatcher, and
//! `sqlx-sqlite` closes spans on its connection worker thread, where a
//! scoped dispatcher is not in effect.

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agentic_core::executor::{
    ExecuteRequest, ExecutionContext, MessagesRequestContext, MessagesUpstream, run_messages_loop, run_messages_stream,
};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::tool::ToolRegistry;
use agentic_core::types::io::ResponsesInput;
use agentic_core::types::request_response::RequestPayload;
use either::Either;
use futures::StreamExt as _;
use opentelemetry::Value;
use opentelemetry::trace::{Status, TraceContextExt as _, TraceId, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::json;
use tracing::{Instrument as _, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

#[path = "execution_trace/compaction.rs"]
mod compaction;
#[path = "execution_trace/stages.rs"]
mod stages;
mod support;
#[path = "../../agentic-server/tests/execution_traces/attributes.rs"]
mod trace_attributes;
use support::{MockResponse, MockServer, TestFixture, text_response};

const PROMPT: &str = "the prompt text must never be exported";

/// Attributes `agentic.execute` may carry, and nothing else.
const ALLOWED_EXECUTE_ATTRIBUTES: &[&str] = &[
    "agentic.api",
    "agentic.route",
    "agentic.stream",
    "agentic.execution.outcome",
    "agentic.delivery.outcome",
    "error.type",
];

static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();

/// Install the bridge once per test binary and return the shared exporter.
fn exporter() -> &'static InMemorySpanExporter {
    EXPORTER.get_or_init(|| {
        opentelemetry::global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        // Same bridge configuration as the server's `build_subscriber`: only
        // declared span fields become attributes.
        let layer = tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_level(false)
            .with_tracked_inactivity(false)
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.is_span()
                    && *metadata.level() <= tracing::Level::INFO
                    && (metadata.target().starts_with("agentic_core")
                        || matches!(
                            metadata.name(),
                            "test.root" | "unrelated.consumer" | "http.server.request"
                        ))
            }));
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .expect("installed once per binary");
        // The provider lives for the process; simple export is synchronous.
        std::mem::forget(provider);
        exporter
    })
}

/// One test's trace: a root span every request in the test runs under, and
/// a view of the exporter restricted to that trace.
struct Traces {
    root: Span,
    trace_id: TraceId,
}

impl Traces {
    fn new() -> Self {
        exporter();
        let root = tracing::info_span!("test.root");
        let trace_id = root.context().span().span_context().trace_id();
        assert_ne!(trace_id, TraceId::INVALID, "the bridge is installed");
        Self { root, trace_id }
    }

    /// Run a future as this test's request, under the root span.
    async fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        future.instrument(self.root.clone()).await
    }

    fn root_span_id(&self) -> opentelemetry::trace::SpanId {
        self.root.context().span().span_context().span_id()
    }

    /// Finished spans belonging to this test, root excluded.
    fn finished(&self) -> Vec<SpanData> {
        let spans = exporter()
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .filter(|span| span.span_context.trace_id() == self.trace_id && span.name != "test.root")
            .collect::<Vec<_>>();
        trace_attributes::assert_allowed(&spans, &[PROMPT, "upstream secret detail"]);
        spans
    }

    /// The finished `agentic.execute` span, exactly one.
    ///
    /// A span closes when its last handle drops, and two of those handles
    /// are released off the request's own future: `sqlx-sqlite` hands
    /// `Span::current()` to its connection worker thread with every command
    /// and drops it there after replying, and an aborted executor task is
    /// reaped by the runtime on a later tick. Both land within milliseconds,
    /// so wait briefly rather than racing them.
    async fn execute_span(&self) -> SpanData {
        self.finished_by_name(&["agentic.execute"])
            .await
            .into_iter()
            .find(|span| span.name == "agentic.execute")
            .expect("exactly one agentic.execute span")
    }

    /// This test's finished spans once every named span has closed.
    async fn finished_by_name(&self, names: &[&str]) -> Vec<SpanData> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let spans = self.finished();
            if names.iter().all(|name| spans.iter().any(|span| span.name == *name)) {
                let executes = spans.iter().filter(|span| span.name == "agentic.execute").count();
                assert!(executes <= 1, "more than one agentic.execute span: {spans:#?}");
                return spans;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "spans {names:?} did not close; finished: {:?}",
                spans.iter().map(|span| span.name.as_ref()).collect::<Vec<&str>>()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn assert_execute_attributes(span: &SpanData, stream: bool, execution: &'static str, delivery: &'static str) {
    assert_execute_attributes_for(span, "responses", stream, execution, delivery);
}

fn assert_execute_attributes_for(
    span: &SpanData,
    api: &'static str,
    stream: bool,
    execution: &'static str,
    delivery: &'static str,
) {
    assert_eq!(attribute(span, "agentic.api"), Some(&Value::from(api)));
    assert_eq!(attribute(span, "agentic.route"), Some(&Value::from("executor")));
    assert_eq!(attribute(span, "agentic.stream"), Some(&Value::from(stream)));
    assert_eq!(
        attribute(span, "agentic.execution.outcome"),
        Some(&Value::from(execution)),
        "execution outcome"
    );
    assert_eq!(
        attribute(span, "agentic.delivery.outcome"),
        Some(&Value::from(delivery)),
        "delivery outcome"
    );
    for kv in &span.attributes {
        assert!(
            ALLOWED_EXECUTE_ATTRIBUTES.contains(&kv.key.as_str()),
            "unexpected attribute {} on agentic.execute",
            kv.key
        );
        assert!(
            !kv.value.to_string().contains(PROMPT),
            "prompt text leaked into attribute {}",
            kv.key
        );
    }
}

fn request(stream: bool) -> RequestPayload {
    RequestPayload {
        model: "test-model".to_owned(),
        input: ResponsesInput::Text(PROMPT.to_owned()),
        instructions: None,
        previous_response_id: None,
        conversation_id: None,
        tools: None,
        tool_choice: None,
        stream,
        store: true,
        include: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(64),
        ignore_eos: None,
        truncation: None,
        metadata: None,
        parallel_tool_calls: None,
        prompt_cache_key: None,
        cache_salt: None,
        context_management: None,
    }
}

/// A minimal upstream SSE stream that completes with one assistant message.
fn streaming_text_response(text: &str) -> MockResponse {
    let message = json!({
        "id": "msg_upstream",
        "type": "message",
        "role": "assistant",
        "status": "completed",
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
        json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0,
               "item": message}),
        json!({"type": "response.completed", "sequence_number": 4,
               "response": {"id": "resp_upstream", "status": "completed", "usage": null,
                            "output": [message]}}),
    ];
    let mut body = String::new();
    for event in events {
        writeln!(body, "data: {event}\n").unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse(body)
}

#[allow(clippy::result_large_err)]
async fn run(
    traces: &Traces,
    exec_ctx: Arc<ExecutionContext>,
    payload: RequestPayload,
) -> agentic_core::executor::ExecutorResult<
    Either<agentic_core::types::request_response::ResponsePayload, agentic_core::executor::BoxStream>,
> {
    Box::pin(traces.run(ExecuteRequest::new(payload, exec_ctx).run())).await
}

/// Drain a stream inside the test subscriber's scope, as the transport would
/// while polling the response body.
async fn collect_frames(traces: &Traces, stream: agentic_core::executor::BoxStream) -> Vec<String> {
    traces.run(stream.collect::<Vec<String>>()).await
}

#[tokio::test]
async fn blocking_request_records_completed_and_delivered() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(vec![text_response("hello")]).await;

    let result = run(&traces, Arc::clone(&fixture.exec_ctx), request(false)).await;
    let Either::Left(payload) = result.unwrap() else {
        panic!("non-streaming request returns a payload");
    };
    assert_eq!(payload.status, "completed");

    let span = traces.execute_span().await;
    assert_execute_attributes(&span, false, "completed", "delivered");
    assert_eq!(attribute(&span, "error.type"), None);
    assert_eq!(span.status, Status::Unset);
}

#[tokio::test]
async fn execute_span_is_a_child_of_the_current_span() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(vec![text_response("hello")]).await;

    // Stand-in for the server's `http.server.request` span, which instruments
    // the handler future the same way.
    let exec_ctx = Arc::clone(&fixture.exec_ctx);
    async move {
        let parent = tracing::info_span!("http.server.request");
        ExecuteRequest::new(request(false), exec_ctx)
            .run()
            .instrument(parent)
            .await
            .unwrap();
    }
    .instrument(traces.root.clone())
    .await;

    let spans = traces
        .finished_by_name(&["agentic.execute", "http.server.request"])
        .await;
    let execute = spans.iter().find(|span| span.name == "agentic.execute").unwrap();
    let parent = spans.iter().find(|span| span.name == "http.server.request").unwrap();
    assert_eq!(execute.parent_span_id, parent.span_context.span_id());
    assert_eq!(execute.span_context.trace_id(), parent.span_context.trace_id());
}

#[tokio::test]
async fn streaming_span_stays_open_until_the_terminal_frame() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(vec![streaming_text_response("hello")]).await;

    let result = run(&traces, Arc::clone(&fixture.exec_ctx), request(true)).await;
    let Either::Right(stream) = result.unwrap() else {
        panic!("streaming request returns a stream");
    };
    assert!(
        traces.finished().iter().all(|span| span.name != "agentic.execute"),
        "the span stays open while the stream is unconsumed"
    );

    let frames = tokio::spawn(
        stream
            .collect::<Vec<_>>()
            .instrument(tracing::info_span!(parent: None, "unrelated.consumer")),
    )
    .await
    .unwrap();
    assert!(frames.iter().any(|frame| frame.contains("response.completed")));
    assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));

    let span = traces.execute_span().await;
    assert_execute_attributes(&span, true, "completed", "delivered");
    assert_eq!(span.status, Status::Unset);
    let stages = traces
        .finished_by_name(&["agentic.persist", "agentic.inference_round"])
        .await;
    for child in stages
        .iter()
        .filter(|child| matches!(child.name.as_ref(), "agentic.persist" | "agentic.inference_round"))
    {
        assert_eq!(
            child.parent_span_id,
            span.span_context.span_id(),
            "{} escaped its execution",
            child.name
        );
    }
}

#[tokio::test]
async fn dropping_a_stream_mid_way_is_cancelled_and_disconnected() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(vec![streaming_text_response("hello")]).await;

    let result = run(&traces, Arc::clone(&fixture.exec_ctx), request(true)).await;
    let Either::Right(stream) = result.unwrap() else {
        panic!("streaming request returns a stream");
    };
    let first = async move {
        let mut stream = stream;
        let first = stream.next().await.expect("at least one frame");
        // Client goes away before the terminal frame.
        drop(stream);
        first
    }
    .instrument(traces.root.clone())
    .await;
    assert!(first.contains("response.created"), "{first}");

    let span = traces.execute_span().await;
    assert_execute_attributes(&span, true, "cancelled", "disconnected");
    assert_eq!(attribute(&span, "error.type"), None);
    assert_eq!(span.status, Status::Unset, "cancellation is not an error");
}

#[tokio::test]
async fn upstream_failure_in_a_stream_is_a_delivered_failure() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(vec![MockResponse::Status(
        502,
        r#"{"error":{"message":"upstream secret detail","type":"server_error"}}"#.to_owned(),
    )])
    .await;

    let result = run(&traces, Arc::clone(&fixture.exec_ctx), request(true)).await;
    let Either::Right(stream) = result.unwrap() else {
        panic!("streaming request returns a stream");
    };
    let frames = collect_frames(&traces, stream).await;
    assert!(
        frames.iter().any(|frame| frame.starts_with("event: error")),
        "the failure is delivered as an SSE error frame: {frames:?}"
    );
    assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));

    let span = traces.execute_span().await;
    assert_execute_attributes(&span, true, "failed", "delivered");
    assert_eq!(attribute(&span, "error.type"), Some(&Value::from("upstream_status")));
    assert!(matches!(span.status, Status::Error { .. }));
    for kv in &span.attributes {
        assert!(
            !kv.value.to_string().contains("secret detail"),
            "upstream error body leaked into {}",
            kv.key
        );
    }
}

#[tokio::test]
async fn failure_before_execution_starts_is_not_delivered() {
    let traces = Traces::new();
    let fixture = TestFixture::new_with_responses(Vec::new()).await;
    let mut payload = request(false);
    payload.previous_response_id = Some("resp_does_not_exist".to_owned());

    let Err(error) = run(&traces, Arc::clone(&fixture.exec_ctx), payload).await else {
        panic!("unknown previous_response_id fails rehydration");
    };
    assert_eq!(error.http_status(), 404, "{error}");

    let span = traces.execute_span().await;
    assert_execute_attributes(&span, false, "failed", "not_started");
    assert_eq!(attribute(&span, "error.type"), Some(&Value::from("not_found")));
    assert!(matches!(span.status, Status::Error { .. }));
    assert_eq!(
        span.parent_span_id,
        traces.root_span_id(),
        "a child of the enclosing span"
    );
}

// ---------------------------------------------------------------------------
// Messages API: the same span shape from `run_messages_loop` and
// `run_messages_stream`.
// ---------------------------------------------------------------------------

struct MessagesFixture {
    exec_ctx: Arc<ExecutionContext>,
    registry: Arc<ToolRegistry>,
    upstream: MessagesUpstream,
    server: MockServer,
}

impl MessagesFixture {
    async fn request_bodies(&self) -> Vec<serde_json::Value> {
        self.server.request_bodies().await
    }
}

async fn messages_fixture(responses: Vec<MockResponse>) -> MessagesFixture {
    let server = MockServer::start_deque_on("/v1/messages", responses).await;
    let pool = support::setup_pool().await;
    let exec_ctx = Arc::new(ExecutionContext::new(
        agentic_core::executor::ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        agentic_core::executor::ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        server.url().to_owned(),
    ));
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("authorization", "Bearer private-auth".parse().unwrap());
    headers.insert("x-api-key", "private-api-key".parse().unwrap());
    headers.insert(
        "traceparent",
        "00-11111111111111111111111111111111-2222222222222222-01"
            .parse()
            .unwrap(),
    );
    headers.insert("tracestate", "stale=caller".parse().unwrap());
    let upstream = MessagesUpstream::new(server.url(), Some("secret=private-query"), headers);
    MessagesFixture {
        exec_ctx,
        registry: Arc::new(ToolRegistry::default()),
        upstream,
        server,
    }
}

fn messages_request(stream: bool) -> MessagesRequestContext {
    MessagesRequestContext::from_value(json!({
        "model": "test-model",
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": PROMPT}]
    }))
    .unwrap()
}

fn messages_json_response(stop_reason: &str) -> MockResponse {
    MockResponse::Json(
        json!({
            "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": stop_reason, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })
        .to_string(),
    )
}

fn messages_sse_response(stop_reason: &str) -> MockResponse {
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {
            "id": "msg_upstream", "type": "message", "role": "assistant", "model": "test-model",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "hello"}}),
        ),
        ("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        (
            "message_delta",
            json!({"type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": 1}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let mut body = String::new();
    for (name, event) in events {
        writeln!(body, "event: {name}\ndata: {event}\n").unwrap();
    }
    MockResponse::Sse(body)
}

#[tokio::test]
async fn messages_loop_records_completed_and_delivered() {
    let traces = Traces::new();
    let fixture = messages_fixture(vec![messages_json_response("end_turn")]).await;

    let response = run_messages_loop(
        messages_request(false),
        &fixture.registry,
        &fixture.exec_ctx,
        &fixture.upstream,
    )
    .instrument(traces.root.clone())
    .await
    .unwrap();
    assert_eq!(response.body["stop_reason"], "end_turn");

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", false, "completed", "delivered");
    assert_eq!(span.status, Status::Unset);
}

#[tokio::test]
async fn messages_loop_max_tokens_is_incomplete() {
    let traces = Traces::new();
    let fixture = messages_fixture(vec![messages_json_response("max_tokens")]).await;

    run_messages_loop(
        messages_request(false),
        &fixture.registry,
        &fixture.exec_ctx,
        &fixture.upstream,
    )
    .instrument(traces.root.clone())
    .await
    .unwrap();

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", false, "incomplete", "delivered");
    assert_eq!(span.status, Status::Unset);
}

#[tokio::test]
async fn messages_upstream_error_body_is_a_delivered_failure() {
    let traces = Traces::new();
    // Surfaced verbatim as `Ok`, so the HTTP layer never sees an `Err`: the
    // span must still say the execution failed.
    let fixture = messages_fixture(vec![MockResponse::Json(
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "secret detail"}}).to_string(),
    )])
    .await;

    let response = run_messages_loop(
        messages_request(false),
        &fixture.registry,
        &fixture.exec_ctx,
        &fixture.upstream,
    )
    .instrument(traces.root.clone())
    .await
    .unwrap();
    assert_eq!(response.body["type"], "error");

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", false, "failed", "delivered");
    assert_eq!(attribute(&span, "error.type"), Some(&Value::from("upstream_error")));
    assert!(matches!(span.status, Status::Error { .. }));
    assert!(!span.attributes.iter().any(|kv| kv.value.to_string().contains("secret")));
}

#[tokio::test]
async fn messages_stream_records_completed_and_delivered() {
    let traces = Traces::new();
    let fixture = messages_fixture(vec![messages_sse_response("end_turn")]).await;

    let response = run_messages_stream(
        messages_request(true),
        Arc::clone(&fixture.registry),
        Arc::clone(&fixture.exec_ctx),
        fixture.upstream.clone(),
    )
    .instrument(traces.root.clone())
    .await
    .unwrap();
    assert!(
        traces.finished().iter().all(|span| span.name != "agentic.execute"),
        "open until the stream is drained"
    );

    let frames = tokio::spawn(
        response
            .body
            .collect::<Vec<_>>()
            .instrument(tracing::info_span!(parent: None, "unrelated.consumer")),
    )
    .await
    .unwrap();
    assert!(
        frames.iter().any(|frame| frame.starts_with("event: message_stop")),
        "{frames:?}"
    );
    let upstream_requests = fixture.request_bodies().await;
    assert_eq!(upstream_requests[0]["stream"], true, "the primed request streams");

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", true, "completed", "delivered");
    assert_eq!(span.status, Status::Unset);
    let stages = traces
        .finished_by_name(&["agentic.inference_round", "http.client.request"])
        .await;
    let round = stages
        .iter()
        .find(|child| child.name == "agentic.inference_round")
        .unwrap();
    let client = stages.iter().find(|child| child.name == "http.client.request").unwrap();
    assert_eq!(round.parent_span_id, span.span_context.span_id());
    assert_eq!(client.parent_span_id, round.span_context.span_id());
    let headers = fixture.server.request_headers().await;
    assert_eq!(
        headers[0]["traceparent"],
        format!("00-{}-{}-01", traces.trace_id, client.span_context.span_id())
    );
    assert!(
        headers[0].get("tracestate").is_none_or(http::HeaderValue::is_empty),
        "stale outbound trace state was replaced"
    );
}

#[tokio::test]
async fn messages_stream_dropped_mid_way_is_cancelled() {
    let traces = Traces::new();
    let fixture = messages_fixture(vec![messages_sse_response("end_turn")]).await;

    let response = run_messages_stream(
        messages_request(true),
        Arc::clone(&fixture.registry),
        Arc::clone(&fixture.exec_ctx),
        fixture.upstream.clone(),
    )
    .instrument(traces.root.clone())
    .await
    .unwrap();
    async move {
        let mut stream = response.body;
        let first = stream.next().await.expect("first frame");
        assert!(first.starts_with("event: message_start"), "{first}");
        drop(stream);
    }
    .instrument(traces.root.clone())
    .await;

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", true, "cancelled", "disconnected");
    assert_eq!(span.status, Status::Unset);
}

#[tokio::test]
async fn messages_stream_upstream_rejection_before_headers_is_not_started() {
    let traces = Traces::new();
    let fixture = messages_fixture(vec![MockResponse::Status(
        529,
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "secret detail"}}).to_string(),
    )])
    .await;

    let error = run_messages_stream(
        messages_request(true),
        Arc::clone(&fixture.registry),
        Arc::clone(&fixture.exec_ctx),
        fixture.upstream.clone(),
    )
    .instrument(traces.root.clone())
    .await
    .err()
    .expect("the primed request fails before any frame exists");
    assert_eq!(error.http_status(), 529, "{error}");

    let span = traces.execute_span().await;
    assert_execute_attributes_for(&span, "messages", true, "failed", "not_started");
    assert_eq!(attribute(&span, "error.type"), Some(&Value::from("upstream_status")));
    assert!(matches!(span.status, Status::Error { .. }));
    assert!(!span.attributes.iter().any(|kv| kv.value.to_string().contains("secret")));
}
