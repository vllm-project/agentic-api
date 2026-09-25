//! Model evidence uses the accumulator's existing response lifecycle and budget.

use super::{ResponseAccumulator, Validation};
use crate::executor::error::ExecutorResult;
use crate::executor::response_budget::RETAINED_CONTAINER_OVERHEAD_BYTES;
use crate::types::upstream_identity::{UpstreamModelError, UpstreamModelId};

impl ResponseAccumulator {
    pub(super) fn observe_response_model(
        &mut self,
        model: Option<&UpstreamModelId>,
        invalid: bool,
        terminal: bool,
    ) -> ExecutorResult<()> {
        if invalid {
            if self.validation == Validation::Strict {
                return Err(UpstreamModelError::Invalid.into());
            }
            self.model_evidence_invalidated = true;
        }
        self.observe_upstream_model(model, terminal)
    }

    pub(super) fn observe_upstream_model(
        &mut self,
        model: Option<&UpstreamModelId>,
        terminal: bool,
    ) -> ExecutorResult<()> {
        if let Some(model) = model {
            match &self.upstream_model {
                Some(first) if first != model => match self.validation {
                    Validation::Strict => return Err(UpstreamModelError::Changed.into()),
                    Validation::Lenient => self.model_evidence_invalidated = true,
                },
                None => {
                    if let Some(budget) = &self.budget {
                        budget.consume(RETAINED_CONTAINER_OVERHEAD_BYTES + model.as_str().len())?;
                    }
                    self.upstream_model = Some(model.clone());
                }
                Some(_) => {}
            }
        }
        self.terminal_model_reported = terminal && model.is_some();
        Ok(())
    }

    /// Only explicit terminal metadata is evidence; never use the request or EOF.
    pub(in crate::executor) fn take_upstream_model(&mut self) -> Option<UpstreamModelId> {
        if self.terminal_model_reported && !self.model_evidence_invalidated {
            self.upstream_model.take()
        } else {
            None
        }
    }

    pub(super) fn upstream_model_retained_bytes(&self) -> usize {
        self.upstream_model
            .as_ref()
            .map_or(0, |model| RETAINED_CONTAINER_OVERHEAD_BYTES + model.as_str().len())
    }
}
