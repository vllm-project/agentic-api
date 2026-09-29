//! Execution span for raw forwarding, including body consumption and cancellation.

use std::pin::Pin;
use std::task::{Context, Poll};

use agentic_core::executor::telemetry::{Api, ExecutionSpan, FailureCategory, Route};
use agentic_core::proxy::{ProxyAuth, ProxyBody, ProxyRequest, ProxyResponse, ProxyState, proxy_request_with_path};
use bytes::Bytes;
use futures::Stream;
use tracing::Instrument as _;

pub(crate) async fn trace_proxy_request(
    api: Api,
    request: ProxyRequest,
    path: &str,
    state: &ProxyState,
) -> ProxyResponse {
    let mut execution = ExecutionSpan::start(api, Route::Proxy, request.is_streaming());
    let auth = match api {
        Api::Responses => ProxyAuth::OpenAiBearer,
        Api::Messages => ProxyAuth::Anthropic,
    };
    let response = proxy_request_with_path(request, path, auth, state)
        .instrument(execution.span().clone())
        .await;
    if !response.status.is_success() {
        execution.failed_with(FailureCategory::UpstreamStatus);
    }
    let body = match response.body {
        ProxyBody::Full(bytes) => {
            execution.completed();
            execution.delivered();
            ProxyBody::Full(bytes)
        }
        ProxyBody::Stream(inner) => {
            let remaining = response
                .headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            let mut stream = TracedStream {
                inner,
                execution: Some(execution),
                remaining,
            };
            if remaining == Some(0) {
                stream.complete();
            }
            ProxyBody::Stream(Box::pin(stream))
        }
    };
    ProxyResponse { body, ..response }
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

struct TracedStream {
    inner: ByteStream,
    execution: Option<ExecutionSpan>,
    remaining: Option<u64>,
}

impl TracedStream {
    fn complete(&mut self) {
        if let Some(mut execution) = self.execution.take() {
            execution.completed();
            execution.delivered();
        }
    }
}

impl Stream for TracedStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(execution) = &self.execution else {
            return Poll::Ready(None);
        };
        let span = execution.span().clone();
        let _entered = span.enter();
        let result = self.inner.as_mut().poll_next(cx);
        match &result {
            Poll::Ready(None) => self.complete(),
            Poll::Ready(Some(Err(_))) => {
                if let Some(mut execution) = self.execution.take() {
                    execution.failed_with(FailureCategory::Network);
                    execution.disconnected();
                }
            }
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(remaining) = &mut self.remaining {
                    *remaining = remaining.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    // Hyper need not poll EOF after the declared Content-Length is sent.
                    if *remaining == 0 {
                        self.complete();
                    }
                }
            }
            Poll::Pending => {}
        }
        result
    }
}
