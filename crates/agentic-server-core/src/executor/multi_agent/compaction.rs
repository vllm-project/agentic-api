//! Immutable compaction work and generation-checked canonical replacement.
use crate::executor::compaction::{CompactedWindow, estimate_history_tokens, summarize_items};
use crate::executor::error::ExecutorResult;
use crate::executor::inference::UpstreamHeaders;
use crate::executor::pending_calls::CallLedger;
use crate::executor::request::ExecutionContext;
use crate::executor::telemetry::stages::CompactionTrigger;
use crate::types::agent::AgentIdentity;
use crate::types::io::input::async_records;
use crate::types::io::{CompactionItem, InputItem, ResponseUsage, ResponsesInput};
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
    pub(in crate::executor) window: Box<CompactedWindow>,
    carried: Vec<InputItem>,
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
        let carried = self
            .prefix
            .iter()
            .zip(async_records(&self.prefix))
            .filter_map(|(item, keep)| keep.then_some(item))
            .cloned()
            .collect();
        let window = summarize_items(
            &self.request,
            ResponsesInput::from(self.prefix),
            exec,
            auth,
            CompactionTrigger::InputItem,
        )
        .await?;
        Ok(CompactionResult {
            agent: self.agent,
            generation: self.generation,
            prefix_len,
            window: Box::new(window),
            carried,
        })
    }
}

impl CompactionResult {
    pub(in crate::executor) fn agent(&self) -> &AgentIdentity {
        &self.agent
    }
    pub(in crate::executor) fn usage(&self) -> ResponseUsage {
        self.window.usage
    }

    /// Apply a current result and return the exact checkpoint committed, or leave history untouched when stale.
    pub(in crate::executor) fn commit(self, generation: u64, history: &mut Vec<InputItem>) -> Option<CompactionItem> {
        if self.generation != generation || self.prefix_len > history.len() {
            return None;
        }
        let compaction = self.window.compaction;
        let mut replacement = self.window.retained;
        replacement.extend(self.carried);
        replacement.push(InputItem::Compaction(compaction.clone()));
        history.splice(..self.prefix_len, replacement);
        Some(compaction)
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
            window: Box::new(CompactedWindow {
                retained: vec![],
                compaction: CompactionItem {
                    agent: None,
                    id: Some("cmp_new".into()),
                    encrypted_content: "summary".into(),
                },
                usage: ResponseUsage::default(),
                service_tier: None,
            }),
            carried: Vec::new(),
        };
        let suffix = InputItem::Compaction(CompactionItem {
            agent: None,
            id: Some("cmp_suffix".into()),
            encrypted_content: "later checkpoint".into(),
        });
        let mut history = vec![InputItem::CompactionTrigger, suffix];
        let before = serde_json::to_value(&history).unwrap();
        assert!(result().commit(8, &mut history).is_none());
        assert_eq!(serde_json::to_value(&history).unwrap(), before);
        let mut too_long = result();
        too_long.prefix_len = 3;
        assert!(too_long.commit(7, &mut history).is_none());
        assert_eq!(serde_json::to_value(&history).unwrap(), before);
        let committed = result().commit(7, &mut history).expect("current summary applies");
        assert_eq!(committed.id.as_deref(), Some("cmp_new"));
        assert_eq!(serde_json::to_value(&history[0]).unwrap()["id"], "cmp_new");
        assert_eq!(serde_json::to_value(&history[1]).unwrap(), before[1]);
    }
}
