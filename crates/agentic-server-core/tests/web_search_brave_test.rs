//! Brave Search provider behavior against a local Axum mock (#294).
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
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod support;

const BRAVE_SEARCH_PATH: &str = "/res/v1/web/search";
const GATEWAY_LIMIT: NonZeroUsize = NonZeroUsize::new(5).expect("nonzero gateway limit");

#[derive(Debug)]
struct CapturedBraveRequest {
    subscription_token: String,
    accept: Option<String>,
    accept_encoding: Option<String>,
    params: serde_json::Value,
}

#[derive(Clone)]
struct MockBrave {
    tx: mpsc::Sender<CapturedBraveRequest>,
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
}

fn capture(headers: &HeaderMap, uri: &Uri) -> CapturedBraveRequest {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    CapturedBraveRequest {
        subscription_token: header("x-subscription-token").unwrap_or_default(),
        accept: header("accept"),
        accept_encoding: header("accept-encoding"),
        params: support::query_params_as_json(uri),
    }
}

async fn spawn_mock_brave(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
) -> (
    String,
    mpsc::Receiver<CapturedBraveRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            BRAVE_SEARCH_PATH,
            get(
                |State(mock): State<MockBrave>, headers: HeaderMap, uri: Uri| async move {
                    mock.tx
                        .try_send(capture(&headers, &uri))
                        .expect("test channel has capacity");
                    let mut response = (mock.status, Json(mock.body.clone())).into_response();
                    for (name, value) in mock.headers {
                        response.headers_mut().insert(name, value.parse().unwrap());
                    }
                    response
                },
            ),
        )
        .with_state(MockBrave {
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

fn brave_handler(base_url: &str, max_concurrent_queries: Option<NonZeroUsize>) -> WebSearchHandler {
    let config = WebSearchProviderConfig::new(Some("secret-brave-key".to_owned()), Some(base_url.to_owned()))
        .with_provider(WebSearchProviderKind::Brave)
        .with_max_concurrent_queries(max_concurrent_queries);
    WebSearchHandler::from_config(Arc::new(reqwest::Client::new()), &config, GATEWAY_LIMIT)
}

fn mixed_response() -> serde_json::Value {
    serde_json::json!({
        "type": "search",
        "query": {"original": "rust async", "show_strict_warning": false},
        "mixed": {"type": "mixed", "main": [{"type": "web", "index": 0, "all": false}]},
        "web": {
            "type": "search",
            "family_friendly": true,
            "results": [
                {
                    "title": "Rust async guide",
                    "url": "https://example.com/rust",
                    "description": "A useful guide",
                    "page_age": "2026-09-01T10:00:00",
                    "age": "2 weeks ago",
                    "language": "en",
                    "family_friendly": true,
                    "extra_snippets": ["Use async carefully."],
                    "thumbnail": {"src": "https://imgs.search.brave.com/x", "original": "https://example.com/x.png"},
                    "meta_url": {"scheme": "https", "netloc": "example.com", "hostname": "example.com"}
                },
                {
                    "title": "Tokio",
                    "url": "https://docs.example.org/tokio",
                    "description": "Runtime",
                    "profile": {"name": "Example", "url": "https://docs.example.org"}
                }
            ]
        },
        "news": {
            "type": "news",
            "results": [
                {
                    "title": "Async release",
                    "url": "https://news.example.com/async-release",
                    "description": "Released today",
                    "age": "3 hours ago",
                    "source": "Example News",
                    "breaking": false
                }
            ]
        }
    })
}

fn call(arguments: &str) -> FunctionToolCall {
    FunctionToolCall {
        agent: None,
        id: "fc_brave".to_owned(),
        call_id: "call_brave".to_owned(),
        name: "web_search".to_owned(),
        namespace: None,
        arguments: arguments.to_owned(),
        status: MessageStatus::Completed,
    }
}

#[tokio::test]
async fn brave_handler_maps_web_and_news_results_and_public_sources() {
    let (base_url, mut captured, _handle) = spawn_mock_brave(StatusCode::OK, Vec::new(), mixed_response()).await;
    let handler = brave_handler(&base_url, None);
    let params = WebSearchToolParam::default();
    let arguments = r#"{"query":"rust async","count":50,"freshness":"week","country":"gb","language":"en-GB","safesearch":"moderate","livecrawl":"web"}"#;

    let output = handler
        .execute("call_brave", "web_search", arguments, &params)
        .await
        .unwrap();

    let request = captured.recv().await.expect("mock Brave should receive the request");
    assert_eq!(request.subscription_token, "secret-brave-key");
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    assert_eq!(
        request.accept_encoding, None,
        "core reqwest has no gzip support, so Accept-Encoding must never be sent"
    );
    assert_eq!(
        request.params,
        serde_json::json!({
            "q": "rust async",
            "result_filter": "web,news",
            "text_decorations": "false",
            "count": 20,
            "freshness": "pw",
            "country": "GB",
            "search_lang": "en-gb",
            "safesearch": "moderate"
        }),
        "count is clamped to 20, freshness uses Brave syntax, You.com-only arguments are dropped"
    );

    assert_eq!(output.call_id, "call_brave");
    assert_eq!(
        output.output,
        concat!(
            r#"{"query":"rust async","queries":["rust async"],"#,
            r#""results":{"web":["#,
            r#"{"url":"https://example.com/rust","title":"Rust async guide","description":"A useful guide","#,
            r#""snippets":["Use async carefully."],"page_age":"2026-09-01T10:00:00"},"#,
            r#"{"url":"https://docs.example.org/tokio","title":"Tokio","description":"Runtime"}],"#,
            r#""news":[{"url":"https://news.example.com/async-release","title":"Async release","#,
            r#""description":"Released today","page_age":"3 hours ago"}]},"#,
            r#""metadata":[{"provider":"brave","query":"rust async"}]}"#
        )
    );

    let public = handler
        .public_output(&call(arguments), &output, WebSearchCallStatus::Completed, &params)
        .expect("web_search_call public output");
    assert_eq!(
        serde_json::to_value(&public).unwrap(),
        serde_json::json!({
            "id": "ws_brave",
            "type": "web_search_call",
            "status": "completed",
            "action": {
                "type": "search",
                "query": "rust async",
                "queries": ["rust async"],
                "sources": [
                    {"url": "https://example.com/rust", "title": "Rust async guide"},
                    {"url": "https://docs.example.org/tokio", "title": "Tokio"},
                    {"url": "https://news.example.com/async-release", "title": "Async release"}
                ]
            }
        })
    );
}

#[tokio::test]
async fn brave_handler_returns_empty_sections_without_error() {
    let (base_url, _captured, _handle) = spawn_mock_brave(
        StatusCode::OK,
        Vec::new(),
        serde_json::json!({"type": "search", "query": {"original": "nothing"}}),
    )
    .await;
    let handler = brave_handler(&base_url, None);

    let output = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"nothing"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap();

    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output_json["results"], serde_json::json!({"web": [], "news": []}));
    assert_eq!(
        output_json["metadata"],
        serde_json::json!([{"provider": "brave", "query": "nothing"}])
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
    let action = serde_json::to_value(&item).unwrap()["action"].clone();
    assert!(
        action["sources"].as_array().is_none_or(Vec::is_empty),
        "empty results must not invent sources: {action}"
    );
}

#[tokio::test]
async fn brave_handler_applies_domain_filters_client_side() {
    let (base_url, mut captured, _handle) = spawn_mock_brave(StatusCode::OK, Vec::new(), mixed_response()).await;
    let handler = brave_handler(&base_url, None);

    // Tool-level allowlist wins over the model's arguments and is enforced
    // locally: Brave never sees a domain parameter.
    let params = WebSearchToolParam {
        filters: Some(WebSearchFilters {
            allowed_domains: Some(vec!["Example.com".to_owned()]),
            blocked_domains: None,
        }),
        ..WebSearchToolParam::default()
    };
    let output = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"rust async","include_domains":["example.org"]}"#,
            &params,
        )
        .await
        .unwrap();
    let request = captured.recv().await.unwrap();
    assert!(
        request
            .params
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.contains("domain")),
        "{:?}",
        request.params
    );
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(
        output_json["results"]["web"],
        serde_json::json!([{
            "url": "https://example.com/rust",
            "title": "Rust async guide",
            "description": "A useful guide",
            "snippets": ["Use async carefully."],
            "page_age": "2026-09-01T10:00:00"
        }])
    );
    assert_eq!(
        output_json["results"]["news"][0]["url"], "https://news.example.com/async-release",
        "subdomains of an allowed domain match"
    );

    // A blocklist from the model's arguments removes matching hosts only.
    let output = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"rust async","exclude_domains":["example.com"]}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap();
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(
        output_json["results"]["web"],
        serde_json::json!([{"url": "https://docs.example.org/tokio", "title": "Tokio", "description": "Runtime"}])
    );
    assert_eq!(output_json["results"]["news"], serde_json::json!([]));
}

