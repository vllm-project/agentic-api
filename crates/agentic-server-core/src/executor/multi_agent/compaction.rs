//! Immutable compaction work and generation-checked canonical replacement.
use crate::executor::compaction::{compact_items, estimate_history_tokens};
use crate::executor::error::ExecutorResult;
use crate::executor::pending_calls::resolved_prefix_len;
use crate::executor::request::ExecutionContext;
use crate::types::agent::AgentIdentity;
use crate::types::io::input::open_async_calls;
use crate::types::io::{InputItem, ResponseUsage, ResponsesInput};
use crate::types::request_response::{PromptCacheRetention, RequestPayload};

pub struct CompactionPlan {
    agent: AgentIdentity,
    generation: u64,
    prefix: Vec<InputItem>,
    model: String,
    instructions: Option<String>,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<PromptCacheRetention>,
    service_tier: Option<String>,
}

pub struct CompactionResult {
    pub(super) agent: AgentIdentity,
    pub(super) generation: u64,
    pub(super) prefix_len: usize,
    pub(super) replacement: Vec<InputItem>,
    pub(super) usage: ResponseUsage,
    pub(in crate::executor) service_tier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionCommit {
    Applied,
    Stale,
}

impl CompactionPlan {
    pub(in crate::executor) fn prepare(
        agent: &AgentIdentity,
        generation: u64,
        history: &[InputItem],
        request: &RequestPayload,
    ) -> ExecutorResult<Option<Self>> {
        let threshold = request
            .context_management
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|entry| entry.type_ == "compaction")
            .and_then(|entry| entry.compact_threshold);
        let Some(threshold) = threshold else {
            return Ok(None);
        };
        if estimate_history_tokens(history) <= threshold {
            return Ok(None);
        }
        Self::prepare_explicit(agent, generation, history, request)
    }

    /// Capture only the resolved prefix; pending calls remain outside the snapshot.
    pub(in crate::executor) fn prepare_explicit(
        agent: &AgentIdentity,
        generation: u64,
        history: &[InputItem],
        request: &RequestPayload,
    ) -> ExecutorResult<Option<Self>> {
        let end = resolved_prefix_len(history)?;
        if end == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            agent: agent.clone(),
            generation,
            prefix: history[..end].to_vec(),
            model: request.model.clone(),
            instructions: request.instructions.clone(),
            prompt_cache_key: request.prompt_cache_key.clone(),
            prompt_cache_retention: request.prompt_cache_retention,
            service_tier: request.service_tier.clone(),
        }))
    }

    pub(in crate::executor) async fn execute(
        self,
        exec: &ExecutionContext,
        auth: Option<&str>,
    ) -> ExecutorResult<CompactionResult> {
        let prefix_len = self.prefix.len();
        let discovery = self
            .prefix
            .iter()
            .filter(|item| matches!(item, InputItem::McpListTools(_)))
            .cloned()
            .collect::<Vec<_>>();
        // Pending async calls stay verbatim after the summary so their outputs can still arrive.
        let open_async_calls = open_async_calls(&self.prefix);
        let request = RequestPayload {
            model: self.model,
            instructions: self.instructions,
            prompt_cache_key: self.prompt_cache_key,
            prompt_cache_retention: self.prompt_cache_retention,
            service_tier: self.service_tier,
            ..Default::default()
        };
        let (mut replacement, usage, service_tier) =
            compact_items(&request, ResponsesInput::Items(self.prefix), exec, auth).await?;
        replacement.extend(discovery);
        replacement.extend(open_async_calls);
        Ok(CompactionResult {
            agent: self.agent,
            generation: self.generation,
            prefix_len,
            replacement,
            usage,
            service_tier,
        })
    }
}

impl CompactionResult {
    pub(in crate::executor) fn agent(&self) -> &AgentIdentity {
        &self.agent
    }
    pub(in crate::executor) fn usage(&self) -> ResponseUsage {
        self.usage
    }

    pub(in crate::executor) fn commit(self, generation: u64, history: &mut Vec<InputItem>) -> CompactionCommit {
        if self.generation != generation || self.prefix_len > history.len() {
            return CompactionCommit::Stale;
        }
        history.splice(..self.prefix_len, self.replacement);
        CompactionCommit::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_summary_preserves_history_and_applied_summary_preserves_suffix() {
        let result = || CompactionResult {
            agent: AgentIdentity::root(),
            generation: 7,
            prefix_len: 1,
            replacement: vec![],
            usage: ResponseUsage::default(),
            service_tier: None,
        };
        let mut history = vec![InputItem::Unknown, InputItem::CompactionTrigger];
        assert_eq!(result().commit(8, &mut history), CompactionCommit::Stale);
        assert_eq!(history.len(), 2);
        assert_eq!(result().commit(7, &mut history), CompactionCommit::Applied);
        assert!(matches!(history.as_slice(), [InputItem::CompactionTrigger]));
    }
}
