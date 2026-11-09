//! Compaction wire contracts and the request settings used to summarize context.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{PromptCacheOptions, PromptCacheRetention, RequestPayload};
use crate::types::io::{InputItem, ResponseUsage, ResponsesInput};

/// Server-side context management configuration for a Responses request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ContextManagement {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

/// Request body accepted by `POST /v1/responses/compact`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompactRequest {
    pub model: String,
    #[serde(default)]
    pub input: Option<ResponsesInput>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<PromptCacheOptions>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    /// Compatibility fields sent by current SDK and Codex clients.
    #[serde(flatten)]
    pub compatibility: HashMap<String, Value>,
}

/// Result returned by `POST /v1/responses/compact`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompactedResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub output: Vec<InputItem>,
    pub usage: ResponseUsage,
}

impl<T: ?Sized, C> RequestPayload<T, C> {
    /// The token threshold of the first `compaction` entry in `context_management`, which turns on automatic
    /// compaction.
    #[must_use]
    pub fn compact_threshold(&self) -> Option<u64> {
        self.context_management
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|entry| entry.type_ == "compaction")
            .and_then(|entry| entry.compact_threshold)
    }
}

impl RequestPayload {
    /// The settings a compaction summary request keeps from this request: the model, instructions, prompt-cache
    /// settings and service tier. It offers no tools and stores nothing.
    #[must_use]
    pub fn summary_settings(&self) -> Self {
        Self {
            model: self.model.clone(),
            instructions: self.instructions.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            prompt_cache_retention: self.prompt_cache_retention,
            prompt_cache_options: self.prompt_cache_options,
            service_tier: self.service_tier.clone(),
            ..Self::default()
        }
    }
}
