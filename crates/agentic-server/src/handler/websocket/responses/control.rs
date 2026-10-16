//! Connection-owned routes and bounded tasks. No agent history lives here.
use super::{StreamId, WsError, WsEventLimit, WsOutboundEvent, stream_ws_response};
use crate::auth::AuthenticatedPrincipal;
mod injection;
mod steering;
use agentic_core::{
    executor::{
        BoxStream, ResponseRunOwner, ResponseSession, RunningResponse,
        multi_agent::RunControl,
        response_events::{ResponseCommitState, ResponseEventSink},
        session::{ResponseCheckpointPin, SteeringQueue},
    },
    types::{
        injection::ResponseInjectRequest,
        request_response::RequestPayload,
        websocket::{
            AcceptedSteer, FailedSteer, SteeringEvent, SteeringFailure, SteeringFailureCode, SteeringInputEcho,
        },
    },
};
use injection::inject;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const MAX_CONTROLS: usize = 32;
// Each accepted steer may also need one pending outcome, plus a publication fence.
const MAX_STEERING_OUTCOMES: usize = MAX_CONTROLS * 2 + 1;
const MAX_CONTROL_BYTES: usize = 12 * 1024 * 1024;
const MAX_ROUTES: usize = 128;
const RETAIN_COMPLETED: Duration = Duration::from_secs(60);

pub(super) struct Registration {
    pub(super) response: RunningResponse,
    pub(super) lane: Option<StreamId>,
    pub(super) reply: oneshot::Sender<(ResponseRunOwner, Arc<SteeringTarget>)>,
    pub(super) session: Arc<ResponseSession>,
    pub(super) steering_supported: bool,
}
struct Route {
    cancellation: CancellationToken,
    control: Option<RunControl>,
    sink: ResponseEventSink,
    lane: Option<StreamId>,
    steering: Arc<SteeringTarget>,
}
struct RetainedRoute {
    ordinal: usize,
    route: Arc<Route>,
    completed: Option<Instant>,
}
pub(super) struct ControlCompletion {
    bytes: usize,
    pub(super) result: Result<(), WsError>,
}

pub(super) struct Controls {
    routes: HashMap<String, RetainedRoute>,
    pub(super) registrations: mpsc::Receiver<Registration>,
    pub(super) register: mpsc::Sender<Registration>,
    pub(super) tasks: JoinSet<ControlCompletion>,
    pub(super) relays: JoinSet<Result<(), WsError>>,
    pub(super) steering_workers: JoinSet<Result<(), WsError>>,
    steering_bytes: Arc<AtomicUsize>,
    // Settings are bounded even when no steering input has been accepted.
    steering_settings_bytes: Arc<AtomicUsize>,
    // Rejections must remain deliverable when accepted steering fills its budget.
    // A second bounded allowance covers failure echoes until their relay flushes.
    steering_rejection_bytes: Arc<AtomicUsize>,
    next_ordinal: usize,
    bytes: usize,
    rejected_owner: Option<ResponseRunOwner>,
    outbound: mpsc::Sender<WsOutboundEvent>,
    limit: WsEventLimit,
    // Unknown targets have no response sequence owner. Their connection-level
    // failures use the same relay implementation, separate from response routes.
    unrouted: ResponseEventSink,
}
impl Controls {
    pub(super) fn new(outbound: mpsc::Sender<WsOutboundEvent>, limit: WsEventLimit) -> Self {
        let (register, registrations) = mpsc::channel(64);
        let (unrouted, events) = ResponseEventSink::channel(limit.executor_limit(None));
        let mut result = Self {
            routes: HashMap::new(),
            registrations,
            register,
            tasks: JoinSet::new(),
            relays: JoinSet::new(),
            steering_workers: JoinSet::new(),
            steering_bytes: Arc::new(AtomicUsize::new(0)),
            steering_settings_bytes: Arc::new(AtomicUsize::new(0)),
            steering_rejection_bytes: Arc::new(AtomicUsize::new(0)),
            next_ordinal: 0,
            bytes: 0,
            rejected_owner: None,
            outbound,
            limit,
            unrouted,
        };
        result.spawn_relay(events, None);
        result
    }
    fn spawn_relay(&mut self, events: BoxStream, lane: Option<StreamId>) {
        let outbound = self.outbound.clone();
        let limit = self.limit;
        self.relays
            .spawn(async move { stream_ws_response(&outbound, events, lane.as_ref(), limit).await });
    }
    fn spawn_steering_worker(&mut self, sink: ResponseEventSink) -> mpsc::Sender<SteeringOutcome> {
        let (outcomes, mut receive_outcomes) = mpsc::channel(MAX_STEERING_OUTCOMES);
        let worker_sink = sink;
        self.steering_workers.spawn(async move {
            while let Some(outcome) = receive_outcomes.recv().await {
                match outcome {
                    SteeringOutcome::Event { event, charge } => {
                        worker_sink.emit_steering(event).await?;
                        worker_sink.flush().await?;
                        drop(charge);
                    }
                    SteeringOutcome::Barrier(reply) => {
                        let _ = reply.send(());
                    }
                }
            }
            Ok(())
        });
        outcomes
    }

