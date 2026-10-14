//! Transient continuation state owned by one serial response session.

#[cfg(test)]
#[path = "session_budget_tests.rs"]
mod budget_tests;

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::Notify;

mod checkpoint;
mod steering;

pub(super) use checkpoint::canonical_session_history;
pub(crate) use checkpoint::{ResponseCheckpoint, RetainedCheckpoint};
pub use checkpoint::{ResponseCheckpointPin, ResponseContinuation};
pub use steering::{SteeringQueue, SteeringSuccessorPublication, SteeringSuccessorReservation};

use super::{ExecutorError, ExecutorResult};

#[cfg(test)]
use crate::storage::{InOutItem, ResponseMetadata};
#[cfg(test)]
use crate::types::io::{InputItem, OutputItem};
#[cfg(test)]
use crate::types::websocket::SteeringRequiredInput;
#[cfg(test)]
use crate::utils::common::serialized_size_up_to;

/// One latest response checkpoint with a lifetime independent of durable storage.
///
/// A session admits one active turn. The owner must outlive execution; dropping it
/// clears cached state and prevents a late completion from publishing another
/// checkpoint. Active turns must still be cancelled/joined by their caller.
/// Budgets constrain retained item count and serialized size, not total heap use.
#[derive(Debug)]
pub struct ResponseSession {
    state: Arc<Mutex<SessionState>>,
    idle: Arc<Notify>,
    group: Option<Arc<Mutex<SessionGroupState>>>,
    budget: Option<Arc<CheckpointBudget>>,
}

#[derive(Debug)]
struct SessionState {
    latest: Option<Arc<RetainedCheckpoint>>,
    active: bool,
    closed: bool,
    max_items: NonZeroUsize,
    max_bytes: NonZeroUsize,
}

/// A bounded set of independent serial sessions sharing cached parent lookup.
///
/// The caller owns routing and scheduling. Create one member per logical session
/// and keep it while idle; queue disposal must not discard its retained state.
/// The group caps sessions created over its entire lifetime, not only active
/// sessions. Every member inherits the same item and serialized-byte budgets.
/// An aggregate serialized-byte budget covers cached checkpoints, pinned parents
/// and prepared replacements (including those awaiting durable persistence).
/// Sharing the same immutable parent counts it once. Replacement needs headroom
/// for old and new state until publication succeeds; no credit is granted for a
/// still-live parent. Executor input copies and temporary construction allocations
/// are outside this retention budget. Callers must separately bound active work;
/// this is not a heap-memory limit.
///
/// Dropping the group invalidates every member even if its handle survives.
/// Callers must still cancel and join active execution tasks.
#[derive(Debug)]
pub struct ResponseSessionGroup {
    state: Arc<Mutex<SessionGroupState>>,
    max_sessions: NonZeroUsize,
    max_items: NonZeroUsize,
    max_bytes: NonZeroUsize,
    budget: Arc<CheckpointBudget>,
}

#[derive(Debug)]
struct SessionGroupState {
    closed: bool,
    members: Vec<Weak<Mutex<SessionState>>>,
}

impl ResponseSessionGroup {
    #[must_use]
    pub fn new(
        max_sessions: NonZeroUsize,
        max_items: NonZeroUsize,
        max_bytes: NonZeroUsize,
        max_retained_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionGroupState {
                closed: false,
                members: Vec::new(),
            })),
            max_sessions,
            max_items,
            max_bytes,
            budget: Arc::new(CheckpointBudget {
                used: AtomicUsize::new(0),
                limit: max_retained_bytes,
            }),
        }
    }

    /// Create another independent member without retaining its handle here.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error at the lifetime session cap or after
    /// closure, and a stream error if the group state lock is poisoned.
    pub fn new_session(&self) -> ExecutorResult<ResponseSession> {
        let mut state = lock_group(&self.state)?;
        if state.closed || state.members.len() >= self.max_sessions.get() {
            return Err(ExecutorError::InvalidRequest(
                "response session group is closed or has reached its session limit".to_owned(),
            ));
        }
        let mut session = ResponseSession::new(self.max_items, self.max_bytes);
        session.group = Some(Arc::clone(&self.state));
        session.budget = Some(Arc::clone(&self.budget));
        state.members.push(Arc::downgrade(&session.state));
        Ok(session)
    }
}

