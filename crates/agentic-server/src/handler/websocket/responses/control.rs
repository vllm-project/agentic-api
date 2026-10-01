//! Connection-owned routes and bounded tasks. No agent history lives here.
use super::{StreamId, WsError, WsEventLimit, WsOutboundEvent, stream_ws_response};
use crate::auth::AuthenticatedPrincipal;
use agentic_core::{
    executor::{
        BoxStream, ExecutorError, ResponseRunOwner, RunningResponse,
        multi_agent::{
            ClientCallError, RunControl,
            control::{ControlAdmissionError, OutputDecision},
        },
        response_events::{ResponseCommitState, ResponseEventSink},
    },
    types::{
        client_calls::{ClientToolOutput, ClientToolOutputBatch},
        injection::{InjectionEvent, InjectionFailure, InjectionFailureCode, ResponseInjectRequest},
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
    pub response: RunningResponse,
    pub lane: Option<StreamId>,
    pub reply: oneshot::Sender<ResponseRunOwner>,
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
    pub result: Result<(), WsError>,
}

pub(super) struct Controls {
    routes: HashMap<String, RetainedRoute>,
    pub registrations: mpsc::Receiver<Registration>,
    pub register: mpsc::Sender<Registration>,
    pub tasks: JoinSet<ControlCompletion>,
    pub relays: JoinSet<Result<(), WsError>>,
    bytes: usize,
    rejected_owner: Option<ResponseRunOwner>,
    outbound: mpsc::Sender<WsOutboundEvent>,
    limit: WsEventLimit,
    // Unknown targets have no response sequence owner. Their connection-level
    // failures use the same relay implementation, separate from response routes.
    unrouted: ResponseEventSink,
}
impl Controls {
    pub fn new(outbound: mpsc::Sender<WsOutboundEvent>, limit: WsEventLimit) -> Self {
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
    pub fn cancel_live_controls(&self) {
        for entry in self.routes.values() {
            if entry.route.control.is_some() {
                entry.route.cancellation.cancel();
            }
        }
    }

    pub fn expire(&mut self) {
        let now = Instant::now();
        self.routes.retain(|_, entry| {
            if entry.route.sink.commit_state() != ResponseCommitState::Running {
                let completed = entry.completed.get_or_insert(now);
                return Arc::strong_count(&entry.route) > 1 || now.duration_since(*completed) < RETAIN_COMPLETED;
            }
            true
        });
    }
    pub fn register(&mut self, registration: Registration) -> Result<(), WsError> {
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
    pub fn dispatch(
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
    pub fn finish(&mut self, completion: Result<ControlCompletion, tokio::task::JoinError>) -> bool {
        match completion {
            Ok(completion) => {
                self.bytes -= completion.bytes;
                completion.result.is_ok()
            }
            Err(_) => false,
        }
    }
    pub async fn shutdown(&mut self) {
        self.registrations.close();
        if let Some(mut owner) = self.rejected_owner.take() {
            let _ = owner.join().await;
        }
        while let Ok(registration) = self.registrations.try_recv() {
            let mut owner = registration.response.owner;
            owner.cancel();
            let _ = owner.join().await;
        }
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.routes.clear();
        self.relays.abort_all();
        while self.relays.join_next().await.is_some() {}
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
    if route.sink.commit_state() == ResponseCommitState::Committed {
        return route
            .sink
            .emit_local(failed(request, InjectionFailureCode::ResponseAlreadyCompleted))
            .await
            .map_err(WsError::from);
    }
    let Some(control) = &route.control else {
        return Err(WsError::from(ExecutorError::InvalidRequest(
            "response does not accept multi-agent injection".into(),
        )));
    };
    let batch = ClientToolOutputBatch {
        response_id: request.response_id.clone(),
        outputs: request.input.iter().cloned().map(ClientToolOutput::from).collect(),
    };
    let decision = match control.try_submit_outputs(batch) {
        Ok(submission) => Some(submission.decision().await.map_err(|_| WsError::SendFailed)?),
        Err(rejected) if rejected.reason == ControlAdmissionError::Closed => None,
        Err(_) => return Err(WsError::TooManyRequests),
    };
    match decision {
        Some(OutputDecision::Accepted) => {
            route
                .sink
                .emit_local(InjectionEvent::Created {
                    response_id: request.response_id,
                })
                .await?;
        }
        Some(OutputDecision::Rejected(rejected)) => {
            let message = match rejected.reason {
                ClientCallError::AlreadyResolved(call_id) | ClientCallError::DuplicateCall(call_id) => {
                    format!("Tool call '{call_id}' already has an output.")
                }
                ClientCallError::UnknownCall(call_id) => {
                    format!(
                        "Tool call '{call_id}' is not pending on response '{}'.",
                        request.response_id
                    )
                }
                reason => return Err(WsError::from(ExecutorError::InvalidRequest(reason.to_string()))),
            };
            route
                .sink
                .emit_local(InjectionEvent::Failed {
                    response_id: request.response_id,
                    input: request.input,
                    error: InjectionFailure {
                        code: InjectionFailureCode::InvalidInput,
                        message,
                    },
                })
                .await?;
        }
        Some(OutputDecision::Finalizing(_)) | None => {
            route.sink.wait_committed().await?;
            route
                .sink
                .emit_local(failed(request, InjectionFailureCode::ResponseAlreadyCompleted))
                .await?;
        }
    }
    Ok(())
}
