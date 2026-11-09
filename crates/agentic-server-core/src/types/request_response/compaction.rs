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
        let index = self.compaction_entry_index()?;
        self.context_management.as_ref()?[index].compact_threshold
    }

    /// Supply a threshold only when the first compaction policy has none, adding that policy if absent.
    pub(crate) fn set_default_compact_threshold(&mut self, default: u64) {
        let index = self.compaction_entry_index();
        let management = self.context_management.get_or_insert_with(Vec::new);
        if let Some(index) = index {
            management[index].compact_threshold.get_or_insert(default);
        } else {
            management.push(ContextManagement {
                type_: "compaction".into(),
                compact_threshold: Some(default),
            });
        }
    }

    fn compaction_entry_index(&self) -> Option<usize> {
        self.context_management
            .as_ref()?
            .iter()
            .position(|entry| entry.type_ == "compaction")
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compaction_default_preserves_explicit_thresholds_and_uses_the_first_policy() {
        for (first, expected) in [(None, 100), (Some(0), 0), (Some(10), 10)] {
            let mut request: RequestPayload = serde_json::from_value(json!({
                "model": "test",
                "input": [],
                "context_management": [
                    {"type": "other", "compact_threshold": 1},
                    {"type": "compaction", "compact_threshold": first},
                    {"type": "compaction", "compact_threshold": 200}
                ]
            }))
            .unwrap();
            assert_eq!(request.compact_threshold(), first);
            request.set_default_compact_threshold(100);
            assert_eq!(request.compact_threshold(), Some(expected));
            assert_eq!(
                serde_json::to_value(request.context_management).unwrap(),
                json!([
                    {"type": "other", "compact_threshold": 1},
                    {"type": "compaction", "compact_threshold": expected},
                    {"type": "compaction", "compact_threshold": 200}
                ]),
            );
        }
    }

    #[test]
    fn compaction_default_adds_a_missing_policy_once() {
        for management in [Value::Null, json!([]), json!([{"type": "other"}])] {
            let mut request: RequestPayload = serde_json::from_value(json!({
                "model": "test", "input": [], "context_management": management
            }))
            .unwrap();
            assert_eq!(request.compact_threshold(), None);
            request.set_default_compact_threshold(100);
            request.set_default_compact_threshold(200);
            assert_eq!(request.compact_threshold(), Some(100));
            let mut expected = management.as_array().cloned().unwrap_or_default();
            expected.push(json!({"type": "compaction", "compact_threshold": 100}));
            assert_eq!(
                serde_json::to_value(request.context_management).unwrap(),
                json!(expected)
            );
        }
    }
}
