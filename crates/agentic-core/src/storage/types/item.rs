//! Domain types for conversation items.

use std::convert::TryFrom;

use serde::{Deserialize, Serialize};

use crate::storage::StorageError;
use crate::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem};
use crate::utils::common::serialize_to_string;

/// Item kind (input vs output) for storage and retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Input,
    Output,
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
        match item {
            InOutItem::Input(input) => serialize_to_string(input).map_err(StorageError::Serialization),
            InOutItem::Output(output) => serialize_to_string(output).map_err(StorageError::Serialization),
        }
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
                InOutItem::Output(OutputItem::Message(msg)) => {
                    let text = msg.content.into_iter().map(|content| content.text).collect::<String>();
                    Some(InputItem::Message(InputMessage {
                        role: msg.role,
                        content: InputMessageContent::Text(text),
                    }))
                }
                InOutItem::Output(OutputItem::FunctionCall(_) | OutputItem::Unknown) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::{OutputMessage, OutputTextContent};

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
                assert!(matches!(&message.content, InputMessageContent::Text(text) if text == "answer"));
            }
            InputItem::FunctionCallOutput(_) | InputItem::Unknown => panic!("expected message"),
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
