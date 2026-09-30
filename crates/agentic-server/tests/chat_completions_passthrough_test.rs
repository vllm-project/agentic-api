//! `/v1/chat/completions` and `/v1/completions` are forwarded to the upstream
//! verbatim: the gateway neither parses nor rewrites either direction.
#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{spawn_gateway, test_config, test_state};
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Irregular whitespace and an unknown field, so the forwarded body can be compared
/// byte for byte against what the upstream actually sent.
const UPSTREAM_COMPLETION: &str = r#"{"id":"chatcmpl-1",  "object":"chat.completion",
  "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
  "unknown_field":{"kept":true}}"#;

const UPSTREAM_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: [DONE]\n\n"
);

#[derive(Clone, Default)]
struct Seen {
    path: Arc<Mutex<String>>,
    body: Arc<Mutex<Bytes>>,
    authorization: Arc<Mutex<Option<String>>>,
    calls: Arc<AtomicUsize>,
}

/// Upstream that records what it was called with and answers JSON or SSE.
async fn spawn_upstream(seen: Seen) -> (String, tokio::task::JoinHandle<()>) {
    async fn handle(seen: Seen, path: &'static str, req: axum::extract::Request) -> axum::response::Response {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let stream = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("stream")?.as_bool())
            .unwrap_or(false);

        *seen.path.lock().await = path.to_owned();
        *seen.body.lock().await = body;
        *seen.authorization.lock().await = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        seen.calls.fetch_add(1, Ordering::SeqCst);

        if stream {
            (
                StatusCode::OK,
                [
                    (http::header::CONTENT_TYPE, "text/event-stream"),
                    (http::HeaderName::from_static("x-upstream-marker"), "kept"),
                ],
                UPSTREAM_SSE,
            )
                .into_response()
        } else {
            (
                StatusCode::OK,
                [
                    (http::header::CONTENT_TYPE, "application/json"),
                    (http::HeaderName::from_static("x-upstream-marker"), "kept"),
                ],
                UPSTREAM_COMPLETION,
            )
                .into_response()
        }
    }

    let chat = seen.clone();
    let text = seen.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |req| handle(chat.clone(), "/v1/chat/completions", req)),
        )
        .route(
            "/v1/completions",
            post(move |req| handle(text.clone(), "/v1/completions", req)),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

async fn spawn_pair() -> (String, Seen, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let seen = Seen::default();
    let (upstream_url, upstream) = spawn_upstream(seen.clone()).await;
    let (gateway_url, gateway) = spawn_gateway(test_state(&test_config(&upstream_url))).await;
    (gateway_url, seen, upstream, gateway)
}

#[tokio::test]
async fn chat_completions_request_and_response_are_forwarded_unchanged() {
    let (gateway_url, seen, _upstream, _gateway) = spawn_pair().await;
    let request = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"unknown_request_field":7}"#;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-upstream-marker").unwrap(), "kept");
    assert_eq!(response.text().await.unwrap(), UPSTREAM_COMPLETION);

    assert_eq!(*seen.path.lock().await, "/v1/chat/completions");
    assert_eq!(&seen.body.lock().await[..], request.as_bytes());
}

#[tokio::test]
async fn completions_is_forwarded_to_its_own_upstream_path() {
    let (gateway_url, seen, _upstream, _gateway) = spawn_pair().await;
    let request = r#"{"model":"m","prompt":"hi"}"#;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), UPSTREAM_COMPLETION);
    assert_eq!(*seen.path.lock().await, "/v1/completions");
    assert_eq!(&seen.body.lock().await[..], request.as_bytes());
}

#[tokio::test]
async fn streaming_chat_completions_is_relayed_as_sse() {
    let (gateway_url, _seen, _upstream, _gateway) = spawn_pair().await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.text().await.unwrap(), UPSTREAM_SSE);
}

#[tokio::test]
async fn configured_api_key_is_injected_only_when_the_client_sends_none() {
    let (gateway_url, seen, _upstream, _gateway) = spawn_pair().await;
    let client = reqwest::Client::new();
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;

    client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(seen.authorization.lock().await.as_deref(), Some("Bearer test-key"));

    client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::AUTHORIZATION, "Bearer caller-key")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(seen.authorization.lock().await.as_deref(), Some("Bearer caller-key"));
}

#[tokio::test]
async fn oversized_body_is_rejected_before_the_upstream_is_called() {
    let seen = Seen::default();
    let (upstream_url, _upstream) = spawn_upstream(seen.clone()).await;
    let config = test_config(&upstream_url);
    let state = common::test_state_with_max_request_body_size(&config, std::num::NonZeroUsize::new(64).unwrap());
    let (gateway_url, _gateway) = spawn_gateway(state).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .body(format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"{}"}}]}}"#,
            "x".repeat(256)
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
}
