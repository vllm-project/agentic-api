//! Connection-owned routes and bounded tasks. No agent history lives here.
use super::{StreamId, WsError, WsEventLimit, WsOutboundEvent, stream_ws_response};
use crate::auth::AuthenticatedPrincipal;
use agentic_core::{
    executor::{
        BoxStream, ResponseRunOwner, RunningResponse,
        multi_agent::{
            ClientCallError, RunControl,
            control::{ControlAdmissionError, OutputDecision},
        },
        response_events::{ResponseCommitState, ResponseEventSink},
    },
    types::{
        client_calls::ClientToolOutputBatch,
        injection::{InjectionEvent, InjectionFailure, InjectionFailureCode, InjectionInput, ResponseInjectRequest},
    },
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const MAX_CONTROLS: usize = 32;
const MAX_CONTROL_BYTES: usize = 12 * 1024 * 1024;
const MAX_ROUTES: usize = 128;
const RETAIN_COMPLETED: Duration = Duration::from_secs(60);

pub(super) struct Registration {
    pub(super) response: RunningResponse,
    pub(super) lane: Option<StreamId>,
    pub(super) reply: oneshot::Sender<ResponseRunOwner>,
}
struct Route {
    cancellation: CancellationToken,
    control: Option<RunControl>,
    sink: ResponseEventSink,
    lane: Option<StreamId>,
}
struct RetainedRoute {
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
    pub(super) fn cancel_live_controls(&self) {
        for entry in self.routes.values() {
            if entry.route.control.is_some() {
                entry.route.cancellation.cancel();
            }
        }
    }

    pub(super) fn expire(&mut self) {
        let now = Instant::now();
        self.routes.retain(|_, entry| {
            if entry.route.sink.commit_state() != ResponseCommitState::Running {
                let completed = entry.completed.get_or_insert(now);
                return Arc::strong_count(&entry.route) > 1 || now.duration_since(*completed) < RETAIN_COMPLETED;
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
                .filter(|(_, entry)| entry.completed.is_some() && Arc::strong_count(&entry.route) == 1)
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
        let Registration { response, lane, reply } = registration;
        let RunningResponse {
            response_id,
            control,
            events,
            sink,
            owner,
        } = response;
        let cancellation = owner.cancellation_token();
        self.routes.insert(
            response_id,
            RetainedRoute {
                route: Arc::new(Route {
                    cancellation,
                    control,
                    sink,
                    lane: lane.clone(),
                }),
                completed: None,
            },
        );
        self.spawn_relay(events, lane);
        if let Err(owner) = reply.send(owner) {
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
        if let Some(mut owner) = self.rejected_owner.take() {
            if let Err(error) = owner.join().await {
                tracing::debug!(%error, "responses websocket execution ended during disposal");
            }
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
            if let Err(error) = completion {
                if !error.is_cancelled() {
                    tracing::warn!(%error, "responses websocket control task failed during disposal");
                }
            }
        }
        self.routes.clear();
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

async fn inject(
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
