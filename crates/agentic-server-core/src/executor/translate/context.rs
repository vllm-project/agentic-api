use crate::events::WireEvent;
use crate::executor::error::ExecutorResult;
use crate::tool::custom::CustomToolMap;
use crate::tool::{NamespaceMap, ToolType};
use crate::types::event::ResponseStatus;
use crate::types::io::MultiAgentAction;
use crate::types::io::OutputItem;
use crate::types::io::ToolChoice;
use crate::types::request_response::{RequestPayload, ResponsePayload, StandardResponseFields};
use crate::types::tools::ResponsesTool;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// An owned snapshot of the effective tool classification and availability for one round.
#[derive(Default)]
pub(in crate::executor) struct TranslationContext {
    tool_types: HashMap<String, ToolType>,
    gateway_owned_names: HashSet<String>,
    withheld_function_names: HashSet<String>,
    tool_search_active: bool,
    collaboration_enabled: bool,
    namespace_map: Option<NamespaceMap>,
    custom_tool_map: Option<CustomToolMap>,
    response_tools: Option<Vec<ResponsesTool>>,
    response_tool_choice: Option<ToolChoice>,
    response_fields: StandardResponseFields,
}

impl std::fmt::Debug for TranslationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslationContext")
            .field("tool_count", &self.tool_types.len())
            .field("withheld_count", &self.withheld_function_names.len())
            .field("tool_search_active", &self.tool_search_active)
            .finish_non_exhaustive()
    }
}

impl TranslationContext {
    pub(in crate::executor) fn with_collaboration(mut self, enabled: bool) -> Self {
        self.collaboration_enabled = enabled;
        self
    }

    pub(super) fn is_collaboration(&self, name: &str) -> bool {
        self.collaboration_enabled && MultiAgentAction::from_tool_name(name).is_some()
    }
    pub(in crate::executor) fn new(
        tool_types: HashMap<String, ToolType>,
        withheld_function_names: HashSet<String>,
        tool_search_active: bool,
    ) -> Self {
        Self {
            tool_types,
            withheld_function_names,
            tool_search_active,
            ..Self::default()
        }
    }

    /// Resolved ownership copied from the registry, including opt-in gateway shell execution.
    pub(in crate::executor) fn with_gateway_owned_names(mut self, names: HashSet<String>) -> Self {
        self.gateway_owned_names = names;
        self
    }

    pub(super) fn is_gateway_owned(&self, name: &str) -> bool {
        self.gateway_owned_names.contains(name)
    }

    /// Owned public mappings; construction performs no registry lookups.
    pub(in crate::executor) fn with_response_metadata(
        mut self,
        namespace_map: Option<NamespaceMap>,
        custom_tool_map: Option<CustomToolMap>,
        response_tools: Option<Vec<ResponsesTool>>,
        response_tool_choice: Option<ToolChoice>,
    ) -> Self {
        self.namespace_map = namespace_map;
        self.custom_tool_map = custom_tool_map;
        self.response_tools = super::tool_search::public_response_tools(response_tools);
        self.response_tool_choice = response_tool_choice;
        self
    }

    pub(in crate::executor) fn with_request_fields(mut self, request: &RequestPayload) -> Self {
        self.response_fields.apply_request(request);
        self
    }

    pub(super) fn restore_stream_event_wire(&self, wire: &mut WireEvent) -> ExecutorResult<()> {
        super::tool_search::restore_response_tools(wire, self.response_tools.as_deref())?;
        if self.response_tools.is_some()
            && let Some(choice) = self.response_tool_choice.as_ref()
            && let Some(response) = wire.rest.get_mut("response").and_then(serde_json::Value::as_object_mut)
            && response.contains_key("tool_choice")
        {
            response.insert("tool_choice".to_owned(), serde_json::to_value(choice)?);
        }
        super::custom::CustomTranslator::restore_response_wire(wire, self.custom_tool_map.as_ref());
        let _ = super::namespace::CodexNamespaceTranslator::restore_response_wire(wire, self.namespace_map.as_ref());
        if let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) {
            complete_response_fields(response, &self.response_fields)?;
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                for item in output {
                    normalize_output_item(item);
                }
            }
        }
        if let Some(item) = wire.rest.get_mut("item") {
            normalize_output_item(item);
        }
        if let Some(part) = wire.rest.get_mut("part") {
            normalize_content_part(part);
        }
        Ok(())
    }

    pub(super) fn restore_response_metadata(&self, payload: &mut ResponsePayload) {
        if let Some(tools) = &self.response_tools {
            payload.tools = Some(tools.clone());
            payload.tool_choice = Some(self.response_tool_choice.clone().unwrap_or_default());
        }
    }

    pub(in crate::executor) fn tool_type(&self, name: &str) -> ToolType {
        if self.tool_search_active && name == crate::tool::tool_search::TOOL_SEARCH_NAME {
            ToolType::ToolSearch
        } else {
            self.tool_types.get(name).copied().unwrap_or(ToolType::Function)
        }
    }

    pub(super) fn is_withheld_function(&self, name: &str) -> bool {
        self.withheld_function_names.contains(name)
    }

    pub(super) fn tool_search_is_active(&self) -> bool {
        self.tool_search_active
    }

    pub(super) fn validate_json_body(&self, body: &str) -> ExecutorResult<()> {
        crate::tool::tool_search::validate_blocking_response(
            body,
            self.tool_search_active,
            &self.withheld_function_names,
        )?;
        Ok(())
    }

    pub(in crate::executor) fn normalize_response_output(
        &self,
        output: &mut Vec<OutputItem>,
        status: ResponseStatus,
        unfinished_stream_item_ids: &HashSet<String>,
    ) -> ExecutorResult<()> {
        super::tool_search::normalize_response_output(self, output, status, unfinished_stream_item_ids)?;
        super::namespace::CodexNamespaceTranslator::restore_output_items(output, self.namespace_map.as_ref());
        Ok(())
    }
}

