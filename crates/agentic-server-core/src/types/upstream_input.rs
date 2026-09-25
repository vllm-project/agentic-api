//! Borrowed, upstream-only reasoning projection. Canonical history is never edited.

use std::borrow::Cow;

use serde::{Serialize, Serializer, ser::SerializeSeq};

use super::io::{InputItem, OpaqueReasoning, ReasoningStatus, ReasoningSummaryContent, ResponsesInput};
use super::request_response::UpstreamRequest;

/// Input after the existing namespace and model-context transformations.
///
/// The default representation preserves the vLLM wire contract. The reserved
/// opaque adapter selects its projection only after executor preflight checks;
/// serialization itself does not grant permission to execute or replay history.
#[derive(Debug)]
pub struct UpstreamInput<'a> {
    input: Cow<'a, ResponsesInput>,
    projection: Projection,
}

#[derive(Debug, Clone, Copy)]
enum Projection {
    Vllm,
    Opaque,
}

impl<'a> From<Cow<'a, ResponsesInput>> for UpstreamInput<'a> {
    fn from(input: Cow<'a, ResponsesInput>) -> Self {
        Self {
            input,
            projection: Projection::Vllm,
        }
    }
}

impl UpstreamRequest<'_> {
    /// Select a stateless opaque wire view without changing gateway persistence
    /// or canonical items. The executor must validate provenance before using it.
    pub(crate) fn with_opaque_replay(mut self) -> Self {
        self.store = Some(false);
        self.input.projection = Projection::Opaque;
        self
    }
}

impl Serialize for UpstreamInput<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if matches!(self.projection, Projection::Vllm) {
            return self.input.serialize(serializer);
        }
        match self.input.as_ref() {
            ResponsesInput::Text(text) => serializer.serialize_str(text),
            ResponsesInput::Items(items) => {
                let mut sequence = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    match item {
                        InputItem::Reasoning(reasoning) => sequence.serialize_element(&OpaqueReasoningInput {
                            kind: ReasoningKind::Reasoning,
                            id: &reasoning.id,
                            summary: &reasoning.summary,
                            encrypted_content: reasoning.encrypted_content.as_ref(),
                            status: reasoning.status,
                        })?,
                        // Other kinds retain the one existing item serialization path.
                        _ => sequence.serialize_element(item)?,
                    }
                }
                sequence.end()
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningKind {
    Reasoning,
}

/// Deliberately excludes plaintext `content` and internal provenance. Optional
/// fields are absent rather than null; opaque strings and summaries are borrowed
/// verbatim, without decoding, copying, trimming, or reordering.
#[derive(Serialize)]
struct OpaqueReasoningInput<'a> {
    #[serde(rename = "type")]
    kind: ReasoningKind,
    id: &'a str,
    summary: &'a [ReasoningSummaryContent],
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<&'a OpaqueReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<ReasoningStatus>,
}

#[cfg(test)]
mod tests;
