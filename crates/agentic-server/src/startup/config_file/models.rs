//! File configuration and startup validation for explicit model capabilities.

use std::path::Path;

use agentic_core::error::Error;
use agentic_core::types::ServiceTier;
use agentic_core::types::model_capabilities::{InputModalities, ModelOverride, SupportedReasoningLevels};
use serde::{Deserialize, Serialize};

/// Per-model overrides for capabilities the gateway cannot infer from upstream metadata.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ModelFileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<InputModalities>,
    /// Explicit reasoning levels shared by the catalog and Responses admission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_reasoning_levels: Option<SupportedReasoningLevels>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_supported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs_supported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tiers: Option<Vec<ServiceTier>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options_supported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention_supported: Option<bool>,
}

impl ModelFileConfig {
    pub(super) fn is_empty(&self) -> bool {
        self.input_modalities.is_none()
            && self.supported_reasoning_levels.is_none()
            && self.sampling_supported.is_none()
            && self.logprobs_supported.is_none()
            && self.service_tiers.is_none()
            && self.prompt_cache_options_supported.is_none()
            && self.prompt_cache_retention_supported.is_none()
    }

    pub(super) fn validate(&self, path: &Path, model_id: &str) -> Result<(), Error> {
        if model_id.trim().is_empty() {
            return Err(Error::Config(format!(
                "configuration file {} contains an empty model ID",
                path.display()
            )));
        }
        if self.is_empty() {
            return Err(Error::Config(format!(
                "configuration file {} contains no settings for model {model_id:?}; set input_modalities, supported_reasoning_levels, or a Responses capability",
                path.display()
            )));
        }
        if let Some(tiers) = &self.service_tiers {
            if tiers.is_empty() {
                return Err(Error::Config(format!(
                    "configuration file {}: model {model_id:?} service_tiers must list at least one tier",
                    path.display()
                )));
            }
            for (index, tier) in tiers.iter().enumerate() {
                if tiers[..index].contains(tier) {
                    return Err(Error::Config(format!(
                        "configuration file {}: model {model_id:?} service_tiers lists {tier:?} more than once",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

impl From<&ModelFileConfig> for ModelOverride {
    fn from(model: &ModelFileConfig) -> Self {
        Self {
            input_modalities: model.input_modalities,
            supported_reasoning_levels: model.supported_reasoning_levels.clone(),
            sampling_supported: model.sampling_supported,
            logprobs_supported: model.logprobs_supported,
            service_tiers: model.service_tiers.clone(),
            prompt_cache_options_supported: model.prompt_cache_options_supported,
            prompt_cache_retention_supported: model.prompt_cache_retention_supported,
        }
    }
}
