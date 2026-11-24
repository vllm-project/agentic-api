//! Upstream error metadata must survive the non-streaming Responses executor path.
//!
//! Regression coverage for <https://github.com/vllm-project/agentic-api/issues/250>: when a
//! request is routed through the in-process executor (for example `store: true`) and the
//! upstream returns a non-2xx response, the gateway must preserve the upstream status, body,
//! and processed metadata headers (`retry-after`, request IDs, rate-limit headers) and must not
//! relabel a non-JSON error body as `application/json`.

// `common` is compiled into every test binary; this one never calls `spawn_mock_llm`,
// so silence the resulting dead-code warning (same as the other tests that skip it).
#[allow(dead_code)]
mod common;

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http::{HeaderMap, StatusCode};
use tokio::net::TcpListener;

use common::{spawn_gateway, test_config, test_state};

const NON_STREAMING_STORED_REQUEST: &str = r#"{"model":"test","input":"hi","store":true,"stream":false}"#;
const NON_STREAMING_STATELESS_REQUEST: &str = r#"{"model":"test","input":"hi","store":false,"stream":false}"#;

/// Spawn a mock upstream whose `POST /v1/responses` always answers with the given error.
async fn spawn_error_upstream(
    status: StatusCode,
    content_type: &'static str,
    body: &'static str,
    extra_headers: HeaderMap,
) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let extra_headers = extra_headers.clone();
            async move {
                let mut response = Response::builder()
                    .status(status)
                    .header("content-type", content_type)
                    .body(axum::body::Body::from(body))
                    .unwrap();
                response.headers_mut().extend(extra_headers);
                response.into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

fn rate_limit_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", "7".parse().unwrap());
    headers.insert("x-request-id", "req_example".parse().unwrap());
    headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
    headers
}

async fn post_responses(gateway_url: &str, body: &'static str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway_url}/v1/responses"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn executor_path_preserves_upstream_json_error_metadata() {
    let upstream_body = r#"{"error":{"message":"rate limited","type":"rate_limit_error"}}"#;
    let (llm_url, _upstream) = spawn_error_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "application/json",
        upstream_body,
        rate_limit_headers(),
    )
    .await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = post_responses(&gateway_url, NON_STREAMING_STORED_REQUEST).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(response.headers()["x-request-id"], "req_example");
    assert_eq!(response.headers()["x-ratelimit-remaining-requests"], "0");
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.text().await.unwrap(), upstream_body);
}

#[tokio::test]
async fn executor_path_preserves_upstream_plain_text_error_content_type() {
    let upstream_body = "rate limited";
    let (llm_url, _upstream) = spawn_error_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "text/plain",
        upstream_body,
        rate_limit_headers(),
    )
    .await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = post_responses(&gateway_url, NON_STREAMING_STORED_REQUEST).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(response.headers()["x-request-id"], "req_example");
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.text().await.unwrap(), upstream_body);
}

#[tokio::test]
async fn executor_path_still_filters_connection_nominated_headers() {
    let mut headers = rate_limit_headers();
    headers.insert("connection", "x-upstream-hop".parse().unwrap());
    headers.insert("x-upstream-hop", "1".parse().unwrap());
    let (llm_url, _upstream) =
        spawn_error_upstream(StatusCode::TOO_MANY_REQUESTS, "text/plain", "rate limited", headers).await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = post_responses(&gateway_url, NON_STREAMING_STORED_REQUEST).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "7");
    assert!(response.headers().get("x-upstream-hop").is_none());
}

/// Stateless HTTP execution preserves the same upstream error metadata as stored requests.
#[tokio::test]
async fn stateless_execution_preserves_upstream_error_metadata() {
    let upstream_body = "rate limited";
    let (llm_url, _upstream) = spawn_error_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "text/plain",
        upstream_body,
        rate_limit_headers(),
    )
    .await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = post_responses(&gateway_url, NON_STREAMING_STATELESS_REQUEST).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(response.headers()["x-request-id"], "req_example");
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.text().await.unwrap(), upstream_body);
}

/// Control: gateway-originated errors keep their JSON envelope and content type.
#[tokio::test]
async fn malformed_client_json_still_returns_json_error_envelope() {
    let (llm_url, _upstream) = spawn_error_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "text/plain",
        "unreachable",
        rate_limit_headers(),
    )
    .await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;

    let response = post_responses(&gateway_url, r#"{"model":"test","input":"hi","store":true"#).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(response.headers().get("retry-after").is_none());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn initial_streaming_error_preserves_http_status_and_headers() {
    for store in [false, true] {
        let body = "rate limited";
        let (llm_url, upstream) =
            spawn_error_upstream(StatusCode::TOO_MANY_REQUESTS, "text/plain", body, rate_limit_headers()).await;
        let (gateway_url, gateway) = spawn_gateway(test_state(&test_config(&llm_url))).await;
        let response = reqwest::Client::new()
            .post(format!("{gateway_url}/v1/responses"))
            .json(&serde_json::json!({"model":"test", "input":"hi", "store":store, "stream":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "7");
        assert_eq!(response.headers()["x-request-id"], "req_example");
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(response.text().await.unwrap(), body);
        gateway.abort();
        upstream.abort();
    }
}

#[tokio::test]
async fn responses_executor_forwards_filtered_headers_and_preserves_auth_precedence() {
    for stream in [false, true] {
        let (sender, mut received) = tokio::sync::mpsc::channel(4);
        let app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap| {
                let sender = sender.clone();
                async move {
                    sender.send(headers).await.unwrap();
                    (StatusCode::TOO_MANY_REQUESTS, "rate limited")
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (gateway_url, gateway) = spawn_gateway(test_state(&test_config(&url))).await;
        for (caller_auth, api_key, expected_auth) in [
            (None, Some("caller-api-key"), None),
            (Some("Bearer caller-token"), None, Some("Bearer caller-token")),
            (
                Some("Basic caller-token"),
                Some("caller-api-key"),
                Some("Basic caller-token"),
            ),
            (None, None, Some("Bearer test-key")),
        ] {
            let mut request = reqwest::Client::new()
                .post(format!("{gateway_url}/v1/responses"))
                .header("openai-organization", "org_caller")
                .header("openai-project", "proj_caller")
                .header("connection", "x-client-hop")
                .header("x-client-hop", "must-not-forward")
                .json(&serde_json::json!({"model":"test", "input":"hi", "store":false, "stream":stream}));
            if let Some(auth) = caller_auth {
                request = request.header("authorization", auth);
            }
            if let Some(key) = api_key {
                request = request.header("x-api-key", key);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let headers = received.recv().await.unwrap();
            assert_eq!(headers.get("authorization").map(|v| v.to_str().unwrap()), expected_auth);
            assert_eq!(headers.get("x-api-key").map(|v| v.to_str().unwrap()), api_key);
            assert_eq!(headers["openai-organization"], "org_caller");
            assert_eq!(headers["openai-project"], "proj_caller");
            assert!(headers.get("x-client-hop").is_none());
        }
        gateway.abort();
        upstream.abort();
    }
}
