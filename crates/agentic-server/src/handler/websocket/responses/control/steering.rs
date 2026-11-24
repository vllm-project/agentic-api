//! Accepted-batch lifecycle, pending outcomes, and successor handoff.
mod admission;
use super::super::{StreamId, WsError};
use super::{
    ControlCompletion, Controls, MAX_CONTROL_BYTES, MAX_CONTROLS, SteeringBatch, SteeringByteCharge, SteeringOutcome,
    SteeringPhase, SteeringSettings, SteeringState, SteeringTarget,
};
use agentic_core::{
    executor::{RunningResponse, session::ResponseCheckpointPin},
    types::{
        request_response::RequestPayload,
        websocket::{AcceptedSteer, SteeringEvent, SteeringFailureCode, SteeringPendingReason, SteeringRequiredInput},
    },
    utils::common::serialized_size_up_to,
};
use std::sync::{Arc, atomic::AtomicUsize};
use tokio::sync::oneshot;

impl SteeringState {
    pub(super) fn new(settings: RequestPayload, supported: bool, budget: &Arc<AtomicUsize>) -> Result<Self, WsError> {
        let supported = supported
            && !settings.multi_agent.as_ref().is_some_and(|config| config.enabled)
            && settings.context_management.as_ref().is_none_or(Vec::is_empty);
        let settings = if supported {
            let fixed_bytes = std::mem::size_of::<SteeringTarget>();
            serialized_size_up_to(&settings, MAX_CONTROL_BYTES.saturating_sub(fixed_bytes))
                .map_err(WsError::SerializeJson)?
                .and_then(|bytes| SteeringByteCharge::reserve(budget, bytes.saturating_add(fixed_bytes)))
                .map(|charge| SteeringSettings {
                    payload: settings,
                    _charge: charge,
                })
        } else {
            None
        };
        Ok(Self {
            settings,
            supported,
            batch: None,
            phase: SteeringPhase::Running,
            required_supported: true,
        })
    }

    /// Close admission and transfer the whole accepted batch to its final owner.
    fn close(&mut self) -> Option<SteeringBatch> {
        self.phase = SteeringPhase::Closed;
        self.settings = None;
        self.batch.take()
    }
}

impl SteeringTarget {
    pub(in super::super) async fn prepare_successor(
        &self,
        payload: &RequestPayload,
        execution_context: Arc<agentic_core::executor::ExecutionContext>,
        auth: Option<&str>,
        max_event_bytes: usize,
        span: agentic_core::executor::telemetry::ExecutionSpan,
    ) -> Result<
        (
            RunningResponse,
            Option<agentic_core::executor::session::SteeringSuccessorPublication>,
        ),
        WsError,
    > {
        let mut span = Some(span);
        loop {
            let builder = {
                let state = self.state.lock().map_err(|_| WsError::SendFailed)?;
                state.phase.require_ready()?;
                let batch = state.batch.as_ref().ok_or(WsError::SendFailed)?;
                let pin = batch.pin.as_ref().ok_or_else(|| {
                    WsError::from(agentic_core::executor::ExecutorError::InvalidRequest(
                        "steering parent checkpoint is unavailable".into(),
                    ))
                })?;
                let mut builder =
                    agentic_core::executor::ExecuteRequest::new(payload.clone(), Arc::clone(&execution_context))
                        .with_auth(auth.map(str::to_owned))
                        .with_session(&self.session, Some(pin))?
                        .with_max_stream_event_bytes(max_event_bytes);
                if let Some(span) = span.take() {
                    builder = builder.with_execution_span(span);
                }
                builder.with_steering(batch.queue.as_ref().ok_or(WsError::SendFailed)?)?
            };
            let prepared = builder.prepare_retained().await?;
            let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
            // A failure callback may close this target while preparation awaits.
            // Never start the copied batch after its ownership was resolved.
            state.phase.require_ready()?;
            let batch = state.batch.as_mut().ok_or(WsError::SendFailed)?;
            let queue = batch.queue.take().ok_or(WsError::SendFailed)?;
            match queue.reserve_successor(prepared) {
                Ok(reservation) => {
                    state.phase = SteeringPhase::Reserved;
                    let (response, publication) = reservation.start_gated();
                    return Ok((response, Some(publication)));
                }
                Err(rejected) => {
                    let (queue, prepared) = *rejected;
                    batch.queue = Some(queue);
                    drop(prepared);
                }
            }
        }
    }

