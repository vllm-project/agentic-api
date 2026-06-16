//! Domain types for conversation items.

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::StorageError;
use crate::types::io::{InputItem, OutputItem};

pub(crate) const STORED_ITEM_KIND_KEY: &str = "_agentic_item_kind";

/// Item kind (input vs output) for storage and retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Input,
    Output,
}

impl ItemKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    pub(crate) fn from_stored_str(value: &str) -> Option<Self> {
        match value {
            "input" => Some(Self::Input),
            "output" => Some(Self::Output),
            _ => None,
        }
    }
}

/// Union type for conversation items (input or output).
#[derive(Debug, Clone)]
pub enum InOutItem {
    Input(InputItem),
    Output(OutputItem),
}

impl From<InputItem> for InOutItem {
    fn from(item: InputItem) -> Self {
        Self::Input(item)
    }
}

impl From<OutputItem> for InOutItem {
    fn from(item: OutputItem) -> Self {
        Self::Output(item)
    }
}

impl TryFrom<&InOutItem> for String {
    type Error = StorageError;

    fn try_from(item: &InOutItem) -> Result<Self, Self::Error> {
        let (mut value, kind) = match item {
            InOutItem::Input(input) => (
                serde_json::to_value(input).map_err(StorageError::Serialization)?,
                ItemKind::Input,
            ),
            InOutItem::Output(output) => (
                serde_json::to_value(output).map_err(StorageError::Serialization)?,
                ItemKind::Output,
            ),
        };

        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                STORED_ITEM_KIND_KEY.to_string(),
                Value::String(kind.as_str().to_string()),
            );
        }

        serde_json::to_string(&value).map_err(StorageError::Serialization)
    }
}

impl InOutItem {
    /// Converts stored history into input items suitable for a model request.
    #[must_use]
    pub fn into_input_items(history: Vec<InOutItem>) -> Vec<InputItem> {
        history
            .into_iter()
            .filter_map(|i| match i {
                InOutItem::Input(item) => Some(item),
                InOutItem::Output(OutputItem::Message(msg)) => Some(InputItem::Message(msg.into())),
                InOutItem::Output(OutputItem::Reasoning(r)) => Some(InputItem::Reasoning(r)),
                InOutItem::Output(OutputItem::FunctionCall(_) | OutputItem::Unknown) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{
        InputContent, InputMessage, InputMessageContent, OutputMessage, OutputTextContent, ReasoningOutput,
        ReasoningTextContent,
    };

    #[test]
    fn test_inout_item_from_input() {
        let input = InputItem::Message(InputMessage {
            role: "user".to_string(),
            content: InputMessageContent::Text("hello".to_string()),
        });
        let item: InOutItem = input.into();
        assert!(matches!(item, InOutItem::Input(_)));
    }

    #[test]
    fn test_inout_item_from_output() {
        let output = OutputItem::Message(OutputMessage::new("msg_1", "completed"));
        let item: InOutItem = output.into();
        assert!(matches!(item, InOutItem::Output(_)));
    }

    #[test]
    fn test_inout_item_to_string() {
        let input = InputItem::Message(InputMessage {
            role: "user".to_string(),
            content: InputMessageContent::Text("test".to_string()),
        });
        let item = InOutItem::Input(input);
        let json = String::try_from(&item).expect("serialization failed");
        assert!(json.contains(STORED_ITEM_KIND_KEY));
        assert!(json.contains("input"));
        assert!(json.contains("user"));
        assert!(json.contains("test"));
    }

    #[test]
    fn test_into_input_items_converts_output_messages() {
        let mut output = OutputMessage::new("out1", "done");
        output.content.push(OutputTextContent::new("answer"));
        let items = vec![
            InOutItem::Input(InputItem::Message(InputMessage {
                role: "user".to_string(),
                content: InputMessageContent::Text("msg1".to_string()),
            })),
            InOutItem::Output(OutputItem::Message(output)),
            InOutItem::Input(InputItem::Message(InputMessage {
                role: "user".to_string(),
                content: InputMessageContent::Text("msg2".to_string()),
            })),
        ];

        let inputs = InOutItem::into_input_items(items);
        assert_eq!(inputs.len(), 3);
        match &inputs[1] {
            InputItem::Message(message) => {
                assert_eq!(message.role, "assistant");
                match &message.content {
                    InputMessageContent::Parts(parts) => {
                        assert_eq!(parts.len(), 1);
                        match &parts[0] {
                            InputContent::Text(t) => {
                                assert_eq!(t.type_, "output_text");
                                assert_eq!(t.text, "answer");
                            }
                            InputContent::Image(_) => panic!("expected text part"),
                        }
                    }
                    InputMessageContent::Text(_) => panic!("expected parts content"),
                }
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn test_into_input_items_includes_reasoning() {
        let mut reasoning = ReasoningOutput::new("rs_1");
        reasoning.content.push(ReasoningTextContent::new("thinking..."));
        let items = vec![
            InOutItem::Output(OutputItem::Reasoning(reasoning)),
            InOutItem::Output(OutputItem::Message(OutputMessage::new("msg_1", "completed"))),
        ];

        let inputs = InOutItem::into_input_items(items);
        assert_eq!(inputs.len(), 2);
        assert!(matches!(inputs[0], InputItem::Reasoning(_)));
        if let InputItem::Reasoning(r) = &inputs[0] {
            assert_eq!(r.id, "rs_1");
            assert_eq!(r.content[0].text, "thinking...");
        }
    }

    #[test]
    fn test_item_kind_serialization() {
        let kind = ItemKind::Input;
        let json = serde_json::to_string(&kind).expect("serialization failed");
        assert_eq!(json, "\"input\"");

        let kind2 = ItemKind::Output;
        let json2 = serde_json::to_string(&kind2).expect("serialization failed");
        assert_eq!(json2, "\"output\"");
    }
}
