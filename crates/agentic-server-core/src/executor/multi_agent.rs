//! Task lifecycle support for engine-owned multi-agent coordination.
//!
//! This module does not execute inference rounds or own pipeline ingestion.
//! The coordinator will retain the agent tree, mailboxes and pending calls
//! outside cancellable work; `RunOwner` provides bounded task admission and
//! joined teardown. Public multi-agent execution remains gated until the tree,
//! persistence and shared delivery are connected.

mod checkpoint;
pub(in crate::executor) mod collaboration;
mod compaction;
pub use checkpoint::{CheckpointLimits, ValidatedTreeCheckpoint};
pub use compaction::{CompactionCommit, CompactionPlan, CompactionResult};
mod pending_calls;
mod registry;

pub use pending_calls::{ClientCallError, ClientCallView, PendingClientCalls, RejectedClientOutputs};
pub use registry::{AgentPhase, AgentRegistry, AgentState, AgentView, RegistryError, RegistryLimits};

use std::collections::HashMap;
use std::future::Future;

use tokio::task::{AbortHandle, Id, JoinError, JoinSet};

use crate::executor::error::ExecutorResult;
use crate::types::agent::AgentTurnKey;

/// An ephemeral task handle scoped to one [`RunOwner`].
///
/// This is not an agent identity or a durable turn ID. The coordinator must
/// retain its agent/turn mapping when an interrupted task is removed; published
/// client calls still belong to their original turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentTaskId(Id);

/// Admission failures are scheduling decisions, not upstream execution errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpawnRejection {
    #[error("the agent already has an unjoined task")]
    AgentActive,
    #[error("the concurrent subagent task limit has been reached")]
    SubagentLimit,
}

/// The result of joining one task, including its identity on cancellation/panic.
#[derive(Debug)]
pub struct AgentTaskCompletion<T> {
    pub task: AgentTaskId,
    pub owner: AgentTurnKey,
    pub outcome: AgentTaskOutcome<T>,
}

/// Task teardown is distinct from the agent's logical state and pending calls.
#[derive(Debug)]
pub enum AgentTaskOutcome<T> {
    /// Includes recoverable execution failures reported by the task itself.
    Finished(ExecutorResult<T>),
    /// An interruption requested through the owner actually cancelled the task.
    Interrupted,
    /// Panic or unexpected task cancellation; the original source is retained.
    JoinFailed(JoinError),
}

struct OwnedTask {
    abort: AbortHandle,
    owner: AgentTurnKey,
    interrupt_requested: bool,
}

/// Scoped ownership of concurrent turn work, independent of the transport.
///
/// This adapts the Rust Cookbook scoped-thread pattern to owned Tokio tasks:
/// all spawned futures are `Send + 'static`, and the owner explicitly joins
/// them. Dropping the owner aborts remaining tasks through `JoinSet`; normal
/// teardown must use [`Self::cancel_and_join`] or [`Self::finish_and_join`].
///
/// At most one root and `max_concurrent_subagents` descendant tasks are retained.
/// Finished tasks occupy their slots until joined so an undrained result queue
/// cannot grow without bound. The coordinator drains completions before making
/// new admission decisions. Slots are reusable; this is not a lifetime limit.
///
/// The coordinator keeps canonical histories, mailboxes and pending calls outside
/// these cancellable futures. Work and retained results must use the shared
/// response byte budget supplied by the engine. This owner bounds task count; it
/// does not estimate arbitrary future or result sizes and adds no event channel.
pub struct RunOwner<T: 'static> {
    tasks: JoinSet<ExecutorResult<T>>,
    owned: HashMap<Id, OwnedTask>,
    max_concurrent_subagents: usize,
    subagents: usize,
}

