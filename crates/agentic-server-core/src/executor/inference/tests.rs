use std::convert::Infallible;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures::stream;

use super::*;
use crate::config::{DEFAULT_MAX_UPSTREAM_JSON_BYTES, DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES};
const MAX_SSE_LINE_BYTES: usize = DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES;

fn drain_complete_utf8_lines(buffer: &mut Vec<u8>) -> ExecutorResult<Vec<String>> {
    let mut scanned = 0;
    drain_complete_utf8_lines_limited(buffer, &mut scanned, MAX_SSE_LINE_BYTES)
}

async fn read_sse_body(body: String, keep_open: bool) -> Vec<ExecutorResult<String>> {
    let app = axum::Router::new().route(
        "/v1/responses",
        post(move || {
            let body = body.clone();
            async move {
                let chunks = async_stream::stream! {
                    yield Ok::<_, Infallible>(Bytes::from(body));
                    if keep_open {
                        std::future::pending::<()>().await;
                    }
                };
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let resp = send_request(
        &reqwest::Client::new(),
        &url,
        "{}".to_owned(),
        None,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let result = response_lines(resp, Duration::from_secs(1), MAX_SSE_LINE_BYTES)
        .collect()
        .await;
    server.abort();
    let _ = server.await;
    result
}

#[tokio::test]
async fn sse_data_spacing_preserves_field_values() {
    let lines = read_sse_body(
        concat!(
            ":comment\r\nevent: ignored\r\nid: 1\r\nretry: 1000\r\n",
            "Data: ignored\r\ndatabase: ignored\r\n data: ignored\r\n",
            "data:{\"delta\":\"雪 ☃: data: text\"}\r\n\r\n",
            "data: {\"spaced\":true}\n\n",
            "data:  {\"leading_space\":true}\n\n",
            "data:\t{\"tab\":true}\n\n",
            "data:\n\ndata: \n\ndata:{malformed}\n\n",
            "data:  [DONE]\n\ndata:[DONE]extra\n\n",
        )
        .to_owned(),
        false,
    )
    .await
    .into_iter()
    .collect::<ExecutorResult<Vec<_>>>()
    .unwrap();
    assert_eq!(
        lines,
        [
            "data: {\"delta\":\"雪 ☃: data: text\"}",
            "data: {\"spaced\":true}",
            "data:  {\"leading_space\":true}",
            "data: \t{\"tab\":true}",
            "data: ",
            "data: ",
            "data: {malformed}",
            "data:  [DONE]",
            "data: [DONE]extra",
        ]
    );
}

#[tokio::test]
async fn sse_data_spacing_stops_at_both_done_markers() {
    for separator in ["", " "] {
        let body = format!("data: {{}}\n\ndata:{separator}[DONE]\r\n\r\ndata: ignored\n\n");
        let lines = read_sse_body(body, true)
            .await
            .into_iter()
            .collect::<ExecutorResult<Vec<_>>>()
            .expect("[DONE] must stop without waiting for upstream EOF or a chunk timeout");
        assert_eq!(lines, ["data: {}"]);
    }
}

#[tokio::test]
async fn sse_data_spacing_preserves_line_limit() {
    for separator in ["", " "] {
        let prefix = format!("data:{separator}");
        let line = format!("{prefix}{}", "x".repeat(MAX_SSE_LINE_BYTES - prefix.len()));
        let accepted = read_sse_body(format!("{line}\n\n"), false).await;
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            accepted[0].as_ref().unwrap().len(),
            MAX_SSE_LINE_BYTES + usize::from(separator.is_empty())
        );

        let rejected = read_sse_body(format!("{line}x\n\n"), false).await;
        assert_eq!(rejected.len(), 1);
        assert!(
            rejected[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("upstream SSE line exceeded")
        );
    }
}

#[tokio::test]
async fn sse_data_spacing_preserves_chunk_timeout_without_done() {
    let lines = read_sse_body("data:{}\n\n".to_owned(), true).await;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].as_ref().unwrap(), "data: {}");
    assert!(lines[1].as_ref().unwrap_err().to_string().contains("chunk timeout"));
}

async fn oversized_body_server(status: StatusCode) -> (String, tokio::task::JoinHandle<()>) {
    let limit = if status.is_success() {
        DEFAULT_MAX_UPSTREAM_JSON_BYTES
    } else {
        MAX_ERROR_RESPONSE_BYTES
    };
    let app = axum::Router::new().route(
        "/v1/responses",
        post(move || async move {
            let half = limit / 2;
            let chunks = stream::iter([
                Ok::<_, Infallible>(Bytes::from(vec![b'x'; half + 1])),
                Ok(Bytes::from(vec![b'x'; limit - half])),
            ]);
            Response::builder()
                .status(status)
                .body(Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind response limit server");
    let address = listener.local_addr().expect("response limit server address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{address}/v1/responses"), server)
}

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

    assert!(
        drain_complete_utf8_lines(&mut buffer)
            .expect("partial UTF-8 line")
            .is_empty()
    );

    buffer.extend_from_slice(&bytes[split_at..]);
    let lines = drain_complete_utf8_lines(&mut buffer).expect("complete UTF-8 line");

    assert!(buffer.is_empty());
    assert_eq!(lines, vec![line]);
    assert!(!lines[0].contains('\u{FFFD}'));
}

#[test]
fn utf8_line_reader_rejects_an_oversized_unterminated_line() {
    let mut buffer = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    let error = drain_complete_utf8_lines(&mut buffer).expect_err("oversized SSE line must fail");
    assert!(error.to_string().contains("upstream SSE line exceeded"));
}

#[tokio::test]
async fn blocking_response_reader_rejects_a_cumulative_oversized_body() {
    let (url, server) = oversized_body_server(StatusCode::OK).await;
    let error = fetch_response_json(
        "{}".to_owned(),
        &url,
        &reqwest::Client::new(),
        None,
        DEFAULT_MAX_UPSTREAM_JSON_BYTES,
    )
    .await
    .expect_err("oversized successful response must fail");

    assert!(error.to_string().contains("upstream response exceeded"));
    server.abort();
}

#[tokio::test]
async fn non_success_response_discards_a_cumulative_oversized_body() {
    let (url, server) = oversized_body_server(StatusCode::BAD_GATEWAY).await;
    let error = send_request(&reqwest::Client::new(), &url, "{}".to_owned(), None, Duration::ZERO)
        .await
        .expect_err("non-success response must fail");

    let ExecutorError::LLMRequest { status, body, .. } = error else {
        panic!("expected upstream request error");
    };
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.is_empty());
    server.abort();
}

#[tokio::test]
async fn response_header_wait_has_a_deadline() {
    let app = axum::Router::new().route(
        "/v1/responses",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            "{}"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let error = send_request(
        &reqwest::Client::new(),
        &url,
        "{}".to_owned(),
        None,
        Duration::from_millis(20),
    )
    .await
    .expect_err("withheld response headers must time out");

    assert!(matches!(
        error,
        ExecutorError::LLMTransport {
            status: StatusCode::GATEWAY_TIMEOUT,
            ..
        }
    ));
    server.abort();
}

mod error_bodies;

#[tokio::test]
async fn zero_timeout_json_allows_delayed_headers_and_body() {
    let app = axum::Router::new().route(
        "/v1/responses",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let chunks = async_stream::stream! {
                tokio::time::sleep(Duration::from_millis(100)).await;
                yield Ok::<_, Infallible>(Bytes::from_static(b"{\"ok\":true}"));
            };
            Response::builder()
                .header("x-request-id", "delayed-json")
                .body(Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (body, headers) = tokio::time::timeout(
        Duration::from_secs(3),
        fetch_response_json("{}".to_owned(), &url, &reqwest::Client::new(), None, 32),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(body, r#"{"ok":true}"#);
    assert_eq!(headers["x-request-id"], "delayed-json");
    server.abort();
}

#[tokio::test]
async fn streaming_progress_has_no_total_or_semantic_token_deadline() {
    // Both token-like data and comments/partial lines keep transport alive across
    // several progress windows. Completion is bounded by the test, not production.
    for chunk in ["data: {}\n\n", ":keepalive\n\n", "x"] {
        let app = axum::Router::new().route(
            "/v1/responses",
            post(move || async move {
                let chunks = async_stream::stream! {
                    for _ in 0..12 {
                        yield Ok::<_, Infallible>(Bytes::from_static(chunk.as_bytes()));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    yield Ok(Bytes::from_static(b"\ndata: [DONE]\n\n"));
                };
                Response::builder().body(Body::from_stream(chunks)).unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let timeout = Duration::from_millis(200);
        let resp = send_request(&reqwest::Client::new(), &url, "{}".to_owned(), None, timeout)
            .await
            .unwrap();
        let lines = tokio::time::timeout(
            Duration::from_secs(5),
            response_lines(resp, timeout, 128).collect::<Vec<_>>(),
        )
        .await
        .unwrap()
        .into_iter()
        .collect::<ExecutorResult<Vec<_>>>()
        .unwrap();
        assert_eq!(lines.len(), if chunk.starts_with("data:") { 12 } else { 0 });
        server.abort();
    }
}

#[tokio::test]
async fn connection_failure_maps_to_bad_gateway() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = send_request(
        &reqwest::Client::new(),
        &url,
        "{}".to_owned(),
        None,
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ExecutorError::LLMTransport {
            status: StatusCode::BAD_GATEWAY,
            ..
        }
    ));
}