impl Drop for ResponseSessionGroup {
    fn drop(&mut self) {
        let members = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            std::mem::take(&mut state.members)
        };
        // Never hold the group lock while closing members. A begin operation
        // takes the group lock before looking at any member, with no await.
        for member in members {
            if let Some(member) = member.upgrade() {
                let mut state = member.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.closed = true;
                state.latest = None;
            }
        }
    }
}

impl ResponseSession {
    #[must_use]
    pub fn new(max_items: NonZeroUsize, max_bytes: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionState {
                latest: None,
                active: false,
                closed: false,
                max_items,
                max_bytes,
            })),
            idle: Arc::new(Notify::new()),
            group: None,
            budget: None,
        }
    }

    /// Wait until the active turn has published or dropped its continuation.
    ///
    /// This is not cancellation: callers must first drop the execution stream
    /// or cancel and join its owning task. Stream-owned producers are disposed
    /// synchronously on drop; this also fences independently scheduled callers.
    /// The ending lease releases its parent reference before becoming idle;
    /// other sessions may still keep that shared checkpoint alive and charged.
    ///
    /// # Errors
    ///
    /// Returns a stream error if the session state lock is poisoned.
    pub async fn wait_until_idle(&self) -> ExecutorResult<()> {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock_state(&self.state)?.active {
                return Ok(());
            }
            notified.await;
        }
    }

    /// Pin this session's latest published checkpoint without occupying its
    /// execution slot. Lookup is local to the owning session, never storage.
    /// An active response cannot be pinned until its checkpoint is published.
    ///
    /// The pin shares the existing immutable checkpoint and its byte reservation.
    /// It survives cache disposal or replacement, but does not change steering
    /// eligibility, accept input, or prevent ordinary cache eviction.
    ///
    /// # Errors
    /// Returns an invalid-request error for a closed session/group, or a stream
    /// error if either state lock is poisoned. An unavailable ID returns `None`.
    pub fn pin_response(&self, response_id: &str) -> ExecutorResult<Option<ResponseCheckpointPin>> {
        let group = self.group.as_ref().map(|group| lock_group(group)).transpose()?;
        if group.as_ref().is_some_and(|group| group.closed) {
            return Err(ExecutorError::InvalidRequest(
                "response session group has closed".to_owned(),
            ));
        }
        let state = lock_state(&self.state)?;
        if state.closed {
            return Err(ExecutorError::InvalidRequest("response session has closed".to_owned()));
        }
        Ok(state
            .latest
            .as_ref()
            .filter(|checkpoint| checkpoint.response_id == response_id)
            .map(|checkpoint| ResponseCheckpointPin {
                owner: Arc::downgrade(&self.state),
                checkpoint: Arc::clone(checkpoint),
            }))
    }

    /// Acquire a serial execution lease from an explicitly retained parent,
    /// including after a failed preparation has evicted that parent from cache.
    /// Pass the request's actual `previous_response_id`; a pin cannot retarget it.
    ///
    /// The pin remains owned by the caller across attempts. No parent lookup or
    /// storage fallback replaces its exact immutable snapshot. The caller still
    /// decides target eligibility and when a successful successor consumes it.
    ///
    /// # Errors
    /// Rejects a mismatched previous response ID, a pin from another session,
    /// or a busy/closed session. Poisoned state locks return a stream error.
    pub fn begin_pinned(
        &self,
        previous_id: Option<&str>,
        pin: &ResponseCheckpointPin,
    ) -> ExecutorResult<ResponseContinuation> {
        if previous_id != Some(pin.response_id()) || !Weak::ptr_eq(&pin.owner, &Arc::downgrade(&self.state)) {
            return Err(ExecutorError::InvalidRequest(
                "pinned response does not match the request's previous_response_id or owning session".to_owned(),
            ));
        }
        let mut continuation = self.begin(None)?;
        continuation.parent = Some(Arc::clone(&pin.checkpoint));
        Ok(continuation)
    }

    /// Discard a referenced checkpoint when a request fails before execution.
    ///
    /// An unrelated checkpoint is preserved. This does not cancel an active turn
    /// or release its execution slot; the caller still owns that turn's lifetime.
    ///
    /// # Errors
    ///
    /// Returns a stream error if the session state lock is poisoned.
    pub fn discard_cached_response(&self, response_id: &str) -> ExecutorResult<()> {
        let mut state = lock_state(&self.state)?;
        if state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.response_id == response_id)
        {
            state.latest = None;
        }
        Ok(())
    }

    pub(crate) fn begin(&self, previous_id: Option<&str>) -> ExecutorResult<ResponseContinuation> {
        let group = self.group.as_ref().map(|group| lock_group(group)).transpose()?;
        if group.as_ref().is_some_and(|group| group.closed) {
            return Err(ExecutorError::InvalidRequest(
                "response session group has closed".to_owned(),
            ));
        }
        let mut state = lock_state(&self.state)?;
        if state.closed || state.active {
            return Err(ExecutorError::InvalidRequest(
                "response session is closed or already executing a turn".to_owned(),
            ));
        }
        let parent = state
            .latest
            .as_ref()
            .filter(|entry| Some(entry.response_id.as_str()) == previous_id)
            .cloned();
        state.active = true;
        let mut continuation = ResponseContinuation {
            state: Arc::clone(&self.state),
            idle: Arc::clone(&self.idle),
            parent,
            budget: self.budget.clone(),
            history_replaced: false,
            recorded_output_count: 0,
            finished: false,
        };
        drop(state);
        // Resolve a fork only when execution begins, not when its request queues.
        // Never hold two member locks. Once cloned, the immutable parent remains
        // pinned even if its source session advances or fails.
        if let (None, Some(previous_id), Some(group)) = (continuation.parent.as_ref(), previous_id, group.as_ref()) {
            for member in group.members.iter().filter_map(Weak::upgrade) {
                if Arc::ptr_eq(&member, &self.state) {
                    continue;
                }
                let source = lock_state(&member)?;
                if source.closed {
                    continue;
                }
                if let Some(parent) = source
                    .latest
                    .as_ref()
                    .filter(|parent| parent.response_id == previous_id)
                {
                    continuation.parent = Some(Arc::clone(parent));
                    break;
                }
            }
        }
        Ok(continuation)
    }
}

