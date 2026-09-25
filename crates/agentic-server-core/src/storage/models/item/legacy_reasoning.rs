//! Bounded compatibility projection for reasoning rows written before typed reasoning.
//!
//! Earlier releases stored any content discriminator and untyped summary, opaque
//! state, and status values. Keep every field that still decodes; drop only the
//! rest. Rows that cannot be read even this way still fail closed.

use serde::Deserialize;
use serde_json::Value;

use super::Item;
use crate::types::io::{
    OpaqueReasoning, ReasoningOutput, ReasoningStatus, ReasoningSummaryContent, ReasoningTextContent,
};

const MAX_LEGACY_REASONING_BYTES: usize = 16 * 1024 * 1024;
const MAX_LEGACY_CONTENT_PARTS: usize = 4096;

#[derive(Deserialize)]
struct LegacyReasoning {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: Option<Vec<LegacyText>>,
    #[serde(default)]
    summary: Option<Vec<Value>>,
    #[serde(default)]
    encrypted_content: Option<Value>,
    #[serde(default)]
    status: Option<Value>,
}

/// Earlier releases required string text but accepted any content discriminator.
#[derive(Deserialize)]
struct LegacyText {
    text: String,
}

impl Item {
    pub(super) fn legacy_reasoning(&self, data: &Value) -> Option<ReasoningOutput> {
        if self.data.len() > MAX_LEGACY_REASONING_BYTES || data.get("type")?.as_str()? != "reasoning" {
            return None;
        }
        let legacy = LegacyReasoning::deserialize(data).ok()?;
        let parts = legacy.content.unwrap_or_default();
        if legacy.kind != "reasoning" || parts.len() > MAX_LEGACY_CONTENT_PARTS {
            return None;
        }
        let mut reasoning = ReasoningOutput::new(legacy.id);
        reasoning.content = parts
            .into_iter()
            .map(|part| ReasoningTextContent::new(part.text))
            .collect();
        reasoning.summary = legacy
            .summary
            .unwrap_or_default()
            .iter()
            .filter_map(|part| ReasoningSummaryContent::deserialize(part).ok())
            .collect();
        reasoning.encrypted_content = match legacy.encrypted_content {
            Some(Value::String(state)) => OpaqueReasoning::try_from(state).ok(),
            _ => None,
        };
        reasoning.status = legacy
            .status
            .and_then(|status| ReasoningStatus::deserialize(&status).ok());
        Some(reasoning)
    }
}