    pub(super) fn cancel_live_controls(&self) {
        for entry in self.routes.values() {
            if entry.route.control.is_some() {
                entry.route.cancellation.cancel();
            }
        }
    }

    pub(super) fn expire(&mut self) {
        self.release_inactive_settings();
        let now = Instant::now();
        self.routes.retain(|_, entry| {
            if entry.route.sink.commit_state() != ResponseCommitState::Running {
                let completed = entry.completed.get_or_insert(now);
                return entry.route.steering.retains_accepted()
                    || Arc::strong_count(&entry.route) > 1
                    || now.duration_since(*completed) < RETAIN_COMPLETED;
            }
            true
        });
    }
    pub(super) fn register(&mut self, registration: Registration) -> Result<(), WsError> {
        self.expire();
        if self.routes.len() == MAX_ROUTES {
            let oldest = self
                .routes
                .iter()
                .filter(|(_, entry)| {
                    entry.completed.is_some()
                        && Arc::strong_count(&entry.route) == 1
                        && !entry.route.steering.retains_accepted()
                })
                .min_by_key(|(_, entry)| entry.completed)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.routes.remove(&id);
            }
        }
        // Retired relays can still be draining their final bounded frame.
        if self.routes.len() >= MAX_ROUTES || self.relays.len() >= MAX_ROUTES * 2 {
            registration.response.owner.cancel();
            self.rejected_owner = Some(registration.response.owner);
            return Err(WsError::TooManyRequests);
        }
        let Some(ordinal) = self.next_ordinal.checked_add(1) else {
            registration.response.owner.cancel();
            self.rejected_owner = Some(registration.response.owner);
            return Err(WsError::TooManyRequests);
        };
        self.next_ordinal = ordinal;
        let Registration {
            response,
            lane,
            reply,
            session,
            steering_supported,
        } = registration;
        let RunningResponse {
            response_id,
            settings,
            control,
            events,
            sink,
            owner,
        } = response;
        let state = match SteeringState::new(settings, steering_supported, &self.steering_settings_bytes) {
            Ok(state) => state,
            Err(error) => {
                owner.cancel();
                self.rejected_owner = Some(owner);
                return Err(error);
            }
        };
        let cancellation = owner.cancellation_token();
        let outcomes = self.spawn_steering_worker(sink.clone());
        let steering = Arc::new(SteeringTarget {
            session,
            lane: lane.clone(),
            sink: sink.clone(),
            outcomes,
            state: Mutex::new(state),
        });
        self.routes.insert(
            response_id,
            RetainedRoute {
                ordinal,
                route: Arc::new(Route {
                    cancellation,
                    control,
                    sink,
                    lane: lane.clone(),
                    steering: Arc::clone(&steering),
                }),
                completed: None,
            },
        );
        self.spawn_relay(events, lane);
        if let Err((owner, _)) = reply.send((owner, steering)) {
            owner.cancel();
            self.rejected_owner = Some(owner);
            return Err(WsError::SendFailed);
        }
        Ok(())
    }
    pub(super) fn dispatch(
        &mut self,
        request: ResponseInjectRequest,
        bytes: usize,
        principal: Option<Arc<AuthenticatedPrincipal>>,
    ) -> Result<(), WsError> {
        self.expire();
        if self.tasks.len() >= MAX_CONTROLS || bytes > MAX_CONTROL_BYTES.saturating_sub(self.bytes) {
            return Err(WsError::TooManyRequests);
        }
        let route = self
            .routes
            .get(&request.response_id)
            .map(|entry| Arc::clone(&entry.route));
        let unrouted = self.unrouted.clone();
        let outbound = self.outbound.clone();
        let limit = self.limit;
        self.bytes += bytes;
        self.tasks.spawn(async move {
            let lane = route.as_ref().and_then(|route| route.lane.clone());
            if let Some(event) = super::websocket_identity_error_event(principal.as_deref()) {
                let _ = super::queue_ws_json(&outbound, event, lane.as_ref(), limit).await;
                return ControlCompletion {
                    bytes,
                    result: Err(WsError::SendFailed),
                };
            }
            let sink = route
                .as_ref()
                .map_or_else(|| unrouted.clone(), |route| route.sink.clone());
            let result = match inject(route.clone(), unrouted, request).await {
                Ok(()) => sink.flush().await.map_err(WsError::from),
                Err(error) => Err(error),
            };
            drop(route);
            if let Err(error) = &result {
                // Uncharacterized validation and transport failures use the
                // connection error path; characterized rejections use the relay.
                if let Some(frame) = error.to_ws_frame() {
                    let _ = super::queue_ws_json(&outbound, frame, lane.as_ref(), limit).await;
                }
            }
            ControlCompletion { bytes, result }
        });
        Ok(())
    }
    pub(super) fn finish(&mut self, completion: Result<ControlCompletion, tokio::task::JoinError>) -> bool {
        match completion {
            Ok(completion) => {
                self.bytes -= completion.bytes;
                match completion.result {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::debug!(%error, "responses websocket control ended with a transport failure");
                        false
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "responses websocket control task failed");
                false
            }
        }
    }
    pub(super) async fn shutdown(&mut self) {
        self.registrations.close();
        if let Some(mut owner) = self.rejected_owner.take()
            && let Err(error) = owner.join().await
        {
            tracing::debug!(%error, "responses websocket execution ended during disposal");
        }
        while let Ok(registration) = self.registrations.try_recv() {
            let mut owner = registration.response.owner;
            owner.cancel();
            if let Err(error) = owner.join().await {
                tracing::debug!(%error, "responses websocket execution ended during disposal");
            }
        }
        self.tasks.abort_all();
        while let Some(completion) = self.tasks.join_next().await {
            if let Err(error) = completion
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "responses websocket control task failed during disposal");
            }
        }
        self.routes.clear();
        self.steering_workers.abort_all();
        while let Some(result) = self.steering_workers.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "steering outcome worker failed during disposal");
            }
        }
        self.relays.abort_all();
        while let Some(completion) = self.relays.join_next().await {
            match completion {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "responses websocket relay failed during disposal"),
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::warn!(%error, "responses websocket relay task failed during disposal"),
            }
        }
    }
}

