//! Serialization of public response lifecycle chunks.
use super::ResponsePayload;
use crate::utils::common::serialize_to_string;
use serde_json::json;

impl ResponsePayload {
    #[must_use]
    pub fn as_created_response_chunk(&self) -> String {
        let mut response = self.clone();
        "in_progress".clone_into(&mut response.status);
        let event = json!({
            "type": "response.created",
            "response": response,
        });
        let json_str = serialize_to_string(&event).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    #[must_use]
    pub fn as_responses_chunk(&self) -> String {
        let json_str = serialize_to_string(self).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    #[must_use]
    pub fn as_terminal_response_chunk(&self) -> String {
        let event = json!({
            "type": self.terminal_event_type(),
            "response": self,
        });
        let json_str = serialize_to_string(&event).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    pub(crate) fn terminal_event_type(&self) -> &'static str {
        match self.status.as_str() {
            "incomplete" => "response.incomplete",
            "failed" | "error" => "response.failed",
            "in_progress" => "response.in_progress",
            _ => "response.completed",
        }
    }
}
