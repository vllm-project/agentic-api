//! Retain the existing sequence owner and bounded delivery queue beyond completion.
use crate::executor::BoxStream;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

use crate::events::{EventFrame, EventPayload, SSEEventType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{
    GatewayStreamAccumulator, StreamEvent, checked_stream_event_limited, executor_error_frame, terminal_response_frame,
};
use crate::types::{injection::InjectionEvent, request_response::ResponsePayload};
use crate::utils::common::serialize_to_value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseCommitState {
    Running,
    Committed,
    Failed,
    Aborted,
}

/// Every producer reserves bounded queue capacity before locking presentation state.
/// Stamping, serialization, enqueue, and sequence commit have no intervening await.
#[derive(Clone)]
pub struct ResponseEventSink {
    state: Arc<Mutex<GatewayStreamAccumulator>>,
    sender: mpsc::Sender<StreamEvent>,
    commit: watch::Sender<ResponseCommitState>,
    max_bytes: usize,
}

impl ResponseEventSink {
    /// A bounded stream for connection-level events with no associated response.
    #[must_use]
    pub fn channel(max_bytes: usize) -> (Self, BoxStream) {
        let (sender, receiver) = mpsc::channel(1);
        let sink = Self::new(sender, max_bytes);
        let stream = Self::stream(receiver);
        (sink, stream)
    }

    pub(super) fn stream(receiver: mpsc::Receiver<StreamEvent>) -> BoxStream {
        futures::stream::unfold(receiver, |mut receiver| async move {
            while let Some(event) = receiver.recv().await {
                if let Some(flushed) = event.flushed {
                    let _ = flushed.send(());
                } else {
                    return Some((event.content, receiver));
                }
            }
            None
        })
        .boxed()
    }

    /// Wait until the relay has consumed all previously enqueued events.
    /// # Errors
    /// A dropped relay is an explicit delivery failure.
    pub async fn flush(&self) -> ExecutorResult<()> {
        let (send, receive) = oneshot::channel();
        self.sender
            .send(StreamEvent {
                content: String::new(),
                sequence_number: 0,
                flushed: Some(send),
            })
            .await
            .map_err(|_| closed())?;
        receive.await.map_err(|_| closed())
    }

    pub(super) fn new(sender: mpsc::Sender<StreamEvent>, max_bytes: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(GatewayStreamAccumulator::with_max_stream_event_bytes(
                max_bytes,
            ))),
            sender,
            commit: watch::channel(ResponseCommitState::Running).0,
            max_bytes,
        }
    }

    #[must_use]
    pub fn commit_state(&self) -> ResponseCommitState {
        *self.commit.borrow()
    }

    /// Wait for persistence, not merely closure of input admission.
    /// # Errors
    /// Returns an error if execution or persistence fails.
    pub async fn wait_committed(&self) -> ExecutorResult<()> {
        let mut state = self.commit.subscribe();
        loop {
            match *state.borrow_and_update() {
                ResponseCommitState::Committed => return Ok(()),
                ResponseCommitState::Failed | ResponseCommitState::Aborted => return Err(closed()),
                ResponseCommitState::Running => {}
            }
            state.changed().await.map_err(|_| closed())?;
        }
    }

    pub(super) fn mark_aborted(&self) {
        self.commit.send_replace(ResponseCommitState::Aborted);
    }

    pub(super) fn mark_failed(&self) {
        self.commit.send_replace(ResponseCommitState::Failed);
    }

    /// Emit through exactly the same sequence owner and queue as response events.
    /// # Errors
    /// A closed queue or oversized acknowledgement is a delivery failure, not rejection.
    pub async fn emit_local(&self, event: InjectionEvent) -> ExecutorResult<()> {
        let value = serialize_to_value(&event).map_err(ExecutorError::JsonError)?;
        let wire: WireEvent = serde_json::from_value(value).map_err(ExecutorError::JsonError)?;
        let mut frame = EventFrame {
            event_type: SSEEventType::Other,
            payload: EventPayload::None,
            wire,
        };
        self.emit_frame(&mut frame, 0).await?;
        Ok(())
    }

    pub(super) async fn emit_frame(&self, frame: &mut EventFrame, offset: usize) -> ExecutorResult<bool> {
        let permit = self.sender.reserve().await.map_err(|_| closed())?;
        let mut state = self.state.lock().map_err(|_| closed())?;
        let mut published = state.clone();
        if !published.process_event(frame, offset) {
            return Ok(false);
        }
        let content = checked_stream_event_limited(frame, self.max_bytes)?;
        let sequence_number = frame.sequence_number().expect("delivery assigned a sequence");
        permit.send(StreamEvent {
            flushed: None,
            content,
            sequence_number,
        });
        *state = published;
        Ok(true)
    }

    pub(super) fn validate_terminal(&self, payload: &ResponsePayload) -> ExecutorResult<()> {
        let mut frame = terminal_response_frame(payload)?;
        // Late acknowledgements may advance the counter during persistence.
        frame.wire.sequence_number = Some(u64::MAX);
        checked_stream_event_limited(&frame, self.max_bytes)?;
        Ok(())
    }

    pub(super) async fn emit_terminal(&self, payload: &ResponsePayload) -> ExecutorResult<()> {
        self.emit_frame(&mut terminal_response_frame(payload)?, 0).await?;
        self.commit.send_replace(ResponseCommitState::Committed);
        Ok(())
    }

    pub(super) async fn emit_error(&self, error: &ExecutorError) -> ExecutorResult<()> {
        self.emit_frame(&mut executor_error_frame(error), 0).await?;
        Ok(())
    }
}

fn closed() -> ExecutorError {
    ExecutorError::StreamError("response delivery failed or closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn acknowledgement() -> InjectionEvent {
        InjectionEvent::Created {
            response_id: "resp_test".into(),
        }
    }

    fn sequence(chunk: &str) -> u64 {
        let data = chunk.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
        serde_json::from_str::<serde_json::Value>(data).unwrap()["sequence_number"]
            .as_u64()
            .unwrap()
    }

    #[tokio::test]
    async fn cancelling_full_queue_admission_does_not_consume_sequence() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        sink.emit_local(acknowledgement()).await.unwrap();
        assert!(sink.emit_local(acknowledgement()).now_or_never().is_none());
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);
        sink.emit_local(acknowledgement()).await.unwrap();
        assert_eq!(sequence(&stream.next().await.unwrap()), 1);
    }

    #[tokio::test]
    async fn flush_waits_for_relay_and_does_not_emit_a_wire_event() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        sink.emit_local(acknowledgement()).await.unwrap();
        let flushing = tokio::spawn(async move { sink.flush().await });
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);
        assert!(stream.next().await.is_none());
        flushing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropped_relay_and_oversized_acknowledgement_are_delivery_failures() {
        let (sink, stream) = ResponseEventSink::channel(4096);
        drop(stream);
        assert!(sink.emit_local(acknowledgement()).await.is_err());
        assert!(sink.flush().await.is_err());
        let (sink, mut stream) = ResponseEventSink::channel(1);
        assert!(sink.emit_local(acknowledgement()).await.is_err());
        drop(sink);
        assert!(stream.next().await.is_none());
    }
}