/// Connection-local admission and exact-input ownership for one published response.
pub(super) struct SteeringTarget {
    session: Arc<ResponseSession>,
    pub(super) lane: Option<StreamId>,
    sink: ResponseEventSink,
    outcomes: mpsc::Sender<SteeringOutcome>,
    state: Mutex<SteeringState>,
}

struct SteeringState {
    settings: Option<SteeringSettings>,
    supported: bool,
    batch: Option<SteeringBatch>,
    phase: SteeringPhase,
    required_supported: bool,
}

/// Settings and their connection-level reservation have the same lifetime.
struct SteeringSettings {
    payload: RequestPayload,
    _charge: SteeringByteCharge,
}

/// Resources owned by one accepted batch, released together on commitment or failure.
/// Ordinary responses never allocate a batch or pin their checkpoint for steering.
struct SteeringBatch {
    queue: Option<SteeringQueue>,
    echoes: Vec<SteerEcho>,
    pin: Option<ResponseCheckpointPin>,
}

/// Parent completion and successor ownership are separate from execution status.
/// A completed parent remains eligible until a successor reserves its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SteeringPhase {
    Running,
    Ready,
    Reserved,
    Closed,
}

impl SteeringPhase {
    fn require_ready(self) -> Result<(), WsError> {
        match self {
            Self::Ready => Ok(()),
            Self::Running | Self::Reserved | Self::Closed => {
                Err(WsError::from(agentic_core::executor::ExecutorError::InvalidRequest(
                    "steering parent is not ready for successor preparation".into(),
                )))
            }
        }
    }
}

