//! HTTP transport layer for LLM backend communication.
//!
//! Handles sending requests, reading streaming chunks, and mapping network
//! and HTTP errors to [`ExecutorError`].

use crate::types::request_response::UpstreamRequest;
use crate::utils::common::serialize_to_string;

use std::time::Duration;

use async_stream::stream;
use futures::{Stream, StreamExt};
use tracing::Instrument as _;

use crate::executor::error::{ExecutorError, ExecutorResult, ResourceLimit};
use crate::executor::telemetry::stages;
use crate::proxy::processed_response_headers;

/// Request-scoped upstream headers. The HTTP handler supplies the proxy's filtered
/// end-to-end headers; existing embedded and WebSocket callers can supply a bearer token.
#[derive(Clone)]
pub(crate) enum UpstreamHeaders {
    Bearer(String),
    Forwarded(http::HeaderMap),
}

fn llm_transport_error(is_timeout: bool) -> ExecutorError {
    ExecutorError::LLMTransport {
        status: if is_timeout {
            http::StatusCode::GATEWAY_TIMEOUT
        } else {
            http::StatusCode::BAD_GATEWAY
        },
        message: if is_timeout { "LLM timeout" } else { "LLM unavailable" },
    }
}

fn classify_reqwest_error(source: reqwest::Error) -> ExecutorError {
    if source.is_timeout() {
        llm_transport_error(true)
    } else {
        ExecutorError::NetworkError(source)
    }
}

/// Fetch the next raw bytes chunk from a streaming response.
///
/// Returns `Ok(Some(bytes))` on data, `Ok(None)` when the stream ends cleanly,
/// and `Err` on a network failure or chunk timeout.
pub(in crate::executor) async fn next_chunk<S>(
    stream: &mut S,
    timeout: Duration,
) -> ExecutorResult<Option<bytes::Bytes>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let item = if timeout.is_zero() {
        stream.next().await
    } else {
        tokio::time::timeout(timeout, stream.next()).await.map_err(|_| {
            ExecutorError::StreamError("chunk timeout: no data received within the configured window".into())
        })?
    };
    item.transpose().map_err(classify_reqwest_error)
}

fn drain_complete_utf8_lines_limited(
    buffer: &mut Vec<u8>,
    scanned: &mut usize,
    max_sse_line_bytes: usize,
) -> ExecutorResult<Vec<String>> {
    let mut lines = Vec::new();
    while let Some(rel_pos) = buffer[*scanned..].iter().position(|byte| *byte == b'\n') {
        let pos = *scanned + rel_pos;
        if pos > max_sse_line_bytes {
            return Err(ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::UpstreamSseLine,
                max_bytes: max_sse_line_bytes,
            });
        }
        let line = buffer.drain(..=pos).collect::<Vec<_>>();
        *scanned = 0;
        let line_end = if pos > 0 && line.get(pos - 1) == Some(&b'\r') {
            pos - 1
        } else {
            pos
        };
        if let Ok(line) = std::str::from_utf8(&line[..line_end]) {
            lines.push(line.to_string());
        }
    }
    *scanned = buffer.len();
    if buffer.len() > max_sse_line_bytes {
        return Err(ExecutorError::ResourceLimitExceeded {
            limit: ResourceLimit::UpstreamSseLine,
            max_bytes: max_sse_line_bytes,
        });
    }
    Ok(lines)
}

async fn response_text_limited(
    resp: reqwest::Response,
    chunk_timeout: Duration,
    max_bytes: usize,
) -> ExecutorResult<String> {
    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = next_chunk(&mut stream, chunk_timeout).await? {
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err(ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::UpstreamJsonBody,
                max_bytes,
            });
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body)
        .map_err(|_| ExecutorError::StreamError("upstream response body was not valid UTF-8".to_owned()))
}

/// Maximum size of an upstream error response body retained in [`ExecutorError::LLMRequest`].
pub(in crate::executor) const MAX_ERROR_RESPONSE_BYTES: usize = 1024 * 1024;

/// Build, send, and validate an HTTP POST to the LLM backend.
///
/// Connection failures map to 502 and connect/header timeouts to 504. Non-success
/// responses retain their status and filtered headers. `chunk_timeout` bounds the
/// entire send/header wait and each error-body read; zero disables these bounds.
/// There is no total deadline. Unreadable error bodies are discarded.
pub(in crate::executor) async fn send_request(
    client: &reqwest::Client,
    url: &str,
    body: String,
    upstream_headers: Option<&UpstreamHeaders>,
    chunk_timeout: Duration,
) -> ExecutorResult<reqwest::Response> {
    send_request_traced(client, url, body, upstream_headers, chunk_timeout)
        .instrument(stages::http_client(url))
        .await
}

