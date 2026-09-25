//! Closed nested request shapes for the pinned opaque replay candidate.
//!
//! These Serde sentinels validate only which wire fields are admitted. The
//! authoritative typed `RequestPayload` still owns values and semantics. The
//! transport's request/frame byte ceiling bounds every parsed sequence here.

use std::collections::HashSet;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};

use agentic_core::types::io::ReasoningSummaryContent;
use agentic_core::types::reasoning_profile::{
    MAX_OPAQUE_CONTENT_PARTS, MAX_OPAQUE_INPUT_ITEMS, MAX_OPAQUE_REASONING_SUMMARIES,
};

const MAX_DOCUMENT_MEMBERS: usize = 4_096;
const MAX_DOCUMENT_ELEMENTS: usize = 32_768;

/// Consume each sentinel immediately; retain no second copy of caller input.
pub(super) struct BoundedSequence<T, const LIMIT: usize>(PhantomData<T>);

impl<'de, T: Deserialize<'de>, const LIMIT: usize> Deserialize<'de> for BoundedSequence<T, LIMIT> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SequenceVisitor<T, const LIMIT: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for SequenceVisitor<T, LIMIT> {
            type Value = BoundedSequence<T, LIMIT>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "an array with at most {LIMIT} entries")
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> Result<Self::Value, S::Error> {
                let mut count = 0;
                while let Some(item) = sequence.next_element::<T>()? {
                    if count == LIMIT {
                        return Err(serde::de::Error::custom(
                            "opaque replay request array exceeds its item limit",
                        ));
                    }
                    drop(item);
                    count += 1;
                }
                Ok(BoundedSequence(PhantomData))
            }
        }

        deserializer.deserialize_seq(SequenceVisitor(PhantomData))
    }
}

struct EmptyArray;

impl<'de> Deserialize<'de> for EmptyArray {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EmptyArrayVisitor;

        impl<'de> Visitor<'de> for EmptyArrayVisitor {
            type Value = EmptyArray;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an empty array")
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> Result<Self::Value, S::Error> {
                if sequence.next_element::<IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("unqualified nonempty replay metadata"));
                }
                Ok(EmptyArray)
            }
        }

        deserializer.deserialize_seq(EmptyArrayVisitor)
    }
}

/// Keep intentionally open JSON documents open while rejecting lossy duplicate
/// keys. Depth and total bytes are bounded by the JSON transport deserializer.
struct NoDuplicateDocument;

impl<'de> Deserialize<'de> for NoDuplicateDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DocumentVisitor;

        impl<'de> Visitor<'de> for DocumentVisitor {
            type Value = NoDuplicateDocument;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded JSON document without duplicate keys")
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NoDuplicateDocument)
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> Result<Self::Value, S::Error> {
                let mut count = 0;
                while sequence.next_element::<NoDuplicateDocument>()?.is_some() {
                    if count == MAX_DOCUMENT_ELEMENTS {
                        return Err(serde::de::Error::custom(
                            "opaque replay document array exceeds its item limit",
                        ));
                    }
                    count += 1;
                }
                Ok(NoDuplicateDocument)
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if keys.len() == MAX_DOCUMENT_MEMBERS {
                        return Err(serde::de::Error::custom("opaque replay document exceeds its key limit"));
                    }
                    if !keys.insert(key) {
                        return Err(serde::de::Error::custom("duplicate key in opaque replay document"));
                    }
                    map.next_value::<NoDuplicateDocument>()?;
                }
                Ok(NoDuplicateDocument)
            }
        }

        deserializer.deserialize_any(DocumentVisitor)
    }
}

