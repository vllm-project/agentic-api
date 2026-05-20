use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::stream;
use http::StatusCode;
use tokio::net::TcpListener;

use agentic_api::handler::{self, AppState};
use agentic_api::store::ogx::OgxStore;

pub async fn start_gateway(vllm_port: u16, ogx_port: Option<u16>, api_key: Option<&str>) -> (String, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");

    let ogx_base = match ogx_port {
        Some(p) => format!("http://127.0.0.1:{p}"),
        None => "http://127.0.0.1:1".to_owned(),
    };

    let client = reqwest::Client::new();
    let ogx_store = Arc::new(OgxStore::new(&ogx_base, client.clone()));

    let state = Arc::new(AppState {
        vllm_base_url: format!("http://127.0.0.1:{vllm_port}"),
        openai_api_key: api_key.map(String::from),
        max_iterations: 10,
        client,
        response_store: ogx_store.clone(),
        vector_search: ogx_store,
    });

    let app = Router::new()
        .route("/v1/responses", post(handler::handle_responses))
        .route("/health", get(handler::health))
        .with_state(state);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
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

use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn get_response_handler() -> Response {
        let body = serde_json::json!({
            "id": "resp_prev",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "previous answer"}]
            }]
        });
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string(&body).unwrap(),
        )
            .into_response()
    }

    async fn list_input_items_handler() -> Response {
        let body = serde_json::json!({
            "object": "list",
            "data": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "previous question"}]
            }]
        });
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string(&body).unwrap(),
        )
            .into_response()
    }

    let app = Router::new()
        .route("/v1/vector_stores/{store_id}/search", post(search_handler))
        .route("/v1/responses/{response_id}", get(get_response_handler))
        .route("/v1/responses/{response_id}/input_items", get(list_input_items_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (port, handle)
}