impl Drop for ResponseSession {
    fn drop(&mut self) {
        // Recover a poisoned guard only for disposal, never for reuse.
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.latest = None;
    }
}

/// Shared byte-reservation mechanism. Each owner supplies an independent budget
/// and defines how its retained payloads are measured.
#[derive(Debug)]
struct CheckpointBudget {
    used: AtomicUsize,
    limit: NonZeroUsize,
}

impl CheckpointBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> ExecutorResult<CheckpointReservation> {
        self.try_reserve(bytes).ok_or_else(aggregate_budget_error)
    }

    /// Shared reservation arithmetic; each owner chooses its capacity error.
    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<CheckpointReservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit.get())
            })
            .ok()?;
        Some(CheckpointReservation {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

#[derive(Debug)]
struct CheckpointReservation {
    budget: Arc<CheckpointBudget>,
    bytes: usize,
}

impl Drop for CheckpointReservation {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes, "retained byte charge released exactly once");
    }
}

fn aggregate_budget_error() -> ExecutorError {
    ExecutorError::PayloadTooLarge(
        "response continuation exceeds the aggregate retained checkpoint budget; release inactive state or replay a compacted input window"
            .to_owned(),
    )
}

fn lock_state(state: &Mutex<SessionState>) -> ExecutorResult<MutexGuard<'_, SessionState>> {
    state
        .lock()
        .map_err(|_| ExecutorError::StreamError("response session state is unavailable".to_owned()))
}

fn lock_group(state: &Mutex<SessionGroupState>) -> ExecutorResult<MutexGuard<'_, SessionGroupState>> {
    state
        .lock()
        .map_err(|_| ExecutorError::StreamError("response session group state is unavailable".to_owned()))
}

#[cfg(test)]
mod tests;
