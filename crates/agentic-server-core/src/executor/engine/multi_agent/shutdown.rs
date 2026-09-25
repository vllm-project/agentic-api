//! Root completion stops new work while owned rounds drain through normal ingestion.
use super::MultiAgentRun;
use crate::executor::{error::ExecutorResult, multi_agent::AgentState, pipeline::AgentPipeline};
use crate::types::{agent::AgentTurnKey, agent_commands::AgentTarget};

impl MultiAgentRun {
    pub(super) async fn finish_root(
        &mut self,
        root: &AgentTurnKey,
        pipeline: &mut AgentPipeline,
    ) -> ExecutorResult<()> {
        self.root_finished = true;
        let active = self
            .registry
            .agents()
            .filter(|agent| !agent.identity.is_root() && matches!(agent.state, AgentState::Active(_)))
            .map(|agent| agent.identity.to_string())
            .collect::<Vec<_>>();
        for target in active {
            // Reuse cooperative interruption: resolve mailbox calls immediately,
            // retain client-call ownership, and finish already-started round items.
            // The driver keeps draining frames and joins before terminal publication.
            self.interrupt_agent(root, AgentTarget { target }, pipeline).await?;
        }
        Ok(())
    }
}
