//! HTTP transport layer for LLM backend communication.
//!
//! Handles sending requests, reading streaming chunks, and mapping network
//! and HTTP errors to [`ExecutorError`].

use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures::{Stream, StreamExt};

use crate::executor::error::{ExecutorError, ExecutorResult};

/// SSE stream of raw lines sent to the client (`data: …\n\n` per event).
pub type BoxStream = std::pin::Pin<Box<dyn Stream<Item = String> + Send>>;

/// Wire-format marker signalling end-of-stream to the client.
pub(super) const DONE_MARKER: &str = "data: [DONE]\n\n";

/// Fetch the next raw bytes chunk from a streaming response.
///
/// Returns `Ok(Some(bytes))` on data, `Ok(None)` when the stream ends cleanly,
/// and `Err` on a network failure or chunk timeout.
pub(super) async fn next_chunk<S>(stream: &mut S, timeout: Duration) -> ExecutorResult<Option<bytes::Bytes>>
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
    item.transpose().map_err(ExecutorError::NetworkError)
}

fn drain_complete_utf8_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
        let line = buffer.drain(..=pos).collect::<Vec<_>>();
        let line_end = if pos > 0 && line.get(pos - 1) == Some(&b'\r') {
            pos - 1
        } else {
            pos
        };
        if let Ok(line) = std::str::from_utf8(&line[..line_end]) {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Build, send, and validate an HTTP POST to the LLM backend.
///
/// Shared by both the blocking path (caller reads `.text()`) and the streaming
/// path (caller reads `.bytes_stream()`). Maps connect/timeout failures and
/// non-2xx status codes to [`ExecutorError::LLMRequest`].
pub(super) async fn send_request(
    client: &reqwest::Client,
    url: &str,
    body: String,
    auth: Option<&str>,
) -> ExecutorResult<reqwest::Response> {
    let mut req = client.post(url).header("Content-Type", "application/json").body(body);
    if let Some(key) = auth {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await.map_err(|e| ExecutorError::LLMRequest {
        status: if e.is_timeout() {
            http::StatusCode::GATEWAY_TIMEOUT
        } else {
            http::StatusCode::BAD_GATEWAY
        },
        body: if e.is_timeout() {
            "upstream timeout".into()
        } else {
            "upstream unavailable".into()
        },
    })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        // Log and discard any error reading the error body — the status code
        // is the primary signal; an empty body is acceptable here.
        let body = resp
            .text()
            .await
            .inspect_err(|e| tracing::debug!("failed to read error response body: {e}"))
            .unwrap_or_default();
        return Err(ExecutorError::LLMRequest {
            status: http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR),
            body,
        });
    }

    Ok(resp)
}

/// Makes a non-streaming HTTP POST to the LLM backend and returns the full JSON body.
///
/// Used by `run_blocking` so it can pass the result to [`ResponseAccumulator::from_json`](crate::executor::accumulator::ResponseAccumulator::from_json).
pub(super) async fn fetch_response_json(
    upstream_json: String,
    url: &str,
    client: &reqwest::Client,
    auth: Option<&str>,
) -> ExecutorResult<String> {
    let resp = send_request(client, url, upstream_json, auth).await?;
    // Preserve the reqwest::Error as the typed source (NetworkError).
    resp.text().await.map_err(ExecutorError::NetworkError)
}

/// Step 2 — Call the LLM inference backend; yields raw SSE lines (`data: …`).
///
/// Always requests `stream=true` upstream. Stops on `[DONE]`.
///
/// # Errors
/// Each stream item is `Result<String, ExecutorError>`. The stream yields `Err` on:
/// - [`ExecutorError::LLMRequest`] — connect timeout (504), connection failure (502),
///   or non-2xx HTTP status from the backend
/// - [`ExecutorError::NetworkError`] — network failure while reading the response body
pub fn call_inference(
    upstream_json: String,
    url: String,
    client: Arc<reqwest::Client>,
    auth: Option<String>,
    chunk_timeout: Duration,
) -> impl Stream<Item = Result<String, ExecutorError>> + Send + 'static {
    stream! {
        let resp = match send_request(&client, &url, upstream_json, auth.as_deref()).await {
            Ok(r) => r,
            Err(e) => { yield Err(e); return; }
        };

        let mut bytes = resp.bytes_stream();
        let mut buf = Vec::with_capacity(8192);

        loop {
            let chunk = match next_chunk(&mut bytes, chunk_timeout).await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => { yield Err(e); return; }
            };

            buf.extend_from_slice(&chunk);

            for line in drain_complete_utf8_lines(&mut buf) {
                match line.as_str() {
                    "data: [DONE]" => return,
                    l if l.starts_with("data: ") => yield Ok(line),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_line_reader_preserves_split_multibyte_characters() {
        let snowman = "\u{2603}";
        let line = format!(r#"data: {{"delta":"snow {snowman}"}}"#);
        let bytes = format!("{line}\n").into_bytes();
        let split_at = bytes
            .windows(snowman.len())
            .position(|window| window == snowman.as_bytes())
            .expect("snowman bytes present")
            + 1;
        let mut buffer = bytes[..split_at].to_vec();

        assert!(drain_complete_utf8_lines(&mut buffer).is_empty());

        buffer.extend_from_slice(&bytes[split_at..]);
        let lines = drain_complete_utf8_lines(&mut buffer);

        assert!(buffer.is_empty());
        assert_eq!(lines, vec![line]);
        assert!(!lines[0].contains('\u{FFFD}'));
    }
}
