//! Coordinator-owned client-call routing, independent of an agent's current turn.
//!
//! The existing executor `pending_calls` module validates a single canonical item
//! sequence during rehydration. This table tracks concurrent owners and retained
//! output acceptance; it neither reparses item history nor changes agent phases.
//! Its batch decisions are internal policy, not new HTTP/WebSocket error schemas.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use indexmap::IndexMap;

use super::{AgentRegistry, AgentState};
use crate::executor::error::ExecutorError;
use crate::executor::response_budget::{ExecutorResponseBudget, RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize};
use crate::types::agent_tree::StoredClientCall;
use crate::types::client_calls::{
    ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration, ClientCallResolution, ClientToolOutput,
    ClientToolOutputBatch, RoutedClientOutput,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientCallError {
    #[error("response identity must not be empty")]
    EmptyResponseId,
    #[error("client outputs target a different response")]
    WrongResponse,
    #[error("client call_id must not be empty")]
    EmptyCallId,
    #[error("client-call table capacity reached")]
    CallLimit,
    #[error("client-output batch exceeds the call-table capacity")]
    BatchLimit,
    #[error("call owner is not an active registered turn")]
    InvalidOwner,
    #[error("duplicate client call_id: {0}")]
    DuplicateCall(String),
    #[error("unknown client call_id: {0}")]
    UnknownCall(String),
    #[error("client call_id already resolved: {0}")]
    AlreadyResolved(String),
    #[error("output for {call_id} has kind {actual:?}; expected {expected:?}")]
    KindMismatch {
        call_id: String,
        expected: ClientCallKind,
        actual: ClientCallKind,
    },
    #[error(transparent)]
    Budget(#[from] Box<ExecutorError>),
}

impl From<ExecutorError> for ClientCallError {
    fn from(error: ExecutorError) -> Self {
        Self::Budget(Box::new(error))
    }
}

/// A rejection returns the untouched typed batch to its owner.
#[derive(Debug, thiserror::Error)]
#[error("client outputs rejected: {reason}")]
pub struct RejectedClientOutputs {
    pub input: ClientToolOutputBatch,
    #[source]
    pub reason: ClientCallError,
}

enum Resolution {
    Pending,
    /// `None` means the accepted output was moved into canonical context. The
    /// ownership record remains, preventing duplicate acceptance and ID reuse.
    Resolved {
        output: Option<Box<ClientToolOutput>>,
    },
}

struct CallRecord {
    owner: ClientCallOwner,
    resolution: Resolution,
}

#[derive(Debug)]
pub struct ClientCallView<'a> {
    pub call_id: &'a ClientCallId,
    pub owner: &'a ClientCallOwner,
    pub resolution: ClientCallResolution,
    pub output: Option<&'a ClientToolOutput>,
}

/// Mutated only by the coordinator, with no await between validation and commit.
/// Calls must be registered before their completed call items become visible.
pub struct PendingClientCalls {
    response_id: String,
    calls: IndexMap<ClientCallId, CallRecord>,
    max_calls: NonZeroUsize,
    budget: ExecutorResponseBudget,
}

impl PendingClientCalls {
    /// Create a table with an independent cumulative byte ceiling. Engine
    /// integration shares its response budget through `with_budget`.
    /// `max_calls` includes resolved records retained for duplicate detection.
    ///
    /// # Errors
    /// Rejects an empty response ID or insufficient retained-byte capacity.
    pub fn new(
        response_id: String,
        max_calls: NonZeroUsize,
        max_retained_bytes: usize,
    ) -> Result<Self, ClientCallError> {
        Self::with_budget(
            response_id,
            max_calls,
            ExecutorResponseBudget::with_limit(max_retained_bytes),
        )
    }

    pub(in crate::executor) fn with_budget(
        response_id: String,
        max_calls: NonZeroUsize,
        budget: ExecutorResponseBudget,
    ) -> Result<Self, ClientCallError> {
        if response_id.is_empty() {
            return Err(ClientCallError::EmptyResponseId);
        }
        budget.consume(RETAINED_CONTAINER_OVERHEAD_BYTES.saturating_add(response_id.len()))?;
        Ok(Self {
            response_id,
            calls: IndexMap::new(),
            max_calls,
            budget,
        })
    }

    /// Register a classified batch of client calls without partial insertion.
    /// Registration checks the current turn; later output acceptance deliberately
    /// uses the stored owner even if that turn is interrupted or superseded.
    ///
    /// # Errors
    /// Rejects duplicates, unknown/inactive/superseded owners, or capacity exhaustion.
    pub fn register_calls(
        &mut self,
        agents: &AgentRegistry,
        calls: &[ClientCallRegistration],
    ) -> Result<(), ClientCallError> {
        if calls.len() > self.max_calls.get() - self.calls.len() {
            return Err(ClientCallError::CallLimit);
        }
        let mut seen = HashSet::with_capacity(calls.len());
        let mut bytes = 0usize;
        for call in calls {
            if self.calls.contains_key(&call.call_id) || !seen.insert(&call.call_id) {
                return Err(ClientCallError::DuplicateCall(call.call_id.as_str().into()));
            }
            let owner = agents
                .get(&call.owner.agent_turn.agent)
                .ok_or(ClientCallError::InvalidOwner)?;
            if owner.turn != call.owner.agent_turn.turn || !matches!(owner.state, AgentState::Active(_)) {
                return Err(ClientCallError::InvalidOwner);
            }
            bytes = bytes.saturating_add(registration_bytes(call));
        }
        self.budget.consume(bytes)?;
        for call in calls {
            self.calls.insert(
                call.call_id.clone(),
                CallRecord {
                    owner: call.owner.clone(),
                    resolution: Resolution::Pending,
                },
            );
        }
        Ok(())
    }

    pub(in crate::executor) fn restore(
        response_id: String,
        records: &[StoredClientCall],
        agents: &AgentRegistry,
        max_calls: NonZeroUsize,
        budget: ExecutorResponseBudget,
    ) -> Result<Self, ClientCallError> {
        let mut table = Self::with_budget(response_id, max_calls, budget)?;
        if records.len() > max_calls.get() {
            return Err(ClientCallError::CallLimit);
        }
        for call in records {
            if agents.get(&call.owner.agent_turn.agent).is_none() {
                return Err(ClientCallError::InvalidOwner);
            }
            if table.calls.contains_key(&call.call_id) {
                return Err(ClientCallError::DuplicateCall(call.call_id.as_str().into()));
            }
            table.budget.consume(registration_bytes(&ClientCallRegistration {
                call_id: call.call_id.clone(),
                owner: call.owner.clone(),
            }))?;
            table.calls.insert(
                call.call_id.clone(),
                CallRecord {
                    owner: call.owner.clone(),
                    resolution: if call.resolved {
                        Resolution::Resolved { output: None }
                    } else {
                        Resolution::Pending
                    },
                },
            );
        }
        Ok(table)
    }

    pub(in crate::executor) fn checkpoint(&self) -> Vec<StoredClientCall> {
        self.calls
            .iter()
            .map(|(id, call)| StoredClientCall {
                call_id: id.clone(),
                owner: call.owner.clone(),
                resolved: !matches!(call.resolution, Resolution::Pending),
            })
            .collect()
    }

    #[must_use]
    pub fn get(&self, call_id: &str) -> Option<ClientCallView<'_>> {
        self.calls.get_key_value(call_id).map(|(id, record)| view(id, record))
    }

    /// Outstanding calls in registration order, across all agents and turns.
    pub fn pending(&self) -> impl Iterator<Item = ClientCallView<'_>> {
        self.calls
            .iter()
            .filter(|(_, record)| matches!(record.resolution, Resolution::Pending))
            .map(|(id, record)| view(id, record))
    }

    /// Validate the whole batch, reserve bytes, then retain every output exactly
    /// once. A partial batch may resolve a subset of pending calls. No current
    /// agent-state lookup or implicit resumption occurs here; transport policy
    /// and model visibility for old turns remain coordinator decisions.
    ///
    /// # Errors
    /// Returns the intact batch on any validation or budget failure, without
    /// resolving even its valid prefix. These are not reference API error codes.
    pub fn accept_outputs(&mut self, batch: ClientToolOutputBatch) -> Result<(), RejectedClientOutputs> {
        let validation = self
            .validate_batch(&batch)
            .and_then(|bytes| self.budget.consume(bytes).map_err(Into::into));
        if let Err(reason) = validation {
            return Err(RejectedClientOutputs { input: batch, reason });
        }
        self.commit_outputs(batch.outputs);
        Ok(())
    }

    /// Move an accepted output to canonical context once, retaining ownership
    /// and resolution metadata. Only the coordinator calls this, applying the
    /// returned value before its next await/acknowledgement. Its byte reservation
    /// follows the transfer. Unknown, pending or already transferred IDs return
    /// `None`; this method never restarts a cancelled task.
    pub fn take_accepted(&mut self, call_id: &str) -> Option<RoutedClientOutput> {
        let record = self.calls.get_mut(call_id)?;
        let Resolution::Resolved { output } = &mut record.resolution else {
            return None;
        };
        let output = *output.take()?;
        Some(RoutedClientOutput {
            owner: record.owner.clone(),
            output,
        })
    }

    fn validate_batch(&self, batch: &ClientToolOutputBatch) -> Result<usize, ClientCallError> {
        if batch.response_id != self.response_id {
            return Err(ClientCallError::WrongResponse);
        }
        if batch.outputs.len() > self.max_calls.get() {
            return Err(ClientCallError::BatchLimit);
        }
        let mut seen = HashSet::with_capacity(batch.outputs.len());
        let mut bytes = 0usize;
        for output in &batch.outputs {
            let id = output.call_id();
            if id.is_empty() {
                return Err(ClientCallError::EmptyCallId);
            }
            if !seen.insert(id) {
                return Err(ClientCallError::DuplicateCall(id.into()));
            }
            let record = self
                .calls
                .get(id)
                .ok_or_else(|| ClientCallError::UnknownCall(id.into()))?;
            if !matches!(record.resolution, Resolution::Pending) {
                return Err(ClientCallError::AlreadyResolved(id.into()));
            }
            if record.owner.kind != output.kind() {
                return Err(ClientCallError::KindMismatch {
                    call_id: id.into(),
                    expected: record.owner.kind,
                    actual: output.kind(),
                });
            }
            bytes = bytes.saturating_add(output.retained_bytes());
        }
        Ok(bytes)
    }

    fn commit_outputs(&mut self, outputs: Vec<ClientToolOutput>) {
        for output in outputs {
            let record = self
                .calls
                .get_mut(output.call_id())
                .expect("validated batch retains its registered calls");
            record.resolution = Resolution::Resolved {
                output: Some(Box::new(output)),
            };
        }
    }
}

