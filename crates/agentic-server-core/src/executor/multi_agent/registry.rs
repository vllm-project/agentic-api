//! Synchronous tree metadata and mailbox ownership for the engine's coordinator.
//!
//! No task, inference, persistence, or public-event operations occur here. The
//! engine retains canonical model contexts separately. In-flight work must be
//! joined before the coordinator settles a turn or activates its replacement.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use crate::types::agent_tree::StoredAgent;
use indexmap::IndexMap;

use crate::executor::error::ExecutorError;
use crate::executor::response_budget::{ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES};
use crate::types::agent::{
    AgentCompletion, AgentIdentity, AgentMail, AgentMailContent, AgentTurnId, AgentTurnKey, InvalidAgentIdentity,
};

/// Deployment metadata limits, separate from active-turn admission limits.
#[derive(Debug, Clone, Copy)]
pub struct RegistryLimits {
    pub max_agents: NonZeroUsize,
    pub max_mailbox_messages: NonZeroUsize,
}

pub use crate::types::agent_tree::{AgentPhase, AgentState};

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("agent is not registered")]
    UnknownAgent,
    #[error("operation belongs to a superseded agent turn")]
    StaleTurn,
    #[error("agent turn is no longer active")]
    InactiveTurn,
    #[error("agent identity already exists")]
    DuplicateAgent,
    #[error("follow-up tasks must target a non-root agent")]
    RootFollowup,
    #[error("agent registry capacity reached")]
    AgentLimit,
    #[error("recipient mailbox is full")]
    MailboxFull,
    #[error("mail can only be consumed at a runnable boundary")]
    NotRunnable,
    #[error(transparent)]
    InvalidIdentity(#[from] InvalidAgentIdentity),
    #[error(transparent)]
    Budget(#[from] Box<ExecutorError>),
}

impl From<ExecutorError> for RegistryError {
    fn from(error: ExecutorError) -> Self {
        Self::Budget(Box::new(error))
    }
}

struct AgentRecord {
    parent: Option<AgentIdentity>,
    turn: AgentTurnId,
    state: AgentState,
    mailbox: VecDeque<AgentMail>,
}

/// Read-only metadata. The coordinator owns mutation; callers cannot replace
/// turn IDs or clear mail through an agent view.
#[derive(Debug)]
pub struct AgentView<'a> {
    pub identity: &'a AgentIdentity,
    pub parent: Option<&'a AgentIdentity>,
    pub turn: AgentTurnId,
    pub state: AgentState,
    pub queued_messages: usize,
}

pub struct AgentRegistry {
    agents: IndexMap<AgentIdentity, AgentRecord>,
    limits: RegistryLimits,
    budget: ExecutorResponseBudget,
}

impl AgentRegistry {
    /// Create the root and its first activation with an independent budget.
    /// Engine integration uses `with_budget` to share the response ceiling.
    /// The byte ceiling is cumulative: draining mail transfers its reservation
    /// to the receiving context rather than refunding retained bytes.
    ///
    /// # Errors
    /// Fails if the byte budget cannot retain root metadata.
    pub fn new(limits: RegistryLimits, max_retained_bytes: usize) -> Result<Self, RegistryError> {
        Self::with_budget(limits, ExecutorResponseBudget::with_limit(max_retained_bytes))
    }

    pub(in crate::executor) fn with_budget(
        limits: RegistryLimits,
        budget: ExecutorResponseBudget,
    ) -> Result<Self, RegistryError> {
        let root = AgentIdentity::root();
        budget.consume(record_bytes(&root, None))?;
        let mut agents = IndexMap::new();
        agents.insert(root, AgentRecord::new(None));
        Ok(Self { agents, limits, budget })
    }

