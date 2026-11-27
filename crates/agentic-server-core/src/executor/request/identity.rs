//! The gateway's public identity on a response: stamped on typed payloads and on provider events.

use std::num::NonZeroU64;

use serde_json::Value;

use super::RequestContext;
use crate::events::WireEvent;
use crate::types::request_response::{ResponsePayload, StandardResponseFields};
use crate::utils::common::utcnow_str;

impl RequestContext {
    /// A gateway-built response with this request's identity and echoed settings and no output yet. It stamps no
    /// `completed_at`, unlike [`Self::inject_ids`], since a shell can be presented before the response completes.
    pub(in crate::executor) fn response_shell(&self, model: String, status: &str) -> ResponsePayload {
        let mut payload = ResponsePayload {
            id: String::new(),
            object: "response".to_owned(),
            created_at: utcnow_str(),
            model,
            status: status.to_owned(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation: None,
            instructions: None,
            max_tool_calls: None,
            service_tier: None,
            tools: None,
            tool_choice: None,
            standard_fields: StandardResponseFields::default(),
        };
        self.inject_identity(&mut payload);
        payload
    }

    /// Inject our `response_id` and `conversation` into a `ResponsePayload`
    /// received from the LLM (which carries the upstream's own IDs).
    pub(crate) fn inject_ids(&self, payload: &mut ResponsePayload) {
        self.inject_identity(payload);
        payload.standard_fields.completed_at = (payload.status == "completed").then(utcnow_str);
    }

    fn inject_identity(&self, payload: &mut ResponsePayload) {
        payload.id.clone_from(&self.response_id);
        payload.conversation.clone_from(&self.conversation_id);
        payload
            .previous_response_id
            .clone_from(&self.original_request.previous_response_id);
        payload.max_tool_calls = self.max_tool_calls().map(NonZeroU64::get);
        payload.standard_fields.apply_request(&self.original_request);
    }

    /// The wire counterpart of [`Self::inject_identity`] for a provider event that carries a
    /// `response`: the gateway's response ID and the request's tool-call limit, plus its parent
    /// and conversation when the request has them.
    pub(crate) fn inject_wire_identity(&self, wire: &mut WireEvent) {
        let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) else {
            return;
        };
        response.insert("id".to_owned(), Value::String(self.response_id.clone()));
        if let Some(previous_response_id) = &self.original_request.previous_response_id {
            response.insert(
                "previous_response_id".to_owned(),
                Value::String(previous_response_id.clone()),
            );
        }
        if let Some(conversation_id) = &self.conversation_id {
            response.insert("conversation".to_owned(), serde_json::json!({"id": conversation_id}));
        }
        let max_tool_calls = self
            .max_tool_calls()
            .map_or(Value::Null, |limit| Value::from(limit.get()));
        response.insert("max_tool_calls".to_owned(), max_tool_calls);
    }
}