impl<T: Send + 'static> RunOwner<T> {
    pub(in crate::executor) fn has_turn(&self, turn: &AgentTurnKey) -> bool {
        self.owned.values().any(|task| task.owner == *turn)
    }
    /// Supply an already validated deployment/admission limit. Zero admits only
    /// root work; resolving the public parameter's default belongs to admission.
    #[must_use]
    pub fn new(max_concurrent_subagents: usize) -> Self {
        Self {
            tasks: JoinSet::new(),
            owned: HashMap::new(),
            max_concurrent_subagents,
            subagents: 0,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owned.is_empty()
    }

    /// Admit work for an exact agent activation and return without waiting for
    /// its answer. Root work is recognized by canonical identity and does not
    /// consume a descendant slot. Only one task per agent may be unjoined.
    ///
    /// Creating/forking the agent, deciding whether to resume or start a turn,
    /// and projecting public collaboration items remain coordinator operations.
    /// This owner does not retain an unbounded history of completed activations.
    ///
    /// # Errors
    /// Returns `AgentActive` for overlapping work on the same agent, or
    /// `SubagentLimit` if all descendant slots are occupied. A rejected future
    /// is dropped without polling or spawning it.
    pub fn spawn_turn(
        &mut self,
        owner: AgentTurnKey,
        work: impl Future<Output = ExecutorResult<T>> + Send + 'static,
    ) -> Result<AgentTaskId, SpawnRejection> {
        if self.owned.values().any(|task| task.owner.agent == owner.agent) {
            return Err(SpawnRejection::AgentActive);
        }
        if !owner.agent.is_root() && self.subagents >= self.max_concurrent_subagents {
            return Err(SpawnRejection::SubagentLimit);
        }
        let abort = self.tasks.spawn(work);
        let id = abort.id();
        if !owner.agent.is_root() {
            self.subagents += 1;
        }
        self.owned.insert(
            id,
            OwnedTask {
                abort,
                owner,
                interrupt_requested: false,
            },
        );
        Ok(AgentTaskId(id))
    }

    /// Request cancellation. Capacity is released only after joining, when the
    /// future has actually dropped. A completion that already won the race is
    /// returned unchanged by `join_next`, never overwritten as interrupted.
    /// Matching the full turn key prevents a late interrupt from cancelling a
    /// different follow-up activation of the same agent.
    pub fn interrupt(&mut self, turn: &AgentTurnKey) -> bool {
        let Some(owned) = self.owned.values_mut().find(|task| &task.owner == turn) else {
            return false;
        };
        owned.interrupt_requested = true;
        owned.abort.abort();
        true
    }

    /// Join the next completed task. Cancellation-safe when used in `select!`.
    pub async fn join_next(&mut self) -> Option<AgentTaskCompletion<T>> {
        let result = self.tasks.join_next_with_id().await?;
        Some(self.complete(result))
    }

    /// Drain ready completions before considering another admission.
    pub fn try_join_next(&mut self) -> Option<AgentTaskCompletion<T>> {
        let result = self.tasks.try_join_next_with_id()?;
        Some(self.complete(result))
    }

    fn complete(&mut self, result: Result<(Id, ExecutorResult<T>), JoinError>) -> AgentTaskCompletion<T> {
        let id = match &result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let owned = self.owned.remove(&id).expect("all tasks are registered by this owner");
        if !owned.owner.agent.is_root() {
            self.subagents -= 1;
        }
        let outcome = match result {
            Ok((_, result)) => AgentTaskOutcome::Finished(result),
            Err(error) if error.is_cancelled() && owned.interrupt_requested => AgentTaskOutcome::Interrupted,
            Err(error) => AgentTaskOutcome::JoinFailed(error),
        };
        AgentTaskCompletion {
            task: AgentTaskId(id),
            owner: owned.owner,
            outcome,
        }
    }

    /// Cancel and join every task, returning every completion/failure for the
    /// coordinator to reconcile. No panic or racing successful result is lost.
    pub async fn cancel_and_join(mut self) -> Vec<AgentTaskCompletion<T>> {
        for owned in self.owned.values_mut() {
            owned.interrupt_requested = true;
            owned.abort.abort();
        }
        self.finish_and_join().await
    }

    /// Join remaining work without cancelling it. The returned collection has
    /// at most one root plus the configured descendant limit. This is resource
    /// teardown, not durable commit or public response completion.
    pub async fn finish_and_join(mut self) -> Vec<AgentTaskCompletion<T>> {
        let mut completed = Vec::with_capacity(self.owned.len());
        while let Some(result) = self.join_next().await {
            completed.push(result);
        }
        completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::error::ExecutorError;
    use crate::types::agent::{AgentIdentity, AgentTurnId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn turn(path: &str) -> AgentTurnKey {
        AgentTurnKey {
            agent: AgentIdentity::try_from(path.to_owned()).unwrap(),
            turn: AgentTurnId::new(),
        }
    }

    async fn pending_turn() -> ExecutorResult<()> {
        std::future::pending().await
    }

    #[tokio::test]
    async fn root_is_excluded_and_joined_slots_admit_a_fourth_lifetime_child() {
        let mut owner = RunOwner::new(3);
        let root = owner.spawn_turn(turn("/root"), pending_turn()).unwrap();
        let first = owner.spawn_turn(turn("/root/task_1"), pending_turn()).unwrap();
        owner.spawn_turn(turn("/root/task_2"), pending_turn()).unwrap();
        owner.spawn_turn(turn("/root/task_3"), pending_turn()).unwrap();

        let polled = Arc::new(AtomicBool::new(false));
        let work_polled = Arc::clone(&polled);
        assert_eq!(
            owner.spawn_turn(turn("/root/task_4"), async move {
                work_polled.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Err(SpawnRejection::SubagentLimit)
        );
        assert!(!polled.load(Ordering::SeqCst));
        let first_turn = owner.owned[&first.0].owner.clone();
        assert!(owner.interrupt(&first_turn));
        // An abort request is not proof the work stopped. Do not oversubscribe.
        assert_eq!(
            owner.spawn_turn(turn("/root/task_5"), pending_turn()),
            Err(SpawnRejection::SubagentLimit)
        );
        let completed = owner.join_next().await.unwrap();
        assert_eq!(completed.task, first);
        assert!(matches!(completed.outcome, AgentTaskOutcome::Interrupted));
        assert!(!owner.interrupt(&first_turn));

        let fourth = owner.spawn_turn(turn("/root/task_6"), async { Ok(()) }).unwrap();
        let completed = owner.join_next().await.unwrap();
        assert_eq!(completed.task, fourth);
        assert!(matches!(completed.outcome, AgentTaskOutcome::Finished(Ok(()))));
        assert!(owner.try_join_next().is_none());

        let cancelled = owner.cancel_and_join().await;
        assert_eq!(cancelled.len(), 3);
        assert!(cancelled.iter().any(|completion| completion.task == root));
        assert!(
            cancelled
                .iter()
                .all(|completion| matches!(completion.outcome, AgentTaskOutcome::Interrupted))
        );
    }

    #[tokio::test]
    async fn root_has_one_slot_and_can_run_without_descendants() {
        let mut owner = RunOwner::new(0);
        assert_eq!(
            owner.spawn_turn(turn("/root/task_7"), pending_turn()),
            Err(SpawnRejection::SubagentLimit)
        );
        owner.spawn_turn(turn("/root"), async { Ok(()) }).unwrap();
        assert_eq!(
            owner.spawn_turn(turn("/root"), pending_turn()),
            Err(SpawnRejection::AgentActive)
        );
        assert!(matches!(
            owner.join_next().await.unwrap().outcome,
            AgentTaskOutcome::Finished(Ok(()))
        ));
        owner.spawn_turn(turn("/root"), async { Ok(()) }).unwrap();
        assert_eq!(owner.finish_and_join().await.len(), 1);
    }

    #[tokio::test]
    async fn finish_joins_all_work_and_preserves_execution_errors_and_panics() {
        let mut owner = RunOwner::new(2);
        let success = owner.spawn_turn(turn("/root"), async { Ok(7) }).unwrap();
        let failure = owner
            .spawn_turn(turn("/root/task_8"), async {
                Err(ExecutorError::StreamError("round failed".into()))
            })
            .unwrap();
        let panic = owner
            .spawn_turn(turn("/root/task_9"), async { panic!("turn panic") })
            .unwrap();

        let expected_owners: HashMap<_, _> = owner
            .owned
            .iter()
            .map(|(id, task)| (AgentTaskId(*id), task.owner.clone()))
            .collect();
        let completed = owner.finish_and_join().await;
        assert_eq!(completed.len(), 3);
        for completion in completed {
            assert_eq!(completion.owner, expected_owners[&completion.task]);
            match completion.outcome {
                AgentTaskOutcome::Finished(Ok(value)) => {
                    assert_eq!(completion.task, success);
                    assert_eq!(value, 7);
                }
                AgentTaskOutcome::Finished(Err(ExecutorError::StreamError(message))) => {
                    assert_eq!(completion.task, failure);
                    assert_eq!(message, "round failed");
                }
                AgentTaskOutcome::JoinFailed(error) => {
                    assert_eq!(completion.task, panic);
                    assert!(error.is_panic());
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn completed_work_is_bounded_and_wins_a_late_interrupt() {
        let mut owner = RunOwner::new(1);
        let task = owner.spawn_turn(turn("/root/task_10"), async { Ok(42) }).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !owner.owned[&task.0].abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // A completed but undrained result still consumes retained capacity.
        assert_eq!(
            owner.spawn_turn(turn("/root/task_11"), async { Ok(0) }),
            Err(SpawnRejection::SubagentLimit)
        );
        let task_turn = owner.owned[&task.0].owner.clone();
        assert!(owner.interrupt(&task_turn));
        let completed = owner.try_join_next().unwrap();
        assert_eq!(completed.task, task);
        assert!(matches!(completed.outcome, AgentTaskOutcome::Finished(Ok(42))));
        assert!(owner.join_next().await.is_none());
        owner.spawn_turn(turn("/root/task_12"), async { Ok(0) }).unwrap();
        assert_eq!(owner.finish_and_join().await.len(), 1);
    }

    struct SignalOnDrop(Option<oneshot::Sender<()>>);

    impl Drop for SignalOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn cancel_and_join_drops_work_before_returning() {
        let mut owner = RunOwner::new(1);
        let (dropped_tx, mut dropped_rx) = oneshot::channel();
        let guard = SignalOnDrop(Some(dropped_tx));
        owner
            .spawn_turn(turn("/root/task_13"), async move {
                let _guard = guard;
                pending_turn().await
            })
            .unwrap();
        let cancelled = owner.cancel_and_join().await;
        assert_eq!(cancelled.len(), 1);
        assert!(matches!(cancelled[0].outcome, AgentTaskOutcome::Interrupted));
        assert_eq!(dropped_rx.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn dropping_owner_aborts_remaining_work() {
        let mut owner = RunOwner::new(1);
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let guard = SignalOnDrop(Some(dropped_tx));
        owner
            .spawn_turn(turn("/root/task_14"), async move {
                let _guard = guard;
                started_tx.send(()).unwrap();
                pending_turn().await
            })
            .unwrap();
        started_rx.await.unwrap();
        drop(owner);
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_join_wait_does_not_lose_the_task() {
        let mut owner = RunOwner::new(1);
        let (release, wait) = oneshot::channel();
        let task = owner
            .spawn_turn(turn("/root/task_15"), async move { Ok(wait.await.unwrap()) })
            .unwrap();
        assert!(tokio::time::timeout(Duration::ZERO, owner.join_next()).await.is_err());
        release.send(17).unwrap();
        let completed = owner.join_next().await.unwrap();
        assert_eq!(completed.task, task);
        assert!(matches!(completed.outcome, AgentTaskOutcome::Finished(Ok(17))));
        assert!(owner.join_next().await.is_none());
    }

    #[tokio::test]
    async fn followup_cannot_overlap_or_receive_a_stale_interrupt() {
        let mut owner = RunOwner::new(2);
        let original = turn("/root/review");
        let followup = turn("/root/review");
        let encoded = serde_json::to_string(&original).unwrap();
        let restored: AgentTurnKey = serde_json::from_str(&encoded).unwrap();

        owner.spawn_turn(original.clone(), pending_turn()).unwrap();
        // There is spare capacity, but this agent already has an active task.
        assert_eq!(
            owner.spawn_turn(followup.clone(), pending_turn()),
            Err(SpawnRejection::AgentActive)
        );
        assert!(!owner.interrupt(&followup));
        assert!(owner.interrupt(&restored));
        assert_eq!(
            owner.spawn_turn(followup.clone(), pending_turn()),
            Err(SpawnRejection::AgentActive)
        );
        let interrupted = owner.join_next().await.unwrap();
        assert_eq!(interrupted.owner, original);
        assert!(matches!(interrupted.outcome, AgentTaskOutcome::Interrupted));

        let (release, wait) = oneshot::channel();
        owner
            .spawn_turn(followup.clone(), async move {
                wait.await.unwrap();
                Ok(())
            })
            .unwrap();
        assert!(!owner.interrupt(&restored), "an old turn must not cancel its follow-up");
        release.send(()).unwrap();
        let completed = owner.join_next().await.unwrap();
        assert_eq!(completed.owner, followup);
        assert!(matches!(completed.outcome, AgentTaskOutcome::Finished(Ok(()))));
    }

    #[tokio::test]
    async fn nested_agents_share_capacity_and_use_full_canonical_identity() {
        let mut owner = RunOwner::new(2);
        let left = turn("/root/left/review");
        let right = turn("/root/right/review");
        owner.spawn_turn(turn("/root"), pending_turn()).unwrap();
        owner.spawn_turn(left.clone(), pending_turn()).unwrap();
        owner.spawn_turn(right.clone(), pending_turn()).unwrap();
        assert_eq!(
            owner.spawn_turn(turn("/root/left/review/extra"), pending_turn()),
            Err(SpawnRejection::SubagentLimit)
        );
        assert!(owner.interrupt(&left));
        assert_eq!(owner.join_next().await.unwrap().owner, left);
        owner
            .spawn_turn(turn("/root/left/review/extra"), pending_turn())
            .unwrap();
        let completed = owner.cancel_and_join().await;
        assert_eq!(completed.len(), 3);
        assert!(completed.iter().any(|task| task.owner == right));
    }
}
