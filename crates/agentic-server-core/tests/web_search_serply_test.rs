//! Serply provider behavior against a local Axum mock (#381).
//!
//! Every test binds its own `127.0.0.1:0` listener; nothing here reaches the
//! network. Mock handlers use `try_send` so a slow test never blocks inside
//! the server task.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::config::{WebSearchProviderConfig, WebSearchProviderKind};
use agentic_core::tool::{GatewayExecutor, WebSearchHandler};
use agentic_core::types::event::MessageStatus;
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

const SERPLY_SEARCH_PATH: &str = "/v1/search";
const GATEWAY_LIMIT: NonZeroUsize = NonZeroUsize::new(5).expect("nonzero gateway limit");
const SECRET_KEY: &str = "serply-secret-key";

#[derive(Debug)]
struct CapturedSerplyRequest {
    api_key: Option<String>,
    accept: Option<String>,
    accept_encoding: Option<String>,
    params: serde_json::Value,
}

#[derive(Clone)]
struct MockSerply {
    tx: mpsc::Sender<CapturedSerplyRequest>,
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
}

fn capture(headers: &HeaderMap, uri: &Uri) -> CapturedSerplyRequest {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    CapturedSerplyRequest {
        api_key: header("x-api-key"),
        accept: header("accept"),
        accept_encoding: header("accept-encoding"),
        params: support::query_params_as_json(uri),
    }
}

