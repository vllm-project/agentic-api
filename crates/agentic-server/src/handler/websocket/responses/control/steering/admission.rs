//! Decode, qualify, reserve, and acknowledge one connection-local steering update.
use super::super::super::{WsError, request::SteeringSubmission};
use super::super::{
    ControlCompletion, Controls, MAX_CONTROL_BYTES, MAX_CONTROLS, SteerEcho, SteeringBatch, SteeringByteCharge,
    SteeringOutcome, SteeringPhase, SteeringState, SteeringTarget,
};
use agentic_core::{
    executor::session::{ResponseCheckpointPin, SteeringQueue},
    types::websocket::{AcceptedSteer, SteeringEvent, SteeringFailureCode},
};
use std::num::NonZeroUsize;

impl Controls {
    pub(in super::super::super) fn dispatch_steering(&mut self, text: &str) -> Result<(), WsError> {
        self.release_inactive_settings();
        let submission = SteeringSubmission::parse(text, self.limit.bytes())?;
        let target = submission
            .previous_response_id
            .as_deref()
            .and_then(|id| self.steering_target(id));
        let Some(target) = target else {
            return self.reject_steering(None, submission, SteeringFailureCode::ResponseNotFound);
        };
        self.accept_steering(&target, submission)
    }

    fn accept_steering(&mut self, target: &SteeringTarget, mut submission: SteeringSubmission) -> Result<(), WsError> {
        let mut state = target.state.lock().map_err(|_| WsError::SendFailed)?;
        if let Some(code) = target.rejection_code(
            &state,
            submission.input.is_some(),
            self.is_active_steering_target(target),
        ) {
            return self.reject_steering(Some(target), submission, code);
        }
        let Some(charge) = SteeringByteCharge::reserve(
            &self.steering_bytes,
            submission.echo.as_json().len().saturating_add(256),
        ) else {
            return self.reject_steering(Some(target), submission, SteeringFailureCode::TooManyPendingSteers);
        };
        if let Some(code) = target.reserve_batch(
            &mut state,
            submission.previous_response_id.as_deref().ok_or(WsError::SendFailed)?,
        )? {
            return self.reject_steering(Some(target), submission, code);
        }
        let batch = state.batch.as_mut().ok_or(WsError::SendFailed)?;
        let accepted = batch
            .queue
            .as_mut()
            .ok_or(WsError::SendFailed)?
            .try_accept(submission.input.take().ok_or(WsError::SendFailed)?)
            .cloned();
        let Ok(steer) = accepted else {
            // A rejected first update must not retain a parent pin.
            state
                .batch
                .take_if(|batch| batch.queue.as_ref().is_none_or(SteeringQueue::is_empty));
            return self.reject_steering(Some(target), submission, SteeringFailureCode::TooManyPendingSteers);
        };
        batch.echoes.push(SteerEcho {
            steer: steer.clone(),
            input: submission.echo,
            charge,
        });
        target.notify_accepted(batch, &steer)?;
        target.sink.request_steering_boundary();
        Ok(())
    }

    /// Rejection has its own bounded delivery allowance, independent of admission.
    fn reject_steering(
        &mut self,
        target: Option<&SteeringTarget>,
        submission: SteeringSubmission,
        code: SteeringFailureCode,
    ) -> Result<(), WsError> {
        let charge = SteeringByteCharge::reserve(
            &self.steering_rejection_bytes,
            submission.echo.as_json().len().saturating_add(256),
        )
        .ok_or(WsError::TooManyRequests)?;
        let event = SteeringOutcome::failed(None, submission.previous_response_id, submission.echo, code);
        if let Some(target) = target {
            return target
                .outcomes
                .try_send(SteeringOutcome::Event {
                    event,
                    charge: Some(charge),
                })
                .map_err(|_| WsError::SendFailed);
        }
        if self.tasks.len() >= MAX_CONTROLS {
            return Err(WsError::TooManyRequests);
        }
        let sink = self.unrouted.clone();
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
}

impl SteeringBatch {
    fn new(response_id: &str, pin: Option<ResponseCheckpointPin>) -> Result<Self, WsError> {
        let queue = SteeringQueue::new(
            response_id.to_owned(),
            NonZeroUsize::new(MAX_CONTROLS).expect("positive steering count"),
            NonZeroUsize::new(MAX_CONTROL_BYTES).expect("positive steering bytes"),
        )?;
        Ok(Self {
            queue: Some(queue),
            echoes: Vec::new(),
            pin,
        })
    }
}

impl SteeringTarget {
    /// Pin a completed parent only for the first accepted batch.
    /// Later updates share that ownership; unsteered responses never pin history.
    fn reserve_batch(
        &self,
        state: &mut SteeringState,
        response_id: &str,
    ) -> Result<Option<SteeringFailureCode>, WsError> {
        if state.batch.is_some() {
            return Ok(None);
        }
        let pin = match state.phase {
            SteeringPhase::Running => None,
            SteeringPhase::Ready => {
                let Some(pin) = self.session.pin_response(response_id)? else {
                    return Ok(Some(SteeringFailureCode::ResponseAlreadyCompleted));
                };
                Some(pin)
            }
            SteeringPhase::Reserved | SteeringPhase::Closed => return Ok(Some(SteeringFailureCode::ResponseNotActive)),
        };
        state.batch = Some(SteeringBatch::new(response_id, pin)?);
        Ok(None)
    }

    fn rejection_code(&self, state: &SteeringState, valid_input: bool, active: bool) -> Option<SteeringFailureCode> {
        if self.sink.created_receipt().is_none() {
            return Some(SteeringFailureCode::ResponseNotFound);
        }
        if !valid_input {
            return Some(SteeringFailureCode::InvalidInput);
        }
        if !state.supported || !state.required_supported {
            return Some(SteeringFailureCode::SteeringNotSupported);
        }
        if !active || state.settings.is_none() || matches!(state.phase, SteeringPhase::Reserved | SteeringPhase::Closed)
        {
            return Some(SteeringFailureCode::ResponseNotActive);
        }
        None
    }

    fn notify_accepted(&self, batch: &SteeringBatch, steer: &AcceptedSteer) -> Result<(), WsError> {
        self.outcomes
            .try_send(SteeringOutcome::Event {
                event: SteeringEvent::Accepted { steer: steer.clone() },
                charge: None,
            })
            .map_err(|_| WsError::SendFailed)?;
        if let Some(pin) = &batch.pin {
            self.notify_pending(steer, &pin.required_steering_input()?)?;
        }
        Ok(())
    }
}
