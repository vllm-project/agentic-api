//! Bounded admission and FIFO execution for independent response lanes.
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;

use agentic_core::executor::{ResponseSession, ResponseSessionGroup};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::control::Controls;
use super::{
    RequestExecution, StreamId, WsError, WsEventLimit, WsOutboundEvent, WsRequest, WsRequestParseError,
    handle_ws_request, queue_ws_error, queue_ws_json, telemetry, websocket_identity_error_event,
};
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;

const WS_MAX_OUTSTANDING_REQUESTS: usize = 64;
const WS_MAX_OUTSTANDING_BYTES: usize = 12 * 1024 * 1024;
// Retained state is bounded separately from queued requests and outbound events.
// Limits cover serialized checkpoints, including pinned parents and replacements.
const WS_MAX_SESSION_LANES: usize = 128;
const WS_MAX_CHECKPOINT_ITEMS: usize = 32_768;
const WS_MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
const WS_MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;

pub(super) enum WsWorkItem {
    Execute {
        request: Box<WsRequest>,
        input_bytes: usize,
    },
    Reject {
        error: WsRequestParseError,
        input_bytes: usize,
    },
}

impl WsWorkItem {
    fn stream_id(&self) -> Option<&StreamId> {
        match self {
            Self::Execute { request, .. } => request.stream_id.as_ref(),
            Self::Reject { error, .. } => error.stream_id.as_ref(),
        }
    }

    fn lane(&self) -> Option<StreamId> {
        self.stream_id().cloned()
    }

    fn input_bytes(&self) -> usize {
        match self {
            Self::Execute { input_bytes, .. } | Self::Reject { input_bytes, .. } => *input_bytes,
        }
    }
}

pub(super) struct WsAdmissionError {
    pub(super) error: WsError,
    pub(super) stream_id: Option<StreamId>,
}

pub(super) struct RequestCompletion {
    lane: Option<StreamId>,
    input_bytes: usize,
    result: Result<(), WsError>,
}

#[derive(Default)]
struct WsByteBudget {
    used: usize,
}

impl WsByteBudget {
    fn can_reserve(&self, input_bytes: usize) -> bool {
        input_bytes <= WS_MAX_OUTSTANDING_BYTES.saturating_sub(self.used)
    }

    fn reserve(&mut self, input_bytes: usize) {
        debug_assert!(self.can_reserve(input_bytes));
        self.used += input_bytes;
    }

    fn release(&mut self, input_bytes: usize) {
        self.used = self.used.saturating_sub(input_bytes);
    }
}

pub(super) struct WsMultiplexer {
    pub(super) controls: Controls,
    pub(super) disposal: CancellationToken,
    pub(super) state: Arc<AppState>,
    auth: Option<String>,
    pub(super) principal: Option<Arc<AuthenticatedPrincipal>>,
    outbound_tx: mpsc::Sender<WsOutboundEvent>,
    lanes: HashMap<Option<StreamId>, VecDeque<WsWorkItem>>,
    // Idle lanes retain their latest checkpoint until the connection closes.
    pub(super) sessions: HashMap<Option<StreamId>, Arc<ResponseSession>>,
    session_group: ResponseSessionGroup,
    pub(super) request_tasks: JoinSet<RequestCompletion>,
    queued_requests: usize,
    byte_budget: WsByteBudget,
    pub(super) shutdown_token: CancellationToken,
}

