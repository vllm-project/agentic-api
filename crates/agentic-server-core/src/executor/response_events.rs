//! Retain the existing sequence owner and bounded delivery queue beyond completion.
use crate::executor::BoxStream;
use futures::StreamExt;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{mpsc, oneshot, watch};

use crate::events::{EventFrame, EventPayload, SSEEventType, WireEvent};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::{
    GatewayStreamAccumulator, StreamEvent, StreamFrame, checked_stream_event_limited, executor_error_frame,
    synthetic_event, terminal_response_frame,
};
use crate::executor::telemetry::metrics::ExecutionClock;
use crate::types::{injection::InjectionEvent, request_response::ResponsePayload, websocket::SteeringEvent};
use crate::utils::common::{deserialize_from_value, serialize_to_value};

/// Execution and persistence outcome, independent of event-delivery success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseCommitState {
    Running,
    Committed,
    Failed,
    Aborted,
}

/// Evidence that this sink enqueued `response.created` and committed its sequence.
/// This proves publication to the bounded relay queue, not peer delivery,
/// successful execution, or incorporation of a particular steering batch.
/// Only the event sink can construct a receipt; observers share it through `Arc`.
#[derive(Debug)]
pub struct ResponseCreatedReceipt {
    response_id: String,
    previous_response_id: Option<String>,
    sequence_number: u64,
    source: Weak<Mutex<GatewayStreamAccumulator>>,
}

impl ResponseCreatedReceipt {
    /// The gateway response ID actually published on the wire.
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    /// The parent ID actually published on the wire, if this is a continuation.
    #[must_use]
    pub fn previous_response_id(&self) -> Option<&str> {
        self.previous_response_id.as_deref()
    }

    #[must_use]
    pub fn sequence_number(&self) -> u64 {
        self.sequence_number
    }

    /// Clones of a sink share identity; unrelated sinks cannot reuse its receipt.
    #[must_use]
    pub fn belongs_to(&self, sink: &ResponseEventSink) -> bool {
        Weak::ptr_eq(&self.source, &Arc::downgrade(&sink.state))
    }
}

struct ResponseState {
    commit: ResponseCommitState,
    created: Option<Arc<ResponseCreatedReceipt>>,
    steering_requested: bool,
    execution_joined: bool,
}

/// Every producer reserves bounded queue capacity before locking presentation state.
/// Stamping, serialization, enqueue, and sequence commit have no intervening await.
#[derive(Clone)]
pub struct ResponseEventSink {
    state: Arc<Mutex<GatewayStreamAccumulator>>,
    sender: mpsc::Sender<StreamEvent>,
    outcome: watch::Sender<ResponseState>,
    max_bytes: usize,
}

impl ResponseEventSink {
    /// A bounded stream for connection-level events with no associated response.
    #[must_use]
    pub fn channel(max_bytes: usize) -> (Self, BoxStream) {
        let (sender, receiver) = mpsc::channel(1);
        let sink = Self::new(sender, max_bytes);
        let stream = Self::stream(receiver, None);
        (sink, stream)
    }

