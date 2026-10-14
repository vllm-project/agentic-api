//! Accepted steering retention and successor reservation/publication ownership.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{CheckpointBudget, CheckpointReservation, ExecutorError, ExecutorResult};
use crate::executor::RunningResponse;
use crate::executor::engine::PreparedRetainedResponse;
use crate::executor::response_budget::RetainedSize;
use crate::executor::response_events::{ResponseCreatedReceipt, ResponseEventSink};
use crate::types::websocket::{AcceptedSteer, SteeringInput};
use crate::utils::uuid7_str;

/// Ordered accepted input retained for one qualified response target.
///
/// The connection owner validates target ownership, lane and effective
/// capabilities before admission. This collection owns input retention and can
/// reserve a matching prepared successor; it does not decide eligibility or
/// stop execution.
/// Admission does not require a published checkpoint and is independent of
/// ordinary response completion and cache eviction.
///
/// Entry and logical-byte limits include descriptors and input; the byte limit
/// also charges the fixed queue identity. Original-wire echoes, outcome delivery,
/// parent pins and aggregate connection limits require separate owner accounting.
/// This is not a heap-memory limit. Dropping the queue disposes its unapplied
/// input; callers must retain it until a qualified handoff or connection cleanup.
#[must_use = "retain accepted steering until successor handoff or connection cleanup"]
pub struct SteeringQueue {
    response_id: String,
    entries: Vec<QueuedSteer>,
    max_entries: NonZeroUsize,
    budget: Arc<CheckpointBudget>,
    _identity_reservation: CheckpointReservation,
}

struct QueuedSteer {
    steer: AcceptedSteer,
    input: SteeringInput,
    // Payload fields drop before their reservation returns retained capacity.
    _reservation: CheckpointReservation,
}

impl fmt::Debug for SteeringQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SteeringQueue")
            .field("response_id", &self.response_id)
            .field("entries", &self.entries.len())
            .field("retained_bytes", &self.budget.used.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl SteeringQueue {
    /// Create independent entry and logical-byte bounds for one target.
    /// Storage/cache state and checkpoint budgets are not changed.
    ///
    /// # Errors
    /// Rejects an empty target ID or a byte limit too small for the queue identity.
    pub fn new(response_id: String, max_entries: NonZeroUsize, max_bytes: NonZeroUsize) -> ExecutorResult<Self> {
        if response_id.is_empty() {
            return Err(ExecutorError::InvalidRequest(
                "steering target must not be empty".to_owned(),
            ));
        }
        let budget = Arc::new(CheckpointBudget {
            used: AtomicUsize::new(0),
            limit: max_bytes,
        });
        let identity_bytes = std::mem::size_of::<Self>().saturating_add(response_id.len());
        let identity_reservation = budget.try_reserve(identity_bytes).ok_or_else(|| {
            ExecutorError::PayloadTooLarge("steering queue identity exceeds the retained-byte limit".to_owned())
        })?;
        Ok(Self {
            response_id,
            entries: Vec::new(),
            max_entries,
            budget,
            _identity_reservation: identity_reservation,
        })
    }

    /// Transfer input into the ordered collection after reserving capacity.
    /// The borrowed descriptor can be cloned for asynchronous outcome delivery;
    /// failed delivery or a dropped descriptor does not revoke the accepted input.
    ///
    /// # Errors
    /// Returns the untouched input when either retention limit would be exceeded.
    /// Rejection preserves every previously accepted entry and its charge.
    pub fn try_accept(&mut self, input: SteeringInput) -> Result<&AcceptedSteer, SteeringInput> {
        const ID_PREFIX: &str = "steer_";
        if self.entries.len() >= self.max_entries.get() {
            return Err(input);
        }
        // uuid7_str uses UUID's hyphenated Display format. Reserve the descriptor
        // before allocating its ID or taking ownership of the submitted input.
        let id_bytes = ID_PREFIX.len() + uuid::fmt::Hyphenated::LENGTH;
        let bytes = std::mem::size_of::<QueuedSteer>()
            .saturating_add(id_bytes)
            .saturating_add(self.response_id.len())
            .saturating_add(input.retained_bytes());
        let Some(reservation) = self.budget.try_reserve(bytes) else {
            return Err(input);
        };
        let steer = AcceptedSteer {
            id: uuid7_str(ID_PREFIX),
            previous_response_id: self.response_id.clone(),
        };
        debug_assert_eq!(steer.id.len(), id_bytes, "UUID display must match its reserved length");
        let index = self.entries.len();
        self.entries.push(QueuedSteer {
            steer,
            input,
            _reservation: reservation,
        });
        Ok(&self.entries[index].steer)
    }

    /// Read accepted descriptors and input in acceptance order without consuming
    /// them or releasing capacity.
    #[must_use]
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&AcceptedSteer, &SteeringInput)> {
        self.entries.iter().map(|entry| (&entry.steer, &entry.input))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Seal the complete accepted batch with the exact request that incorporates
    /// it. Validation and ownership transfer are synchronous. Consuming both
    /// values prevents further admission into this batch or independent startup
    /// of the prepared request. No response work or events are started here.
    ///
    /// Moves the existing queue and its byte reservations without copying input
    /// or releasing capacity. Prepared execution copies remain outside the queue
    /// budget; the owner must bound preparation and connection-wide retained work.
    /// Reservation is not commitment. A later startup handoff must register the
    /// successor before publication and bind commitment to its exact event sink.
    /// # Errors
    /// Returns both values untouched for unbound, unrelated or stale preparation.
    /// The rejection pair is boxed to keep the recoverable error small; dropping
    /// its prepared response releases the lease while the queue can be retried.
    pub fn reserve_successor(
        self,
        prepared: PreparedRetainedResponse,
    ) -> Result<SteeringSuccessorReservation, Box<(Self, PreparedRetainedResponse)>> {
        if !prepared.matches_steering(&self) {
            return Err(Box::new((self, prepared)));
        }
        Ok(SteeringSuccessorReservation { prepared, queue: self })
    }
}

/// One sealed batch paired with its validated, unstarted successor.
///
/// Private fields and a consuming constructor preserve the relationship between
/// accepted inputs and prepared execution. This handle cannot be cloned, reopened
/// for admission or detached into independently startable request state.
/// It retains the prepared request's existing parent snapshot and session lease.
/// Dropping it before startup disposes inputs and releases the lease; connection
/// cleanup may do this, but ordinary failure reporting must retain ownership
/// until the corresponding failed outcomes have been delivered.
#[must_use = "retain the successor reservation through startup or explicit connection cleanup"]
pub struct SteeringSuccessorReservation {
    // Drop canonical execution copies and the lease before refunding queue bytes.
    prepared: PreparedRetainedResponse,
    queue: SteeringQueue,
}

impl SteeringSuccessorReservation {
    /// Allocate a gated successor and bind its eventual creation receipt to
    /// this batch. Register routing before activating the returned owner.
    /// # Panics
    /// Must be called inside a Tokio runtime.
    pub fn start_gated(self) -> (RunningResponse, SteeringSuccessorPublication) {
        let response = self.prepared.start_gated();
        let publication = SteeringSuccessorPublication {
            response_id: response.response_id.clone(),
            sink: response.sink.clone(),
            queue: Box::new(self.queue),
        };
        (response, publication)
    }
    #[must_use]
    pub fn response_id(&self) -> &str {
        self.prepared.response_id()
    }

    #[must_use]
    pub fn previous_response_id(&self) -> &str {
        &self.queue.response_id
    }

    /// Borrow accepted descriptors and their original input in acceptance order.
    #[must_use]
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&AcceptedSteer, &SteeringInput)> {
        self.queue.entries()
    }
}

