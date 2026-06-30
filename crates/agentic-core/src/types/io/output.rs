use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::EventPayload;
use crate::executor::error::ExecutorError;
use crate::types::event::MessageStatus;
use crate::utils::uuid7_str;

use super::input::{InputContent, InputMessage, InputMessageContent, InputTextContent};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTextContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
    #[serde(default)]
    pub annotations: Vec<Value>,
}

impl OutputTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            type_: "output_text".into(),
            text: text.into(),
            annotations: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputMessage {
    pub id: String,
    pub role: String,
    pub status: MessageStatus,
    #[serde(default)]
    pub content: Vec<OutputTextContent>,
}

impl OutputMessage {
    pub fn new(id: impl Into<String>, status: MessageStatus) -> Self {
        Self {
            id: id.into(),
            role: "assistant".into(),
            status,
            content: vec![],
        }
    }
}

impl TryFrom<&EventPayload> for OutputMessage {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("msg_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(id, MessageStatus::InProgress))
    }
}

impl From<OutputMessage> for InputMessage {
    fn from(msg: OutputMessage) -> Self {
        let parts = msg
            .content
            .into_iter()
            .map(|c| {
                InputContent::Text(InputTextContent {
                    type_: c.type_,
                    text: c.text,
                })
            })
            .collect();
        Self {
            role: msg.role,
            content: InputMessageContent::Parts(parts),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionToolCall {
    pub id: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default = "default_completed_status")]
    #[serde(deserialize_with = "deserialize_status_or_default")]
    pub status: MessageStatus,
}

fn default_completed_status() -> MessageStatus {
    MessageStatus::Completed
}

fn deserialize_status_or_default<'de, D>(deserializer: D) -> Result<MessageStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<MessageStatus> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or(MessageStatus::Completed))
}

impl TryFrom<&EventPayload> for FunctionToolCall {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded {
            item_id, call_id, name, ..
        } = payload
        else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("fc_")
        } else {
            item_id.clone()
        };
        Ok(Self {
            id,
            call_id: call_id.as_deref().unwrap_or_default().to_owned(),
            name: name.as_deref().unwrap_or_default().to_owned(),
            arguments: String::new(),
            status: MessageStatus::InProgress,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningTextContent {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
}

impl ReasoningTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            type_: "reasoning_text".into(),
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningOutput {
    pub id: String,
    #[serde(default)]
    pub content: Vec<ReasoningTextContent>,
    #[serde(default)]
    pub summary: Vec<Value>,
    pub encrypted_content: Option<Value>,
    pub status: Option<String>,
}

impl ReasoningOutput {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: vec![],
            summary: vec![],
            encrypted_content: None,
            status: None,
        }
    }
}

impl TryFrom<&EventPayload> for ReasoningOutput {
    type Error = ExecutorError;

    fn try_from(payload: &EventPayload) -> Result<Self, Self::Error> {
        let EventPayload::OutputItemAdded { item_id, .. } = payload else {
            return Err(ExecutorError::ParseError("expected OutputItemAdded payload".into()));
        };
        let id = if item_id.is_empty() {
            uuid7_str("rs_")
        } else {
            item_id.clone()
        };
        Ok(Self::new(id))
    }
}

/// Applies a `*Done` event payload onto an in-flight output item.
///
/// `buffer` holds accumulated delta text/arguments. If the payload's own field
/// is empty the buffer is used as the final value and then cleared; otherwise
/// the buffer is discarded and the payload value is used directly.
pub trait ApplyDone {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String);
}

impl ApplyDone for ReasoningOutput {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        let EventPayload::ReasoningDone { text, .. } = payload else {
            return;
        };
        let text = if text.is_empty() {
            std::mem::take(buffer)
        } else {
            buffer.clear();
            text.clone()
        };
        if !text.is_empty() {
            self.content.push(ReasoningTextContent::new(text));
        }
    }
}

impl ApplyDone for FunctionToolCall {
    fn apply_done(&mut self, payload: &EventPayload, buffer: &mut String) {
        let EventPayload::FunctionCallArgsDone {
            arguments,
            call_id,
            name,
            ..
        } = payload
        else {
            return;
        };
        self.arguments = if arguments.is_empty() {
            std::mem::take(buffer)
        } else {
            buffer.clear();
            arguments.clone()
        };
        if let Some(cid) = call_id.as_deref().filter(|s| !s.is_empty()) {
            cid.clone_into(&mut self.call_id);
        }
        if !name.is_empty() {
            name.clone_into(&mut self.name);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "message")]
    Message(OutputMessage),
    #[serde(rename = "function_call")]
    FunctionCall(FunctionToolCall),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningOutput),
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_output_round_trips_through_serde() {
        let json = serde_json::json!({
            "id": "rs_abc",
            "type": "reasoning",
            "summary": [],
            "content": [{"text": "Let me think...", "type": "reasoning_text"}],
            "encrypted_content": null,
            "status": null
        });
        let item: OutputItem = serde_json::from_value(json).unwrap();
        assert!(matches!(item, OutputItem::Reasoning(_)));
        if let OutputItem::Reasoning(r) = &item {
            assert_eq!(r.id, "rs_abc");
            assert_eq!(r.content.len(), 1);
            assert_eq!(r.content[0].text, "Let me think...");
        }
        let serialized = serde_json::to_value(&item).unwrap();
        assert_eq!(serialized["type"], "reasoning");
        assert_eq!(serialized["id"], "rs_abc");
    }

    #[test]
    fn reasoning_input_round_trips_through_serde() {
        use crate::types::io::input::InputItem;
        let reasoning = ReasoningOutput::new("rs_1");
        let item = InputItem::Reasoning(reasoning);
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "reasoning");
        let back: InputItem = serde_json::from_value(json).unwrap();
        assert!(matches!(back, InputItem::Reasoning(_)));
    }

    #[test]
    fn vllm_reasoning_response_deserializes() {
        let vllm_output = serde_json::json!([
            {
                "id": "rs_bb637a529f72b88d",
                "summary": [],
                "type": "reasoning",
                "content": [{"text": "2+2 is 4.", "type": "reasoning_text"}],
                "encrypted_content": null,
                "status": null
            },
            {
                "id": "msg_bb68f033f2ed1725",
                "content": [{"annotations": [], "text": "2+2 equals 4.", "type": "output_text"}],
                "role": "assistant",
                "status": "completed",
                "type": "message"
            }
        ]);
        let items: Vec<OutputItem> = serde_json::from_value(vllm_output).unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], OutputItem::Reasoning(_)));
        assert!(matches!(items[1], OutputItem::Message(_)));
    }
}