    pub(in crate::executor) fn restore(
        agents: &[StoredAgent],
        limits: RegistryLimits,
        budget: ExecutorResponseBudget,
    ) -> Result<Self, RegistryError> {
        if agents.is_empty() || agents.len() > limits.max_agents.get() {
            return Err(RegistryError::AgentLimit);
        }
        let mut records = IndexMap::new();
        for agent in agents {
            if records.contains_key(&agent.identity) {
                return Err(RegistryError::DuplicateAgent);
            }
            match &agent.parent {
                None if agent.identity.is_root() && records.is_empty() => {}
                Some(parent)
                    if records.contains_key(parent)
                        && agent
                            .identity
                            .as_str()
                            .rsplit_once('/')
                            .is_some_and(|(path, _)| path == parent.as_str()) => {}
                _ => return Err(RegistryError::InvalidIdentity(InvalidAgentIdentity)),
            }
            if agent.mailbox.len() > limits.max_mailbox_messages.get() {
                return Err(RegistryError::MailboxFull);
            }
            // An exported tree cannot retain live work. Restore only safe boundaries.
            if matches!(
                agent.state,
                AgentState::Active(AgentPhase::Inferring | AgentPhase::ExecutingTools)
            ) {
                return Err(RegistryError::NotRunnable);
            }
            budget.consume(record_bytes(&agent.identity, agent.parent.as_ref()))?;
            for mail in &agent.mailbox {
                if !agents.iter().any(|entry| entry.identity == mail.sender.agent) {
                    return Err(RegistryError::UnknownAgent);
                }
                budget.consume(mail_bytes(
                    &mail.sender,
                    match &mail.content {
                        AgentMailContent::Message(text)
                        | AgentMailContent::Task(text)
                        | AgentMailContent::TurnFinished(
                            AgentCompletion::Finished(text) | AgentCompletion::Failed(text),
                        ) => text.len(),
                        AgentMailContent::TurnFinished(AgentCompletion::Interrupted) => 0,
                    },
                ))?;
            }
            records.insert(
                agent.identity.clone(),
                AgentRecord {
                    parent: agent.parent.clone(),
                    turn: agent.turn,
                    state: agent.state,
                    mailbox: agent.mailbox.clone().into(),
                },
            );
        }
        Ok(Self {
            agents: records,
            limits,
            budget,
        })
    }

    pub(in crate::executor) fn checkpoint_agent(&self, agent: &mut StoredAgent) -> Result<(), RegistryError> {
        let record = self.agents.get(&agent.identity).ok_or(RegistryError::UnknownAgent)?;
        agent.parent.clone_from(&record.parent);
        agent.turn = record.turn;
        agent.state = record.state;
        agent.mailbox = record.mailbox.iter().cloned().collect();
        Ok(())
    }

    pub(in crate::executor) fn resume_root(&mut self) -> AgentTurnKey {
        let record = self
            .agents
            .get_mut(&AgentIdentity::root())
            .expect("validated tree contains root");
        if !matches!(record.state, AgentState::Active(_)) {
            record.turn = AgentTurnId::new();
        }
        record.state = AgentState::Active(AgentPhase::Runnable);
        AgentTurnKey {
            agent: AgentIdentity::root(),
            turn: record.turn,
        }
    }

