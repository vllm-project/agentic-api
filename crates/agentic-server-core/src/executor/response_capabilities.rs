use super::{ExecutionContext, ExecutorError, ExecutorResult};
use crate::config::ResponsesModelPolicy;
use crate::types::io::ItemInclude;
use crate::types::request_response::RequestPayload;

impl ExecutionContext {
    /// Checks deployment and model capability policy before admitting a Responses request.
    ///
    /// # Errors
    /// Returns a parameter-specific client error when the configured policy rejects a setting.
    pub fn validate_response_capabilities(&self, request: &RequestPayload) -> ExecutorResult<()> {
        let config = &self.responses_config;
        let policy = config.model_policies.get(&request.model);
        let cache_options_supported = policy
            .and_then(|policy| policy.prompt_cache_options_supported)
            .unwrap_or(config.prompt_cache_options_supported);
        request.validate_prompt_cache_options(cache_options_supported)?;

        if let Some(param) = policy.and_then(|policy| policy.unsupported_parameter(request)) {
            return Err(ExecutorError::UnsupportedValue {
                param: param.to_owned(),
                code: Some("unsupported_value"),
                message: format!(
                    "{param} is not supported by the configured policy for model {:?}",
                    request.model
                ),
            });
        }
        Ok(())
    }
}

impl ResponsesModelPolicy {
    fn unsupported_parameter(&self, request: &RequestPayload) -> Option<&'static str> {
        if self.sampling_supported == Some(false) {
            if request.temperature.is_some() {
                return Some("temperature");
            }
            if request.top_p.is_some() {
                return Some("top_p");
            }
        }
        if self.logprobs_supported == Some(false) {
            // Keep forwarding provider extensions for other models and treat null as unset here.
            if request.extra.get("top_logprobs").is_some_and(|value| !value.is_null()) {
                return Some("top_logprobs");
            }
            if request
                .include
                .iter()
                .flatten()
                .any(|field| matches!(field.parse(), Ok(ItemInclude::MessageOutputTextLogprobs)))
            {
                return Some("include");
            }
        }
        if let Some((effort, allowed)) = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_ref())
            .zip(self.reasoning_efforts.as_ref())
            && !allowed.contains(effort)
        {
            return Some("reasoning.effort");
        }
        if let Some((tier, allowed)) = request.service_tier.as_ref().zip(self.service_tiers.as_ref())
            && !allowed.contains(tier)
        {
            return Some("service_tier");
        }
        if self.prompt_cache_retention_supported == Some(false) && request.prompt_cache_retention.is_some() {
            return Some("prompt_cache_retention");
        }
        None
    }
}