fn view<'a>(call_id: &'a ClientCallId, record: &'a CallRecord) -> ClientCallView<'a> {
    let (resolution, output) = match &record.resolution {
        Resolution::Pending => (ClientCallResolution::Pending, None),
        Resolution::Resolved { output: Some(output) } => (ClientCallResolution::Accepted, Some(output.as_ref())),
        Resolution::Resolved { output: None } => (ClientCallResolution::Transferred, None),
    };
    ClientCallView {
        call_id,
        owner: &record.owner,
        resolution,
        output,
    }
}

fn registration_bytes(call: &ClientCallRegistration) -> usize {
    std::mem::size_of::<(ClientCallId, CallRecord)>()
        .saturating_add(RETAINED_CONTAINER_OVERHEAD_BYTES)
        .saturating_add(call.call_id.as_str().len())
        .saturating_add(call.owner.agent_turn.agent.as_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::multi_agent::{AgentPhase, RegistryLimits};
    use crate::types::agent::{AgentCompletion, AgentIdentity, AgentTurnId, AgentTurnKey};
    use crate::types::io::{
        FunctionToolResultMessage, InputFileContent, InputImageContent, InputItem, ShellCallOutcome,
        ShellCallOutputContent, ShellCallOutputMessage, ToolCallOutput, ToolOutputContent,
    };
    use std::collections::HashMap;

    fn registry() -> (AgentRegistry, AgentTurnKey, AgentTurnKey) {
        let mut agents = AgentRegistry::new(
            RegistryLimits {
                max_agents: NonZeroUsize::new(4).unwrap(),
                max_mailbox_messages: NonZeroUsize::new(8).unwrap(),
            },
            65_536,
        )
        .unwrap();
        let agent = AgentIdentity::root();
        let root = AgentTurnKey {
            turn: agents.get(&agent).unwrap().turn,
            agent,
        };
        let child = agents.register_child(&root, "review", "review").unwrap();
        agents.take_mailbox(&child).unwrap();
        (agents, root, child)
    }

    fn call(id: &str, turn: &AgentTurnKey, kind: ClientCallKind) -> ClientCallRegistration {
        ClientCallRegistration {
            call_id: id.to_owned().try_into().unwrap(),
            owner: ClientCallOwner {
                agent_turn: turn.clone(),
                kind,
            },
        }
    }

    fn function(id: &str) -> ClientToolOutput {
        ClientToolOutput::Function(FunctionToolResultMessage {
            call_id: id.into(),
            output: ToolCallOutput::Text("same output".into()),
        })
    }

    fn shell(id: &str) -> ClientToolOutput {
        ClientToolOutput::Shell(ShellCallOutputMessage {
            id: None,
            call_id: id.into(),
            max_output_length: None,
            status: None,
            extra: HashMap::new(),
            output: vec![ShellCallOutputContent {
                stdout: "same output".into(),
                stderr: String::new(),
                outcome: ShellCallOutcome::Exit { exit_code: 0 },
                extra: HashMap::new(),
            }],
        })
    }

    fn batch(outputs: Vec<ClientToolOutput>) -> ClientToolOutputBatch {
        ClientToolOutputBatch {
            response_id: "resp_test".into(),
            outputs,
        }
    }

    fn table() -> PendingClientCalls {
        PendingClientCalls::new("resp_test".into(), NonZeroUsize::new(8).unwrap(), 65_536).unwrap()
    }

    #[test]
    fn mixed_outputs_route_by_call_identity_and_transfer_once() {
        let (agents, root, child) = registry();
        let mut calls = table();
        let registrations = [
            call("f1", &root, ClientCallKind::Function),
            call("s1", &child, ClientCallKind::Shell),
            call("f2", &child, ClientCallKind::Function),
            call("s2", &root, ClientCallKind::Shell),
        ];
        calls.register_calls(&agents, &registrations).unwrap();
        calls.accept_outputs(batch(vec![shell("s2"), function("f2")])).unwrap();
        assert_eq!(
            calls.pending().map(|call| call.call_id.as_str()).collect::<Vec<_>>(),
            ["f1", "s1"]
        );
        let retry = calls
            .accept_outputs(batch(vec![function("f1"), function("f2")]))
            .unwrap_err();
        assert!(matches!(retry.reason, ClientCallError::AlreadyResolved(_)));
        assert_eq!(calls.get("f1").unwrap().resolution, ClientCallResolution::Pending);
        assert!(calls.get("f2").unwrap().output.is_some());
        calls.accept_outputs(batch(vec![shell("s1"), function("f1")])).unwrap();
        assert_eq!(calls.pending().count(), 0);
        for registration in registrations {
            let id = registration.call_id.as_str();
            let expected: InputItem = calls.get(id).unwrap().output.unwrap().clone().into();
            let resolved = calls.take_accepted(id).unwrap();
            assert_eq!(resolved.owner, registration.owner);
            let input: InputItem = resolved.output.into();
            assert_eq!(
                serde_json::to_value(input).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert!(calls.take_accepted(id).is_none());
            assert_eq!(calls.get(id).unwrap().resolution, ClientCallResolution::Transferred);
        }
        assert!(matches!(
            calls.accept_outputs(batch(vec![function("f1")])).unwrap_err().reason,
            ClientCallError::AlreadyResolved(_)
        ));
        assert!(matches!(
            calls.register_calls(&agents, &[call("f1", &root, ClientCallKind::Function)]),
            Err(ClientCallError::DuplicateCall(_))
        ));
    }

    #[test]
    fn invalid_batches_return_the_input_without_resolving_the_valid_prefix() {
        let (agents, root, _) = registry();
        let mut calls = table();
        calls
            .register_calls(
                &agents,
                &[
                    call("f1", &root, ClientCallKind::Function),
                    call("s1", &root, ClientCallKind::Shell),
                ],
            )
            .unwrap();
        let used = calls.budget.used();
        for invalid in [function("missing"), function("s1"), function("f1"), function("")] {
            let input = batch(vec![function("f1"), invalid]);
            let rejected = calls.accept_outputs(input).unwrap_err();
            assert_eq!(rejected.input.outputs.len(), 2);
            assert_eq!(rejected.input.outputs[0].call_id(), "f1");
            assert_eq!(calls.pending().count(), 2);
            assert_eq!(calls.budget.used(), used);
        }
        let mut wrong_response = batch(vec![function("f1")]);
        wrong_response.response_id = "resp_other".into();
        assert!(matches!(
            calls.accept_outputs(wrong_response).unwrap_err().reason,
            ClientCallError::WrongResponse
        ));
        assert_eq!(calls.pending().count(), 2);
        assert!(ClientCallId::try_from(String::new()).is_err());
        assert!(serde_json::from_str::<ClientCallId>("\"\"").is_err());
    }

    #[test]
    fn interrupted_owners_keep_calls_and_acceptance_does_not_resume_a_followup() {
        let (mut agents, root, child) = registry();
        let mut calls = table();
        calls
            .register_calls(&agents, &[call("old", &child, ClientCallKind::Function)])
            .unwrap();
        agents.settle_turn(&child, &AgentCompletion::Interrupted).unwrap();
        let followup = agents.followup_task(&root, &child.agent, "new task").unwrap();
        agents
            .set_phase(&followup, AgentPhase::WaitingForClientOutputs)
            .unwrap();
        assert!(matches!(
            calls.register_calls(&agents, &[call("invalid", &child, ClientCallKind::Function)]),
            Err(ClientCallError::InvalidOwner)
        ));
        calls
            .register_calls(&agents, &[call("new", &followup, ClientCallKind::Function)])
            .unwrap();
        calls.accept_outputs(batch(vec![function("old")])).unwrap();
        assert_eq!(calls.take_accepted("old").unwrap().owner.agent_turn, child);
        assert_eq!(calls.get("new").unwrap().resolution, ClientCallResolution::Pending);
        assert_eq!(
            agents.get(&followup.agent).unwrap().state,
            AgentState::Active(AgentPhase::WaitingForClientOutputs)
        );
    }

    #[test]
    fn registration_is_atomic_and_retains_resolved_ids_within_its_limit() {
        let (agents, root, child) = registry();
        let mut calls = PendingClientCalls::new("resp_test".into(), NonZeroUsize::new(2).unwrap(), 4096).unwrap();
        let stale = AgentTurnKey {
            agent: child.agent,
            turn: AgentTurnId::new(),
        };
        let used = calls.budget.used();
        assert!(matches!(
            calls.register_calls(
                &agents,
                &[
                    call("valid", &root, ClientCallKind::Function),
                    call("stale", &stale, ClientCallKind::Function)
                ]
            ),
            Err(ClientCallError::InvalidOwner)
        ));
        assert_eq!(calls.pending().count(), 0);
        assert_eq!(calls.budget.used(), used);
        let same = call("same", &root, ClientCallKind::Function);
        assert!(matches!(
            calls.register_calls(&agents, &[same.clone(), same]),
            Err(ClientCallError::DuplicateCall(_))
        ));
        assert_eq!(calls.pending().count(), 0);
        calls
            .register_calls(
                &agents,
                &[
                    call("a", &root, ClientCallKind::Function),
                    call("b", &root, ClientCallKind::Function),
                ],
            )
            .unwrap();
        calls.accept_outputs(batch(vec![function("a")])).unwrap();
        calls.take_accepted("a").unwrap();
        assert!(matches!(
            calls.register_calls(&agents, &[call("c", &root, ClientCallKind::Function)]),
            Err(ClientCallError::CallLimit)
        ));
    }

    #[test]
    fn retained_budget_covers_media_and_shell_extensions_before_batch_commit() {
        let (agents, root, _) = registry();
        let media = [
            ToolOutputContent::InputImage(InputImageContent {
                image_url: Some("x".repeat(2048)),
                ..Default::default()
            }),
            ToolOutputContent::InputFile(InputFileContent {
                file_data: Some("x".repeat(2048)),
                ..Default::default()
            }),
        ];
        let mut outputs: Vec<_> = media
            .into_iter()
            .map(|part| {
                ClientToolOutput::Function(FunctionToolResultMessage {
                    call_id: "large".into(),
                    output: ToolCallOutput::Content(vec![part]),
                })
            })
            .collect();
        let ClientToolOutput::Shell(mut extended) = shell("large") else {
            unreachable!()
        };
        extended.output[0]
            .extra
            .insert("extension".into(), serde_json::json!({"nested": ["x".repeat(2048)]}));
        outputs.push(ClientToolOutput::Shell(extended));
        let ClientToolOutput::Shell(mut stderr) = shell("large") else {
            unreachable!()
        };
        stderr.output[0].stderr = "x".repeat(2048);
        outputs.push(ClientToolOutput::Shell(stderr));

        for output in outputs {
            let budget = ExecutorResponseBudget::with_limit(16_384);
            let mut calls =
                PendingClientCalls::with_budget("resp_test".into(), NonZeroUsize::new(4).unwrap(), budget.clone())
                    .unwrap();
            calls
                .register_calls(
                    &agents,
                    &[
                        call("prefix", &root, ClientCallKind::Function),
                        call("large", &root, output.kind()),
                    ],
                )
                .unwrap();
            budget.consume(16_384 - budget.used() - 512).unwrap();
            let used = budget.used();
            let rejected = calls
                .accept_outputs(batch(vec![function("prefix"), output]))
                .unwrap_err();
            assert!(matches!(rejected.reason, ClientCallError::Budget(_)));
            assert_eq!(rejected.input.outputs.len(), 2);
            assert_eq!(calls.pending().count(), 2);
            assert_eq!(budget.used(), used);
        }
    }
}
