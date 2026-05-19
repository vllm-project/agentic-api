use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::stream;
use http::StatusCode;
use tokio::net::TcpListener;

use agentic_core::config::Config;
use agentic_core::proxy::ProxyState;

pub fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("env-llm-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
    }
}

pub fn test_config_no_key(llm_url: &str) -> Config {
    Config {
        openai_api_key: None,
        ..test_config(llm_url)
    }
}

async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}

async fn responses_handler(req: Request) -> Response {
    let headers = req.headers().clone();
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap_or_default();

    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();

    if body
        .get("echo_auth")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
        let resp_body = serde_json::json!({"authorization": auth});
        return (
            StatusCode::OK,
            [("content-type", "application/json"), ("x-llm", "responses")],
            serde_json::to_string(&resp_body).unwrap(),
        )
            .into_response();
    }

    if body.get("force_error").and_then(serde_json::Value::as_u64) == Some(429) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json"), ("x-llm", "error")],
            r#"{"error":{"message":"rate limited","code":"rate_limit"}}"#,
        )
            .into_response();
    }

    if body.get("stream").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        let chunks: Vec<Result<Bytes, Infallible>> = vec![
            Ok(Bytes::from(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            )),
            Ok(Bytes::from("data: [DONE]\n\n")),
        ];
        let body = Body::from_stream(stream::iter(chunks));
        return (
            StatusCode::OK,
            [
                ("content-type", "text/event-stream; charset=utf-8"),
                ("x-llm", "responses-stream"),
            ],
            body,
        )
            .into_response();
    }

    let out = r#"{"id":"resp_test","object":"response","status":"completed"}"#;
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("x-llm", "responses"),
            ("connection", "keep-alive"),
        ],
        out,
    )
        .into_response()
}

pub async fn spawn_llm() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(responses_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), handle)
}

pub async fn spawn_gateway(config: Config) -> (String, SocketAddr, tokio::task::JoinHandle<()>) {
    let state = ProxyState::new(config).unwrap();
    let router = agentic_server::app::build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (format!("http://{addr}"), addr, handle)
}

pub async fn spawn_mid_stream_failure_llm() -> (String, tokio::task::JoinHandle<()>) {
    async fn handler(_req: Request) -> Response {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                )))
                .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(tx);
        });
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = Body::from_stream(stream);
        (
            StatusCode::OK,
            [
                ("content-type", "text/event-stream; charset=utf-8"),
                ("x-llm", "fake-stream"),
            ],
            body,
        )
            .into_response()
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), handle)
}

pub async fn spawn_timeout_llm() -> (String, tokio::task::JoinHandle<()>) {
    async fn handler(_req: Request) -> Response {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        StatusCode::OK.into_response()
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), handle)
}

pub fn proxy_state_with_short_timeout(config: Config) -> ProxyState {
    let stream_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(100))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let non_stream_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(100))
        .read_timeout(Duration::from_millis(100))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    ProxyState {
        config,
        stream_client,
        non_stream_client,
    }
}
