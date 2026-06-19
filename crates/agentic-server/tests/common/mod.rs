use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::stream;
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use agentic_core::config::Config;
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler};
use agentic_core::proxy::ProxyState;
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_core::uuid7_str;
use agentic_core::vector_search::ogx::OgxStore;
use agentic_server::app::{AppState, ServerConfig, build_router};

pub fn test_config(llm_url: &str) -> Config {
    Config {
        llm_api_base: llm_url.to_owned(),
        openai_api_key: Some("test-key".to_owned()),
        llm_ready_timeout_s: 5.0,
        llm_ready_interval_s: 0.1,
        db_url: None,
        ogx_base_url: "http://127.0.0.1:1".to_owned(),
        max_iterations: 10,
    }
}

pub fn test_state(config: &Config) -> AppState {
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
        config.openai_api_key.clone(),
    ));
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    AppState {
        proxy_state,
        exec_ctx,
        llm_api_base: config.llm_api_base.clone(),
    }
}

/// Spawn a minimal mock LLM that responds to `GET /health` with 200.
pub async fn spawn_mock_llm() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/health", get(|| async { StatusCode::OK.into_response() }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), handle)
}

/// Spawn the gateway router bound to a random port.
pub async fn spawn_gateway(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let router = build_router(state, &ServerConfig::from_env());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), handle)
}

pub async fn start_gateway(vllm_port: u16, ogx_port: Option<u16>, api_key: Option<&str>) -> (String, u16) {
    let ogx_base = match ogx_port {
        Some(p) => format!("http://127.0.0.1:{p}"),
        None => "http://127.0.0.1:1".to_owned(),
    };
    start_gateway_with_ogx_base(vllm_port, &ogx_base, api_key).await
}

pub async fn start_gateway_with_ogx_base(vllm_port: u16, ogx_base: &str, api_key: Option<&str>) -> (String, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");

    let llm_url = format!("http://127.0.0.1:{vllm_port}");

    let mut config = test_config(&llm_url);
    config.openai_api_key = api_key.map(String::from);
    config.ogx_base_url = ogx_base.to_owned();
    config.db_url = Some(format!("sqlite:///tmp/{}.db", uuid7_str("agentic-api-test-")));

    let proxy_state = ProxyState::new(config.clone()).unwrap();
    let pool = create_pool_with_schema(config.db_url.as_deref()).await.unwrap();
    let ogx_store = Arc::new(OgxStore::new(ogx_base, reqwest::Client::new()));
    let exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(pool.clone())),
        ResponseHandler::new(ResponseStore::new(pool)),
        Arc::new(reqwest::Client::new()),
        config.llm_api_base.clone(),
        config.openai_api_key.clone(),
    )
    .with_vector_search(ogx_store, config.max_iterations);

    let state = AppState {
        proxy_state,
        exec_ctx: Arc::new(exec_ctx),
        llm_api_base: config.llm_api_base,
    };

    let server_config = ServerConfig::from_env();
    let router = build_router(state, &server_config);

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (addr, port)
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
            [("content-type", "application/json")],
            serde_json::to_string(&resp_body).unwrap(),
        )
            .into_response();
    }

    if body.get("force_error").and_then(serde_json::Value::as_u64) == Some(429) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
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
            [("content-type", "text/event-stream; charset=utf-8")],
            body,
        )
            .into_response();
    }

    let out = r#"{"id":"resp_test","object":"response","status":"completed","output":[]}"#;
    (StatusCode::OK, [("content-type", "application/json")], out).into_response()
}

pub async fn spawn_vllm() -> (u16, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(responses_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}

pub async fn spawn_mid_stream_failure_vllm() -> (u16, tokio::task::JoinHandle<()>) {
    async fn handler(_req: Request) -> Response {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(Bytes::from(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                )))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            drop(tx);
        });
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = Body::from_stream(stream);
        (
            StatusCode::OK,
            [("content-type", "text/event-stream; charset=utf-8")],
            body,
        )
            .into_response()
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/v1/responses", post(handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}

pub async fn spawn_vllm_with_tool_calls(responses: Vec<serde_json::Value>) -> (u16, tokio::task::JoinHandle<()>) {
    let responses = Arc::new(responses);
    let counter = Arc::new(AtomicUsize::new(0));

    let app = Router::new().route("/health", get(health_handler)).route(
        "/v1/responses",
        post({
            let responses = Arc::clone(&responses);
            let counter = Arc::clone(&counter);
            move |_req: Request| {
                let responses = Arc::clone(&responses);
                let counter = Arc::clone(&counter);
                async move {
                    let idx = counter.fetch_add(1, Ordering::SeqCst);
                    let resp = responses.get(idx).unwrap_or(responses.last().unwrap());
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        serde_json::to_string(resp).unwrap(),
                    )
                        .into_response()
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}

pub async fn spawn_vllm_recording(
    responses: Vec<serde_json::Value>,
) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>, tokio::task::JoinHandle<()>) {
    let responses = Arc::new(responses);
    let counter = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));

    let app = Router::new().route("/health", get(health_handler)).route(
        "/v1/responses",
        post({
            let responses = Arc::clone(&responses);
            let counter = Arc::clone(&counter);
            let requests_for_handler = Arc::clone(&requests);
            move |req: Request| {
                let responses = Arc::clone(&responses);
                let counter = Arc::clone(&counter);
                let requests_for_handler = Arc::clone(&requests_for_handler);
                async move {
                    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
                    requests_for_handler.lock().await.push(body);

                    let idx = counter.fetch_add(1, Ordering::SeqCst);
                    let resp = responses.get(idx).unwrap_or(responses.last().unwrap());
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        serde_json::to_string(resp).unwrap(),
                    )
                        .into_response()
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, requests, handle)
}

pub async fn spawn_ogx() -> (u16, tokio::task::JoinHandle<()>) {
    async fn search_handler(_req: Request) -> Response {
        let body = serde_json::json!({
            "object": "vector_store.search_results.page",
            "search_query": ["test query"],
            "data": [{
                "file_id": "file_abc",
                "filename": "doc.txt",
                "score": 0.95,
                "attributes": {},
                "content": [{"type": "text", "text": "relevant content from doc"}]
            }],
            "has_more": false
        });
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string(&body).unwrap(),
        )
            .into_response()
    }

    let app = Router::new().route("/v1/vector_stores/{store_id}/search", post(search_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}

pub async fn spawn_ogx_recording() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));

    let app = Router::new().route(
        "/v1/vector_stores/{store_id}/search",
        post({
            let requests_for_handler = Arc::clone(&requests);
            move |req: Request| {
                let requests_for_handler = Arc::clone(&requests_for_handler);
                async move {
                    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
                    requests_for_handler.lock().await.push(body);

                    let response = serde_json::json!({
                        "object": "vector_store.search_results.page",
                        "search_query": ["test query"],
                        "data": [{
                            "file_id": "file_abc",
                            "filename": "doc.txt",
                            "score": 0.95,
                            "attributes": {},
                            "content": [{"type": "text", "text": "relevant content from doc"}]
                        }],
                        "has_more": false
                    });
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        serde_json::to_string(&response).unwrap(),
                    )
                        .into_response()
                }
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, requests, handle)
}
