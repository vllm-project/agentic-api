//! Bounded compatibility projection for provenance-free reasoning rows written
//! before the typed reasoning schema. Never reconstruct replay trust from JSON.

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::Item;
use crate::types::io::{ReasoningOutput, ReasoningTextContent};

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
    // Earlier schemas accepted arbitrary summary, state, and status values.
    // They cannot be replayed as provider state without provenance.
    #[serde(default)]
    summary: Option<IgnoredAny>,
    #[serde(default)]
    encrypted_content: Option<IgnoredAny>,
    #[serde(default)]
    status: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct LegacyText {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

impl Item {
    pub(super) fn legacy_reasoning(&self, data: &Value) -> Option<ReasoningOutput> {
        if self.reasoning_provenance.is_some()
            || self.data.len() > MAX_LEGACY_REASONING_BYTES
            || data.get("type")?.as_str()? != "reasoning"
        {
            return None;
        }
        let legacy: LegacyReasoning = serde_json::from_value(data.clone()).ok()?;
        if legacy.kind != "reasoning" {
            return None;
        }
        let parts = legacy.content.unwrap_or_default();
        if parts.len() > MAX_LEGACY_CONTENT_PARTS {
            return None;
        }
        let mut reasoning = ReasoningOutput::new(legacy.id);
        reasoning.content = parts
            .into_iter()
            .map(|part| {
                let _ = part.kind;
                ReasoningTextContent::new(part.text)
            })
            .collect();
        let _ = (legacy.summary, legacy.encrypted_content, legacy.status);
        Some(reasoning)
    }
}