    /// Hand each frame to the adapter; with an execution `clock`, this is where
    /// the first-event and first-text timings are taken.
    pub(super) fn stream(receiver: mpsc::Receiver<StreamEvent>, clock: Option<ExecutionClock>) -> BoxStream {
        futures::stream::unfold((receiver, clock), |(mut receiver, clock)| async move {
            while let Some(event) = receiver.recv().await {
                match event {
                    StreamEvent::Frame(frame) => {
                        if let Some(clock) = &clock {
                            clock.client_event(frame.text);
                        }
                        return Some((frame.content, (receiver, clock)));
                    }
                    StreamEvent::Flush(flushed) => {
                        // A cancelled flush waiter does not cancel event delivery.
                        let _ = flushed.send(());
                    }
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
        self.sender.send(StreamEvent::Flush(send)).await.map_err(|_| closed())?;
        receive.await.map_err(|_| closed())
    }

    pub(super) fn new(sender: mpsc::Sender<StreamEvent>, max_bytes: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(GatewayStreamAccumulator::with_max_stream_event_bytes(
                max_bytes,
            ))),
            sender,
            outcome: watch::channel(ResponseState {
                commit: ResponseCommitState::Running,
                created: None,
                steering_requested: false,
                execution_joined: false,
            })
            .0,
            max_bytes,
        }
    }

    /// Read the latest execution state without waiting for a terminal outcome.
    #[must_use]
    pub fn commit_state(&self) -> ResponseCommitState {
        self.outcome.borrow().commit
    }

    /// Read the durable publication fact without waiting or consuming it.
    #[must_use]
    pub fn created_receipt(&self) -> Option<Arc<ResponseCreatedReceipt>> {
        self.outcome.borrow().created.clone()
    }

    /// Request an item boundary without cancelling inference or started tools.
    /// The connection owner must retain accepted input before calling this.
    pub fn request_steering_boundary(&self) {
        self.outcome.send_modify(|state| state.steering_requested = true);
    }

    #[must_use]
    pub fn steering_requested(&self) -> bool {
        self.outcome.borrow().steering_requested
    }

    /// Wait for the monotonic boundary request; cancelling a waiter cannot
    /// consume the request. This is independent of connection cancellation.
    /// # Errors
    /// Returns a delivery error if the state publisher disconnects.
    pub async fn wait_steering(&self) -> ExecutorResult<()> {
        let mut state = self.outcome.subscribe();
        loop {
            if state.borrow_and_update().steering_requested {
                return Ok(());
            }
            state.changed().await.map_err(|_| closed())?;
        }
    }

    pub(super) fn mark_joined(&self) {
        self.outcome.send_modify(|state| state.execution_joined = true);
    }

    pub(crate) fn execution_joined(&self) -> bool {
        self.outcome.borrow().execution_joined
    }

    /// Wait for publication of `response.created`, independently of execution
    /// completion. Cancelling this observer does not cancel publication.
    /// An error does not stop producers: cancel and join execution before using
    /// absence of a receipt to authorize rollback of successor input.
    /// # Errors
    /// Returns an error if execution ends or the relay closes before publication.
    pub async fn wait_created(&self) -> ExecutorResult<Arc<ResponseCreatedReceipt>> {
        let mut outcome = self.outcome.subscribe();
        loop {
            let (created, commit) = {
                let current = outcome.borrow_and_update();
                (current.created.clone(), current.commit)
            };
            if let Some(created) = created {
                return Ok(created);
            }
            if commit != ResponseCommitState::Running {
                return self.created_or_error();
            }
            tokio::select! {
                changed = outcome.changed() => changed.map_err(|_| closed())?,
                () = self.sender.closed() => return self.created_or_error(),
            }
        }
    }

    fn created_or_error(&self) -> ExecutorResult<Arc<ResponseCreatedReceipt>> {
        // Enqueue and watch publication happen under this lock. A relay can
        // close between them; synchronize before reporting missing creation.
        // No watch borrow is held while acquiring presentation state.
        let _state = self.state.lock().map_err(|_| closed())?;
        self.created_receipt().ok_or_else(|| {
            ExecutorError::StreamError("response ended or delivery closed before response.created publication".into())
        })
    }

    /// Wait for persistence or terminal execution failure, rather than merely
    /// closure of input admission. Inspect [`Self::commit_state`] to distinguish
    /// a committed checkpoint from failed or aborted execution.
    /// # Errors
    /// Returns an error if the state publisher disconnects before finishing.
    pub async fn wait_finished(&self) -> ExecutorResult<()> {
        let mut state = self.outcome.subscribe();
        loop {
            match state.borrow_and_update().commit {
                ResponseCommitState::Committed | ResponseCommitState::Failed | ResponseCommitState::Aborted => {
                    return Ok(());
                }
                ResponseCommitState::Running => {}
            }
            state.changed().await.map_err(|_| closed())?;
        }
    }

    pub(super) fn mark_aborted(&self) {
        self.outcome
            .send_modify(|state| state.commit = ResponseCommitState::Aborted);
    }

    pub(super) fn mark_failed(&self) {
        self.outcome
            .send_modify(|state| state.commit = ResponseCommitState::Failed);
    }

    /// Emit through exactly the same sequence owner and queue as response events.
    /// # Errors
    /// A closed queue or oversized acknowledgement is a delivery failure, not rejection.
    pub async fn emit_local(&self, event: InjectionEvent) -> ExecutorResult<()> {
        let value = serialize_to_value(&event).map_err(ExecutorError::JsonError)?;
        let wire: WireEvent = deserialize_from_value(value).map_err(ExecutorError::JsonError)?;
        let mut frame = EventFrame {
            event_type: SSEEventType::Other,
            payload: EventPayload::None,
            wire,
        };
        self.emit_frame(&mut frame, 0).await?;
        Ok(())
    }

    /// Emit a decided steering outcome through the response's retained sequence
    /// owner, including after its terminal event. This does not admit steering
    /// input or change execution/persistence state.
    ///
    /// Failed input uses shared typed serialization here. Exact original-input
    /// echoes still require bounded adapter integration before size validation
    /// and enqueue; callers must not rewrite an already emitted event.
    /// # Errors
    /// A closed queue or oversized event is a delivery failure, not rejection
    /// of an accepted steer, and must not trigger replay of its input.
    pub async fn emit_steering(&self, event: SteeringEvent) -> ExecutorResult<()> {
        let event_type = SSEEventType::from(&event);
        let mut rest = serde_json::Map::new();
        match event {
            SteeringEvent::Accepted { steer } => {
                rest.insert("steer".into(), serialize_to_value(&steer)?);
            }
            SteeringEvent::Pending {
                steer,
                reason,
                required_input,
            } => {
                rest.insert("steer".into(), serialize_to_value(&steer)?);
                rest.insert("reason".into(), serialize_to_value(&reason)?);
                rest.insert("required_input".into(), serialize_to_value(&required_input)?);
            }
            SteeringEvent::Failed { steer, error } => {
                rest.insert("steer".into(), serialize_to_value(&steer)?);
                rest.insert("error".into(), serialize_to_value(&error)?);
            }
        }
        let mut frame = synthetic_event(event_type, rest)?;
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
        let created = if frame.event_type == SSEEventType::ResponseCreated {
            // The relay rewrites wire IDs to gateway IDs without rewriting the
            // normalized payload. Borrow final wire metadata, not payload IDs.
            let response = frame.wire.rest.get("response");
            let response_id = response
                .and_then(|response| response.get("id"))
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| ExecutorError::StreamError("response.created has no response ID".into()))?;
            let previous_response_id = match response.and_then(|response| response.get("previous_response_id")) {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(id)) if !id.is_empty() => Some(id.clone()),
                Some(_) => {
                    return Err(ExecutorError::StreamError(
                        "response.created has an invalid parent ID".into(),
                    ));
                }
            };
            Some(Arc::new(ResponseCreatedReceipt {
                response_id: response_id.to_owned(),
                previous_response_id,
                sequence_number,
                source: Arc::downgrade(&self.state),
            }))
        } else {
            None
        };
        permit.send(StreamEvent::Frame(StreamFrame {
            content,
            sequence_number,
            text: frame.event_type == SSEEventType::OutputTextDelta,
        }));
        *state = published;
        if let Some(created) = created {
            self.outcome.send_modify(|outcome| outcome.created = Some(created));
        }
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
        let mut frame = terminal_response_frame(payload)?;
        let state = if frame.event_type == SSEEventType::ResponseFailed {
            ResponseCommitState::Failed
        } else {
            ResponseCommitState::Committed
        };
        self.emit_frame(&mut frame, 0).await?;
        self.outcome.send_modify(|outcome| outcome.commit = state);
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
    use crate::types::websocket::{
        AcceptedSteer, FailedSteer, SteeringFailure, SteeringFailureCode, SteeringPendingReason, SteeringRequiredInput,
    };
    use futures::FutureExt;
    use serde_json::json;

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

    fn accepted() -> SteeringEvent {
        SteeringEvent::Accepted {
            steer: AcceptedSteer {
                id: "steer_test".into(),
                previous_response_id: "resp_test".into(),
            },
        }
    }

    fn failed(input: &str) -> SteeringEvent {
        let json = crate::utils::common::serialize_to_string(&input).unwrap();
        let bytes = json.len();
        SteeringEvent::Failed {
            steer: FailedSteer {
                id: Some("steer_test".into()),
                previous_response_id: Some("resp_test".into()),
                input: crate::types::websocket::SteeringInputEcho::from_json(json, bytes).unwrap(),
            },
            error: SteeringFailure {
                code: SteeringFailureCode::SuccessorCreationFailed,
                message: "Successor creation failed.".into(),
            },
        }
    }

    fn created(response_id: &str, previous_response_id: Option<&str>) -> EventFrame {
        let data = crate::utils::common::serialize_to_string(&json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "previous_response_id": previous_response_id,
                "status": "in_progress",
            }
        }))
        .unwrap();
        crate::events::normalize_sse_line(&format!("data: {data}")).unwrap()
    }

    #[tokio::test]
    async fn created_receipt_uses_published_ids_and_is_bound_to_its_sink() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        assert!(sink.created_receipt().is_none());
        let waiting = sink.wait_created();
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());

        let mut frame = created("upstream_id", None);
        assert!(matches!(&frame.payload, EventPayload::Response { id, .. } if id == "upstream_id"));
        // Match the relay's gateway ID rewrite, which leaves the normalized
        // upstream payload intact. The receipt must describe delivered IDs.
        frame.wire.rest["response"]["id"] = json!("resp_successor");
        frame.wire.rest["response"]["previous_response_id"] = json!("resp_parent");
        sink.emit_frame(&mut frame, 0).await.unwrap();
        let receipt = waiting.await.unwrap();
        assert_eq!(receipt.response_id(), "resp_successor");
        assert_eq!(receipt.previous_response_id(), Some("resp_parent"));
        assert_eq!(receipt.sequence_number(), 0);
        assert!(receipt.belongs_to(&sink));
        assert!(receipt.belongs_to(&sink.clone()));
        let (unrelated, _stream) = ResponseEventSink::channel(4096);
        assert!(!receipt.belongs_to(&unrelated));
        assert_eq!(sink.commit_state(), ResponseCommitState::Running);
        assert!(sink.wait_finished().now_or_never().is_none());
        // Publication is observable before the relay consumes the event.
        assert!(Arc::ptr_eq(&receipt, &sink.created_receipt().unwrap()));
        let chunk = stream.next().await.unwrap();
        let delivered = chunk.lines().find_map(crate::events::normalize_sse_line).unwrap();
        assert_eq!(delivered.wire.rest["response"]["id"], receipt.response_id());
        assert_eq!(delivered.sequence_number(), Some(receipt.sequence_number()));
        let payload: ResponsePayload = deserialize_from_value(json!({
            "id": "resp_successor", "object": "response", "created_at": 0,
            "model": "test-model", "status": "completed", "output": []
        }))
        .unwrap();
        sink.emit_terminal(&payload).await.unwrap();
        assert_eq!(sink.commit_state(), ResponseCommitState::Committed);
        assert!(Arc::ptr_eq(&receipt, &sink.wait_created().await.unwrap()));
    }

    #[tokio::test]
    async fn duplicate_creation_and_later_failure_preserve_the_first_receipt() {
        for aborted in [false, true] {
            let (sink, mut stream) = ResponseEventSink::channel(4096);
            sink.emit_frame(&mut created("resp_first", Some("resp_parent")), 0)
                .await
                .unwrap();
            let receipt = sink.created_receipt().unwrap();
            stream.next().await.unwrap();
            assert!(!sink.emit_frame(&mut created("resp_duplicate", None), 0).await.unwrap());
            assert!(Arc::ptr_eq(&receipt, &sink.created_receipt().unwrap()));
            assert!(stream.next().now_or_never().is_none());
            if aborted {
                sink.mark_aborted();
            } else {
                sink.mark_failed();
            }
            sink.wait_finished().await.unwrap();
            drop(stream);
            assert!(Arc::ptr_eq(&receipt, &sink.wait_created().await.unwrap()));
        }
    }

    #[tokio::test]
    async fn cancelled_creation_does_not_publish_or_consume_a_sequence() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        sink.emit_local(acknowledgement()).await.unwrap();
        let mut frame = created("resp_cancelled", None);
        assert!(sink.emit_frame(&mut frame, 0).now_or_never().is_none());
        assert!(sink.created_receipt().is_none());
        assert!(sink.wait_created().now_or_never().is_none());
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);
        sink.emit_frame(&mut created("resp_retry", None), 0).await.unwrap();
        let receipt = sink.wait_created().await.unwrap();
        assert_eq!(receipt.response_id(), "resp_retry");
        assert_eq!(receipt.sequence_number(), 1);
        assert_eq!(sequence(&stream.next().await.unwrap()), 1);
    }

    #[tokio::test]
    async fn failed_creation_checks_allow_a_corrected_retry() {
        let (sink, mut stream) = ResponseEventSink::channel(512);
        let mut oversized = created(&"x".repeat(512), None);
        assert!(matches!(
            sink.emit_frame(&mut oversized, 0).await,
            Err(ExecutorError::ResourceLimitExceeded { .. })
        ));
        assert!(sink.created_receipt().is_none());
        assert!(sink.emit_frame(&mut created("", None), 0).await.is_err());
        let mut invalid_parent = created("resp_test", None);
        invalid_parent.wire.rest["response"]["previous_response_id"] = json!(42);
        assert!(sink.emit_frame(&mut invalid_parent, 0).await.is_err());
        assert!(sink.created_receipt().is_none());
        assert!(stream.next().now_or_never().is_none());
        sink.emit_frame(&mut created("resp_retry", None), 0).await.unwrap();
        let receipt = sink.wait_created().await.unwrap();
        assert_eq!(receipt.previous_response_id(), None);
        assert_eq!(receipt.sequence_number(), 0);
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);
    }

    #[tokio::test]
    async fn creation_wait_ends_when_execution_or_delivery_ends_before_creation() {
        for aborted in [false, true] {
            let (sink, _stream) = ResponseEventSink::channel(4096);
            let waiting = sink.wait_created();
            tokio::pin!(waiting);
            assert!(waiting.as_mut().now_or_never().is_none());
            if aborted {
                sink.mark_aborted();
            } else {
                sink.mark_failed();
            }
            assert!(waiting.await.is_err());
            assert!(sink.created_receipt().is_none());
        }
        let (sink, stream) = ResponseEventSink::channel(4096);
        let waiting = sink.wait_created();
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        drop(stream);
        assert!(waiting.await.is_err());
        assert!(sink.emit_frame(&mut created("resp_closed", None), 0).await.is_err());
        assert!(sink.created_receipt().is_none());
    }

    #[tokio::test]
    async fn steering_controls_keep_parent_sequence_after_terminal_completion() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        // Both typed control APIs share the same delivery counter.
        sink.emit_local(acknowledgement()).await.unwrap();
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);
        let payload: ResponsePayload = serde_json::from_value(json!({
            "id": "resp_test", "object": "response", "created_at": 0,
            "model": "test-model", "status": "completed", "output": []
        }))
        .unwrap();
        sink.emit_terminal(&payload).await.unwrap();
        let terminal = stream.next().await.unwrap();
        assert_eq!(sequence(&terminal), 1);
        assert!(terminal.contains("response.completed"));
        assert_eq!(sink.commit_state(), ResponseCommitState::Committed);

        let pending = SteeringEvent::Pending {
            steer: AcceptedSteer {
                id: "steer_test".into(),
                previous_response_id: "resp_test".into(),
            },
            reason: SteeringPendingReason::WaitingForRequiredInput,
            required_input: vec![SteeringRequiredInput::Function {
                call_id: "call_test".to_owned().try_into().unwrap(),
                name: "get_weather".into(),
            }],
        };
        for (index, event) in [accepted(), pending, failed("Keep the earlier steering input.")]
            .into_iter()
            .enumerate()
        {
            let mut expected = serde_json::to_value(&event).unwrap();
            expected["sequence_number"] = json!(index + 2);
            sink.emit_steering(event).await.unwrap();
            let chunk = stream.next().await.unwrap();
            let data = chunk.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(data).unwrap(), expected);
            assert_eq!(sink.commit_state(), ResponseCommitState::Committed);
        }

        // Successors have an independent sink; retaining parent controls does
        // not lend their sequence counter to the successor.
        let (successor, mut successor_stream) = ResponseEventSink::channel(4096);
        successor.emit_terminal(&payload).await.unwrap();
        assert_eq!(sequence(&successor_stream.next().await.unwrap()), 0);
    }

    #[tokio::test]
    async fn steering_delivery_limits_and_cancelled_waits_do_not_consume_sequence() {
        let (sink, mut stream) = ResponseEventSink::channel(4096);
        sink.emit_steering(accepted()).await.unwrap();
        // The one-entry queue is full. Dropping a waiting delivery future
        // cannot stamp a sequence or enqueue an extra event.
        assert!(sink.emit_steering(accepted()).now_or_never().is_none());
        assert_eq!(sequence(&stream.next().await.unwrap()), 0);

        let oversized = failed(&"x".repeat(4096));
        assert!(matches!(
            sink.emit_steering(oversized).await,
            Err(ExecutorError::ResourceLimitExceeded {
                limit: crate::executor::ResourceLimit::StreamEvent,
                max_bytes: 4096,
            })
        ));
        sink.emit_steering(accepted()).await.unwrap();
        assert_eq!(sequence(&stream.next().await.unwrap()), 1);
        assert_eq!(sink.commit_state(), ResponseCommitState::Running);
        drop(stream);
        assert!(sink.emit_steering(accepted()).await.is_err());
    }

    #[tokio::test]
    async fn terminal_wait_observes_failed_and_aborted_execution() {
        for aborted in [false, true] {
            let (sink, _stream) = ResponseEventSink::channel(4096);
            let wait = sink.wait_finished();
            tokio::pin!(wait);
            assert!(wait.as_mut().now_or_never().is_none());
            if aborted {
                sink.mark_aborted();
            } else {
                sink.mark_failed();
            }
            wait.await.unwrap();
            // Reusing the retained sink after terminal failure still observes
            // completion immediately, rather than waiting for a nonexistent commit.
            assert!(sink.wait_finished().now_or_never().unwrap().is_ok());
        }
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
