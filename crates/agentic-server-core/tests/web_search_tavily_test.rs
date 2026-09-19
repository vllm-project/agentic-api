//! Tavily provider behavior against a local Axum mock (#327).
//!
//! Every test binds its own `127.0.0.1:0` listener; nothing here reaches the
//! network. Mock handlers use `try_send` so a slow test never blocks inside
//! the server task.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::config::{WebSearchProviderConfig, WebSearchProviderKind};
use agentic_core::tool::{GatewayExecutor, WebSearchHandler};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::OutputItem;
use agentic_core::types::io::output::{FunctionToolCall, WebSearchCallStatus};
use agentic_core::types::tools::{WebSearchFilters, WebSearchToolParam};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod support;

const TAVILY_SEARCH_PATH: &str = "/search";
const GATEWAY_LIMIT: NonZeroUsize = NonZeroUsize::new(5).expect("nonzero gateway limit");
const SECRET_KEY: &str = "tvly-secret-key";

#[derive(Debug)]
struct CapturedTavilyRequest {
    authorization: Option<String>,
    content_type: Option<String>,
    accept: Option<String>,
    accept_encoding: Option<String>,
    body: serde_json::Value,
}

#[derive(Clone)]
struct MockTavily {
    tx: mpsc::Sender<CapturedTavilyRequest>,
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
}

fn capture(headers: &HeaderMap, body: &Bytes) -> CapturedTavilyRequest {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    CapturedTavilyRequest {
        authorization: header("authorization"),
        content_type: header("content-type"),
        accept: header("accept"),
        accept_encoding: header("accept-encoding"),
        body: serde_json::from_slice(body).expect("Tavily request body must be JSON"),
    }
}

async fn spawn_mock_tavily(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
) -> (
    String,
    mpsc::Receiver<CapturedTavilyRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            TAVILY_SEARCH_PATH,
            post(
                |State(mock): State<MockTavily>, headers: HeaderMap, body: Bytes| async move {
                    mock.tx
                        .try_send(capture(&headers, &body))
                        .expect("test channel has capacity");
                    let mut response = (mock.status, Json(mock.body.clone())).into_response();
                    for (name, value) in mock.headers {
                        response.headers_mut().insert(name, value.parse().unwrap());
                    }
                    response
                },
            ),
        )
        .with_state(MockTavily {
            tx,
            status,
            headers,
            body,
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), rx, handle)
}

fn tavily_handler(base_url: &str, max_concurrent_queries: Option<NonZeroUsize>) -> WebSearchHandler {
    let config = WebSearchProviderConfig::new(Some(SECRET_KEY.to_owned()), Some(base_url.to_owned()))
        .with_provider(WebSearchProviderKind::Tavily)
        .with_max_concurrent_queries(max_concurrent_queries);
    WebSearchHandler::from_config(Arc::new(reqwest::Client::new()), &config, GATEWAY_LIMIT)
}

fn search_response() -> serde_json::Value {
    serde_json::json!({
        "query": "rust async",
        "answer": null,
        "images": [],
        "results": [
            {
                "title": "Rust async guide",
                "url": "https://example.com/rust",
                "content": "A useful guide",
                "score": 0.97,
                "raw_content": null,
                "published_date": "2026-09-01",
                "favicon": "https://example.com/favicon.ico",
                "id": "res_1"
            },
            {
                "title": "Tokio",
                "url": "https://docs.example.org/tokio",
                "content": "Runtime",
                "score": 0.81,
                "published_date": null
            }
        ],
        "auto_parameters": {"topic": "general", "search_depth": "basic"},
        "response_time": 1.42,
        "request_id": "req_tavily_1"
    })
}

fn error_body(message: &str) -> serde_json::Value {
    serde_json::json!({"detail": {"error": message}})
}

fn call(arguments: &str) -> FunctionToolCall {
    FunctionToolCall {
        id: "fc_tavily".to_owned(),
        call_id: "call_tavily".to_owned(),
        name: "web_search".to_owned(),
        namespace: None,
        arguments: arguments.to_owned(),
        status: MessageStatus::Completed,
    }
}