    #[must_use]
    pub fn get(&self, identity: &AgentIdentity) -> Option<AgentView<'_>> {
        self.agents
            .get_key_value(identity)
            .map(|(identity, record)| view(identity, record))
    }

    /// Stable creation order; this is not a public listing projection.
    pub fn agents(&self) -> impl Iterator<Item = AgentView<'_>> {
        self.agents.iter().map(|(identity, record)| view(identity, record))
    }

    /// Register a child and its initial task. Execution admission and context
    /// forking must be coordinated by the caller before publishing success.
    ///
    /// # Errors
    /// Rejects stale/inactive parents, duplicate/invalid names, or capacity
    /// exhaustion. No child or mail is inserted on failure.
    pub fn register_child(
        &mut self,
        parent: &AgentTurnKey,
        task_name: &str,
        task: &str,
    ) -> Result<AgentTurnKey, RegistryError> {
        self.active(parent)?;
        let identity = parent.agent.child(task_name)?;
        if self.agents.contains_key(&identity) {
            return Err(RegistryError::DuplicateAgent);
        }
        if self.agents.len() >= self.limits.max_agents.get() {
            return Err(RegistryError::AgentLimit);
        }
        self.budget
            .consume(record_bytes(&identity, Some(&parent.agent)).saturating_add(mail_bytes(parent, task.len())))?;
        let mut record = AgentRecord::new(Some(parent.agent.clone()));
        record.mailbox.push_back(AgentMail {
            sender: parent.clone(),
            content: AgentMailContent::Task(task.to_owned()),
        });
        let key = AgentTurnKey {
            agent: identity.clone(),
            turn: record.turn,
        };
        self.agents.insert(identity, record);
        Ok(key)
    }

    /// Queue mail without starting an idle agent. An already active mailbox
    /// waiter becomes runnable with its existing turn ID.
    ///
    /// # Errors
    /// Rejects stale/inactive senders, unknown recipients, or mailbox/budget exhaustion.
    pub fn send_message(
        &mut self,
        sender: &AgentTurnKey,
        recipient: &AgentIdentity,
        text: &str,
    ) -> Result<(), RegistryError> {
        self.active(sender)?;
        self.reserve_mail(recipient, sender, text.len())?;
        self.enqueue(
            recipient,
            AgentMail {
                sender: sender.clone(),
                content: AgentMailContent::Message(text.into()),
            },
        );
        Ok(())
    }

    /// Queue task input. Active targets keep their activation; settled targets
    /// receive a new turn ID. Previously queued mail remains in FIFO order.
    ///
    /// # Errors
    /// Rejects stale/inactive senders, root/unknown targets, and capacity exhaustion.
    pub fn followup_task(
        &mut self,
        sender: &AgentTurnKey,
        recipient: &AgentIdentity,
        task: &str,
    ) -> Result<AgentTurnKey, RegistryError> {
        self.active(sender)?;
        if recipient.is_root() {
            return Err(RegistryError::RootFollowup);
        }
        let record = self.reserve_mail(recipient, sender, task.len())?;
        if !matches!(record.state, AgentState::Active(_)) {
            record.turn = AgentTurnId::new();
            record.state = AgentState::Active(AgentPhase::Runnable);
        }
        let key = AgentTurnKey {
            agent: recipient.clone(),
            turn: record.turn,
        };
        self.enqueue(
            recipient,
            AgentMail {
                sender: sender.clone(),
                content: AgentMailContent::Task(task.into()),
            },
        );
        Ok(key)
    }

    pub(in crate::executor) fn resume_queued_task(&mut self, identity: &AgentIdentity) -> bool {
        let Some(record) = self.agents.get_mut(identity) else {
            return false;
        };
        if matches!(record.state, AgentState::Active(_))
            || !record
                .mailbox
                .iter()
                .any(|mail| matches!(mail.content, AgentMailContent::Task(_)))
        {
            return false;
        }
        record.turn = AgentTurnId::new();
        record.state = AgentState::Active(AgentPhase::Runnable);
        true
    }

    /// Record the coordinator's current execution phase. Work scheduling and
    /// pending-call validation happen in the coordinator, not this metadata store.
    /// Queued mail prevents parking an active turn in a mailbox wait.
    ///
    /// # Errors
    /// Rejects an unknown, superseded, or already settled turn.
    pub fn set_phase(&mut self, turn: &AgentTurnKey, phase: AgentPhase) -> Result<(), RegistryError> {
        let record = self.active(turn)?;
        let phase = if phase == AgentPhase::WaitingForMailbox && !record.mailbox.is_empty() {
            AgentPhase::Runnable
        } else {
            phase
        };
        record.state = AgentState::Active(phase);
        Ok(())
    }

    /// Move mail to the agent's canonical context at a safe execution boundary.
    /// The existing cumulative byte reservation follows that transfer.
    ///
    /// # Errors
    /// Rejects a stale/inactive turn or a turn that is not runnable.
    pub fn take_mailbox(&mut self, turn: &AgentTurnKey) -> Result<VecDeque<AgentMail>, RegistryError> {
        let record = self.active(turn)?;
        if record.state != AgentState::Active(AgentPhase::Runnable) {
            return Err(RegistryError::NotRunnable);
        }
        Ok(std::mem::take(&mut record.mailbox))
    }

    /// Settle joined work and notify its parent exactly once. The caller keeps
    /// the completion on rejection and can retry after draining the parent.
    /// Pending client calls and canonical history are never removed here.
    ///
    /// # Errors
    /// Rejects stale/already settled turns or exhausted parent mailbox capacity.
    /// On failure both turn state and parent mailbox remain unchanged.
    pub fn settle_turn(&mut self, turn: &AgentTurnKey, result: &AgentCompletion) -> Result<(), RegistryError> {
        let record = self.active(turn)?;
        let parent = record.parent.clone();
        if let Some(parent) = &parent {
            let text_bytes = match result {
                AgentCompletion::Finished(text) | AgentCompletion::Failed(text) => text.len(),
                AgentCompletion::Interrupted => 0,
            };
            self.reserve_mail(parent, turn, text_bytes)?;
        }
        let state = match result {
            AgentCompletion::Finished(_) => AgentState::Idle,
            AgentCompletion::Interrupted => AgentState::Interrupted,
            AgentCompletion::Failed(_) => AgentState::Failed,
        };
        self.active(turn)?.state = state;
        if let Some(parent) = parent {
            self.enqueue(
                &parent,
                AgentMail {
                    sender: turn.clone(),
                    content: AgentMailContent::TurnFinished(result.clone()),
                },
            );
        }
        Ok(())
    }

    fn active(&mut self, turn: &AgentTurnKey) -> Result<&mut AgentRecord, RegistryError> {
        let record = self.agents.get_mut(&turn.agent).ok_or(RegistryError::UnknownAgent)?;
        if record.turn != turn.turn {
            return Err(RegistryError::StaleTurn);
        }
        if !matches!(record.state, AgentState::Active(_)) {
            return Err(RegistryError::InactiveTurn);
        }
        Ok(record)
    }

    fn reserve_mail(
        &mut self,
        recipient: &AgentIdentity,
        sender: &AgentTurnKey,
        text_bytes: usize,
    ) -> Result<&mut AgentRecord, RegistryError> {
        let record = self.agents.get_mut(recipient).ok_or(RegistryError::UnknownAgent)?;
        if record.mailbox.len() >= self.limits.max_mailbox_messages.get() {
            return Err(RegistryError::MailboxFull);
        }
        self.budget.consume(mail_bytes(sender, text_bytes))?;
        Ok(record)
    }

    fn enqueue(&mut self, recipient: &AgentIdentity, mail: AgentMail) {
        let record = self
            .agents
            .get_mut(recipient)
            .expect("recipient validated before reservation");
        record.mailbox.push_back(mail);
        if record.state == AgentState::Active(AgentPhase::WaitingForMailbox) {
            record.state = AgentState::Active(AgentPhase::Runnable);
        }
    }
}