    pub(in super::super) fn commit_successor(&self) -> Result<(), WsError> {
        let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        match state.phase {
            SteeringPhase::Reserved => drop(state.close()),
            SteeringPhase::Running | SteeringPhase::Ready | SteeringPhase::Closed => {
                return Err(WsError::from(agentic_core::executor::ExecutorError::InvalidRequest(
                    "steering successor has no outstanding reservation".into(),
                )));
            }
        }
        Ok(())
    }

    pub(in super::super) async fn finish_response(&self) -> Result<(), WsError> {
        self.mark_ready(
            self.sink
                .created_receipt()
                .as_ref()
                .map_or("", |receipt| receipt.response_id()),
        )?;
        if self.missing_parent_for_steers() {
            self.fail_successor().await?;
        }
        Ok(())
    }

    pub(in super::super) async fn flush_outcomes(&self) -> Result<(), WsError> {
        let (reply, receive) = oneshot::channel();
        self.outcomes
            .send(SteeringOutcome::Barrier(reply))
            .await
            .map_err(|_| WsError::SendFailed)?;
        receive.await.map_err(|_| WsError::SendFailed)
    }

    fn close_unprepared_successor(&self) -> Result<bool, WsError> {
        let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        match state.phase {
            SteeringPhase::Ready => {
                // Close admission synchronously before a deferred failure worker
                // can race another request claiming this same accepted batch.
                state.phase = SteeringPhase::Closed;
                Ok(true)
            }
            SteeringPhase::Running | SteeringPhase::Reserved | SteeringPhase::Closed => Ok(false),
        }
    }

    pub(in super::super) async fn fail_successor(&self) -> Result<(), WsError> {
        let echoes = {
            let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
            let Some(batch) = state.close() else {
                return Ok(());
            };
            batch.echoes
        };
        for echo in echoes {
            self.outcomes
                .send(SteeringOutcome::Event {
                    event: SteeringOutcome::failed(
                        Some(echo.steer.id),
                        Some(echo.steer.previous_response_id),
                        echo.input,
                        SteeringFailureCode::SuccessorCreationFailed,
                    ),
                    charge: Some(echo.charge),
                })
                .await
                .map_err(|_| WsError::SendFailed)?;
        }
        self.flush_outcomes().await
    }

    pub(super) fn retains_accepted(&self) -> bool {
        self.state.lock().map_or(true, |state| state.batch.is_some())
    }