fn complete_response_fields(response: &mut Map<String, Value>, fields: &StandardResponseFields) -> ExecutorResult<()> {
    let Value::Object(standard) = serde_json::to_value(fields)? else {
        unreachable!("standard response fields serialize as an object");
    };
    for (key, value) in standard {
        if key == "completed_at" {
            response.entry(key).or_insert(value);
        } else {
            response.insert(key, value);
        }
    }
    let required_defaults = [
        ("error", Value::Null),
        ("incomplete_details", Value::Null),
        ("previous_response_id", Value::Null),
        ("instructions", Value::Null),
        ("tools", json!([])),
        ("tool_choice", json!("auto")),
        ("usage", Value::Null),
        ("max_tool_calls", Value::Null),
        ("service_tier", json!("default")),
    ];
    for (key, default) in required_defaults {
        if !response.contains_key(key) {
            response.insert(key.to_owned(), default);
        }
    }
    Ok(())
}

fn normalize_output_item(item: &mut Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        for field in ["content", "summary"] {
            if item.get(field).is_some_and(Value::is_null) {
                item.insert(field.to_owned(), json!([]));
            }
        }
        if item.get("encrypted_content").is_some_and(Value::is_null) {
            item.remove("encrypted_content");
        }
    }
    if item.get("type").and_then(Value::as_str) == Some("message") && item.get("phase").is_some_and(Value::is_null) {
        item.remove("phase");
    }
    if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
        for part in content {
            normalize_content_part(part);
        }
    }
}

fn normalize_content_part(part: &mut Value) {
    let Some(part) = part.as_object_mut() else {
        return;
    };
    if part.get("type").and_then(Value::as_str) == Some("output_text")
        && part.get("logprobs").is_some_and(Value::is_null)
    {
        part.insert("logprobs".to_owned(), json!([]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_lifecycle_has_required_standard_fields() {
        let mut wire = WireEvent::new("response.created");
        wire.rest.insert(
            "response".to_owned(),
            json!({
                "id": "resp_1", "object": "response", "created_at": 1,
                "model": "test", "status": "in_progress", "output": [],
                "text": null, "top_logprobs": null, "parallel_tool_calls": true,
                "temperature": 0.9
            }),
        );
        TranslationContext::default()
            .with_request_fields(&RequestPayload {
                store: false,
                ..RequestPayload::default()
            })
            .restore_stream_event_wire(&mut wire)
            .expect("normalize response event");
        let response = &wire.rest["response"];
        assert_eq!(response["text"]["format"]["type"], "text");
        assert_eq!(response["top_logprobs"], 0);
        assert_eq!(response["store"], false);
        assert_eq!(response["parallel_tool_calls"], false);
        assert_eq!(response["temperature"], 1.0);
        assert_eq!(response["completed_at"], Value::Null);
        assert_eq!(response["tools"], json!([]));
    }

    #[test]
    fn upstream_reasoning_and_content_parts_use_public_event_shapes() {
        let context = TranslationContext::default();
        let mut reasoning = WireEvent::new("response.reasoning_text.delta");
        context
            .restore_stream_event_wire(&mut reasoning)
            .expect("normalize reasoning event");
        assert_eq!(reasoning.event_type.as_deref(), Some("response.reasoning_text.delta"));

        let mut reasoning_part = WireEvent::new("response.reasoning_part.added");
        context
            .restore_stream_event_wire(&mut reasoning_part)
            .expect("preserve reasoning part event");
        assert_eq!(
            reasoning_part.event_type.as_deref(),
            Some("response.reasoning_part.added")
        );

        let mut item = WireEvent::new("response.output_item.added");
        item.rest.insert(
            "item".to_owned(),
            json!({
                "type": "reasoning", "id": "rs_1", "content": null,
                "summary": null, "encrypted_content": null
            }),
        );
        context.restore_stream_event_wire(&mut item).expect("normalize item");
        assert_eq!(item.rest["item"]["content"], json!([]));
        assert_eq!(item.rest["item"]["summary"], json!([]));
        assert!(item.rest["item"].get("encrypted_content").is_none());

        let mut part = WireEvent::new("response.content_part.done");
        part.rest.insert(
            "part".to_owned(),
            json!({
                "type": "output_text", "text": "ok", "annotations": [], "logprobs": null
            }),
        );
        context.restore_stream_event_wire(&mut part).expect("normalize part");
        assert_eq!(part.rest["part"]["logprobs"], json!([]));
    }
}