/// One started successor's outstanding batch and its publication identity.
/// Creation remains authoritative if a waiter is cancelled or execution fails.
/// Before reporting pre-creation failure, join the returned response owner.
#[must_use = "resolve successor publication or dispose it during connection cleanup"]
pub struct SteeringSuccessorPublication {
    response_id: String,
    sink: ResponseEventSink,
    queue: Box<SteeringQueue>,
}

impl SteeringSuccessorPublication {
    /// # Errors
    /// Returns a delivery/execution error if creation was not published. This
    /// observation does not stop producers; join execution before failure recovery.
    pub async fn wait_created(&self) -> ExecutorResult<Arc<ResponseCreatedReceipt>> {
        self.sink.wait_created().await
    }

    /// Consume the batch once its exact successor's creation was published.
    /// # Errors
    /// Returns this owner unchanged for an unrelated sink, parent or successor.
    pub fn commit(self, receipt: &ResponseCreatedReceipt) -> Result<(), Self> {
        if receipt.belongs_to(&self.sink)
            && receipt.response_id() == self.response_id
            && receipt.previous_response_id() == Some(self.queue.response_id.as_str())
        {
            Ok(())
        } else {
            Err(self)
        }
    }

    /// Release the uncommitted canonical batch after execution has been joined.
    /// The adapter retains the original raw inputs and their IDs for failure delivery.
    /// No canonical input is serialized or copied at this ownership boundary.
    /// # Errors
    /// Returns this owner unchanged while producers may still run or after creation.
    pub fn fail_before_creation(self) -> Result<(), Self> {
        if !self.sink.execution_joined() || self.sink.created_receipt().is_some() {
            Err(self)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for SteeringSuccessorReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SteeringSuccessorReservation")
            .field("response_id", &self.response_id())
            .field("previous_response_id", &self.previous_response_id())
            .field("entries", &self.queue.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