struct SteerEcho {
    steer: AcceptedSteer,
    input: SteeringInputEcho,
    charge: SteeringByteCharge,
}

enum SteeringOutcome {
    Event {
        event: SteeringEvent,
        charge: Option<SteeringByteCharge>,
    },
    Barrier(oneshot::Sender<()>),
}

impl SteeringOutcome {
    fn failed(
        id: Option<String>,
        previous_response_id: Option<String>,
        input: SteeringInputEcho,
        code: SteeringFailureCode,
    ) -> SteeringEvent {
        let message = match code {
            SteeringFailureCode::ResponseNotFound => "The target response was not found on this connection.",
            SteeringFailureCode::InvalidInput => {
                "Steering requires a previous_response_id and user text or user messages."
            }
            SteeringFailureCode::SteeringNotSupported => "Steering is not supported for this response configuration.",
            SteeringFailureCode::TooManyPendingSteers => "Too many pending steering inputs.",
            SteeringFailureCode::ResponseAlreadyCompleted => "The target response has already completed.",
            SteeringFailureCode::ResponseNotActive => "The target response is no longer active.",
            SteeringFailureCode::SuccessorCreationFailed => "The steering successor could not be created.",
        }
        .to_owned();
        SteeringEvent::Failed {
            steer: FailedSteer {
                id,
                previous_response_id,
                input,
            },
            error: SteeringFailure { code, message },
        }
    }
}

struct SteeringByteCharge {
    budget: Arc<AtomicUsize>,
    bytes: usize,
}
impl SteeringByteCharge {
    fn reserve(budget: &Arc<AtomicUsize>, bytes: usize) -> Option<Self> {
        budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= MAX_CONTROL_BYTES)
            })
            .ok()?;
        Some(Self {
            budget: Arc::clone(budget),
            bytes,
        })
    }
}
impl Drop for SteeringByteCharge {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod steering_tests {
    use super::*;

