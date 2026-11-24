//! Bounded handoff of translated agent events to the response delivery owner.
use tokio::sync::{mpsc, oneshot};

use crate::events::EventFrame;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::agent::AgentIdentity;
use crate::utils::common::serialized_size_up_to;

pub(in crate::executor) struct AgentFrame {
    pub agent: AgentIdentity,
    pub round: usize,
    pub frame: EventFrame,
    pub delivered: oneshot::Sender<()>,
}

#[derive(Clone)]
pub(in crate::executor) struct AgentFrameSink {
    pub agent: AgentIdentity,
    pub round: usize,
    pub sender: mpsc::Sender<AgentFrame>,
}

impl AgentFrameSink {
    pub async fn send(&self, frame: &EventFrame, max_bytes: usize) -> ExecutorResult<()> {
        if serialized_size_up_to(&frame.wire, max_bytes)
            .map_err(ExecutorError::JsonError)?
            .is_none()
        {
            return Err(ExecutorError::StreamError(
                "agent event exceeds stream byte limit".into(),
            ));
        }
        let (delivered, received) = oneshot::channel();
        self.sender
            .send(AgentFrame {
                agent: self.agent.clone(),
                round: self.round,
                frame: frame.clone(),
                delivered,
            })
            .await
            .map_err(|_| closed())?;
        // Completion cannot overtake its last event. Cancellation drops this
        // waiter; the response owner still owns any already accepted frame.
        received.await.map_err(|_| closed())
    }
}

fn closed() -> ExecutorError {
    ExecutorError::StreamError("agent delivery owner closed".into())
}