impl WsMultiplexer {
    pub(super) fn new(
        state: Arc<AppState>,
        auth: Option<String>,
        principal: Option<AuthenticatedPrincipal>,
        outbound_tx: mpsc::Sender<WsOutboundEvent>,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            disposal: CancellationToken::new(),
            controls: Controls::new(outbound_tx.clone(), WsEventLimit::from_state(&state)),
            state,
            auth,
            principal: principal.map(Arc::new),
            outbound_tx,
            lanes: HashMap::new(),
            sessions: HashMap::new(),
            session_group: ResponseSessionGroup::new(
                NonZeroUsize::new(WS_MAX_SESSION_LANES).expect("positive session limit"),
                NonZeroUsize::new(WS_MAX_CHECKPOINT_ITEMS).expect("positive item limit"),
                NonZeroUsize::new(WS_MAX_CHECKPOINT_BYTES).expect("positive checkpoint limit"),
                NonZeroUsize::new(WS_MAX_RETAINED_BYTES).expect("positive retention limit"),
            ),
            request_tasks: JoinSet::new(),
            queued_requests: 0,
            byte_budget: WsByteBudget::default(),
            shutdown_token,
        }
    }

    pub(super) fn event_limit(&self) -> WsEventLimit {
        WsEventLimit::from_state(&self.state)
    }

    pub(super) fn has_capacity_for(&self, input_bytes: usize) -> bool {
        self.request_tasks.len() + self.queued_requests < WS_MAX_OUTSTANDING_REQUESTS
            && self.byte_budget.can_reserve(input_bytes)
    }

    pub(super) fn schedule(&mut self, mut work: WsWorkItem) -> Result<(), WsAdmissionError> {
        if !self.has_capacity_for(work.input_bytes()) {
            return Err(WsAdmissionError {
                error: WsError::TooManyRequests,
                stream_id: work.stream_id().cloned(),
            });
        }

        let lane = work.lane();
        if !self.sessions.contains_key(&lane) {
            if self.sessions.len() >= WS_MAX_SESSION_LANES {
                return Err(WsAdmissionError {
                    error: WsError::TooManyRequests,
                    stream_id: lane,
                });
            }
            let session = self.session_group.new_session().map_err(|error| WsAdmissionError {
                error: WsError::from(error),
                stream_id: lane.clone(),
            })?;
            self.sessions.insert(lane.clone(), Arc::new(session));
        }
        if let WsWorkItem::Execute { request, .. } = &mut work {
            request.execution = Some(telemetry::QueuedExecution::new());
        }
        self.byte_budget.reserve(work.input_bytes());
        if let Some(queue) = self.lanes.get_mut(&lane) {
            queue.push_back(work);
            self.queued_requests += 1;
            debug!(stream_id = ?lane, queued_requests = queue.len(), "queued websocket request on active lane");
            return Ok(());
        }

        self.lanes.insert(lane.clone(), VecDeque::new());
        self.spawn(lane, work);
        Ok(())
    }

    fn schedule_next(&mut self, lane: Option<StreamId>) {
        let next = self.lanes.get_mut(&lane).and_then(VecDeque::pop_front);
        if let Some(work) = next {
            self.queued_requests -= 1;
            self.spawn(lane, work);
        } else {
            self.lanes.remove(&lane);
        }
    }

    pub(super) fn discard_queued(&mut self) {
        let discarded_bytes = self
            .lanes
            .values()
            .flat_map(|queue| queue.iter())
            .map(WsWorkItem::input_bytes)
            .sum::<usize>();
        self.byte_budget.release(discarded_bytes);
        self.lanes.clear();
        self.queued_requests = 0;
    }

    pub(super) fn finish(
        &mut self,
        completion: Result<RequestCompletion, tokio::task::JoinError>,
        draining: bool,
    ) -> bool {
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => {
                warn!(%error, "responses websocket request task failed");
                return false;
            }
        };
        self.byte_budget.release(completion.input_bytes);
        if let Err(error) = completion.result {
            warn!(%error, "responses websocket request failed without a client-visible event");
            return false;
        }
        if !draining && !self.shutdown_token.is_cancelled() {
            self.schedule_next(completion.lane);
        }
        true
    }

    pub(super) fn reap_ready(&mut self, draining: bool) -> bool {
        while let Some(completion) = self.request_tasks.try_join_next() {
            if !self.finish(completion, draining) {
                return false;
            }
        }
        true
    }

    fn spawn(&mut self, lane: Option<StreamId>, work: WsWorkItem) {
        let register = self.controls.register.clone();
        let disposal = self.disposal.clone();
        let state = Arc::clone(&self.state);
        let auth = self.auth.clone();
        let principal = self.principal.clone();
        let outbound_tx = self.outbound_tx.clone();
        let stream_id = work.stream_id().cloned();
        let input_bytes = work.input_bytes();
        let event_limit = self.event_limit();
        // schedule creates a session before admitting work; idle sessions survive schedule_next.
        let session = Arc::clone(self.sessions.get(&lane).expect("admitted lane has a session"));
        self.request_tasks.spawn(async move {
            // Admission may precede dispatch by an entire inference/tool round.
            // Recheck here for both new lanes and work dequeued by schedule_next.
            if let Some(event) = websocket_identity_error_event(principal.as_deref()) {
                return RequestCompletion {
                    lane,
                    input_bytes,
                    result: queue_ws_json(&outbound_tx, event, stream_id.as_ref(), event_limit).await,
                };
            }
            let result = match work {
                WsWorkItem::Execute { request, .. } => {
                    handle_ws_request(
                        *request,
                        RequestExecution {
                            state: &state,
                            auth,
                            outbound_tx: &outbound_tx,
                            disposal: &disposal,
                            session: &session,
                            event_limit,
                            register: &register,
                        },
                    )
                    .await
                }
                WsWorkItem::Reject { error, .. } => {
                    if let Some(parent) = error.previous_response_id.as_deref() {
                        session
                            .discard_cached_response(parent)
                            .map_err(WsError::from)
                            .and(Err(error.error))
                    } else {
                        Err(error.error)
                    }
                }
            };
            // Dropping a failed executor stream aborts its worker asynchronously.
            // Do not dispatch the next turn until its lease has been released.
            let result = match session.wait_until_idle().await {
                Ok(()) => result,
                Err(error) => Err(WsError::from(error)),
            };
            let result = match result {
                Ok(()) => Ok(()),
                Err(error) => queue_ws_error(&outbound_tx, error, stream_id.as_ref(), event_limit).await,
            };
            RequestCompletion {
                lane,
                input_bytes,
                result,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_capacity_is_bounded_by_count_and_input_bytes() {
        assert_eq!(WS_MAX_OUTSTANDING_REQUESTS, 64);
        let mut budget = WsByteBudget::default();
        budget.reserve(WS_MAX_OUTSTANDING_BYTES - 1);
        assert!(budget.can_reserve(1));
        budget.reserve(1);
        assert!(!budget.can_reserve(1));
        budget.release(WS_MAX_OUTSTANDING_BYTES);
        assert!(budget.can_reserve(WS_MAX_OUTSTANDING_BYTES));
    }
}
