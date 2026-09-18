//! SearXNG provider behavior against a local Axum mock (#326).
//!
//! Every test binds its own `127.0.0.1:0` listener; nothing here reaches the
//! network. Mock handlers use `try_send` so a slow test never blocks inside
//! the server task.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentic_core::config::{WebSearchProviderConfig, WebSearchProviderKind};
use agentic_core::tool::{GatewayExecutor, SEARXNG_BASE_URL_HINT, WebSearchHandler};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::OutputItem;
use agentic_core::types::io::output::{FunctionToolCall, WebSearchCallStatus};
use agentic_core::types::tools::{WebSearchContextSize, WebSearchFilters, WebSearchToolParam};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

mod support;

const SEARXNG_SEARCH_PATH: &str = "/search";
const GATEWAY_LIMIT: NonZeroUsize = NonZeroUsize::new(5).expect("nonzero gateway limit");

#[derive(Debug)]
struct CapturedSearxngRequest {
    authorization: Option<String>,
    accept: Option<String>,
    accept_encoding: Option<String>,
    params: serde_json::Value,
}

/// Response body served by the mock: JSON like a healthy instance, or raw
/// text with an explicit content type like a misconfigured one.
#[derive(Clone)]
enum MockBody {
    Json(serde_json::Value),
    Raw {
        content_type: &'static str,
        body: &'static str,
    },
}

#[derive(Clone)]
struct MockSearxng {
    tx: mpsc::Sender<CapturedSearxngRequest>,
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: MockBody,
}

fn capture(headers: &HeaderMap, uri: &Uri) -> CapturedSearxngRequest {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    CapturedSearxngRequest {
        authorization: header("authorization"),
        accept: header("accept"),
        accept_encoding: header("accept-encoding"),
        params: support::query_params_as_json(uri),
    }
}

