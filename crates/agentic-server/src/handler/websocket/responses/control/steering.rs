//! Steering admission, pending outcomes, and successor handoff on retained routes.
use super::super::{StreamId, WsError};
use super::{
    ControlCompletion, Controls, MAX_CONTROL_BYTES, MAX_CONTROLS, SteerEcho, SteeringByteCharge, SteeringOutcome,
    SteeringPhase, SteeringState, SteeringTarget,
};
use agentic_core::{
    executor::{
        RunningResponse,
        session::{ResponseCheckpointPin, SteeringQueue},
    },
    types::{
        request_response::RequestPayload,
        websocket::{
            FailedSteer, SteeringEvent, SteeringFailure, SteeringFailureCode, SteeringInputEcho, SteeringPendingReason,
        },
    },
};
use std::sync::Arc;
use tokio::sync::oneshot;

impl SteeringTarget {
    fn rejection_code(&self, state: &SteeringState, valid_input: bool, active: bool) -> Option<SteeringFailureCode> {
        if self.sink.created_receipt().is_none() {
            Some(SteeringFailureCode::ResponseNotFound)
        } else if !valid_input {
            Some(SteeringFailureCode::InvalidInput)
        } else if self.settings.multi_agent.as_ref().is_some_and(|config| config.enabled)
            || !self.supported
            || !state.required_supported
            || self
                .settings
                .context_management
                .as_ref()
                .is_some_and(|settings| !settings.is_empty())
        {
            Some(SteeringFailureCode::SteeringNotSupported)
        } else if matches!(state.phase, SteeringPhase::Reserved | SteeringPhase::Closed) || !active {
            Some(SteeringFailureCode::ResponseNotActive)
        } else if state.phase == SteeringPhase::Ready && state.queue.is_none() && state.pin.is_none() {
            Some(SteeringFailureCode::ResponseAlreadyCompleted)
        } else {
            None
        }
    }

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
                let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
                state.phase.require_ready()?;
                if state.pin.is_none() {
                    state.pin = self
                        .session
                        .pin_response(payload.previous_response_id.as_deref().ok_or(WsError::SendFailed)?)?;
                }
                let pin = state.pin.as_ref().ok_or_else(|| {
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
                match state.queue.as_ref() {
                    Some(queue) if !queue.is_empty() => builder.with_steering(queue)?,
                    _ => builder,
                }
            };
            let prepared = builder.prepare_retained().await?;
            let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
            // A failure callback may close this target while preparation awaits.
            // Never start the copied batch after its ownership was resolved.
            state.phase.require_ready()?;
            let Some(queue) = state.queue.take() else {
                return Ok((prepared.start_gated(), None));
            };
            match queue.reserve_successor(prepared) {
                Ok(reservation) => {
                    state.phase = SteeringPhase::Reserved;
                    let (response, publication) = reservation.start_gated();
                    return Ok((response, Some(publication)));
                }
                Err(rejected) => {
                    let (queue, prepared) = *rejected;
                    state.queue = Some(queue);
                    drop(prepared);
                }
            }
        }
    }

    pub(in super::super) fn commit_successor(&self) -> Result<(), WsError> {
        let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        match state.phase {
            SteeringPhase::Reserved => state.phase = SteeringPhase::Closed,
            SteeringPhase::Running | SteeringPhase::Ready | SteeringPhase::Closed => {
                return Err(WsError::from(agentic_core::executor::ExecutorError::InvalidRequest(
                    "steering successor has no outstanding reservation".into(),
                )));
            }
        }
        state.echoes.clear();
        state.pin = None;
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
            match state.phase {
                SteeringPhase::Closed if state.echoes.is_empty() => return Ok(()),
                SteeringPhase::Running | SteeringPhase::Ready | SteeringPhase::Reserved | SteeringPhase::Closed => {
                    state.phase = SteeringPhase::Closed;
                }
            }
            state.pin = None;
            state.queue = None;
            std::mem::take(&mut state.echoes)
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
        self.state.lock().map_or(true, |state| !state.echoes.is_empty())
    }

    pub(in super::super) fn has_pending_steers(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.queue.as_ref().is_some_and(|queue| !queue.is_empty()))
    }

    pub(in super::super) fn missing_parent_for_steers(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| !state.echoes.is_empty() && state.pin.is_none())
    }

    pub(in super::super) fn mark_ready(&self, response_id: &str) -> Result<(), WsError> {
        let pin = self.session.pin_response(response_id)?;
        let mut state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        match state.phase {
            SteeringPhase::Running => state.phase = SteeringPhase::Ready,
            SteeringPhase::Ready | SteeringPhase::Reserved | SteeringPhase::Closed => return Ok(()),
        }
        if state.pin.is_none() {
            state.pin = pin;
        }
        let required = if let Ok(required) = state
            .pin
            .as_ref()
            .map(ResponseCheckpointPin::required_steering_input)
            .transpose()
        {
            required.unwrap_or_default()
        } else {
            // Steering qualification must not change ordinary client-call continuation.
            state.required_supported = false;
            state.pin = None;
            Vec::new()
        };
        if required.is_empty() && state.queue.is_none() {
            state.pin = None;
        }
        if !required.is_empty() {
            for echo in &state.echoes {
                self.outcomes
                    .try_send(SteeringOutcome::Event {
                        event: SteeringEvent::Pending {
                            steer: echo.steer.clone(),
                            reason: SteeringPendingReason::WaitingForRequiredInput,
                            required_input: required.clone(),
                        },
                        charge: None,
                    })
                    .map_err(|_| WsError::SendFailed)?;
            }
        }
        Ok(())
    }

    pub(in super::super) fn automatic_request(self: &Arc<Self>) -> Result<Option<super::super::WsRequest>, WsError> {
        let state = self.state.lock().map_err(|_| WsError::SendFailed)?;
        if state.phase != SteeringPhase::Ready || state.queue.as_ref().is_none_or(SteeringQueue::is_empty) {
            return Ok(None);
        }
        let Some(pin) = &state.pin else {
            return Ok(None);
        };
        if !pin.required_steering_input()?.is_empty() {
            return Ok(None);
        }
        let mut payload = self.settings.clone();
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
    fn reject_unrouted_steering(
        &mut self,
        target: Option<String>,
        input: SteeringInputEcho,
        charge: SteeringByteCharge,
    ) -> Result<(), WsError> {
        if self.tasks.len() >= MAX_CONTROLS {
            return Err(WsError::TooManyRequests);
        }
        let sink = self.unrouted.clone();
        let event = SteeringOutcome::failed(None, target, input, SteeringFailureCode::ResponseNotFound);
        self.tasks.spawn(async move {
            let result = async {
                sink.emit_steering(event).await?;
                sink.flush().await?;
                Ok(())
            }
            .await;
            drop(charge);
            ControlCompletion { bytes: 0, result }
        });
        Ok(())
    }

    pub(in super::super) fn dispatch_steering(&mut self, text: &str) -> Result<(), WsError> {
        let submission = super::super::request::SteeringSubmission::parse(text, self.limit.bytes())?;
        let charge = SteeringByteCharge::reserve(
            &self.steering_bytes,
            submission.echo.as_json().len().saturating_add(256),
        )
        .ok_or(WsError::TooManyRequests)?;
        let super::super::request::SteeringSubmission {
            previous_response_id: target_id,
            echo: input_echo,
            input: parsed,
        } = submission;
        let route = target_id.as_deref().and_then(|id| self.steering_target(id));
        let Some(target) = route else {
            return self.reject_unrouted_steering(target_id, input_echo, charge);
        };
        let mut state = target.state.lock().map_err(|_| WsError::SendFailed)?;
        if state.phase == SteeringPhase::Ready && state.pin.is_none() && state.required_supported {
            state.pin = target
                .session
                .pin_response(target_id.as_deref().ok_or(WsError::SendFailed)?)?;
        }
        let code = target.rejection_code(&state, parsed.is_some(), self.is_active_steering_target(&target));
        if let Some(code) = code {
            return target
                .outcomes
                .try_send(SteeringOutcome::Event {
                    event: SteeringOutcome::failed(None, target_id, input_echo, code),
                    charge: Some(charge),
                })
                .map_err(|_| WsError::SendFailed);
        }
        if state.queue.is_none() {
            state.queue = Some(SteeringQueue::new(
                target_id.clone().ok_or(WsError::SendFailed)?,
                std::num::NonZeroUsize::new(MAX_CONTROLS).expect("positive steering count"),
                std::num::NonZeroUsize::new(MAX_CONTROL_BYTES).expect("positive steering bytes"),
            )?);
        }
        let descriptor = state
            .queue
            .as_mut()
            .ok_or(WsError::SendFailed)?
            .try_accept(parsed.ok_or(WsError::SendFailed)?)
            .cloned();
        match descriptor {
            Ok(steer) => {
                target
                    .outcomes
                    .try_send(SteeringOutcome::Event {
                        event: SteeringEvent::Accepted { steer: steer.clone() },
                        charge: None,
                    })
                    .map_err(|_| WsError::SendFailed)?;
                if state.phase == SteeringPhase::Ready {
                    let required = state
                        .pin
                        .as_ref()
                        .map(ResponseCheckpointPin::required_steering_input)
                        .transpose()?
                        .unwrap_or_default();
                    if !required.is_empty() {
                        target
                            .outcomes
                            .try_send(SteeringOutcome::Event {
                                event: SteeringEvent::Pending {
                                    steer: steer.clone(),
                                    reason: SteeringPendingReason::WaitingForRequiredInput,
                                    required_input: required,
                                },
                                charge: None,
                            })
                            .map_err(|_| WsError::SendFailed)?;
                    }
                }
                state.echoes.push(SteerEcho {
                    steer,
                    input: input_echo,
                    charge,
                });
                target.sink.request_steering_boundary();
                Ok(())
            }
            Err(_) => target
                .outcomes
                .try_send(SteeringOutcome::Event {
                    event: SteeringOutcome::failed(
                        None,
                        target_id,
                        input_echo,
                        SteeringFailureCode::TooManyPendingSteers,
                    ),
                    charge: Some(charge),
                })
                .map_err(|_| WsError::SendFailed),
        }
    }

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

    fn is_active_steering_target(&self, target: &Arc<SteeringTarget>) -> bool {
        // Lane execution is FIFO, so registration order also orders published
        // responses. Read the canonical creation receipt directly: the relay
        // may have delivered it before its execution task wakes to observe it.
        self.routes
            .values()
            .filter(|entry| entry.route.lane == target.lane && entry.route.sink.created_receipt().is_some())
            .max_by_key(|entry| entry.ordinal)
            .is_some_and(|entry| Arc::ptr_eq(&entry.route.steering, target))
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
            if route.route.lane.as_ref() == lane {
                if let Some(request) = route.route.steering.automatic_request()? {
                    return Ok(Some(request));
                }
            }
        }
        Ok(None)
    }
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
