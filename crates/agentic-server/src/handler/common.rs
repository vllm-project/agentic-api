use std::sync::Arc;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use http::StatusCode;
use tracing::warn;

use agentic_core::executor::{BoxStream, ExecutionContext, ExecutorError};
use agentic_core::proxy::{ProxyBody, ProxyResponse, error_response};
use agentic_core::types::request_response::RequestPayload;

use crate::app::AppState;

pub(super) const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

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

pub(super) async fn read_bytes(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_BODY_SIZE).await.map_err(|_| {
        convert_response(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "request body too large",
        ))
    })
}

pub(super) async fn read_and_parse(body: Body) -> Result<(Bytes, RequestPayload), Response> {
    let bytes = read_bytes(body).await?;
    let payload = serde_json::from_slice::<RequestPayload>(&bytes)
        .map_err(|e| executor_error_response(ExecutorError::from(e)))?;
    Ok((bytes, payload))
}

pub(super) fn extract_store(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|j| j.get("store").and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

pub(super) fn resolve_exec_ctx_from_headers(state: &AppState, headers: &HeaderMap) -> Arc<ExecutionContext> {
    let request_auth = headers
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

pub(super) fn resolve_exec_ctx(state: &AppState, parts: &Parts) -> Arc<ExecutionContext> {
    resolve_exec_ctx_from_headers(state, &parts.headers)
}

pub(super) fn sse_response(stream: BoxStream) -> Response {
    let byte_stream = stream.map(|line| Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(byte_stream))
        .expect("valid SSE response")
}
