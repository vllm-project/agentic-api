//! Include non-wire reasoning provenance in checkpoint retention accounting.

use super::{ResponseCheckpoint, ResponseContinuation, RetainedCheckpoint, aggregate_budget_error};
use crate::executor::ExecutorResult;
use crate::types::io::InputItem;
use crate::types::reasoning_replay::REASONING_PROVENANCE_RETAINED_BYTES;
use crate::utils::common::serialized_size_up_to;

impl ResponseContinuation {
    /// A durable fallback becomes a live pinned parent before inference. It must
    /// share the aggregate budget instead of bypassing it through storage.
    pub(crate) fn retain_parent(&self, checkpoint: ResponseCheckpoint) -> ExecutorResult<RetainedCheckpoint> {
        let bytes = if let Some(budget) = &self.budget {
            checkpoint
                .retained_size_up_to(budget.limit.get())?
                .ok_or_else(aggregate_budget_error)?
        } else {
            0 // Standalone sessions retain their existing per-completion policy.
        };
        self.retain(checkpoint, bytes)
    }
}

impl ResponseCheckpoint {
    pub(super) fn retained_size_up_to(&self, limit: usize) -> Result<Option<usize>, serde_json::Error> {
        let internal_bytes = self.history.iter().fold(0_usize, |bytes, item| {
            if matches!(item, InputItem::Reasoning(_)) {
                bytes.saturating_add(REASONING_PROVENANCE_RETAINED_BYTES)
            } else {
                bytes
            }
        });
        let Some(wire_limit) = limit.checked_sub(internal_bytes) else {
            return Ok(None);
        };
        Ok(serialized_size_up_to(self, wire_limit)?.map(|bytes| bytes + internal_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::session::{ResponseSession, ResponseSessionGroup};
    use crate::storage::{InOutItem, ResponseMetadata};
    use crate::types::io::ReasoningOutput;
    use crate::types::reasoning_replay::ReasoningProvenance;
    use std::num::NonZeroUsize;
    use std::sync::atomic::Ordering;

    fn checkpoint(provenance: Option<ReasoningProvenance>) -> ResponseCheckpoint {
        let mut reasoning = ReasoningOutput::new("rs_1");
        reasoning.replay_provenance = provenance;
        ResponseCheckpoint {
            response_id: "resp_1".to_owned(),
            conversation_id: None,
            history: vec![InputItem::Reasoning(reasoning)],
            metadata: ResponseMetadata::default(),
            durable: false,
        }
    }

    #[test]
    fn non_wire_provenance_has_a_fixed_charge_even_when_unknown() {
        for provenance in [None, Some(ReasoningProvenance::client_submitted())] {
            let checkpoint = checkpoint(provenance);
            let wire = serialized_size_up_to(&checkpoint, usize::MAX).unwrap().unwrap();
            let retained = wire + REASONING_PROVENANCE_RETAINED_BYTES;
            assert!(checkpoint.retained_size_up_to(wire).unwrap().is_none());
            assert!(checkpoint.retained_size_up_to(retained - 1).unwrap().is_none());
            assert_eq!(checkpoint.retained_size_up_to(retained).unwrap(), Some(retained));
            let session = ResponseSession::new(NonZeroUsize::new(10).unwrap(), NonZeroUsize::new(wire).unwrap());
            let lease = session.begin(None).unwrap();
            assert!(
                lease
                    .checkpoint(
                        checkpoint.response_id,
                        None,
                        &checkpoint.metadata,
                        &checkpoint.history.into_iter().map(InOutItem::Input).collect::<Vec<_>>(),
                        false
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn pinned_parent_and_abandoned_candidate_refund_non_wire_bytes() {
        let checkpoint = checkpoint(Some(ReasoningProvenance::client_submitted()));
        let bytes = checkpoint.retained_size_up_to(usize::MAX).unwrap().unwrap();
        let group = ResponseSessionGroup::new(
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(10).unwrap(),
            NonZeroUsize::new(bytes).unwrap(),
            NonZeroUsize::new(bytes).unwrap(),
        );
        let session = group.new_session().unwrap();
        let lease = session.begin(None).unwrap();
        let retained = lease.retain_parent(checkpoint).unwrap();
        assert_eq!(group.budget.used.load(Ordering::Acquire), bytes);
        assert!(lease.retain_parent(super::tests::checkpoint(None)).is_err());
        drop(retained);
        assert_eq!(group.budget.used.load(Ordering::Acquire), 0);
        let retained = lease
            .checkpoint(
                "resp_1".to_owned(),
                None,
                &ResponseMetadata::default(),
                &[InOutItem::Input(InputItem::Reasoning(ReasoningOutput::new("rs_1")))],
                false,
            )
            .unwrap();
        assert_eq!(group.budget.used.load(Ordering::Acquire), bytes);
        drop(retained);
        assert_eq!(group.budget.used.load(Ordering::Acquire), 0);
    }
}