    #[test]
    fn settings_budget_bounds_unsteered_targets_and_refunds_dropped_snapshots() {
        let budget = Arc::new(AtomicUsize::new(0));
        let settings = || RequestPayload {
            instructions: Some("x".repeat(MAX_CONTROL_BYTES / 2)),
            ..RequestPayload::default()
        };
        let first = SteeringState::new(settings(), true, &budget).unwrap();
        assert!(first.settings.is_some());
        let used = budget.load(Ordering::Acquire);
        assert!(used > MAX_CONTROL_BYTES / 2 && used <= MAX_CONTROL_BYTES);
        for _ in 0..60 {
            let full = SteeringState::new(settings(), true, &budget).unwrap();
            assert!(full.settings.is_none());
            assert!(full.supported, "budget exhaustion is not a configuration rejection");
            assert_eq!(budget.load(Ordering::Acquire), used);
        }
        drop(first);
        assert_eq!(budget.load(Ordering::Acquire), 0);
        let available = SteeringState::new(settings(), true, &budget).unwrap();
        assert!(available.settings.is_some());
        drop(available);
        let unsupported = SteeringState::new(settings(), false, &budget).unwrap();
        assert!(unsupported.settings.is_none());
        assert_eq!(budget.load(Ordering::Acquire), 0);
        let oversized = SteeringState::new(
            RequestPayload {
                instructions: Some("x".repeat(MAX_CONTROL_BYTES)),
                ..RequestPayload::default()
            },
            true,
            &budget,
        )
        .unwrap();
        assert!(oversized.settings.is_none());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    fn target_fixture() -> (SteeringTarget, Controls, mpsc::Receiver<WsOutboundEvent>) {
        let (outbound, receive) = mpsc::channel(8);
        let mut controls = Controls::new(outbound, WsEventLimit(4096));
        let (sink, events) = ResponseEventSink::channel(4096);
        controls.spawn_relay(events, None);
        let outcomes = controls.spawn_steering_worker(sink.clone());
        let settings = RequestPayload::default();
        let target = SteeringTarget {
            session: Arc::new(ResponseSession::new(
                std::num::NonZeroUsize::new(16).unwrap(),
                std::num::NonZeroUsize::new(4096).unwrap(),
            )),
            lane: None,
            sink,
            outcomes,
            state: Mutex::new(SteeringState::new(settings, true, &controls.steering_settings_bytes).unwrap()),
        };
        (target, controls, receive)
    }

    #[tokio::test]
    async fn unreserved_commit_preserves_input_and_closed_parent_cannot_reopen() {
        use agentic_core::types::{io::ResponsesInput, websocket::SteeringInput};
        let (target, mut controls, mut receive) = target_fixture();
        let target = Arc::new(target);
        assert!(controls.steering_settings_bytes.load(Ordering::Acquire) > 0);
        let mut queue = SteeringQueue::new(
            "resp_parent".into(),
            std::num::NonZeroUsize::new(1).unwrap(),
            std::num::NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        let steer = queue
            .try_accept(SteeringInput::try_from(ResponsesInput::Text("KEEP_PRIVATE_INPUT".into())).unwrap())
            .unwrap()
            .clone();
        {
            let mut state = target.state.lock().unwrap();
            state.batch = Some(SteeringBatch {
                queue: Some(queue),
                pin: None,
                echoes: vec![SteerEcho {
                    steer,
                    input: SteeringInputEcho::from_json("\"KEEP_PRIVATE_INPUT\"".into(), 4096).unwrap(),
                    charge: SteeringByteCharge::reserve(&controls.steering_bytes, 128).unwrap(),
                }],
            });
        }
        assert!(target.commit_successor().is_err());
        target.mark_ready("resp_parent").unwrap();
        assert!(target.commit_successor().is_err());
        {
            let state = target.state.lock().unwrap();
            assert_eq!(state.phase, SteeringPhase::Ready);
            assert_eq!(state.batch.as_ref().unwrap().queue.as_ref().unwrap().len(), 1);
            assert_eq!(
                state.batch.as_ref().unwrap().echoes[0].input.as_json(),
                "\"KEEP_PRIVATE_INPUT\""
            );
            assert_eq!(controls.steering_bytes.load(Ordering::Acquire), 128);
        }
        controls.fail_automatic_admission(Arc::clone(&target)).unwrap();
        // Admission closes before the deferred task drains the accepted record.
        assert_eq!(target.state.lock().unwrap().phase, SteeringPhase::Closed);
        assert!(target.state.lock().unwrap().phase.require_ready().is_err());
        assert!(target.automatic_request().unwrap().is_none());
        let completion = controls.tasks.join_next().await.unwrap();
        assert!(controls.finish(completion));
        let failure = receive.recv().await.unwrap();
        let wire: serde_json::Value = agentic_core::utils::common::deserialize_from_str(&failure.0).unwrap();
        assert_eq!(wire["error"]["code"], "successor_creation_failed");
        assert_eq!(wire["steer"]["input"], "KEEP_PRIVATE_INPUT");
        target.mark_ready("resp_parent").unwrap();
        assert_eq!(target.state.lock().unwrap().phase, SteeringPhase::Closed);
        assert_eq!(controls.steering_bytes.load(Ordering::Acquire), 0);
        assert_eq!(controls.steering_settings_bytes.load(Ordering::Acquire), 0);
        assert!(target.commit_successor().is_err());
        assert!(target.automatic_request().unwrap().is_none());
        controls.shutdown().await;
    }

    #[tokio::test]
    async fn failed_automatic_admission_closes_before_its_worker_is_polled() {
        let (target, mut controls, _receive) = target_fixture();
        let target = Arc::new(target);
        target.mark_ready("resp_parent").unwrap();
        controls.fail_automatic_admission(Arc::clone(&target)).unwrap();
        // No await has given the deferred failure task a chance to own cleanup.
        // The reader has already prevented a second automatic/prepared claimant.
        assert_eq!(target.state.lock().unwrap().phase, SteeringPhase::Closed);
        assert!(target.state.lock().unwrap().phase.require_ready().is_err());
        assert!(target.automatic_request().unwrap().is_none());
        let completion = controls.tasks.join_next().await.unwrap();
        assert!(controls.finish(completion));
        {
            let mut state = target.state.lock().unwrap();
            state.phase = SteeringPhase::Reserved;
        }
        // A stale admission failure cannot resolve a producer-owned reservation.
        controls.fail_automatic_admission(Arc::clone(&target)).unwrap();
        assert_eq!(target.state.lock().unwrap().phase, SteeringPhase::Reserved);
        assert!(controls.tasks.is_empty());
        for _ in 0..MAX_CONTROLS {
            controls.tasks.spawn(std::future::pending());
        }
        assert!(matches!(
            controls.fail_automatic_admission(Arc::clone(&target)),
            Err(WsError::TooManyRequests)
        ));
        assert_eq!(target.state.lock().unwrap().phase, SteeringPhase::Reserved);
        controls.shutdown().await;
    }

    #[test]
    fn retained_steering_bytes_are_shared_and_refunded_once() {
        let budget = Arc::new(AtomicUsize::new(0));
        let first = SteeringByteCharge::reserve(&budget, MAX_CONTROL_BYTES - 2).unwrap();
        let second = SteeringByteCharge::reserve(&budget, 2).unwrap();
        assert!(SteeringByteCharge::reserve(&budget, 1).is_none());
        drop(second);
        assert_eq!(budget.load(Ordering::Acquire), MAX_CONTROL_BYTES - 2);
        assert!(SteeringByteCharge::reserve(&budget, usize::MAX).is_none());
        drop(first);
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn steering_outcomes_are_ordered_and_barrier_waits_for_relay() {
        let (outbound, mut receive) = mpsc::channel(8);
        let mut controls = Controls::new(outbound, WsEventLimit(4096));
        let (sink, events) = ResponseEventSink::channel(4096);
        controls.spawn_relay(events, None);
        let outcomes = controls.spawn_steering_worker(sink);
        for id in ["steer_first", "steer_second"] {
            outcomes
                .send(SteeringOutcome::Event {
                    event: SteeringEvent::Accepted {
                        steer: AcceptedSteer {
                            id: id.into(),
                            previous_response_id: "resp_parent".into(),
                        },
                    },
                    charge: None,
                })
                .await
                .unwrap();
        }
        let (reply, barrier) = oneshot::channel();
        outcomes.send(SteeringOutcome::Barrier(reply)).await.unwrap();
        barrier.await.unwrap();
        for (number, id) in ["steer_first", "steer_second"].into_iter().enumerate() {
            let wire = receive.recv().await.unwrap();
            let event: serde_json::Value = agentic_core::utils::common::deserialize_from_str(&wire.0).unwrap();
            assert_eq!(event["steer"]["id"], id);
            assert_eq!(event["sequence_number"], number);
        }
        controls.shutdown().await;
        assert!(controls.steering_workers.is_empty());
        assert!(controls.relays.is_empty());
    }
}