impl AgentRecord {
    fn new(parent: Option<AgentIdentity>) -> Self {
        Self {
            parent,
            turn: AgentTurnId::new(),
            state: AgentState::Active(AgentPhase::Runnable),
            mailbox: VecDeque::new(),
        }
    }
}

fn view<'a>(identity: &'a AgentIdentity, record: &'a AgentRecord) -> AgentView<'a> {
    AgentView {
        identity,
        parent: record.parent.as_ref(),
        turn: record.turn,
        state: record.state,
        queued_messages: record.mailbox.len(),
    }
}

fn record_bytes(identity: &AgentIdentity, parent: Option<&AgentIdentity>) -> usize {
    std::mem::size_of::<(AgentIdentity, AgentRecord)>()
        .saturating_add(RETAINED_CONTAINER_OVERHEAD_BYTES)
        .saturating_add(identity.as_str().len())
        .saturating_add(parent.map_or(0, |parent| parent.as_str().len()))
}

fn mail_bytes(sender: &AgentTurnKey, text_bytes: usize) -> usize {
    std::mem::size_of::<AgentMail>()
        .saturating_add(RETAINED_CONTAINER_OVERHEAD_BYTES)
        .saturating_add(sender.agent.as_str().len())
        .saturating_add(text_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(agents: usize, mail: usize) -> RegistryLimits {
        RegistryLimits {
            max_agents: NonZeroUsize::new(agents).unwrap(),
            max_mailbox_messages: NonZeroUsize::new(mail).unwrap(),
        }
    }

    fn root(registry: &AgentRegistry) -> AgentTurnKey {
        let agent = AgentIdentity::root();
        AgentTurnKey {
            turn: registry.get(&agent).unwrap().turn,
            agent,
        }
    }

    #[test]
    fn registry_preserves_parentage_and_rejects_duplicate_or_excess_agents() {
        let mut registry = AgentRegistry::new(limits(3, 4), 16_384).unwrap();
        let root = root(&registry);
        let child = registry.register_child(&root, "review", "review code").unwrap();
        let nested = registry.register_child(&child, "tests", "check tests").unwrap();
        assert_eq!(registry.get(&nested.agent).unwrap().parent, Some(&child.agent));
        let used = registry.budget.used();
        assert!(matches!(
            registry.register_child(&root, "review", "again"),
            Err(RegistryError::DuplicateAgent)
        ));
        assert!(matches!(
            registry.register_child(&root, "extra", "more"),
            Err(RegistryError::AgentLimit)
        ));
        assert_eq!(registry.budget.used(), used);
        let names: Vec<_> = registry.agents().map(|agent| agent.identity.as_str()).collect();
        assert_eq!(names, ["/root", "/root/review", "/root/review/tests"]);
        let mail = registry.take_mailbox(&nested).unwrap();
        assert_eq!(mail[0].sender, child);
        assert_eq!(mail[0].content, AgentMailContent::Task("check tests".into()));
    }

    #[test]
    fn idle_mail_does_not_start_a_turn_and_followup_preserves_queued_input() {
        let mut registry = AgentRegistry::new(limits(2, 4), 16_384).unwrap();
        let root = root(&registry);
        let child = registry.register_child(&root, "review", "first").unwrap();
        registry.take_mailbox(&child).unwrap();
        registry
            .settle_turn(&child, &AgentCompletion::Finished("answer".into()))
            .unwrap();
        registry
            .send_message(&root, &child.agent, "additional context")
            .unwrap();
        let idle = registry.get(&child.agent).unwrap();
        assert_eq!(idle.state, AgentState::Idle);
        assert_eq!(idle.turn, child.turn);
        assert!(matches!(
            registry.take_mailbox(&child),
            Err(RegistryError::InactiveTurn)
        ));

        let followup = registry.followup_task(&root, &child.agent, "second task").unwrap();
        assert_ne!(followup.turn, child.turn);
        let mail = registry.take_mailbox(&followup).unwrap();
        assert_eq!(mail[0].content, AgentMailContent::Message("additional context".into()));
        assert_eq!(mail[1].content, AgentMailContent::Task("second task".into()));
        assert!(matches!(
            registry.settle_turn(&child, &AgentCompletion::Interrupted),
            Err(RegistryError::StaleTurn)
        ));
        let root_mail = registry.take_mailbox(&root).unwrap();
        assert_eq!(root_mail.len(), 1);
        assert_eq!(root_mail[0].sender, child);
    }

    #[test]
    fn mail_waits_wake_without_new_turns_and_mail_is_only_taken_at_safe_boundaries() {
        let mut registry = AgentRegistry::new(limits(2, 5), 16_384).unwrap();
        let root = root(&registry);
        let child = registry.register_child(&root, "review", "task").unwrap();
        // Do not lose a wakeup when mail arrived before the wait was registered.
        registry.set_phase(&child, AgentPhase::WaitingForMailbox).unwrap();
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::Runnable)
        );
        registry.take_mailbox(&child).unwrap();
        registry.set_phase(&child, AgentPhase::WaitingForMailbox).unwrap();
        registry.send_message(&root, &child.agent, "wake").unwrap();
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::Runnable)
        );
        registry.take_mailbox(&child).unwrap();
        registry.set_phase(&child, AgentPhase::Inferring).unwrap();
        let followup = registry.followup_task(&root, &child.agent, "while running").unwrap();
        assert_eq!(followup, child);
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::Inferring)
        );
        assert!(matches!(registry.take_mailbox(&child), Err(RegistryError::NotRunnable)));
        registry.set_phase(&child, AgentPhase::WaitingForClientOutputs).unwrap();
        registry
            .send_message(&root, &child.agent, "queued during client wait")
            .unwrap();
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::WaitingForClientOutputs)
        );
        registry.set_phase(&child, AgentPhase::Runnable).unwrap();
        assert_eq!(registry.take_mailbox(&child).unwrap().len(), 2);
    }

    #[test]
    fn settlement_and_followup_rejections_leave_state_unchanged() {
        let mut registry = AgentRegistry::new(limits(2, 1), 16_384).unwrap();
        let root = root(&registry);
        let child = registry.register_child(&root, "review", "task").unwrap();
        registry.take_mailbox(&child).unwrap();
        registry.send_message(&root, &root.agent, "fill parent").unwrap();
        let result = AgentCompletion::Interrupted;
        assert!(matches!(
            registry.settle_turn(&child, &result),
            Err(RegistryError::MailboxFull)
        ));
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::Runnable)
        );
        registry.take_mailbox(&root).unwrap();
        registry.settle_turn(&child, &result).unwrap();
        assert!(matches!(
            registry.settle_turn(&child, &result),
            Err(RegistryError::InactiveTurn)
        ));
        assert_eq!(registry.get(&root.agent).unwrap().queued_messages, 1);
        registry.send_message(&root, &child.agent, "fill child").unwrap();
        assert!(matches!(
            registry.followup_task(&root, &child.agent, "retry"),
            Err(RegistryError::MailboxFull)
        ));
        assert_eq!(registry.get(&child.agent).unwrap().state, AgentState::Interrupted);
        assert_eq!(registry.get(&child.agent).unwrap().turn, child.turn);
        assert!(matches!(
            registry.followup_task(&root, &root.agent, "invalid"),
            Err(RegistryError::RootFollowup)
        ));
    }

    #[test]
    fn shared_byte_budget_is_reserved_before_mutation_and_not_refunded_on_transfer() {
        let budget = ExecutorResponseBudget::with_limit(4096);
        let mut registry = AgentRegistry::with_budget(limits(4, 4), budget.clone()).unwrap();
        let root = root(&registry);
        let child = registry.register_child(&root, "review", "task").unwrap();
        let used = budget.used();
        registry.take_mailbox(&child).unwrap();
        assert_eq!(budget.used(), used);
        budget.consume(4096 - used).unwrap(); // Other engine work exhausts the shared ceiling.
        assert!(matches!(
            registry.send_message(&root, &child.agent, ""),
            Err(RegistryError::Budget(_))
        ));
        assert!(matches!(
            registry.register_child(&root, "extra", ""),
            Err(RegistryError::Budget(_))
        ));
        assert!(matches!(
            registry.settle_turn(&child, &AgentCompletion::Failed("failed".into())),
            Err(RegistryError::Budget(_))
        ));
        assert_eq!(registry.agents().count(), 2);
        assert_eq!(registry.get(&child.agent).unwrap().queued_messages, 0);
        assert_eq!(registry.get(&root.agent).unwrap().queued_messages, 0);
        assert_eq!(
            registry.get(&child.agent).unwrap().state,
            AgentState::Active(AgentPhase::Runnable)
        );
        assert_eq!(budget.used(), 4096);
        assert!(matches!(
            AgentRegistry::new(limits(1, 1), 0),
            Err(RegistryError::Budget(_))
        ));
    }
}