async fn execute(handler: &WebSearchHandler, arguments: &str) -> Result<agentic_core::tool::ToolOutput, String> {
    handler
        .execute("call_tavily", "web_search", arguments, &WebSearchToolParam::default())
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn tavily_handler_posts_json_and_maps_results_and_public_sources() {
    let (base_url, mut captured, _handle) = spawn_mock_tavily(StatusCode::OK, Vec::new(), search_response()).await;
    let handler = tavily_handler(&base_url, None);
    let params = WebSearchToolParam::default();
    let arguments = r#"{"query":"rust async","count":50,"freshness":"week","country":"gb","language":"en-GB","safesearch":"moderate","livecrawl":"web","exclude_domains":["spam.example"]}"#;

    let output = handler
        .execute("call_tavily", "web_search", arguments, &params)
        .await
        .unwrap();

    let request = captured.recv().await.expect("mock Tavily should receive the request");
    assert_eq!(request.authorization.as_deref(), Some("Bearer tvly-secret-key"));
    assert_eq!(request.content_type.as_deref(), Some("application/json"));
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    assert_eq!(
        request.accept_encoding, None,
        "core reqwest has no gzip support, so Accept-Encoding must never be sent"
    );
    assert_eq!(
        request.body,
        serde_json::json!({
            "query": "rust async",
            "search_depth": "basic",
            "topic": "general",
            "max_results": 20,
            "time_range": "week",
            "exclude_domains": ["spam.example"],
            "language": "en",
            "safe_search": true,
            "include_published_date": true
        }),
        "count is clamped to 20, freshness maps to time_range, country and You.com-only arguments are dropped"
    );
    assert!(
        request.body.get("api_key").is_none(),
        "the credential travels only in the Authorization header"
    );

    assert_eq!(output.call_id, "call_tavily");
    assert_eq!(
        output.output,
        concat!(
            r#"{"query":"rust async","queries":["rust async"],"#,
            r#""results":{"web":["#,
            r#"{"url":"https://example.com/rust","title":"Rust async guide","description":"A useful guide","#,
            r#""page_age":"2026-09-01"},"#,
            r#"{"url":"https://docs.example.org/tokio","title":"Tokio","description":"Runtime"}],"#,
            r#""news":[]},"#,
            r#""metadata":[{"provider":"tavily","query":"rust async","search_uuid":"req_tavily_1","latency":1.42}]}"#
        )
    );

    let public = handler
        .public_output(&call(arguments), &output, WebSearchCallStatus::Completed, &params)
        .expect("web_search_call public output");
    assert_eq!(
        serde_json::to_value(&public).unwrap(),
        serde_json::json!({
            "id": "ws_tavily",
            "type": "web_search_call",
            "status": "completed",
            "action": {
                "type": "search",
                "query": "rust async",
                "queries": ["rust async"],
                "sources": [
                    {"url": "https://example.com/rust", "title": "Rust async guide"},
                    {"url": "https://docs.example.org/tokio", "title": "Tokio"}
                ]
            }
        })
    );
}

#[tokio::test]
async fn tavily_handler_sends_date_bounds_for_a_freshness_range() {
    let (base_url, mut captured, _handle) = spawn_mock_tavily(StatusCode::OK, Vec::new(), search_response()).await;
    let handler = tavily_handler(&base_url, None);

    execute(&handler, r#"{"query":"rust","freshness":"2026-01-02to2026-02-03"}"#)
        .await
        .unwrap();

    let request = captured.recv().await.unwrap();
    assert_eq!(
        request.body["start_date"], "2026-01-01",
        "Tavily's start_date is exclusive, so the inclusive gateway range is widened by one day"
    );
    assert_eq!(request.body["end_date"], "2026-02-04");
    assert!(request.body.get("time_range").is_none());
    assert!(
        request.body.get("max_results").is_none(),
        "no count means Tavily's default"
    );
}

#[tokio::test]
async fn tavily_handler_returns_empty_sections_without_error() {
    let (base_url, _captured, _handle) = spawn_mock_tavily(
        StatusCode::OK,
        Vec::new(),
        serde_json::json!({"query": "nothing", "results": [], "response_time": 0.3}),
    )
    .await;
    let handler = tavily_handler(&base_url, None);

    let output = execute(&handler, r#"{"query":"nothing"}"#).await.unwrap();

    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output_json["results"], serde_json::json!({"web": [], "news": []}));
    assert_eq!(
        output_json["metadata"],
        serde_json::json!([{"provider": "tavily", "query": "nothing", "latency": 0.3}])
    );
    let public = handler
        .public_output(
            &call(r#"{"query":"nothing"}"#),
            &output,
            WebSearchCallStatus::Completed,
            &WebSearchToolParam::default(),
        )
        .unwrap();
    let OutputItem::WebSearchCall(item) = public else {
        panic!("expected web_search_call");
    };
    assert_eq!(item.status, WebSearchCallStatus::Completed);
    let action = serde_json::to_value(&item).unwrap()["action"].clone();
    assert!(
        action["sources"].as_array().is_none_or(Vec::is_empty),
        "empty results must not invent sources: {action}"
    );
}

#[tokio::test]
async fn tavily_handler_forwards_domain_filters_and_reapplies_them_client_side() {
    // The mock ignores `include_domains`, as a misconfigured proxy might, and
    // returns an off-list result the gateway must still drop.
    let (base_url, mut captured, _handle) = spawn_mock_tavily(
        StatusCode::OK,
        Vec::new(),
        serde_json::json!({"results": [
            {"url": "https://doc.rust-lang.org/book", "title": "The Book", "content": "On list"},
            {"url": "https://notrust-lang.org/x", "title": "Lookalike", "content": "Off list"},
            {"url": "https://example.com/off-list", "title": "Off list", "content": "Off list"}
        ]}),
    )
    .await;
    let handler = tavily_handler(&base_url, None);
    let params = WebSearchToolParam {
        filters: Some(WebSearchFilters {
            allowed_domains: Some(vec!["rust-lang.org".to_owned()]),
            blocked_domains: None,
        }),
        ..WebSearchToolParam::default()
    };

    let output = handler
        .execute(
            "call_tavily",
            "web_search",
            r#"{"query":"rust","include_domains":["ignored.example"]}"#,
            &params,
        )
        .await
        .unwrap();

    let request = captured.recv().await.unwrap();
    assert_eq!(
        request.body["include_domains"],
        serde_json::json!(["rust-lang.org"]),
        "the request-level allowlist wins over the model's list and is forwarded natively"
    );
    assert!(request.body.get("exclude_domains").is_none());
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(
        output_json["results"]["web"],
        serde_json::json!([{"url": "https://doc.rust-lang.org/book", "title": "The Book", "description": "On list"}])
    );

    let (base_url, mut captured, _handle) = spawn_mock_tavily(
        StatusCode::OK,
        Vec::new(),
        serde_json::json!({"results": [
            {"url": "https://spam.example/a", "title": "Spam"},
            {"url": "https://ham.example/b", "title": "Ham"}
        ]}),
    )
    .await;
    let handler = tavily_handler(&base_url, None);
    let output = execute(&handler, r#"{"query":"rust","exclude_domains":["spam.example"]}"#)
        .await
        .unwrap();
    assert_eq!(
        captured.recv().await.unwrap().body["exclude_domains"],
        serde_json::json!(["spam.example"])
    );
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(
        output_json["results"]["web"],
        serde_json::json!([{"url": "https://ham.example/b", "title": "Ham"}])
    );
}

#[tokio::test]
async fn tavily_handler_reports_rejected_credentials_without_leaking_them() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let (base_url, mut captured, _handle) =
            spawn_mock_tavily(status, Vec::new(), error_body("tvly-secret-key is invalid")).await;
        let handler = tavily_handler(&base_url, None);

        let message = execute(&handler, r#"{"query":"rust async"}"#).await.unwrap_err();

        assert_eq!(
            captured.recv().await.unwrap().authorization.as_deref(),
            Some("Bearer tvly-secret-key")
        );
        assert_eq!(
            message,
            format!("execution failed: Tavily rejected the API key ({status}); check TAVILY_API_KEY")
        );
        assert!(!message.contains(SECRET_KEY));
        assert!(!message.contains("is invalid"), "the upstream body is not echoed");
    }
}

#[tokio::test]
async fn tavily_handler_surfaces_rate_limits_without_retrying() {
    let (base_url, mut captured, _handle) = spawn_mock_tavily(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "7")],
        error_body("Rate limit exceeded"),
    )
    .await;
    let handler = tavily_handler(&base_url, None);

    let message = execute(&handler, r#"{"query":"rust async"}"#).await.unwrap_err();

    assert_eq!(
        message,
        "execution failed: Tavily rate limited the request (429 Too Many Requests); \
         the gateway does not retry; retry after 7"
    );
    captured.recv().await.expect("one request");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        captured.try_recv().is_err(),
        "a 429 must not trigger an automatic retry"
    );
}

#[tokio::test]
async fn tavily_handler_preserves_http_date_retry_after_and_reports_its_absence() {
    let retry_after = "Wed, 21 Oct 2026 07:28:00 GMT";
    let (base_url, _captured, _handle) = spawn_mock_tavily(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", retry_after)],
        serde_json::json!({}),
    )
    .await;
    let message = execute(&tavily_handler(&base_url, None), r#"{"query":"q"}"#)
        .await
        .unwrap_err();
    assert!(message.ends_with(retry_after), "{message}");

    let (base_url, _captured, _handle) =
        spawn_mock_tavily(StatusCode::TOO_MANY_REQUESTS, Vec::new(), serde_json::json!({})).await;
    let message = execute(&tavily_handler(&base_url, None), r#"{"query":"q"}"#)
        .await
        .unwrap_err();
    assert!(message.ends_with("no Retry-After header was provided"), "{message}");
}

#[tokio::test]
async fn tavily_handler_reports_plan_limits_without_retrying() {
    for code in [432_u16, 433] {
        let status = StatusCode::from_u16(code).unwrap();
        let (base_url, mut captured, _handle) =
            spawn_mock_tavily(status, Vec::new(), error_body("Usage limit exceeded")).await;

        let message = execute(&tavily_handler(&base_url, None), r#"{"query":"q"}"#)
            .await
            .unwrap_err();

        assert_eq!(
            message,
            format!(
                "execution failed: Tavily reported the plan usage limit was exceeded ({code}); the gateway does not retry"
            )
        );
        captured.recv().await.expect("one request");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(captured.try_recv().is_err(), "a {code} must not trigger a retry");
    }
}

#[tokio::test]
async fn tavily_handler_reports_other_upstream_failures_with_status() {
    let (base_url, _captured, _handle) = spawn_mock_tavily(
        StatusCode::INTERNAL_SERVER_ERROR,
        Vec::new(),
        error_body("upstream exploded"),
    )
    .await;

    let message = execute(&tavily_handler(&base_url, None), r#"{"query":"q"}"#)
        .await
        .unwrap_err();

    assert_eq!(
        message,
        r#"execution failed: Tavily search returned 500 Internal Server Error: {"detail":{"error":"upstream exploded"}}"#
    );
}

#[tokio::test]
async fn tavily_handler_rejects_invalid_json_bodies() {
    let app = Router::new().route(TAVILY_SEARCH_PATH, post(|| async { "not json" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let _handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let message = execute(&tavily_handler(&base_url, None), r#"{"query":"q"}"#)
        .await
        .unwrap_err();
    assert!(
        message.starts_with("execution failed: Tavily search returned invalid JSON:"),
        "{message}"
    );
}

/// Mock that records the peak number of in-flight requests.
async fn spawn_concurrency_tracking_tavily() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            TAVILY_SEARCH_PATH,
            post(
                |State((active, max_active)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
                 Json(body): Json<serde_json::Value>| async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    let query = body["query"].as_str().unwrap_or("unknown").to_owned();
                    Json(serde_json::json!({
                        "results": [{"url": format!("https://example.com/{}", query.replace(' ', "-")), "title": query}]
                    }))
                },
            ),
        )
        .with_state((active, Arc::clone(&max_active)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), max_active, handle)
}

#[tokio::test]
async fn tavily_handler_runs_batched_queries_concurrently_by_default() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_tavily().await;
    let handler = tavily_handler(&base_url, None);

    let output = execute(&handler, r#"{"queries":["one","two","three","four","five"]}"#)
        .await
        .unwrap();

    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output_json["results"]["web"].as_array().unwrap().len(), 5);
    assert_eq!(output_json["metadata"].as_array().unwrap().len(), 5);
    let peak = max_active.load(Ordering::SeqCst);
    assert!(
        (2..=5).contains(&peak),
        "Tavily inherits the gateway ceiling of 5, so batched queries overlap (peak {peak})"
    );
}

#[tokio::test]
async fn tavily_handler_honors_a_lowered_concurrency_override() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_tavily().await;
    let handler = tavily_handler(&base_url, NonZeroUsize::new(1));

    execute(&handler, r#"{"queries":["one","two","three","four","five"]}"#)
        .await
        .unwrap();

    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "max_concurrent_queries = 1 serializes the batch"
    );
}
