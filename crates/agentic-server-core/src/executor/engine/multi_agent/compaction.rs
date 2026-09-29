//! Explicit root compaction uses the same safe-prefix plan as automatic compaction.
use super::{MultiAgentRun, invalid, registry_error, restore_agents};
use crate::executor::{
    error::ExecutorResult,
    multi_agent::{
        CheckpointLimits, CompactionCommit, CompactionPlan, ValidatedTreeCheckpoint, collaboration::attribution,
    },
    request::{ExecutionContext, RequestContext},
    response_budget::ExecutorResponseBudget,
};
use crate::types::{
    agent_tree::StoredTreeSnapshot,
    io::{CompactionItem, InputItem, ResponseUsage},
};

impl MultiAgentRun {
    // No agent tasks are running during this operation: the restored snapshot is
    // exclusively owned until the caller persists the resulting response and tree.
    pub(crate) async fn compact_root(
        ctx: &mut RequestContext,
        exec: &ExecutionContext,
        auth: Option<&str>,
    ) -> ExecutorResult<(CompactionItem, ResponseUsage)> {
        let budget = ExecutorResponseBudget::with_limit(exec.responses_config.max_retained_bytes);
        let mut restored = restore_agents(ctx, exec, &budget)?;
        ctx.new_input_items.retain(|item| !item.is_compaction_trigger());
        if restored.continuing_tree && !ctx.new_input_items.is_empty() {
            restored.continue_input(&ctx.response_id, &ctx.new_input_items)?;
        }
        let root = restored
            .agents
            .iter_mut()
            .find(|agent| agent.identity.is_root())
            .expect("validated tree has a root");
        root.history.retain(|item| !item.is_compaction_trigger());
        let plan = CompactionPlan::prepare_explicit(&root.identity, 0, &root.history, &ctx.enriched_request)?
            .ok_or_else(|| invalid("no resolved root context available for compaction"))?;
        let result = plan.execute(exec, auth).await?;
        let usage = result.usage();
        if result.commit(0, &mut root.history) != CompactionCommit::Applied {
            return Err(invalid("root context changed during explicit compaction"));
        }
        let compaction = root
            .history
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                InputItem::Compaction(item) => Some(item),
                _ => None,
            })
            .expect("compaction plan inserts a compaction item");
        compaction.agent = Some(attribution(&root.identity));
        let compaction = compaction.clone();
        for agent in &mut restored.agents {
            restored.registry.checkpoint_agent(agent).map_err(registry_error)?;
        }
        let config = ctx
            .enriched_request
            .multi_agent
            .clone()
            .expect("multi-agent request validated");
        ctx.multi_agent_tree = Some(ValidatedTreeCheckpoint::from_stored(
            StoredTreeSnapshot {
                version: 1,
                config,
                agents: restored.agents,
                client_calls: restored.pending.checkpoint(),
            },
            &CheckpointLimits::for_response(exec.responses_config.max_retained_bytes),
        )?);
        Ok((compaction, usage))
    }
}