// The fields are deliberately consumed by Serde but never read: this module
// must not become a second request representation or an upstream projector.
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct ClosedInput(ClosedInputValue);

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(untagged)]
enum ClosedInputValue {
    Text(String),
    Items(BoundedSequence<ClosedInputItem, MAX_OPAQUE_INPUT_ITEMS>),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClosedInputItem {
    #[serde(rename = "message")]
    Message(ClosedMessage),
    #[serde(rename = "reasoning")]
    Reasoning(ClosedReasoning),
    #[serde(rename = "function_call")]
    FunctionCall(ClosedFunctionCall),
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(ClosedFunctionCallOutput),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedMessage {
    id: Option<IgnoredAny>,
    role: ClosedMessageRole,
    status: Option<IgnoredAny>,
    phase: Option<IgnoredAny>,
    content: ClosedMessageContent,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClosedMessageRole {
    User,
    Assistant,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(untagged)]
enum ClosedMessageContent {
    Text(String),
    Parts(BoundedSequence<ClosedContentPart, MAX_OPAQUE_CONTENT_PARTS>),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClosedContentPart {
    #[serde(rename = "input_text")]
    InputText(ClosedInputText),
    #[serde(rename = "output_text")]
    OutputText(ClosedOutputText),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedInputText {
    text: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedOutputText {
    text: String,
    // Captured completed assistant items carry these metadata fields. The
    // existing typed content part preserves them unchanged in its extra map.
    annotations: Option<EmptyArray>,
    logprobs: Option<EmptyArray>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedReasoning {
    id: Option<IgnoredAny>,
    content: Option<EmptyArray>,
    summary: Option<BoundedSequence<ReasoningSummaryContent, MAX_OPAQUE_REASONING_SUMMARIES>>,
    encrypted_content: Option<IgnoredAny>,
    status: Option<IgnoredAny>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedFunctionCall {
    id: Option<IgnoredAny>,
    call_id: String,
    name: String,
    namespace: Option<IgnoredAny>,
    arguments: String,
    status: Option<IgnoredAny>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedFunctionCallOutput {
    call_id: String,
    // Structured function-call outputs need separate provider qualification.
    output: String,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct ClosedTool(ClosedToolValue);

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClosedToolValue {
    #[serde(rename = "function")]
    Function(ClosedFunctionTool),
    #[serde(rename = "mcp")]
    Mcp(ClosedMcpTool),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedFunctionTool {
    name: String,
    description: Option<IgnoredAny>,
    // JSON Schema is intentionally an open document, not an untyped gateway
    // parameter map. The existing FunctionToolParam retains it unchanged.
    parameters: Option<NoDuplicateDocument>,
    strict: Option<IgnoredAny>,
    defer_loading: Option<IgnoredAny>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedMcpTool {
    server_label: String,
    server_url: Option<IgnoredAny>,
    connector_id: Option<IgnoredAny>,
    headers: Option<NoDuplicateDocument>,
    authorization: Option<IgnoredAny>,
    allowed_tools: Option<IgnoredAny>,
    require_approval: Option<IgnoredAny>,
    defer_loading: Option<IgnoredAny>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(transparent)]
pub(super) struct ClosedToolChoice(ClosedToolChoiceValue);

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(untagged)]
enum ClosedToolChoiceValue {
    Mode(String),
    NamedFunction(ClosedNamedFunctionChoice),
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedNamedFunctionChoice {
    #[serde(rename = "type")]
    kind: FunctionChoiceTag,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FunctionChoiceTag {
    Function,
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn captured_replay_shapes_remain_admissible() {
        let input = r#"[
            {"type":"message","role":"user","content":"hello"},
            {"type":"reasoning","id":"rs_1","content":[],"summary":[{"type":"summary_text","text":"brief"}],"encrypted_content":"opaque"},
            {"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"},
            {"type":"function_call_output","call_id":"call_1","output":"found"},
            {"type":"message","id":"msg_1","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"done","annotations":[],"logprobs":[]}]}
        ]"#;
        assert!(serde_json::from_str::<ClosedInput>(input).is_ok());
        assert!(serde_json::from_str::<Vec<ClosedTool>>(
            r#"[{"type":"function","name":"lookup","parameters":{"type":"object","future_schema_keyword":true},"strict":true},{"type":"mcp","server_label":"local","server_url":"https://example.test","require_approval":"never"}]"#
        ).is_ok());
        assert!(serde_json::from_str::<ClosedToolChoice>(r#"{"type":"function","name":"lookup"}"#).is_ok());
    }

    #[test]
    fn unknown_and_duplicate_nested_fields_are_rejected() {
        for input in [
            r#"[{"type":"message","role":"user","content":"hi","surprise":1}]"#,
            r#"[{"type":"message","role":"system","content":"hi"}]"#,
            r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi","surprise":1}]}]"#,
            r#"[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","logprobs":[],"surprise":1}]}]"#,
            r#"[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","logprobs":[{"token":"hi"}]}]}]"#,
            r#"[{"type":"reasoning","encrypted_content":"opaque","surprise":1}]"#,
            r#"[{"type":"reasoning","encrypted_content":"opaque","content":[{"type":"reasoning_text","text":"private"}]}]"#,
            r#"[{"type":"function_call","call_id":"c","name":"n","arguments":"{}","arguments":"other"}]"#,
            r#"[{"type":"function_call_output","call_id":"c","output":"ok","surprise":1}]"#,
            r#"[{"type":"message","type":"function_call_output","role":"user","content":"hi"}]"#,
            r#"[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.test/image"}]}]"#,
        ] {
            assert!(
                serde_json::from_str::<ClosedInput>(input).is_err(),
                "unexpectedly admitted: {input}"
            );
        }
        for tool in [
            r#"{"type":"function","name":"lookup","surprise":1}"#,
            r#"{"type":"mcp","server_label":"local","_agentic_discovered_tools":[]}"#,
            r#"{"type":"function","name":"lookup","name":"other"}"#,
            r#"{"type":"function","name":"lookup","parameters":{"type":"object","type":"array"}}"#,
            r#"{"type":"mcp","server_label":"local","headers":{"X-Key":"one","X-Key":"two"}}"#,
        ] {
            assert!(serde_json::from_str::<ClosedTool>(tool).is_err());
        }
        for choice in [
            r#"{"type":"function","name":"lookup","surprise":1}"#,
            r#"{"function":{"name":"lookup"}}"#,
            r#"{"type":"function","name":"lookup","name":"other"}"#,
        ] {
            assert!(serde_json::from_str::<ClosedToolChoice>(choice).is_err());
        }
    }

    #[test]
    fn nested_sequences_have_explicit_item_caps() {
        type TwoItems = BoundedSequence<IgnoredAny, 2>;
        assert!(serde_json::from_str::<TwoItems>("[1,2]").is_ok());
        assert!(serde_json::from_str::<TwoItems>("[1,2,3]").is_err());

        let mut document = String::from("{");
        for index in 0..=MAX_DOCUMENT_MEMBERS {
            if index != 0 {
                document.push(',');
            }
            write!(document, "\"k{index}\":null").expect("write to String");
        }
        document.push('}');
        assert!(serde_json::from_str::<NoDuplicateDocument>(&document).is_err());
    }
}
