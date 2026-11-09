//! Immutable compaction work and generation-checked canonical replacement.
use crate::executor::compaction::{compact_items, estimate_history_tokens};
use crate::executor::error::ExecutorResult;
use crate::executor::inference::UpstreamHeaders;
use crate::executor::pending_calls::CallLedger;
use crate::executor::request::ExecutionContext;
use crate::types::agent::AgentIdentity;
use crate::types::io::input::async_records;
use crate::types::io::{InputItem, ResponseUsage, ResponsesInput};
use crate::types::request_response::RequestPayload;

pub struct CompactionPlan {
    agent: AgentIdentity,
    generation: u64,
    prefix: Vec<InputItem>,
    request: RequestPayload,
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
        let Some(threshold) = request.compact_threshold() else {
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
        let prefix = &history[..CallLedger::scan(history)?.resolved_prefix_len()];
        Ok((!prefix.is_empty()).then(|| Self {
            agent: agent.clone(),
            generation,
            prefix: prefix.to_vec(),
            request: RequestPayload {
                tools: request.tools.clone(),
                ..request.summary_settings()
            },
        }))
    }

    pub(in crate::executor) async fn execute(
        self,
        exec: &ExecutionContext,
        auth: Option<&UpstreamHeaders>,
    ) -> ExecutorResult<CompactionResult> {
        let prefix_len = self.prefix.len();
        let discovery = self
            .prefix
            .iter()
            .filter(|item| matches!(item, InputItem::McpListTools(_)))
            .cloned()
            .collect::<Vec<_>>();
        // Async calls and their last outputs stay with the summary: an open call's output can still
        // arrive, and a further output for an answered call still finds its call. The model view
        // shows them only while the call is open or answered again after the summary.
        let async_records: Vec<InputItem> = self
            .prefix
            .iter()
            .zip(async_records(&self.prefix))
            .filter(|(_, record)| *record)
            .map(|(item, _)| item.clone())
            .collect();
        let window = compact_items(&self.request, ResponsesInput::from(self.prefix), exec, auth).await?;
        let mut replacement = window.retained;
        replacement.extend(async_records);
        replacement.push(InputItem::Compaction(window.compaction));
        replacement.extend(discovery);
        Ok(CompactionResult {
            agent: self.agent,
            generation: self.generation,
            prefix_len,
            replacement,
            usage: window.usage,
            service_tier: window.service_tier,
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
        let mut history = vec![InputItem::CompactionTrigger, InputItem::CompactionTrigger];
        assert_eq!(result().commit(8, &mut history), CompactionCommit::Stale);
        assert_eq!(history.len(), 2);
        assert_eq!(result().commit(7, &mut history), CompactionCommit::Applied);
        assert!(matches!(history.as_slice(), [InputItem::CompactionTrigger]));
    }
}
