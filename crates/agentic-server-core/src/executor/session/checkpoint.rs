//! Immutable parent checkpoints and serial execution lease publication.

use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use tokio::sync::Notify;

use super::{CheckpointBudget, CheckpointReservation, ExecutorError, ExecutorResult, SessionState};
use super::{aggregate_budget_error, lock_state};
use crate::storage::ResponseMetadata;
use crate::types::io::InOutItem;
use crate::types::io::input::{async_records, retain_window};
use crate::types::io::{InputItem, OutputItem};
use crate::types::websocket::SteeringRequiredInput;
use crate::utils::common::serialized_size_up_to;

/// An explicitly retained published parent, independent of an active turn.
///
/// The owning session must remain open to begin execution from this pin. The
/// weak owner identity does not keep a closed session usable; the checkpoint's
/// existing byte charge lasts until its final cache, lease, or pin reference drops.
/// Grouped sessions keep the aggregate charge; standalone sessions retain their
/// existing per-checkpoint limit, so callers must bound retained pin lifetimes.
/// Steering owns this handle until successor commitment or disposal, separately
/// from terminal response delivery. This is retention, not steering admission.
#[derive(Debug)]
pub struct ResponseCheckpointPin {
    pub(super) owner: Weak<Mutex<SessionState>>,
    pub(super) checkpoint: Arc<RetainedCheckpoint>,
}

impl ResponseCheckpointPin {
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.checkpoint.response_id
    }

    /// Canonical input items for validating a continuation before acquiring an
    /// execution lease. Borrowing history does not consume the pinned parent.
    #[must_use]
    pub fn history(&self) -> &[InputItem] {
        &self.checkpoint.history
    }

    /// Outstanding client obligations from the same canonical scanner used by
    /// ordinary continuation validation. Steering cannot supply these outputs.
    /// Parent tool declarations restore public custom and shell output types normalized
    /// into functions in canonical history.
    /// # Errors
    /// Rejects malformed call histories or obligation types not qualified for steering.
    pub fn required_steering_input(&self) -> ExecutorResult<Vec<SteeringRequiredInput>> {
        crate::executor::pending_calls::pending_calls(self.history())?
            .into_iter()
            .filter(|pending| !pending.async_execution)
            .map(|pending| {
                pending
                    .into_steering_required_input(
                        self.checkpoint.metadata.effective_tools.as_deref().unwrap_or_default(),
                    )
                    .map_err(|_| {
                        ExecutorError::InvalidRequest("pending client tool type does not support steering".into())
                    })
            })
            .collect()
    }
}

/// An active turn's coherent parent snapshot and completion lease.
///
/// Constructed by the executor, not serialized into split-execution contexts.
/// Dropping an unfinished lease evicts its referenced cached parent and releases
/// the serial execution slot, including on rehydration, tool or stream errors.
#[derive(Debug)]
pub struct ResponseContinuation {
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) idle: Arc<Notify>,
    pub(crate) parent: Option<Arc<RetainedCheckpoint>>,
    pub(super) budget: Option<Arc<CheckpointBudget>>,
    pub(super) history_replaced: bool,
    pub(super) recorded_output_count: usize,
    pub(super) finished: bool,
}

#[derive(Serialize)]
pub(crate) struct ResponseCheckpoint {
    pub response_id: String,
    pub conversation_id: Option<String>,
    pub history: Vec<InputItem>,
    pub metadata: ResponseMetadata,
    pub durable: bool,
}

impl fmt::Debug for ResponseCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseCheckpoint")
            .field("items", &self.history.len())
            .finish_non_exhaustive()
    }
}

/// An immutable checkpoint paired with its lifetime charge. Keeping the fields
/// private and omitting `DerefMut` prevents post-reservation size changes.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RetainedCheckpoint {
    checkpoint: ResponseCheckpoint,
    #[serde(skip)]
    _reservation: Option<CheckpointReservation>,
}

impl Deref for RetainedCheckpoint {
    type Target = ResponseCheckpoint;

    fn deref(&self) -> &Self::Target {
        &self.checkpoint
    }
}

/// Restore only the current canonical window, while preserving orchestration
/// records that the model-facing projection would strip. Durable response chains
/// can still reference superseded parent rows; those must not consume the session
/// checkpoint budget or introduce obsolete pending calls after reconnecting.
pub(in crate::executor) fn canonical_session_history(history: Vec<InputItem>) -> Vec<InputItem> {
    // MCP discovery records stay so the registry knows which servers were already listed.
    retain_window(history, |item| matches!(item, InputItem::McpListTools(_)))
}

