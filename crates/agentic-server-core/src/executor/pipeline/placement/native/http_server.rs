// Test-only byte replay backend. The existing inference module is the only framer.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::Response;
use axum::routing::post;
use bytes::Bytes;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::ArrivalSchedule;
use super::source::LineStream;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::inference::call_inference_limited;

const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct HttpServer {
    url: String,
    client: Arc<reqwest::Client>,
    cancel: CancellationToken,
    server: Option<JoinHandle<std::io::Result<()>>>,
    requests: Arc<AtomicUsize>,
}

impl HttpServer {
    pub async fn start(chunks: Vec<Bytes>, keep_open: bool, spacing: Duration) -> ExecutorResult<Self> {
        if chunks.len() > 32_768
            || chunks
                .iter()
                .try_fold(0usize, |total, chunk| total.checked_add(chunk.len()))
                .is_none_or(|size| size > MAX_WIRE_BYTES)
        {
            return Err(ExecutorError::StreamError(
                "fixture HTTP body exceeds its bound".to_owned(),
            ));
        }
        let client = Arc::new(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .map_err(ExecutorError::NetworkError)?,
        );
        let cancel = CancellationToken::new();
        let body_cancel = cancel.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&requests);
        let chunks = Arc::new(chunks);
        let app = axum::Router::new().route(
            "/v1/responses",
            post(move || {
                accepted.fetch_add(1, Ordering::SeqCst);
                let chunks = Arc::clone(&chunks);
                let body_cancel = body_cancel.clone();
                async move {
                    let body = async_stream::stream! {
                        let arrival = ArrivalSchedule::at(Instant::now(), spacing);
                        for (index, chunk) in chunks.iter().enumerate() {
                            match arrival.wait(index, &body_cancel).await {
                                Ok(true) => yield Ok::<_, std::io::Error>(chunk.clone()),
                                Ok(false) => return,
                                Err(error) => { yield Err(std::io::Error::other(error)); return; },
                            }
                        }
                        if keep_open { body_cancel.cancelled().await; }
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(body))
                        .expect("static fixture headers")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| ExecutorError::StreamError(format!("fixture HTTP bind failed: {error}")))?;
        let address = listener
            .local_addr()
            .map_err(|error| ExecutorError::StreamError(format!("fixture HTTP address failed: {error}")))?;
        let server_cancel = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_cancel.cancelled_owned())
                .await
        });
        Ok(Self {
            url: format!("http://{address}/v1/responses"),
            client,
            cancel,
            server: Some(server),
            requests,
        })
    }

    pub fn stream(&self, chunk_timeout: Duration) -> LineStream {
        // Synthetic POST control, not a recorded model request. The request is
        // lazy: it is actually sent only when the source is polled in run_source.
        Box::pin(call_inference_limited(
            r#"{"model":"placement-model","stream":true,"store":false}"#.to_owned(),
            self.url.clone(),
            Arc::clone(&self.client),
            None,
            chunk_timeout,
            64 * 1024,
        ))
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub async fn stop(mut self) -> ExecutorResult<()> {
        self.cancel.cancel();
        let Some(mut server) = self.server.take() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(_) => Err(ExecutorError::StreamError(
                "fixture HTTP server failed to join cleanly".to_owned(),
            )),
            Err(_) => {
                server.abort();
                let _ = server.await;
                Err(ExecutorError::StreamError(
                    "fixture HTTP server shutdown timed out".to_owned(),
                ))
            }
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Only a panic/dropped harness future uses this fallback. Successful
        // benchmark/test paths must explicitly stop and observe the server join.
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}
