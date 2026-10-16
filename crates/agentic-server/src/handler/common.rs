use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};

use axum::body::{Body, BodyDataStream};
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::StatusCode;
use serde::de::DeserializeOwned;
use tracing::warn;

use agentic_core::executor::{BoxStream, ExecutorError};
use agentic_core::proxy::{
    ProxyAuth, ProxyBody, ProxyRequest, ProxyResponse, ProxyStreamRequest, error_response_for_auth,
    proxy_request_with_path, proxy_stream_with_path,
};

use crate::app::AppState;

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

/// Forward an upstream error verbatim: its status, body, and processed metadata headers.
///
/// The headers were already filtered by `processed_response_headers` when the inference
/// call captured them, so hop-by-hop and stale representation headers are gone while
/// `retry-after`, request IDs, and rate-limit metadata remain. `content-type` is only
/// defaulted to JSON when the upstream did not label its body, so a plain-text error is
/// not relabelled as JSON.
pub fn upstream_error_response(status: StatusCode, body: String, mut headers: HeaderMap) -> Response {
    headers
        .entry(http::header::CONTENT_TYPE)
        .or_insert(http::HeaderValue::from_static("application/json"));
    convert_response(ProxyResponse {
        status,
        headers,
        body: ProxyBody::Full(Bytes::from(body)),
    })
}

/// # Panics
/// Panics if the response builder produces an invalid response (unreachable in practice).
pub fn executor_error_response(err: ExecutorError) -> Response {
    if let ExecutorError::LLMRequest { status, body, headers } = err {
        return upstream_error_response(status, body, headers);
    }
    let status = err.http_status();
    warn!("executor error ({status}): {err}");
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(err.into_response_body()))
        .expect("valid error response")
}

#[allow(clippy::result_large_err)]
pub(super) async fn read_bytes(body: Body, limit: NonZeroUsize) -> Result<Bytes, Response> {
    read_bytes_with_auth(body, ProxyAuth::OpenAiBearer, limit).await
}

#[allow(clippy::result_large_err)]
pub(super) async fn read_bytes_with_auth(body: Body, auth: ProxyAuth, limit: NonZeroUsize) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, limit.get())
        .await
        .map_err(|_| body_too_large(auth))
}

fn body_too_large(auth: ProxyAuth) -> Response {
    convert_response(error_response_for_auth(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        "request body too large",
        auth,
    ))
}

/// Forward one request body to `path` on the configured upstream, unchanged.
///
/// For the stateless endpoints the gateway does not own: no translation, no state, no
/// gateway tool loop. Streaming responses are relayed as they arrive.
pub(super) async fn passthrough(state: &AppState, req: Request, path: &'static str) -> Response {
    let (parts, body) = req.into_parts();
    let body = match read_bytes(body, state.max_request_body_size).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    convert_response(
        proxy_request_with_path(
            ProxyRequest {
                headers: parts.headers,
                body,
                query: parts.uri.query().map(str::to_owned),
            },
            path,
            ProxyAuth::OpenAiBearer,
            &state.proxy_state,
        )
        .await,
    )
}

/// Forward one request to `path` on the configured upstream, streaming its body as it arrives.
///
/// For uploads the gateway does not own, such as multipart audio: the body is never held in
/// memory whole, yet the same request-size limit applies. A declared length over the limit is
/// rejected before the upstream is called; an undeclared one is cut off once it passes it.
pub(super) async fn stream_passthrough(state: &AppState, req: Request, path: &'static str) -> Response {
    let limit = state.max_request_body_size.get();
    let (parts, body) = req.into_parts();
    let content_length = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if content_length.is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX)) {
        return body_too_large(ProxyAuth::OpenAiBearer);
    }

    let (upload, overflow) = LimitedUpload::new(body, limit);
    let response = proxy_stream_with_path(
        ProxyStreamRequest {
            headers: parts.headers,
            body: upload,
            content_length,
            query: parts.uri.query().map(str::to_owned),
        },
        path,
        ProxyAuth::OpenAiBearer,
        &state.proxy_state,
    )
    .await;
    // The upstream request fails once the limit trips; report that as the client's error.
    if overflow.occurred() {
        return body_too_large(ProxyAuth::OpenAiBearer);
    }
    convert_response(response)
}

/// A request body streamed upstream as it arrives, which fails once more than `limit` bytes have
/// passed through it.
///
/// The byte count and the overflow signal stay private: [`LimitedUpload::new`] hands the caller a
/// read-only [`UploadOverflow`] that can only observe this stream's limit.
struct LimitedUpload {
    body: BodyDataStream,
    limit: usize,
    received: usize,
    overflow: Arc<AtomicBool>,
}

impl LimitedUpload {
    /// Wrap `body`, returning the stream to forward and the handle that reports its overflow.
    fn new(body: Body, limit: usize) -> (Self, UploadOverflow) {
        let overflow = Arc::new(AtomicBool::new(false));
        let upload = Self {
            body: body.into_data_stream(),
            limit,
            received: 0,
            overflow: Arc::clone(&overflow),
        };
        (upload, UploadOverflow(overflow))
    }
}

impl Stream for LimitedUpload {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.overflow.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        let chunk = match ready!(this.body.poll_next_unpin(cx)) {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => return Poll::Ready(Some(Err(std::io::Error::other(error)))),
            None => return Poll::Ready(None),
        };
        this.received = this.received.saturating_add(chunk.len());
        if this.received > this.limit {
            // The chunk that crosses the limit is never forwarded.
            this.overflow.store(true, Ordering::Release);
            return Poll::Ready(Some(Err(std::io::Error::other("request body too large"))));
        }
        Poll::Ready(Some(Ok(chunk)))
    }
}

/// Read-only view of whether a [`LimitedUpload`] passed its limit.
struct UploadOverflow(Arc<AtomicBool>);

impl UploadOverflow {
    fn occurred(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[allow(clippy::result_large_err)]
pub(super) async fn read_json<T: DeserializeOwned>(body: Body, limit: NonZeroUsize) -> Result<T, Response> {
    let bytes = read_bytes(body, limit).await?;
    serde_json::from_slice::<T>(&bytes).map_err(|error| executor_error_response(ExecutorError::from(error)))
}

pub(super) fn extract_store(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|j| j.get("store").and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

#[allow(clippy::result_large_err)]
pub(super) fn extract_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Response> {
    serde_json::from_slice::<T>(bytes).map_err(|error| executor_error_response(ExecutorError::from(error)))
}

pub(super) fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("valid error response")
}

pub(super) fn extract_bearer(headers: &HeaderMap, config_key: Option<&str>) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| config_key.filter(|s| !s.is_empty()).map(str::to_string))
}

pub(super) fn sse_response(stream: BoxStream) -> Response {
    sse_response_with_headers(stream, HeaderMap::new())
}

pub(super) fn sse_response_with_headers(stream: BoxStream, mut headers: HeaderMap) -> Response {
    let byte_stream = stream.map(|line| Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)));
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", http::HeaderValue::from_static("no"));
    let mut builder = Response::builder().status(StatusCode::OK);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from_stream(byte_stream))
        .expect("valid SSE response")
}
