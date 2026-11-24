//! Typed output text annotations with a lossless fallback for newer kinds.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::FileCitation;

/// A known file citation or an annotation kind this gateway does not interpret.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum OutputAnnotation {
    FileCitation(FileCitation),
    Other(Value),
}

impl<'de> Deserialize<'de> for OutputAnnotation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.get("type").and_then(Value::as_str) == Some("file_citation") {
            serde_json::from_value(value)
                .map(Self::FileCitation)
                .map_err(serde::de::Error::custom)
        } else {
            Ok(Self::Other(value))
        }
    }
}

impl From<OutputAnnotation> for Value {
    fn from(annotation: OutputAnnotation) -> Self {
        match annotation {
            OutputAnnotation::FileCitation(citation) => serde_json::json!({
                "type": "file_citation",
                "file_id": citation.file_id,
                "filename": citation.filename,
                "index": citation.index,
            }),
            OutputAnnotation::Other(value) => value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OutputAnnotation;

    #[test]
    fn file_citation_is_typed_and_other_annotations_round_trip() {
        let annotations = serde_json::json!([
            {"type":"file_citation","file_id":"file_1","filename":"notes.txt","index":3},
            {"type":"future_citation","extra":{"nested":true}}
        ]);
        let parsed: Vec<OutputAnnotation> = serde_json::from_value(annotations.clone()).unwrap();
        assert!(matches!(parsed[0], OutputAnnotation::FileCitation(_)));
        assert!(matches!(parsed[1], OutputAnnotation::Other(_)));
        assert_eq!(serde_json::to_value(parsed).unwrap(), annotations);
        assert!(
            serde_json::from_value::<OutputAnnotation>(serde_json::json!({"type":"file_citation","file_id":"file_1"}))
                .is_err()
        );
    }
}
