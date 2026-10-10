use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{RequestPayload, ResponseTextConfig, ResponseTextFormat};
use crate::types::io::ToolChoice;
use crate::types::tools::ResponsesTool;

#[allow(clippy::ref_option)] // serde's serialize_with passes a reference to the field.
pub(super) fn serialize_response_tools<S>(tools: &Option<Vec<ResponsesTool>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    tools.as_deref().unwrap_or_default().serialize(serializer)
}

#[allow(clippy::ref_option)] // serde's serialize_with passes a reference to the field.
pub(super) fn serialize_response_tool_choice<S>(choice: &Option<ToolChoice>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    choice.as_ref().unwrap_or(&ToolChoice::Auto).serialize(serializer)
}

#[allow(clippy::ref_option)] // serde's serialize_with passes a reference to the field.
pub(super) fn serialize_response_service_tier<S>(tier: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(tier.as_deref().unwrap_or("default"))
}

/// Standard response fields required by the Open Responses resource schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default)]
pub struct StandardResponseFields {
    pub completed_at: Option<i64>,
    pub truncation: String,
    pub parallel_tool_calls: bool,
    pub text: ResponseTextConfig,
    pub top_p: f64,
    pub presence_penalty: f64,
    pub frequency_penalty: f64,
    pub top_logprobs: u32,
    pub temperature: f64,
    pub reasoning: Option<ResponseReasoning>,
    pub max_output_tokens: Option<u32>,
    pub store: bool,
    pub background: bool,
    pub metadata: HashMap<String, Value>,
    pub safety_identifier: Option<String>,
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResponseReasoning {
    pub effort: Option<String>,
    pub summary: Option<String>,
}

impl Default for StandardResponseFields {
    fn default() -> Self {
        Self {
            completed_at: None,
            truncation: "disabled".into(),
            parallel_tool_calls: false,
            text: ResponseTextConfig {
                format: Some(ResponseTextFormat::Text { extra: Map::new() }),
                verbosity: None,
                extra: Map::new(),
            },
            top_p: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: 1.0,
            reasoning: None,
            max_output_tokens: None,
            store: false,
            background: false,
            metadata: HashMap::new(),
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }
}

impl StandardResponseFields {
    pub(crate) fn apply_request(&mut self, request: &RequestPayload) {
        self.store = request.store;
        self.truncation = request.truncation.clone().unwrap_or_else(|| "disabled".to_owned());
        self.parallel_tool_calls = request.parallel_tool_calls.unwrap_or(false);
        if let Some(text) = request.text.as_deref() {
            self.text = text.clone();
            if self.text.format.is_none() {
                self.text.format = Some(ResponseTextFormat::Text { extra: Map::new() });
            }
        }
        self.top_p = request.top_p.unwrap_or(1.0);
        self.temperature = request.temperature.unwrap_or(1.0);
        self.max_output_tokens = request.max_output_tokens;
        self.reasoning = request.reasoning.as_deref().map(|reasoning| ResponseReasoning {
            effort: reasoning.effort.clone(),
            summary: reasoning.summary.clone(),
        });
        self.metadata = request
            .metadata
            .as_ref()
            .and_then(Value::as_object)
            .map(|metadata| {
                metadata
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.prompt_cache_key.clone_from(&request.prompt_cache_key);
    }
}
