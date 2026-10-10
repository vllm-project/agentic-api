//! `/v1/embeddings` is forwarded to the upstream verbatim, like Chat Completions: the
//! gateway neither parses nor rewrites either direction.
#[allow(dead_code)]
mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{spawn_gateway, test_config, test_state, test_state_with_max_request_body_size};
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Irregular whitespace and an unknown field, so the forwarded body can be compared
/// byte for byte against what the upstream actually sent.
const UPSTREAM_EMBEDDINGS: &str = r#"{"object":"list",  "data":[{"object":"embedding","index":0,"embedding":[0.25,-0.5]}],
  "model":"m","usage":{"prompt_tokens":1,"total_tokens":1},"unknown_field":{"kept":true}}"#;

const UPSTREAM_ERROR: &str = r#"{"error":{"message":"input is too long","type":"BadRequestError","code":400}}"#;

/// Upstream `/v1/embeddings` that answers with a fixed response and records what it was sent.
#[derive(Clone)]
struct Upstream {
    status: StatusCode,
    body: &'static str,
    received: Arc<Mutex<Bytes>>,
    calls: Arc<AtomicUsize>,
}

impl Upstream {
    fn new(status: StatusCode, body: &'static str) -> Self {
        Self {
            status,
            body,
            received: Arc::default(),
            calls: Arc::default(),
        }
    }

    async fn spawn(&self) -> (String, tokio::task::JoinHandle<()>) {
        let upstream = self.clone();
        let app = Router::new().route(
            "/v1/embeddings",
            post(move |body: Bytes| async move {
                *upstream.received.lock().await = body;
                upstream.calls.fetch_add(1, Ordering::SeqCst);
                (
                    upstream.status,
                    [
                        (http::header::CONTENT_TYPE, "application/json"),
                        (http::HeaderName::from_static("x-upstream-marker"), "kept"),
                    ],
                    upstream.body,
                )
                    .into_response()
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), handle)
    }
}

#[tokio::test]
async fn embeddings_request_and_response_are_forwarded_unchanged() {
    let upstream = Upstream::new(StatusCode::OK, UPSTREAM_EMBEDDINGS);
    let (upstream_url, _upstream) = upstream.spawn().await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&upstream_url))).await;
    let request = r#"{"model":"m",  "input":["hi"],"encoding_format":"float","unknown_request_field":7}"#;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/embeddings"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(request)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("x-upstream-marker").unwrap(), "kept");
    assert_eq!(response.text().await.unwrap(), UPSTREAM_EMBEDDINGS);
    assert_eq!(&upstream.received.lock().await[..], request.as_bytes());
}

#[tokio::test]
async fn upstream_errors_are_forwarded_unchanged() {
    let upstream = Upstream::new(StatusCode::BAD_REQUEST, UPSTREAM_ERROR);
    let (upstream_url, _upstream) = upstream.spawn().await;
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(&upstream_url))).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/embeddings"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"m","input":"hi"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.text().await.unwrap(), UPSTREAM_ERROR);
}

#[tokio::test]
async fn oversized_body_is_rejected_before_the_upstream_is_called() {
    let upstream = Upstream::new(StatusCode::OK, UPSTREAM_EMBEDDINGS);
    let (upstream_url, _upstream) = upstream.spawn().await;
    let state = test_state_with_max_request_body_size(&test_config(&upstream_url), NonZeroUsize::new(64).unwrap());
    let (gateway_url, _gateway) = spawn_gateway(state).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/embeddings"))
        .body(format!(r#"{{"model":"m","input":"{}"}}"#, "x".repeat(256)))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);
}