async fn spawn_mock_serply(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: serde_json::Value,
) -> (
    String,
    mpsc::Receiver<CapturedSerplyRequest>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(16);
    let app = Router::new()
        .route(
            SERPLY_SEARCH_PATH,
            get(
                |State(mock): State<MockSerply>, headers: HeaderMap, uri: Uri| async move {
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
        .with_state(MockSerply {
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

fn serply_handler(base_url: &str) -> WebSearchHandler {
    let config = WebSearchProviderConfig::new(Some(SECRET_KEY.to_owned()), Some(base_url.to_owned()))
        .with_provider(WebSearchProviderKind::Serply);
    WebSearchHandler::from_config(Arc::new(reqwest::Client::new()), &config, GATEWAY_LIMIT)
}

fn search_response() -> serde_json::Value {
    serde_json::json!({
        "results": [
            {
                "title": "Rust async guide",
                "link": "https://example.com/rust",
                "description": "A useful guide",
                "position": 1,
                "realPosition": 1,
                "result_type": "organic",
                "metadata": {"display_url": "example.com", "published_time": "2 days ago"}
            },
            {
                "title": "Tokio",
                "link": "https://www.google.com/url?opi=89978449&q=https://docs.example.org/tokio&sa=U",
                "description": "Runtime",
                "position": 2,
                "realPosition": 2,
                "result_type": "organic"
            }
        ],
        "ads": [],
        "related_searches": {"images": [], "text": []},
        "knowledge_graph": {},
        "ts": 1.5,
        "device_type": "desktop",
        "query": "q=rust+async"
    })
}

fn call(arguments: &str) -> FunctionToolCall {
    FunctionToolCall {
        agent: None,
        id: "fc_serply".to_owned(),
        call_id: "call_serply".to_owned(),
        name: "web_search".to_owned(),
        namespace: None,
        arguments: arguments.to_owned(),
        status: MessageStatus::Completed,
    }
}

#[tokio::test]
async fn serply_handler_maps_results_and_public_sources() {
    let (base_url, mut captured, _handle) = spawn_mock_serply(StatusCode::OK, Vec::new(), search_response()).await;
    let handler = serply_handler(&base_url);
    let params = WebSearchToolParam::default();
    let arguments = r#"{"query":"rust async","count":50,"freshness":"week","country":"GB","language":"en","safesearch":"strict","livecrawl":"web"}"#;

    let output = handler
        .execute("call_serply", "web_search", arguments, &params)
        .await
        .unwrap();

    let request = captured.recv().await.expect("mock Serply should receive the request");
    assert_eq!(request.api_key.as_deref(), Some(SECRET_KEY));
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    assert_eq!(
        request.accept_encoding, None,
        "core reqwest has no gzip support, so Accept-Encoding must never be sent"
    );
    assert_eq!(
        request.params,
        serde_json::json!({
            "q": "rust async",
            "num": 10,
            "tbs": "qdr:w",
            "gl": "gb",
            "hl": "en",
            "safe": "active"
        }),
        "num is clamped to 10, freshness uses Google's tbs, You.com-only arguments are dropped"
    );

    assert_eq!(
        output.output,
        concat!(
            r#"{"query":"rust async","queries":["rust async"],"#,
            r#""results":{"web":["#,
            r#"{"url":"https://example.com/rust","title":"Rust async guide","description":"A useful guide","#,
            r#""page_age":"2 days ago"},"#,
            r#"{"url":"https://docs.example.org/tokio","title":"Tokio","description":"Runtime"}],"#,
            r#""news":[]},"#,
            r#""metadata":[{"provider":"serply","query":"rust async","latency":1.5}]}"#
        )
    );

    let public = handler
        .public_output(&call(arguments), &output, WebSearchCallStatus::Completed, &params)
        .expect("web_search_call public output");
    assert_eq!(
        serde_json::to_value(&public).unwrap()["action"]["sources"],
        serde_json::json!([
            {"url": "https://example.com/rust", "title": "Rust async guide"},
            {"url": "https://docs.example.org/tokio", "title": "Tokio"}
        ])
    );
}

#[tokio::test]
async fn serply_handler_sends_site_operators_and_filters_client_side() {
    let (base_url, mut captured, _handle) = spawn_mock_serply(StatusCode::OK, Vec::new(), search_response()).await;
    let handler = serply_handler(&base_url);

    // The tool-level allowlist wins over the model's arguments, is sent as a
    // `site:` operator, and is re-checked on the response.
    let params = WebSearchToolParam {
        filters: Some(WebSearchFilters {
            allowed_domains: Some(vec!["Example.com".to_owned()]),
            blocked_domains: None,
        }),
        ..WebSearchToolParam::default()
    };
    let output = handler
        .execute(
            "call_serply",
            "web_search",
            r#"{"query":"rust async","include_domains":["example.org"],"freshness":"2026-01-02to2026-02-03"}"#,
            &params,
        )
        .await
        .unwrap();
    let request = captured.recv().await.unwrap();
    assert_eq!(
        request.params,
        serde_json::json!({"q": "rust async site:Example.com after:2026-01-02 before:2026-02-04"})
    );
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    let urls = |output: &serde_json::Value| {
        output["results"]["web"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["url"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(urls(&output_json), ["https://example.com/rust"]);

    // A blocklist from the model's arguments becomes `-site:` operators.
    let output = handler
        .execute(
            "call_serply",
            "web_search",
            r#"{"query":"rust async","exclude_domains":["example.com"]}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap();
    let request = captured.recv().await.unwrap();
    assert_eq!(request.params["q"], "rust async -site:example.com");
    let output_json: serde_json::Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(urls(&output_json), ["https://docs.example.org/tokio"]);
}

#[tokio::test]
async fn serply_handler_reports_rejected_credentials_without_leaking_them() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let (base_url, _captured, _handle) = spawn_mock_serply(
            status,
            Vec::new(),
            serde_json::json!({"detail": format!("{SECRET_KEY} is invalid")}),
        )
        .await;

        let error = serply_handler(&base_url)
            .execute(
                "call_serply",
                "web_search",
                r#"{"query":"rust async"}"#,
                &WebSearchToolParam::default(),
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert_eq!(
            message,
            format!("execution failed: Serply rejected the API key ({status}); check SERPLY_API_KEY")
        );
        assert!(!message.contains(SECRET_KEY));
    }
}

#[tokio::test]
async fn serply_handler_surfaces_rate_limits_without_retrying() {
    let (base_url, mut captured, _handle) = spawn_mock_serply(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "7")],
        serde_json::json!({"detail": "Too many requests"}),
    )
    .await;

    let error = serply_handler(&base_url)
        .execute(
            "call_serply",
            "web_search",
            r#"{"query":"rust async"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "execution failed: Serply rate limited the request (429 Too Many Requests); \
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
async fn serply_handler_reports_other_upstream_failures_with_status() {
    let (base_url, _captured, _handle) = spawn_mock_serply(
        StatusCode::UNPROCESSABLE_ENTITY,
        Vec::new(),
        serde_json::json!({"detail": "invalid gl"}),
    )
    .await;

    let error = serply_handler(&base_url)
        .execute(
            "call_serply",
            "web_search",
            r#"{"query":"q"}"#,
            &WebSearchToolParam::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        r#"execution failed: Serply search returned 422 Unprocessable Entity: {"detail":"invalid gl"}"#
    );
}
