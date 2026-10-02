//! Bounded messages to the response's sole state owner. No transport or wire errors.

use std::{num::NonZeroUsize, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::RejectedClientOutputs;
use crate::executor::response_budget::RetainedSize;
use crate::types::client_calls::ClientToolOutputBatch;

/// Includes queued commands, the command being applied, and unread decisions.
pub const MAX_OUTSTANDING_CONTROLS: usize = 32;

/// A coordinator decision, distinct from durable response commit and wire delivery.
#[derive(Debug)]
pub enum OutputDecision {
    /// The batch is in canonical run state, with its owners made runnable as needed.
    Accepted,
    Rejected(RejectedClientOutputs),
    /// Acceptance closed before this command was applied. Persistence may still fail;
    /// this is not a claim that a completed response exists for continuation.
    Finalizing(ClientToolOutputBatch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ControlAdmissionError {
    #[error("client-output batch exceeds the control byte limit")]
    TooLarge,
    #[error("client-output control capacity exhausted")]
    Full,
    #[error("client-output control is closed")]
    Closed,
}

/// Admission failed before transferring ownership; the input has not been applied.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct RejectedControl {
    pub input: ClientToolOutputBatch,
    pub reason: ControlAdmissionError,
}

/// The run failed or was cancelled before returning a decision. Acceptance is
/// unknown: a caller must not silently retry the command.
#[derive(Debug, thiserror::Error)]
#[error("run ended without a client-output decision")]
pub struct ControlLost;

struct Reply {
    decision: OutputDecision,
    _permit: OwnedSemaphorePermit,
}

/// An admitted command's outcome. Dropping this receipt does not revoke the command.
/// Continue draining response events independently while awaiting the decision.
#[must_use = "await the decision; dropping the receipt does not revoke an admitted command"]
pub struct OutputSubmission(oneshot::Receiver<Reply>);

impl OutputSubmission {
    /// # Errors
    /// Returns `ControlLost` if the coordinator fails before replying.
    pub async fn decision(self) -> Result<OutputDecision, ControlLost> {
        self.0.await.map(|reply| reply.decision).map_err(|_| ControlLost)
    }
}

/// A cloneable handle with nonblocking admission, suitable for a socket reader.
/// Call validation and state mutation happen exclusively in the coordinator.
#[derive(Clone)]
pub struct RunControl {
    sender: mpsc::Sender<OutputCommand>,
    slots: Arc<Semaphore>,
    max_batch_bytes: usize,
}

/// Move this endpoint into exactly one `ExecuteRequest::with_run_control` call.
/// Dropping it fails outstanding receipts; it never reports false acceptance.
pub struct RunControlReceiver(mpsc::Receiver<OutputCommand>);

pub(in crate::executor) struct OutputCommand {
    pub(in crate::executor) input: ClientToolOutputBatch,
    reply: oneshot::Sender<Reply>,
    permit: OwnedSemaphorePermit,
}

impl OutputCommand {
    pub(in crate::executor) fn apply(self, decide: impl FnOnce(ClientToolOutputBatch) -> OutputDecision) {
        let decision = decide(self.input);
        // A cancelled waiter must not roll back or replay accepted input. Keep
        // the slot charged through unread rejections, which retain the batch.
        let _ = self.reply.send(Reply {
            decision,
            _permit: self.permit,
        });
    }
}

impl RunControl {
    /// Bound logical retained input per command and the number of outstanding
    /// commands independently. No allocation proportional to the byte ceiling.
    #[must_use]
    pub fn channel(max_batch_bytes: NonZeroUsize) -> (Self, RunControlReceiver) {
        let (sender, receiver) = mpsc::channel(MAX_OUTSTANDING_CONTROLS);
        (
            Self {
                sender,
                slots: Arc::new(Semaphore::new(MAX_OUTSTANDING_CONTROLS)),
                max_batch_bytes: max_batch_bytes.get(),
            },
            RunControlReceiver(receiver),
        )
    }

    /// Admit immediately or return the untouched batch. Queueing is not acceptance.
    /// The bound includes typed payloads, call IDs, and response identity; it is a
    /// logical retention ceiling, not a bound on caller-owned allocation capacity.
    ///
    /// # Errors
    /// Rejects oversized input, exhausted capacity, or a closed run.
    pub fn try_submit_outputs(&self, input: ClientToolOutputBatch) -> Result<OutputSubmission, RejectedControl> {
        let bytes = input.outputs.iter().fold(
            std::mem::size_of::<ClientToolOutputBatch>().saturating_add(input.response_id.len()),
            |bytes, output| bytes.saturating_add(output.retained_bytes()),
        );
        let reason = if self.sender.is_closed() {
            Some(ControlAdmissionError::Closed)
        } else if bytes > self.max_batch_bytes {
            Some(ControlAdmissionError::TooLarge)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(RejectedControl { input, reason });
        }
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            return Err(RejectedControl {
                input,
                reason: ControlAdmissionError::Full,
            });
        };
        let (reply, receiver) = oneshot::channel();
        let command = OutputCommand { input, reply, permit };
        self.sender.try_send(command).map_err(|error| {
            let reason = match &error {
                mpsc::error::TrySendError::Full(_) => ControlAdmissionError::Full,
                mpsc::error::TrySendError::Closed(_) => ControlAdmissionError::Closed,
            };
            RejectedControl {
                input: error.into_inner().input,
                reason,
            }
        })?;
        Ok(OutputSubmission(receiver))
    }
}

impl RunControlReceiver {
    pub(in crate::executor) async fn recv(&mut self) -> Option<OutputCommand> {
        self.0.recv().await
    }

    /// Linearize finalization with admission. No await separates closing the
    /// queue and rejecting commands that lost the acceptance race.
    pub(in crate::executor) fn finish(&mut self) {
        self.0.close();
        while let Ok(command) = self.0.try_recv() {
            command.apply(OutputDecision::Finalizing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        client_calls::ClientToolOutput,
        io::{FunctionToolResultMessage, ToolCallOutput},
    };

    fn batch(text: &str) -> ClientToolOutputBatch {
        ClientToolOutputBatch {
            response_id: "resp_test".into(),
            outputs: vec![ClientToolOutput::Function(FunctionToolResultMessage {
                call_id: "call_test".into(),
                output: ToolCallOutput::Text(text.into()),
            })],
        }
    }

    #[tokio::test]
    async fn admission_is_not_acceptance_and_unread_decisions_keep_capacity() {
        let (control, mut receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let mut receipts = Vec::new();
        for _ in 0..MAX_OUTSTANDING_CONTROLS {
            let receipt = control.try_submit_outputs(batch("result")).unwrap();
            receiver.recv().await.unwrap().apply(OutputDecision::Finalizing);
            receipts.push(receipt);
        }
        let rejected = control.try_submit_outputs(batch("not queued")).err().unwrap();
        assert_eq!(rejected.reason, ControlAdmissionError::Full);
        assert_eq!(rejected.input.outputs[0].call_id(), "call_test");
        assert!(matches!(
            receipts.pop().unwrap().decision().await.unwrap(),
            OutputDecision::Finalizing(_)
        ));
        let receipt = control.try_submit_outputs(batch("replacement")).unwrap();
        receiver.recv().await.unwrap().apply(|_| OutputDecision::Accepted);
        assert!(matches!(receipt.decision().await.unwrap(), OutputDecision::Accepted));
    }

    #[tokio::test]
    async fn dropped_waiter_does_not_revoke_command_or_release_its_slot_early() {
        let (control, mut receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        for _ in 0..MAX_OUTSTANDING_CONTROLS {
            drop(control.try_submit_outputs(batch("result")).unwrap());
        }
        assert_eq!(
            control.try_submit_outputs(batch("full")).err().unwrap().reason,
            ControlAdmissionError::Full
        );
        for _ in 0..MAX_OUTSTANDING_CONTROLS {
            receiver.recv().await.unwrap().apply(|input| {
                assert_eq!(input.outputs.len(), 1);
                OutputDecision::Accepted
            });
        }
        assert_eq!(control.slots.available_permits(), MAX_OUTSTANDING_CONTROLS);
    }

    #[tokio::test]
    async fn finalization_rejects_queued_input_and_closes_admission() {
        let (control, mut receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let accepted = control.try_submit_outputs(batch("first")).unwrap();
        receiver.recv().await.unwrap().apply(|_| OutputDecision::Accepted);
        let racing = control.try_submit_outputs(batch("second")).unwrap();
        receiver.finish();
        assert!(matches!(accepted.decision().await.unwrap(), OutputDecision::Accepted));
        let OutputDecision::Finalizing(input) = racing.decision().await.unwrap() else {
            panic!("expected rejection")
        };
        assert_eq!(input.outputs[0].call_id(), "call_test");
        assert_eq!(
            control.try_submit_outputs(batch("late")).err().unwrap().reason,
            ControlAdmissionError::Closed
        );
        assert!(receiver.recv().await.is_none());
    }

    #[tokio::test]
    async fn failure_is_distinct_from_a_valid_rejection() {
        let (control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let receipt = control.try_submit_outputs(batch("result")).unwrap();
        drop(receiver);
        assert!(matches!(receipt.decision().await, Err(ControlLost)));
        assert_eq!(control.slots.available_permits(), MAX_OUTSTANDING_CONTROLS);
    }

    #[test]
    fn byte_limit_rejects_before_queueing_and_returns_input() {
        let input = batch("result");
        let bytes =
            std::mem::size_of::<ClientToolOutputBatch>() + input.response_id.len() + input.outputs[0].retained_bytes();
        let (control, _receiver) = RunControl::channel(NonZeroUsize::new(bytes - 1).unwrap());
        assert_eq!(
            control.try_submit_outputs(input).err().unwrap().reason,
            ControlAdmissionError::TooLarge
        );
        assert_eq!(control.slots.available_permits(), MAX_OUTSTANDING_CONTROLS);
        let (control, _receiver) = RunControl::channel(NonZeroUsize::new(bytes).unwrap());
        assert!(control.try_submit_outputs(batch("result")).is_ok());
    }
}