impl ResponseContinuation {
    /// The executor has replaced this turn's input with a canonical compacted window.
    pub(crate) fn mark_history_replaced(&mut self) {
        self.history_replaced = true;
    }

    /// The loop has recorded these outputs in canonical inference-round order.
    /// Public output remains complete, but persistence must not append it again.
    pub(crate) fn mark_outputs_recorded(&mut self, output_count: usize) {
        self.recorded_output_count = output_count;
    }

    pub(crate) fn retains_output(&self, index: usize, item: &OutputItem) -> bool {
        (index >= self.recorded_output_count && !item.is_recorded_as_history())
            || matches!(item, OutputItem::McpListTools(_))
    }

    /// Retain orchestration records across compaction without restoring the old
    /// model context. MCP discovery still needs to know which servers were listed,
    /// and async calls with their outputs remain so a later output finds its call.
    pub(crate) fn parent_items(&self) -> impl Iterator<Item = &InputItem> {
        let history = self.parent.as_ref().map_or(&[][..], |parent| parent.history.as_slice());
        let records = if self.history_replaced {
            async_records(history)
        } else {
            Vec::new()
        };
        history.iter().enumerate().filter_map(move |(index, item)| {
            let kept = !self.history_replaced || matches!(item, InputItem::McpListTools(_)) || records[index];
            kept.then_some(item)
        })
    }

    pub(crate) fn checkpoint(
        &self,
        response_id: String,
        conversation_id: Option<String>,
        metadata: &ResponseMetadata,
        new_items: &[InOutItem],
        durable: bool,
    ) -> ExecutorResult<RetainedCheckpoint> {
        let mut history = self.parent_items().cloned().collect::<Vec<_>>();
        history.extend(InOutItem::into_input_items(new_items.to_vec()));
        let mut metadata = metadata.clone();
        if let Some(tools) = metadata.effective_tools.as_mut() {
            for tool in tools {
                tool.sanitize_for_persistence();
            }
        }
        let checkpoint = ResponseCheckpoint {
            response_id,
            conversation_id,
            history: canonical_session_history(history),
            metadata,
            durable,
        };
        let (max_items, max_bytes) = {
            let state = lock_state(&self.state)?;
            (state.max_items.get(), state.max_bytes.get())
        };
        let bytes = if checkpoint.history.len() <= max_items {
            serialized_size_up_to(&checkpoint, max_bytes)?
        } else {
            None
        };
        let Some(bytes) = bytes else {
            return Err(ExecutorError::PayloadTooLarge(
                "response continuation exceeds the session checkpoint budget; replay a compacted input window"
                    .to_owned(),
            ));
        };
        self.retain(checkpoint, bytes)
    }

    /// A durable fallback becomes a live pinned parent before inference. It must
    /// share the aggregate budget instead of bypassing it through storage.
    pub(crate) fn retain_parent(&self, checkpoint: ResponseCheckpoint) -> ExecutorResult<RetainedCheckpoint> {
        let bytes = if let Some(budget) = &self.budget {
            serialized_size_up_to(&checkpoint, budget.limit.get())?.ok_or_else(aggregate_budget_error)?
        } else {
            0 // Standalone sessions retain their existing per-completion policy.
        };
        self.retain(checkpoint, bytes)
    }

    fn retain(&self, checkpoint: ResponseCheckpoint, bytes: usize) -> ExecutorResult<RetainedCheckpoint> {
        let reservation = self.budget.as_ref().map(|budget| budget.reserve(bytes)).transpose()?;
        Ok(RetainedCheckpoint {
            checkpoint,
            _reservation: reservation,
        })
    }

    pub(crate) fn publish(mut self, checkpoint: RetainedCheckpoint) -> ExecutorResult<()> {
        {
            let mut state = lock_state(&self.state)?;
            if state.closed {
                return Err(ExecutorError::InvalidRequest("response session has closed".to_owned()));
            }
            state.latest = Some(Arc::new(checkpoint));
            // Return this lease's parent charge before another turn can observe
            // an idle slot, including a waiter resumed by notify_waiters below.
            self.parent = None;
            state.active = false;
        }
        self.finished = true;
        self.idle.notify_waiters();
        Ok(())
    }
}

impl Drop for ResponseContinuation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.parent.as_ref().is_some_and(|parent| {
            state
                .latest
                .as_ref()
                .is_some_and(|latest| latest.response_id == parent.response_id)
        }) {
            state.latest = None;
        }
        // Field destructors run after Drop returns, too late for a waiter that
        // can immediately reuse the slot and reserve the released capacity.
        self.parent = None;
        state.active = false;
        drop(state);
        self.idle.notify_waiters();
    }
}
