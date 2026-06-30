use std::convert::Infallible;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, serve};
use bytes::Bytes;
use criterion::{Criterion, criterion_group};
use futures::stream;
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use agentic_core::config::Config;
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::proxy::ProxyState;
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_server::app::{AppState, ServerConfig, build_router};
use std::sync::Arc;

fn bench_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("bench-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
        db_url: None,
    }
}

async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}

async fn responses_handler(req: Request) -> Response {
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap_or_default();

    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();

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
            [("content-type", "text/event-stream; charset=utf-8")],
            body,
        )
            .into_response();
    }

    let out = r#"{"id":"resp_bench","object":"response","status":"completed"}"#;
    (StatusCode::OK, [("content-type", "application/json")], out).into_response()
}

async fn spawn_llm() -> String {
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(responses_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

async fn spawn_gateway(config: Config) -> String {
    let proxy_state = ProxyState::new(config.clone()).unwrap();
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
        config.openai_api_key.clone(),
    ));
    let state = AppState {
        proxy_state,
        exec_ctx,
        shutdown_token: CancellationToken::new(),
        llm_api_base: config.llm_api_base,
    };
    let server_config = ServerConfig::from_env();
    let router = build_router(state, &server_config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        serve(listener, router).await.unwrap();
    });

    format!("http://{addr}")
}

fn proxy_benchmarks(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let (llm_url, gateway_url) = rt.block_on(async {
        let llm_url = spawn_llm().await;
        let config = bench_config(&llm_url);
        let gateway_url = spawn_gateway(config).await;
        (llm_url, gateway_url)
    });

    let client = reqwest::Client::new();

    let non_stream_body = serde_json::json!({
        "model": "bench-model",
        "input": [{"role": "user", "content": "hello"}]
    });
    let stream_body = serde_json::json!({
        "model": "bench-model",
        "input": [{"role": "user", "content": "hello"}],
        "stream": true
    });

    let mut group = c.benchmark_group("non_stream");

    group.bench_function("direct", |b| {
        let url = format!("{llm_url}/v1/responses");
        let body = non_stream_body.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let resp = client.post(&url).json(&body).send().await.unwrap();
                resp.bytes().await.unwrap()
            }
        });
    });

    group.bench_function("proxied", |b| {
        let url = format!("{gateway_url}/v1/responses");
        let body = non_stream_body.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let resp = client.post(&url).json(&body).send().await.unwrap();
                resp.bytes().await.unwrap()
            }
        });
    });

    group.finish();

    let mut group = c.benchmark_group("stream");

    group.bench_function("direct", |b| {
        let url = format!("{llm_url}/v1/responses");
        let body = stream_body.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let resp = client.post(&url).json(&body).send().await.unwrap();
                resp.bytes().await.unwrap()
            }
        });
    });

    group.bench_function("proxied", |b| {
        let url = format!("{gateway_url}/v1/responses");
        let body = stream_body.clone();
        b.to_async(&rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let resp = client.post(&url).json(&body).send().await.unwrap();
                resp.bytes().await.unwrap()
            }
        });
    });

    group.finish();
}

criterion_group!(proxy_benches, proxy_benchmarks);