#[tokio::test]
async fn brave_handler_reports_rejected_credentials_without_leaking_them() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let (base_url, mut captured, _handle) = spawn_mock_brave(
            status,
            Vec::new(),
            serde_json::json!({"type": "ErrorResponse", "error": {"status": status.as_u16(), "detail": "secret-brave-key is invalid"}}),
        )
        .await;
        let handler = brave_handler(&base_url, None);

        let error = handler
            .execute(
                "call_brave",
                "web_search",
                r#"{"query":"rust async"}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .unwrap_err();

        assert_eq!(captured.recv().await.unwrap().subscription_token, "secret-brave-key");
        let message = error.to_string();
        assert_eq!(
            message,
            format!("execution failed: Brave Search rejected the API key ({status}); check BRAVE_API_KEY")
        );
        assert!(!message.contains("secret-brave-key"));
    }
}

#[tokio::test]
async fn brave_handler_surfaces_rate_limits_without_retrying() {
    let (base_url, mut captured, _handle) = spawn_mock_brave(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "7"), ("x-ratelimit-reset", "7, 1234")],
        serde_json::json!({"type": "ErrorResponse", "error": {"status": 429, "detail": "Rate limit exceeded"}}),
    )
    .await;
    let handler = brave_handler(&base_url, None);

    let error = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"rust async"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "execution failed: Brave Search rate limited the request (429 Too Many Requests); \
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
async fn brave_handler_falls_back_to_rate_limit_reset_header() {
    let (base_url, _captured, _handle) = spawn_mock_brave(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("x-ratelimit-reset", "3")],
        serde_json::json!({}),
    )
    .await;
    let handler = brave_handler(&base_url, None);

    let error = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"q"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().ends_with("retry after 3"), "{error}");
}

