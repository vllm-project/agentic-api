//! Live client tool-output injection through the existing run control channel.
use super::{Route, WsError};
use agentic_core::{
    executor::{
        multi_agent::{
            ClientCallError,
            control::{ControlAdmissionError, OutputDecision},
        },
        response_events::{ResponseCommitState, ResponseEventSink},
    },
    types::{
        client_calls::ClientToolOutputBatch,
        injection::{InjectionEvent, InjectionFailure, InjectionFailureCode, InjectionInput, ResponseInjectRequest},
    },
};
use std::sync::Arc;

fn failed(request: ResponseInjectRequest, code: InjectionFailureCode) -> InjectionEvent {
    let message = match code {
        InjectionFailureCode::ResponseNotFound => format!("Response '{}' was not found.", request.response_id),
        InjectionFailureCode::ResponseAlreadyCompleted => {
            format!("Response '{}' has already completed.", request.response_id)
        }
        InjectionFailureCode::InvalidInput => "Invalid injection input.".into(),
    };
    InjectionEvent::Failed {
        response_id: request.response_id,
        input: request.input,
        error: InjectionFailure { code, message },
    }
}

pub(super) async fn inject(
    route: Option<Arc<Route>>,
    unrouted: ResponseEventSink,
    request: ResponseInjectRequest,
) -> Result<(), WsError> {
    let Some(route) = route else {
        return unrouted
            .emit_local(failed(request, InjectionFailureCode::ResponseNotFound))
            .await
            .map_err(WsError::from);
    };
    match route.sink.commit_state() {
        ResponseCommitState::Running => {}
        ResponseCommitState::Committed | ResponseCommitState::Failed | ResponseCommitState::Aborted => {
            return route
                .sink
                .emit_local(failed(request, InjectionFailureCode::ResponseAlreadyCompleted))
                .await
                .map_err(WsError::from);
        }
    }
    let Some(control) = &route.control else {
        return route
            .sink
            .emit_local(failed(request, InjectionFailureCode::InvalidInput))
            .await
            .map_err(WsError::from);
    };
    let batch = ClientToolOutputBatch {
        response_id: request.response_id,
        outputs: request.input.into_iter().map(Into::into).collect(),
    };
    let response_id = batch.response_id.clone();
    let decision = match control.try_submit_outputs(batch) {
        Ok(submission) => submission.decision().await.map_err(|_| WsError::SendFailed)?,
        Err(rejected) => match rejected.reason {
            ControlAdmissionError::Closed => OutputDecision::Finalizing(rejected.input),
            ControlAdmissionError::Full | ControlAdmissionError::TooLarge => return Err(WsError::TooManyRequests),
        },
    };
    match decision {
        OutputDecision::Accepted => {
            route.sink.emit_local(InjectionEvent::Created { response_id }).await?;
        }
        OutputDecision::Rejected(rejected) => {
            let message = match rejected.reason {
                ClientCallError::AlreadyResolved(call_id) | ClientCallError::DuplicateCall(call_id) => {
                    format!("Tool call '{call_id}' already has an output.")
                }
                ClientCallError::UnknownCall(call_id) => {
                    format!("Tool call '{call_id}' is not pending on response '{response_id}'.")
                }
                reason @ (ClientCallError::EmptyResponseId
                | ClientCallError::WrongResponse
                | ClientCallError::EmptyCallId
                | ClientCallError::CallLimit
                | ClientCallError::BatchLimit
                | ClientCallError::InvalidOwner
                | ClientCallError::KindMismatch { .. }
                | ClientCallError::Budget(_)) => reason.to_string(),
            };
            route
                .sink
                .emit_local(InjectionEvent::Failed {
                    response_id: rejected.input.response_id,
                    input: rejected.input.outputs.into_iter().map(InjectionInput::from).collect(),
                    error: InjectionFailure {
                        code: InjectionFailureCode::InvalidInput,
                        message,
                    },
                })
                .await?;
        }
        OutputDecision::Finalizing(input) => {
            // Failed/aborted execution also terminates input admission. The
            // acknowledgement still uses the retained response relay.
            route.sink.wait_finished().await?;
            route
                .sink
                .emit_local(failed(
                    ResponseInjectRequest {
                        response_id: input.response_id,
                        input: input.outputs.into_iter().map(InjectionInput::from).collect(),
                    },
                    InjectionFailureCode::ResponseAlreadyCompleted,
                ))
                .await?;
        }
    }
    Ok(())
}