async fn spawn_mock_searxng(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: MockBody,
) -> (
    String,
    mpsc::Receiver<CapturedSearxngRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            SEARXNG_SEARCH_PATH,
            get(
                |State(mock): State<MockSearxng>, headers: HeaderMap, uri: Uri| async move {
                    mock.tx
                        .try_send(capture(&headers, &uri))
                        .expect("test channel has capacity");
                    let mut response = match mock.body {
                        MockBody::Json(body) => (mock.status, Json(body)).into_response(),
                        MockBody::Raw { content_type, body } => {
                            (mock.status, [(header::CONTENT_TYPE, content_type)], body).into_response()
                        }
                    };
                    for (name, value) in mock.headers {
                        response.headers_mut().insert(name, value.parse().unwrap());
                    }
                    response
                },
            ),
        )
        .with_state(MockSearxng {
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

async fn spawn_mock_json(
    status: StatusCode,
    body: serde_json::Value,
) -> (
    String,
    mpsc::Receiver<CapturedSearxngRequest>,
    tokio::task::JoinHandle<()>,
) {
    spawn_mock_searxng(status, Vec::new(), MockBody::Json(body)).await
}

fn searxng_handler(
    base_url: Option<&str>,
    api_key: Option<&str>,
    max_concurrent_queries: Option<NonZeroUsize>,
) -> WebSearchHandler {
    let config = WebSearchProviderConfig::new(api_key.map(str::to_owned), base_url.map(str::to_owned))
        .with_provider(WebSearchProviderKind::Searxng)
        .with_max_concurrent_queries(max_concurrent_queries);
    WebSearchHandler::from_config(Arc::new(reqwest::Client::new()), &config, GATEWAY_LIMIT)
}

/// A representative `format=json` envelope: ranking and cosmetic fields are
/// present so the test proves they are tolerated and dropped.
fn mixed_response() -> serde_json::Value {
    serde_json::json!({
        "query": "rust async",
        "number_of_results": 0,
        "results": [
            {
                "url": "https://example.com/rust",
                "title": "Rust async guide",
                "content": "A useful guide",
                "engine": "duckduckgo",
                "engines": ["duckduckgo", "brave"],
                "parsed_url": ["https", "example.com", "/rust", "", "", ""],
                "template": "default.html",
                "positions": [1, 3],
                "score": 2.5,
                "category": "general",
                "thumbnail": "https://example.com/rust.png"
            },
            {
                "url": "https://docs.example.org/tokio",
                "title": "Tokio",
                "content": "Runtime",
                "category": "general"
            },
            {
                "url": "https://news.example.com/async-release",
                "title": "Async release",
                "content": "Released today",
                "engine": "wikinews",
                "category": "news",
                "publishedDate": "2026-09-01T10:00:00",
                "pubdate": "2026-09-01 10:00:00"
            }
        ],
        "answers": [],
        "corrections": [],
        "infoboxes": [],
        "suggestions": ["rust async book"],
        "unresponsive_engines": [["bing", "timeout"]]
    })
}

fn call(arguments: &str) -> FunctionToolCall {
    FunctionToolCall {
        id: "fc_searxng".to_owned(),
        call_id: "call_searxng".to_owned(),
        name: "web_search".to_owned(),
        namespace: None,
        arguments: arguments.to_owned(),
        status: MessageStatus::Completed,
    }
}

async fn execute(handler: &WebSearchHandler, arguments: &str, params: &WebSearchToolParam) -> Result<String, String> {
    handler
        .execute("call_searxng", "web_search", arguments, params)
        .await
        .map(|output| output.output)
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn searxng_handler_maps_web_and_news_results_and_public_sources() {
    let (base_url, mut captured, _handle) = spawn_mock_json(StatusCode::OK, mixed_response()).await;
    let handler = searxng_handler(Some(&base_url), None, None);
    let params = WebSearchToolParam::default();
    let arguments = r#"{"query":"rust async","count":50,"freshness":"week","country":"gb","language":"en-GB","safesearch":"moderate","livecrawl":"web"}"#;

    let output = handler
        .execute("call_searxng", "web_search", arguments, &params)
        .await
        .unwrap();

    let request = captured.recv().await.expect("mock SearXNG should receive the request");
    assert_eq!(request.authorization, None, "SearXNG is keyless by default");
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    assert_eq!(
        request.accept_encoding, None,
        "core reqwest has no gzip support, so Accept-Encoding must never be sent"
    );
    assert_eq!(
        request.params,
        serde_json::json!({
            "q": "rust async",
            "format": "json",
            "categories": "general,news",
            "time_range": "week",
            "language": "en-GB",
            "safesearch": 1
        }),
        "freshness uses time_range, safesearch is numeric, count/country/livecrawl are not sent"
    );

    assert_eq!(output.call_id, "call_searxng");
    assert_eq!(
        output.output,
        concat!(
            r#"{"query":"rust async","queries":["rust async"],"#,
            r#""results":{"web":["#,
            r#"{"url":"https://example.com/rust","title":"Rust async guide","description":"A useful guide"},"#,
            r#"{"url":"https://docs.example.org/tokio","title":"Tokio","description":"Runtime"}],"#,
            r#""news":[{"url":"https://news.example.com/async-release","title":"Async release","#,
            r#""description":"Released today","page_age":"2026-09-01T10:00:00"}]},"#,
            r#""metadata":[{"provider":"searxng","query":"rust async"}]}"#
        )
    );

    let public = handler
        .public_output(&call(arguments), &output, WebSearchCallStatus::Completed, &params)
        .expect("web_search_call public output");
    assert_eq!(
        serde_json::to_value(&public).unwrap(),
        serde_json::json!({
            "id": "ws_searxng",
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
async fn searxng_handler_sends_bearer_token_when_a_key_is_configured() {
    let (base_url, mut captured, _handle) = spawn_mock_json(StatusCode::OK, mixed_response()).await;
    // A trailing slash on the configured endpoint must not double up.
    let handler = searxng_handler(Some(&format!("{base_url}/")), Some(" proxy-token "), None);

    execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap();

    let request = captured.recv().await.unwrap();
    assert_eq!(request.authorization.as_deref(), Some("Bearer proxy-token"));
}

#[tokio::test]
async fn searxng_handler_returns_empty_sections_without_error() {
    for body in [
        serde_json::json!({"query": "nothing", "results": []}),
        serde_json::json!({"query": "nothing", "results": null}),
        serde_json::json!({}),
    ] {
        let (base_url, _captured, _handle) = spawn_mock_json(StatusCode::OK, body).await;
        let handler = searxng_handler(Some(&base_url), None, None);

        let output = handler
            .execute(
                "call_searxng",
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
            serde_json::json!([{"provider": "searxng", "query": "nothing"}])
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
}

#[tokio::test]
async fn searxng_handler_applies_domain_filters_client_side() {
    let (base_url, mut captured, _handle) = spawn_mock_json(StatusCode::OK, mixed_response()).await;
    let handler = searxng_handler(Some(&base_url), None, None);

    // Tool-level allowlist wins over the model's arguments and is enforced
    // locally: SearXNG never sees a domain parameter.
    let params = WebSearchToolParam {
        filters: Some(WebSearchFilters {
            allowed_domains: Some(vec!["Example.com".to_owned()]),
            blocked_domains: None,
        }),
        ..WebSearchToolParam::default()
    };
    let output = execute(
        &handler,
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
        "no domain parameter must reach SearXNG: {}",
        request.params
    );
    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        output_json["results"]["web"],
        serde_json::json!([{"url": "https://example.com/rust", "title": "Rust async guide", "description": "A useful guide"}])
    );
    assert_eq!(
        output_json["results"]["news"],
        serde_json::json!([{"url": "https://news.example.com/async-release", "title": "Async release",
            "description": "Released today", "page_age": "2026-09-01T10:00:00"}])
    );

    // The model's blocklist applies on a label boundary to both sections.
    let output = execute(
        &handler,
        r#"{"query":"rust async","exclude_domains":["news.example.com","example.org"]}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap();
    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    let urls = |section: &str| {
        output_json["results"][section]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["url"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(urls("web"), ["https://example.com/rust"]);
    assert!(urls("news").is_empty());
}

#[tokio::test]
async fn searxng_handler_truncates_results_to_count_after_filtering() {
    let results: Vec<serde_json::Value> = (0..12)
        .map(|index| {
            let category = if index % 2 == 0 { "general" } else { "news" };
            let host = if index < 4 { "example.com" } else { "other.org" };
            serde_json::json!({"url": format!("https://{host}/{index}"), "title": format!("r{index}"), "category": category})
        })
        .collect();
    let (base_url, mut captured, _handle) =
        spawn_mock_json(StatusCode::OK, serde_json::json!({"results": results})).await;
    let handler = searxng_handler(Some(&base_url), None, None);

    let output = execute(
        &handler,
        r#"{"query":"q","count":1,"include_domains":["example.com"]}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap();
    let request = captured.recv().await.unwrap();
    assert!(request.params.get("count").is_none(), "SearXNG has no count parameter");
    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        output_json["results"],
        serde_json::json!({
            "web": [{"url": "https://example.com/0", "title": "r0"}],
            "news": [{"url": "https://example.com/1", "title": "r1"}]
        }),
        "filtering runs before truncation so the allowlist cannot starve a section"
    );

    // `search_context_size` supplies the default ceiling (`low` = 3) when the model omits `count`.
    let params = WebSearchToolParam {
        search_context_size: Some(WebSearchContextSize::Low),
        ..WebSearchToolParam::default()
    };
    let output = execute(&handler, r#"{"query":"q"}"#, &params).await.unwrap();
    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output_json["results"]["web"].as_array().unwrap().len(), 3);
    assert_eq!(output_json["results"]["news"].as_array().unwrap().len(), 3);

    // Without either, every hit the instance returned is passed on.
    let output = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap();
    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output_json["results"]["web"].as_array().unwrap().len(), 6);
    assert_eq!(output_json["results"]["news"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn searxng_handler_reports_disabled_json_format_on_403() {
    let (base_url, mut captured, _handle) = spawn_mock_searxng(
        StatusCode::FORBIDDEN,
        Vec::new(),
        MockBody::Raw {
            content_type: "text/html; charset=utf-8",
            body: "<!doctype html><title>403 Forbidden</title>",
        },
    )
    .await;
    let handler = searxng_handler(Some(&base_url), Some("proxy-token"), None);

    let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "execution failed: SearXNG refused the request (403 Forbidden); enable the JSON format with \
         `search.formats: [html, json]` in settings.yml, or check SEARXNG_API_KEY if the instance is behind an \
         authenticating proxy"
    );
    assert!(!error.contains("proxy-token"));
    assert_eq!(
        captured.recv().await.unwrap().authorization.as_deref(),
        Some("Bearer proxy-token")
    );
}

#[tokio::test]
async fn searxng_handler_reports_rejected_credential_without_leaking_it() {
    let (base_url, _captured, _handle) = spawn_mock_json(
        StatusCode::UNAUTHORIZED,
        serde_json::json!({"error": "token proxy-token is not valid"}),
    )
    .await;
    let handler = searxng_handler(Some(&base_url), Some("proxy-token"), None);

    let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "execution failed: SearXNG rejected the credential (401 Unauthorized); check SEARXNG_API_KEY"
    );
    assert!(!error.contains("proxy-token"), "upstream body must not be echoed");
}

#[tokio::test]
async fn searxng_handler_surfaces_limiter_blocks_without_retrying() {
    let (base_url, mut captured, _handle) = spawn_mock_searxng(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "30")],
        MockBody::Raw {
            content_type: "text/plain",
            body: "IP is on BLOCKLIST - HTTP header Accept-Encoding did not contain gzip nor deflate",
        },
    )
    .await;
    let handler = searxng_handler(Some(&base_url), None, None);

    let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "execution failed: SearXNG rate limited the request (429 Too Many Requests); the gateway does not retry; \
         retry after 30. If the instance runs with `server.limiter: true`, its bot detection blocks the gateway \
         (which cannot send Accept-Encoding: gzip): add the gateway address to `botdetection.ip_lists.pass_ip` in \
         limiter.toml or disable the limiter"
    );
    captured.recv().await.expect("one request");
    assert!(
        captured.try_recv().is_err(),
        "a rate-limited request must not be retried"
    );

    // Without Retry-After the message omits the hint but keeps the limiter guidance.
    let (base_url, _captured, _handle) = spawn_mock_json(StatusCode::TOO_MANY_REQUESTS, serde_json::json!({})).await;
    let error = execute(
        &searxng_handler(Some(&base_url), None, None),
        r#"{"query":"q"}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap_err();
    assert!(
        error.starts_with(
            "execution failed: SearXNG rate limited the request (429 Too Many Requests); the gateway does not retry. If"
        ),
        "{error}"
    );
}

#[tokio::test]
async fn searxng_handler_reports_html_body_as_misconfigured_endpoint() {
    let (base_url, _captured, _handle) = spawn_mock_searxng(
        StatusCode::OK,
        Vec::new(),
        MockBody::Raw {
            content_type: "text/html; charset=utf-8",
            body: "<!doctype html><html><body>SearXNG web UI</body></html>",
        },
    )
    .await;
    let handler = searxng_handler(Some(&base_url), None, None);

    let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap_err();
    assert!(
        error.starts_with("execution failed: SearXNG returned a non-JSON response ("),
        "{error}"
    );
    assert!(
        error.ends_with(
            "); confirm base_url points at a SearXNG instance and that `search.formats` in settings.yml includes `json`"
        ),
        "{error}"
    );
}

#[tokio::test]
async fn searxng_handler_reports_parameter_errors_and_other_failures_with_status() {
    let (base_url, _captured, _handle) = spawn_mock_json(
        StatusCode::BAD_REQUEST,
        serde_json::json!({"error": "Invalid value \"xx-XXX\" for parameter language"}),
    )
    .await;
    let error = execute(
        &searxng_handler(Some(&base_url), None, None),
        r#"{"query":"q"}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        r#"execution failed: SearXNG rejected a search parameter (400 Bad Request): {"error":"Invalid value \"xx-XXX\" for parameter language"}"#
    );

    let (base_url, _captured, _handle) = spawn_mock_searxng(
        StatusCode::BAD_GATEWAY,
        Vec::new(),
        MockBody::Raw {
            content_type: "text/plain",
            body: "upstream unavailable",
        },
    )
    .await;
    let error = execute(
        &searxng_handler(Some(&base_url), None, None),
        r#"{"query":"q"}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        "execution failed: SearXNG returned 502 Bad Gateway: upstream unavailable"
    );
}

#[tokio::test]
async fn searxng_handler_fails_without_base_url_naming_the_setting() {
    for base_url in [None, Some(""), Some("   ")] {
        let handler = searxng_handler(base_url, None, None);
        let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
            .await
            .unwrap_err();
        assert_eq!(error, format!("invalid tool config: {SEARXNG_BASE_URL_HINT}"));
    }
}

#[tokio::test]
async fn searxng_handler_rejects_unaddressable_base_urls_without_sending() {
    // Core callers bypass the server's startup check, so the provider applies
    // the same rules before any request leaves the gateway.
    let (base_url, mut captured, _handle) = spawn_mock_json(StatusCode::OK, mixed_response()).await;
    for (suffix, needle) in [
        ("?format=json", "must not contain a query or fragment"),
        ("/#search", "must not contain a query or fragment"),
    ] {
        let handler = searxng_handler(Some(&format!("{base_url}{suffix}")), None, None);
        let error = execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
            .await
            .unwrap_err();
        assert!(error.starts_with("invalid tool config: SearXNG base URL"), "{error}");
        assert!(error.contains(needle), "{error}");
    }
    let error = execute(
        &searxng_handler(Some("searxng:8080"), None, None),
        r#"{"query":"q"}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("must be an absolute http(s) URL"), "{error}");
    assert!(
        captured.try_recv().is_err(),
        "no request may be sent for an invalid endpoint"
    );

    // A sub-path mount resolves to `{base}/search`.
    let mounted = spawn_mounted_mock().await;
    let handler = searxng_handler(Some(&format!("{}/searxng/", mounted.0)), None, None);
    execute(&handler, r#"{"query":"q"}"#, &WebSearchToolParam::default())
        .await
        .unwrap();
    assert_eq!(mounted.1.lock().await.as_deref(), Some("/searxng/search"));
}

/// Mock that records the request path of the first `/searxng/search` hit.
async fn spawn_mounted_mock() -> (
    String,
    Arc<tokio::sync::Mutex<Option<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let seen = Arc::new(tokio::sync::Mutex::new(None));
    let app = Router::new()
        .route(
            "/searxng/search",
            get(
                |State(seen): State<Arc<tokio::sync::Mutex<Option<String>>>>, uri: Uri| async move {
                    *seen.lock().await = Some(uri.path().to_owned());
                    Json(serde_json::json!({"results": []}))
                },
            ),
        )
        .with_state(Arc::clone(&seen));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen, handle)
}

/// Mock that records the peak number of in-flight requests.
async fn spawn_concurrency_tracking_searxng() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            SEARXNG_SEARCH_PATH,
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
async fn searxng_handler_inherits_gateway_concurrency_by_default() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_searxng().await;
    let handler = searxng_handler(Some(&base_url), None, None);

    let output = execute(
        &handler,
        r#"{"queries":["one","two","three","four","five"]}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap();

    let output_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output_json["results"]["web"].as_array().unwrap().len(), 5);
    assert_eq!(output_json["metadata"].as_array().unwrap().len(), 5);
    let peak = max_active.load(Ordering::SeqCst);
    assert!(
        (2..=5).contains(&peak),
        "peak concurrency {peak} should reflect the inherited gateway limit of 5"
    );
}

#[tokio::test]
async fn searxng_handler_honors_a_lowered_concurrency_override() {
    let (base_url, max_active, _handle) = spawn_concurrency_tracking_searxng().await;
    let handler = searxng_handler(Some(&base_url), None, NonZeroUsize::new(1));

    execute(
        &handler,
        r#"{"queries":["one","two","three","four","five"]}"#,
        &WebSearchToolParam::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "the operator override serializes batched queries"
    );
}