    pub(in super::super) fn has_pending_steers(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state
                .batch
                .as_ref()
                .and_then(|batch| batch.queue.as_ref())
                .is_some_and(|queue| !queue.is_empty())
        })
    }

    pub(in super::super) fn missing_parent_for_steers(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.batch.as_ref().is_some_and(|batch| batch.pin.is_none()))
    }

    pub(in super::super) fn mark_ready(&self, response_id: &str) -> Result<(), WsError> {
        let pin = self.session.pin_response(response_id)?;
        let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        match state.phase {
            SteeringPhase::Running => state.phase = SteeringPhase::Ready,
            SteeringPhase::Ready | SteeringPhase::Reserved | SteeringPhase::Closed => return Ok(()),
        }
        let Ok(required) = pin
            .as_ref()
            .map(ResponseCheckpointPin::required_steering_input)
            .transpose()
        else {
            // A malformed obligation cannot qualify a successor. Accepted
            // batches remain owned until finish_response reports failure.
            state.required_supported = false;
            return Ok(());
        };
        let required = required.unwrap_or_default();
        if let Some(batch) = state.batch.as_mut() {
            batch.pin = pin;
            for echo in &batch.echoes {
                self.notify_pending(&echo.steer, &required)?;
            }
        }
        Ok(())
    }

    fn notify_pending(&self, steer: &AcceptedSteer, required: &[SteeringRequiredInput]) -> Result<(), WsError> {
        if required.is_empty() {
            return Ok(());
        }
        self.outcomes
            .try_send(SteeringOutcome::Event {
                event: SteeringEvent::Pending {
                    steer: steer.clone(),
                    reason: SteeringPendingReason::WaitingForRequiredInput,
                    required_input: required.to_vec(),
                },
                charge: None,
            })
            .map_err(|_| WsError::SendFailed)
    }

    pub(in super::super) fn automatic_request(self: &Arc<Self>) -> Result<Option<super::super::WsRequest>, WsError> {
        let state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        if state.phase != SteeringPhase::Ready {
            return Ok(None);
        }
        let Some(pin) = state.batch.as_ref().and_then(|batch| batch.pin.as_ref()) else {
            return Ok(None);
        };
        if !pin.required_steering_input()?.is_empty() {
            return Ok(None);
        }
        let mut payload = state.settings.as_ref().ok_or(WsError::SendFailed)?.payload.clone();
        payload.input = agentic_core::types::io::ResponsesInput::Items(Vec::new());
        payload.previous_response_id = Some(pin.response_id().to_owned());
        payload.stream = true;
        Ok(Some(super::super::WsRequest {
            payload,
            stream_id: self.lane.clone(),
            generate: None,
            execution: None,
            steering: Some(Arc::clone(self)),
            automatic: true,
        }))
    }
}

impl Controls {
    pub(in super::super) fn fail_automatic_admission(&mut self, target: Arc<SteeringTarget>) -> Result<(), WsError> {
        if self.tasks.len() >= MAX_CONTROLS {
            return Err(WsError::TooManyRequests);
        }
        if !target.close_unprepared_successor()? {
            return Ok(());
        }
        self.tasks.spawn(async move {
            ControlCompletion {
                bytes: 0,
                result: target.fail_successor().await,
            }
        });
        Ok(())
    }

    /// Registration may precede (or fail before) publication. Only a newer
    /// creation receipt retires a target. Accepted batches keep their settings
    /// until commitment/failure, including while their producer owns a reservation.
    pub(super) fn release_inactive_settings(&self) {
        for entry in self.routes.values() {
            if entry.route.sink.created_receipt().is_some()
                && !self.is_active_steering_target(&entry.route.steering)
                && let Ok(mut state) = entry.route.steering.state.lock()
                && state.batch.is_none()
            {
                state.settings = None;
            }
        }
    }

    fn is_active_steering_target(&self, target: &SteeringTarget) -> bool {
        // Lane execution is FIFO, so registration order also orders published
        // responses. Read the canonical creation receipt directly: the relay
        // may have delivered it before its execution task wakes to observe it.
        self.routes
            .values()
            .filter(|entry| entry.route.lane == target.lane && entry.route.sink.created_receipt().is_some())
            .max_by_key(|entry| entry.ordinal)
            .is_some_and(|entry| std::ptr::eq(entry.route.steering.as_ref(), target))
    }

    pub(in super::super) fn steering_target(&self, response_id: &str) -> Option<Arc<SteeringTarget>> {
        self.routes
            .get(response_id)
            .map(|route| Arc::clone(&route.route.steering))
    }

    pub(in super::super) fn automatic_request(
        &self,
        lane: Option<&StreamId>,
    ) -> Result<Option<super::super::WsRequest>, WsError> {
        for route in self.routes.values() {
            if route.route.lane.as_ref() == lane
                && let Some(request) = route.route.steering.automatic_request()?
            {
                return Ok(Some(request));
            }
        }
        Ok(None)
    }
}