#[tokio::test]
async fn brave_handler_reports_other_upstream_failures_with_status() {
    let (base_url, _captured, _handle) = spawn_mock_brave(
        StatusCode::UNPROCESSABLE_ENTITY,
        Vec::new(),
        serde_json::json!({"error": {"detail": "invalid search_lang"}}),
    )
    .await;
    let handler = brave_handler(&base_url, None);

    let error = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"q"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        r#"execution failed: Brave Search returned 422 Unprocessable Entity: {"error":{"detail":"invalid search_lang"}}"#
    );
}

/// Mock that records the peak number of in-flight requests.
async fn spawn_concurrency_tracking_brave() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            BRAVE_SEARCH_PATH,
            get(
                |State((active, max_active)): State<(Arc<AtomicUsize>, Arc<AtomicUsize>)>, uri: Uri| async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    let query = support::query_params_as_json(&uri)["q"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned();
                    Json(serde_json::json!({
                        "web": {"results": [{"url": format!("https://example.com/{}", query.replace(' ', "-")), "title": query}]}
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
async fn brave_handler_serializes_batched_queries_by_default() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_brave().await;
    let handler = brave_handler(&base_url, None);

    let output = handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"queries":["one","two","three","four","five"]}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap();

    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output_json["results"]["web"].as_array().unwrap().len(), 5);
    assert_eq!(output_json["metadata"].as_array().unwrap().len(), 5);
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "Brave defaults to one in-flight request even though the gateway allows 5"
    );
}

#[tokio::test]
async fn brave_handler_honors_a_raised_concurrency_override() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_brave().await;
    let handler = brave_handler(&base_url, NonZeroUsize::new(3));

    handler
        .execute(
            "call_brave",
            "web_search",
            r#"{"queries":["one","two","three","four","five"]}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap();

    let peak = max_active.load(Ordering::SeqCst);
    assert!(
        (2..=3).contains(&peak),
        "peak concurrency {peak} should reflect the override of 3"
    );
}

#[tokio::test]
async fn brave_handler_preserves_http_date_retry_after() {
    let retry_after = "Wed, 21 Oct 2026 07:28:00 GMT";
    let (base_url, _captured, _handle) = spawn_mock_brave(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", retry_after)],
        serde_json::json!({}),
    )
    .await;
    let error = brave_handler(&base_url, None)
        .execute(
            "call_brave",
            "web_search",
            r#"{"query":"q"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().ends_with(retry_after), "{error}");
}
