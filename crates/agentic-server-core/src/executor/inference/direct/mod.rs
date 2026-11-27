//! Direct inference through an injected model runtime.
//!
//! Implementations own model-specific preparation and submission. They return the
//! existing normalized Responses events; the executor still owns validation,
//! output assembly, tool execution, delivery, and persistence.

#[cfg(test)]
mod tests;

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::events::EventFrame;
use crate::executor::error::{ExecutorError, ExecutorResult, ResourceLimit};
use crate::types::request_response::UpstreamRequest;
use crate::utils::common::serialized_size_up_to;
use crate::utils::uuid7_str;

/// One attempt's identity and cancellation, independent of public response IDs.
#[derive(Debug, Clone)]
pub struct GenerationControl {
    pub request_id: String,
    pub response_id: String,
    pub cancellation: CancellationToken,
    pub max_event_bytes: usize,
}

/// A pull-driven stream; dropping it must cancel unfinished engine work.
pub type EventStream = BoxStream<'static, ExecutorResult<EventFrame>>;

/// Direct runtime boundary implemented by the native vLLM integration.
///
/// Preparation consumes the existing model-visible request projection. An
/// implementation must reject unsupported settings before engine submission,
/// preserve event payload/wire consistency, and make both admission and the
/// returned stream cancellation-safe. Runtime tasks remain owned by the backend.
pub trait DirectInferenceBackend: Debug + Send + Sync {
    fn generate<'a>(
        &'a self,
        request: UpstreamRequest<'a>,
        control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>>;
}

pub(in crate::executor) async fn call<'a>(
    backend: &'a Arc<dyn DirectInferenceBackend>,
    request: UpstreamRequest<'a>,
    response_id: String,
    chunk_timeout: Duration,
    max_event_bytes: usize,
) -> ExecutorResult<EventStream> {
    let cancellation = CancellationToken::new();
    let guard = cancellation.clone().drop_guard();
    let control = GenerationControl {
        request_id: uuid7_str("infer_"),
        response_id,
        cancellation,
        max_event_bytes,
    };
    let mut events = wait(backend.generate(request, control), chunk_timeout).await?;
    Ok(Box::pin(stream! {
        let _guard = guard;
        loop {
            match wait(async { Ok(events.next().await) }, chunk_timeout).await {
                Ok(Some(event)) => {
                    let event = event.and_then(|frame| {
                        if serialized_size_up_to(&frame.wire, max_event_bytes)?.is_none() {
                            return Err(ExecutorError::ResourceLimitExceeded {
                                limit: ResourceLimit::StreamEvent,
                                max_bytes: max_event_bytes,
                            });
                        }
                        Ok(frame)
                    });
                    let failed = event.is_err();
                    yield event;
                    if failed { return; }
                }
                Ok(None) => return,
                Err(error) => { yield Err(error); return; }
            }
        }
    }))
}

async fn wait<T>(future: impl Future<Output = ExecutorResult<T>>, timeout: Duration) -> ExecutorResult<T> {
    if timeout.is_zero() {
        future.await
    } else {
        tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| ExecutorError::LLMTransport {
                status: http::StatusCode::GATEWAY_TIMEOUT,
                message: "direct inference timed out",
            })?
    }
}