async fn send_request_traced(
    client: &reqwest::Client,
    url: &str,
    body: String,
    upstream_headers: Option<&UpstreamHeaders>,
    chunk_timeout: Duration,
) -> ExecutorResult<reqwest::Response> {
    let mut headers = match upstream_headers {
        Some(UpstreamHeaders::Forwarded(forwarded)) => forwarded.clone(),
        Some(UpstreamHeaders::Bearer(_)) | None => http::HeaderMap::new(),
    };
    stages::inject_context(&mut headers);
    headers
        .entry(reqwest::header::CONTENT_TYPE)
        .or_insert(reqwest::header::HeaderValue::from_static("application/json"));
    let mut req = client.post(url).headers(headers).body(body);
    if let Some(UpstreamHeaders::Bearer(key)) = upstream_headers {
        req = req.bearer_auth(key);
    }

    let sent = if chunk_timeout.is_zero() {
        Ok(req.send().await)
    } else {
        tokio::time::timeout(chunk_timeout, req.send()).await
    };
    let resp = match sent {
        Ok(Ok(resp)) => resp,
        Ok(Err(error)) => {
            let is_timeout = error.is_timeout();
            tracing::Span::current().record("error.type", if is_timeout { "timeout" } else { "transport" });
            tracing::Span::current().record("otel.status_code", "ERROR");
            return Err(llm_transport_error(is_timeout));
        }
        Err(_) => {
            tracing::Span::current().record("error.type", "timeout");
            tracing::Span::current().record("otel.status_code", "ERROR");
            return Err(llm_transport_error(true));
        }
    };

    tracing::Span::current().record("http.response.status_code", i64::from(resp.status().as_u16()));
    if !resp.status().is_success() {
        tracing::Span::current().record("error.type", "upstream_status");
        tracing::Span::current().record("otel.status_code", "ERROR");
        let status = resp.status().as_u16();
        let headers = processed_response_headers(resp.headers());
        // Log and discard any error reading the error body — the status code
        // is the primary signal; an empty body is acceptable here.
        let body = response_text_limited(resp, chunk_timeout, MAX_ERROR_RESPONSE_BYTES)
            .await
            .inspect_err(|error| tracing::debug!(%error, "failed to read bounded error response body"))
            .unwrap_or_default();
        return Err(ExecutorError::LLMRequest {
            status: http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR),
            body,
            headers,
        });
    }

    Ok(resp)
}

/// Fetch a size-bounded JSON body and filtered response headers.
/// Generation may finish before headers arrive, so JSON has no header or body idle timeout.
pub(in crate::executor) async fn fetch_response_json(
    upstream_json: String,
    url: &str,
    client: &reqwest::Client,
    headers: Option<&UpstreamHeaders>,
    max_upstream_json_bytes: usize,
) -> ExecutorResult<(String, http::HeaderMap)> {
    let resp = send_request(client, url, upstream_json, headers, Duration::ZERO).await?;
    let response_headers = processed_response_headers(resp.headers());
    let body = response_text_limited(resp, Duration::ZERO, max_upstream_json_bytes).await?;
    Ok((body, response_headers))
}

/// Convert a successful upstream response body into SSE data lines with a `data: ` prefix.
/// The single space after the field colon is optional on the wire.
pub(in crate::executor) fn response_lines(
    resp: reqwest::Response,
    chunk_timeout: Duration,
    max_sse_line_bytes: usize,
) -> impl Stream<Item = Result<String, ExecutorError>> + Send + 'static {
    stream! {
        let mut bytes = resp.bytes_stream();
        let mut buf = Vec::with_capacity(8192);
        let mut scanned = 0;

        loop {
            let chunk = match next_chunk(&mut bytes, chunk_timeout).await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => { yield Err(e); return; }
            };

            buf.extend_from_slice(&chunk);

            let lines = match drain_complete_utf8_lines_limited(&mut buf, &mut scanned, max_sse_line_bytes) {
                Ok(lines) => lines,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            for mut line in lines {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                if data.strip_prefix(' ').unwrap_or(data) == "[DONE]" {
                    return;
                }
                // Keep one transport spelling for Responses and Messages consumers,
                // without trimming whitespace that belongs to the field value.
                if !data.starts_with(' ') {
                    line.insert("data:".len(), ' ');
                }
                yield Ok(line);
            }
        }
    }
}

#[cfg(test)]
mod tests;

pub(in crate::executor) fn encode_request(request: &UpstreamRequest<'_>) -> ExecutorResult<String> {
    Ok(serialize_to_string(request)?)
}
