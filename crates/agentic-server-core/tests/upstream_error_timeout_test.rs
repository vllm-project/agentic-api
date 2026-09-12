//! Streaming error bodies share the configured idle timeout and retain HTTP errors.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::executor::ExecutorError;
use agentic_core::executor::inference::call_inference;
use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::StatusCode;
use tokio::net::TcpListener;

struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn error_server<F, S>(status: StatusCode, body: F) -> (String, Server)
where
    F: Fn() -> S + Clone + Send + Sync + 'static,
    S: Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let stream = body();
            async move {
                Response::builder()
                    .status(status)
                    .header("content-type", "text/plain; charset=utf-8")
                    .header("retry-after", "7")
                    .header("x-request-id", "upstream-error")
                    .header("connection", "x-private-hop")
                    .header("x-private-hop", "discard")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }));
    (url, server)
}

async fn read_error(url: String, timeout: Duration) -> Vec<Result<String, ExecutorError>> {
    call_inference("{}".to_owned(), url, Arc::new(reqwest::Client::new()), None, timeout)
        .collect()
        .await
}

fn assert_error(mut result: Vec<Result<String, ExecutorError>>, expected_status: StatusCode, expected_body: &str) {
    assert_eq!(result.len(), 1, "a failed request emits exactly one error");
    let ExecutorError::LLMRequest { status, body, headers } = result.pop().unwrap().unwrap_err() else {
        panic!("expected the original HTTP error");
    };
    assert_eq!(status, expected_status);
    assert_eq!(body, expected_body);
    assert_eq!(headers["retry-after"], "7");
    assert_eq!(headers["x-request-id"], "upstream-error");
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert!(!headers.contains_key("x-private-hop"));
    assert!(!headers.contains_key("connection"));
}

async fn stalled_error(partial: bool, status: StatusCode) {
    let (url, _server) = error_server(status, move || {
        async_stream::stream! {
            if partial { yield Ok(Bytes::from_static(b"partial error")); }
            std::future::pending::<()>().await;
        }
    })
    .await;
    let result = tokio::time::timeout(Duration::from_secs(2), read_error(url, Duration::from_millis(50)))
        .await
        .expect("the configured idle timeout must bound an upstream error body");
    assert_error(result, status, "");
}

#[tokio::test]
async fn streaming_error_timeout_bounds_an_empty_429_body() {
    stalled_error(false, StatusCode::TOO_MANY_REQUESTS).await;
}

#[tokio::test]
async fn streaming_error_timeout_bounds_a_partial_503_body() {
    stalled_error(true, StatusCode::SERVICE_UNAVAILABLE).await;
}

#[tokio::test]
async fn streaming_error_timeout_resets_for_each_chunk() {
    let (url, _server) = error_server(StatusCode::TOO_MANY_REQUESTS, || {
        async_stream::stream! {
            for chunk in ["rate ", "limited ", "雪"] {
                tokio::time::sleep(Duration::from_millis(80)).await;
                yield Ok(Bytes::from(chunk));
            }
        }
    })
    .await;
    let result = tokio::time::timeout(Duration::from_secs(3), read_error(url, Duration::from_millis(200)))
        .await
        .unwrap();
    assert_error(result, StatusCode::TOO_MANY_REQUESTS, "rate limited 雪");
}

#[tokio::test]
async fn streaming_error_timeout_zero_still_allows_delayed_bodies() {
    let (url, _server) = error_server(StatusCode::BAD_GATEWAY, || {
        async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(120)).await;
            yield Ok(Bytes::from_static(b"upstream unavailable"));
        }
    })
    .await;
    let result = tokio::time::timeout(Duration::from_secs(2), read_error(url, Duration::ZERO))
        .await
        .unwrap();
    assert_error(result, StatusCode::BAD_GATEWAY, "upstream unavailable");
}

#[tokio::test]
async fn streaming_error_timeout_retains_empty_completed_bodies() {
    let (url, _server) = error_server(StatusCode::BAD_GATEWAY, futures::stream::empty).await;
    let result = read_error(url, Duration::from_millis(50)).await;
    assert_error(result, StatusCode::BAD_GATEWAY, "");
}

#[tokio::test]
async fn streaming_error_timeout_preserves_byte_limit_and_utf8_policy() {
    for (bytes, expected) in [
        (vec![b'x'; 1024 * 1024], "x".repeat(1024 * 1024)),
        (vec![b'x'; 1024 * 1024 + 1], String::new()),
        (vec![0xff], String::new()),
        (b"{malformed json".to_vec(), "{malformed json".to_owned()),
    ] {
        let (url, _server) = error_server(StatusCode::BAD_GATEWAY, move || {
            futures::stream::iter(
                bytes
                    .chunks(4096)
                    .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>(),
            )
        })
        .await;
        let result = tokio::time::timeout(Duration::from_secs(3), read_error(url, Duration::from_millis(200)))
            .await
            .unwrap();
        assert_error(result, StatusCode::BAD_GATEWAY, &expected);
    }
}

#[tokio::test]
async fn streaming_error_timeout_cancellation_releases_the_upstream_body() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let route_dropped = Arc::clone(&dropped);
    let started = Arc::new(tokio::sync::Notify::new());
    let route_started = Arc::clone(&started);
    let (url, _server) = error_server(StatusCode::TOO_MANY_REQUESTS, move || {
        let guard = Dropped(Arc::clone(&route_dropped));
        let started = Arc::clone(&route_started);
        async_stream::stream! {
            let _guard = guard;
            started.notify_one();
            yield Ok(Bytes::from_static(b"partial"));
            std::future::pending::<()>().await;
        }
    })
    .await;
    let mut read = Box::pin(read_error(url, Duration::ZERO));
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            () = started.notified() => {},
            _ = &mut read => panic!("the stalled request must remain pending"),
        }
    })
    .await
    .expect("upstream body starts before cancellation");
    drop(read);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelling the read must release the upstream body without waiting for a timer");
}
