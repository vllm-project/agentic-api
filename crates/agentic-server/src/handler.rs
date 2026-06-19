use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use either::Either;
use futures::StreamExt;
use http::StatusCode;
use serde_json::json;
use tracing::warn;

use agentic_core::executor::{BoxStream, ExecutionContext, ExecutorError, create_conversation, execute};
use agentic_core::proxy::{ProxyBody, ProxyRequest, ProxyResponse, error_response, proxy_request};
use agentic_core::types::request_response::RequestPayload;

use crate::app::AppState;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

pub async fn health() -> impl IntoResponse {
    StatusCode::OK
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let base = state.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    let Ok(client) = client else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => StatusCode::OK,
        Ok(resp) => {
            warn!("LLM backend not ready: {}", resp.status());
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(e) => {
            warn!("LLM backend unreachable: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn read_bytes(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_BODY_SIZE).await.map_err(|_| {
        convert_response(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "request body too large",
        ))
    })
}

async fn read_and_parse(body: Body) -> Result<(Bytes, RequestPayload), Response> {
    let bytes = read_bytes(body).await?;
    let payload = serde_json::from_slice::<RequestPayload>(&bytes)
        .map_err(|e| executor_error_response(ExecutorError::from(e)))?;
    Ok((bytes, payload))
}

fn extract_store(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|j| j.get("store").and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

/// # Panics
/// Panics if the response builder produces an invalid response (unreachable in practice).
pub fn convert_response(resp: ProxyResponse) -> Response {
    let mut builder = Response::builder().status(resp.status);
    for (name, value) in &resp.headers {
        builder = builder.header(name, value);
    }
    match resp.body {
        ProxyBody::Full(bytes) => builder.body(Body::from(bytes)).expect("valid response"),
        ProxyBody::Stream(stream) => builder.body(Body::from_stream(stream)).expect("valid response"),
    }
}

async fn proxy_responses(state: &AppState, parts: Parts, body: Bytes) -> Response {
    let proxy_req = ProxyRequest {
        headers: parts.headers,
        body,
        query: parts.uri.query().map(str::to_string),
    };
    convert_response(proxy_request(proxy_req, &state.proxy_state).await)
}

fn resolve_exec_ctx(state: &AppState, parts: &Parts) -> Arc<ExecutionContext> {
    let request_auth = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if request_auth.is_some() && request_auth != state.exec_ctx.client_auth {
        let mut ctx = (*state.exec_ctx).clone();
        ctx.client_auth = request_auth;
        Arc::new(ctx)
    } else {
        Arc::clone(&state.exec_ctx)
    }
}

fn sse_response(stream: BoxStream) -> Response {
    let byte_stream = stream.map(|line| Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(byte_stream))
        .expect("valid SSE response")
}

async fn execute_responses(state: &AppState, parts: Parts, payload: RequestPayload) -> Response {
    match execute(payload, resolve_exec_ctx(state, &parts)).await {
        Ok(Either::Left(response_payload)) => axum::Json(response_payload).into_response(),
        Ok(Either::Right(stream)) => sse_response(stream),
        Err(e) => executor_error_response(e),
    }
}

/// # Panics
/// Panics if the response builder produces an invalid response (unreachable in practice).
pub fn executor_error_response(err: ExecutorError) -> Response {
    let status = err.http_status();
    if !matches!(err, ExecutorError::LLMRequest { .. }) {
        warn!("executor error ({status}): {err}");
    }
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(err.into_response_body()))
        .expect("valid error response")
}

pub async fn conversations(State(state): State<AppState>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    if !extract_store(&bytes) {
        return executor_error_response(ExecutorError::InvalidRequest("conversations require store=true".into()));
    }

    match create_conversation(&state.exec_ctx).await {
        Ok(data) => axum::Json(json!({
            "id": data.conversation_id,
            "created_at": data.created_at,
            "object": "conversation",
            "metadata": {}
        }))
        .into_response(),
        Err(e) => executor_error_response(e),
    }
}

pub async fn responses(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let (bytes, payload) = match read_and_parse(body).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    let should_persist = payload.store || payload.previous_response_id.is_some() || payload.conversation_id.is_some();

    if should_persist || payload.has_file_search_tool() {
        execute_responses(&state, parts, payload).await
    } else {
        proxy_responses(&state, parts, bytes).await
    }
}
