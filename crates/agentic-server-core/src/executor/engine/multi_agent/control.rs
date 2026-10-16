//! Apply live input synchronously in the same owner that finalizes the tree.

use super::MultiAgentRun;
use crate::executor::multi_agent::{
    AgentPhase, AgentState, RunControlReceiver,
    control::{OutputCommand, OutputDecision},
};
use crate::types::{
    agent::AgentTurnKey,
    client_calls::{ClientToolOutput, ClientToolOutputBatch},
    io::InputItem,
};

pub(super) async fn receive(control: &mut Option<RunControlReceiver>) -> Option<OutputCommand> {
    match control {
        Some(control) => control.recv().await,
        None => std::future::pending().await,
    }
}

impl MultiAgentRun {
    pub(super) fn accept_live_outputs(&mut self, input: ClientToolOutputBatch) -> OutputDecision {
        let ids = input
            .outputs
            .iter()
            .map(|output| output.call_id().to_owned())
            .collect::<Vec<_>>();
        if let Err(rejected) = self.pending.accept_outputs(input) {
            return OutputDecision::Rejected(rejected);
        }
        for id in ids {
            let routed = self
                .pending
                .take_accepted(&id)
                .expect("validated output remains until transfer");
            let identity = routed.owner.agent_turn.agent;
            let agent = self.registry.get(&identity).expect("registered call owner exists");
            let (state, turn) = (agent.state, agent.turn);
            let context = self
                .contexts
                .get_mut(&identity)
                .expect("registered call owner has context");
            context.discovery_dirty |= matches!(routed.output, ClientToolOutput::ToolSearch(_));
            let item = InputItem::from(routed.output);
            // A running round's completion replaces history with its own input; keep this output
            // for after that. A compaction is discarded instead (see `generation`).
            if state == AgentState::Active(AgentPhase::Inferring) && !context.compacting {
                context.inputs_during_round.push(item.clone());
            }
            context.stored.history.push(item);
            // Invalidate an in-flight summary snapshot; never overwrite new input.
            context.generation += 1;
            // Pending async calls never hold their owner; it resumes once its synchronous calls resolve.
            if state == AgentState::Active(AgentPhase::WaitingForClientOutputs)
                && !self
                    .pending
                    .awaiting_outputs()
                    .any(|call| call.owner.agent_turn.agent == identity)
            {
                let turn = AgentTurnKey { agent: identity, turn };
                // Membership, current turn, and active phase were checked above.
                self.registry
                    .set_phase(&turn, AgentPhase::Runnable)
                    .expect("validated active owner");
            }
        }
        // As in a continuation request, a late async output is new information for its idle owner,
        // which reads it in a new turn (a subagent within the concurrency limit). A busy or waiting
        // owner reads it at its next turn.
        self.resume_unread_agents();
        OutputDecision::Accepted
    }
}
