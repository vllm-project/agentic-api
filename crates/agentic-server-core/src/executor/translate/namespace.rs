use super::function::FunctionTranslator;
use super::{ToolEvent, ToolTranslator};
use crate::events::{EventFrame, WireEvent};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::ExecutorResult;
use crate::tool::NamespaceMap;
use crate::types::io::{FunctionToolCall, OutputItem};
use serde_json::{Map, Value};

/// Namespace calls retain function-call events; the dispatcher applies their public mapping.
#[derive(Debug)]
pub(super) struct CodexNamespaceTranslator;

impl ToolTranslator for CodexNamespaceTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        FunctionTranslator.translate(event, call)
    }
}

impl CodexNamespaceTranslator {
    pub(super) fn restore_output_items(output: &mut [OutputItem], map: Option<&NamespaceMap>) {
        let Some(map) = map else {
            return;
        };
        for item in output {
            if let OutputItem::FunctionCall(call) = item {
                restore_function_call_with_map(call, map);
            }
        }
    }

    #[must_use]
    pub(super) fn restore_response_wire(wire: &mut WireEvent, map: Option<&NamespaceMap>) -> bool {
        let Some(map) = map else {
            return false;
        };
        restore_response_map_with_map(&mut wire.rest, map)
    }
}

fn restore_function_call_with_map(call: &mut FunctionToolCall, map: &NamespaceMap) -> bool {
    if call.namespace.is_some() {
        return false;
    }
    let Some((namespace, name)) = map.public_member(&call.name) else {
        return false;
    };
    let original_name = call.name.clone();

    call.namespace = Some(namespace.to_owned());
    name.clone_into(&mut call.name);
    tracing::debug!(
        upstream_name = %original_name,
        namespace = %namespace,
        member = %name,
        "restored upstream namespace function call"
    );
    true
}

fn restore_response_value_with_map(value: &mut Value, map: &NamespaceMap) -> bool {
    let mut changed = false;

    if let Some(object) = value.as_object_mut() {
        changed |= restore_response_metadata_with_map(object, map);
    }

    if let Some(item) = value.as_object_mut().and_then(|object| object.get_mut("item")) {
        changed |= restore_call_value_with_map(item, map);
    }

    changed |= restore_call_value_with_map(value, map);

    for key in ["response", "payload"] {
        if let Some(nested) = value.as_object_mut().and_then(|object| object.get_mut(key)) {
            changed |= restore_response_value_with_map(nested, map);
        }
    }

    if let Some(Value::Array(items)) = value.as_object_mut().and_then(|object| object.get_mut("output")) {
        for item in items {
            changed |= restore_call_value_with_map(item, map);
        }
    }

    changed
}

fn restore_call_value_with_map(value: &mut Value, map: &NamespaceMap) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    if object.get("namespace").and_then(Value::as_str).is_some() {
        return false;
    }
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some((namespace, name)) = map.public_member(name) else {
        return false;
    };
    let original_name = object["name"].as_str().unwrap_or_default().to_owned();

    object.insert("namespace".to_string(), Value::String(namespace.to_owned()));
    object.insert("name".to_string(), Value::String(name.to_owned()));
    tracing::debug!(
        upstream_name = %original_name,
        namespace = %namespace,
        member = %name,
        "restored upstream namespace function call"
    );
    true
}

fn restore_response_metadata_with_map(object: &mut Map<String, Value>, map: &NamespaceMap) -> bool {
    object
        .get_mut("tool_choice")
        .is_some_and(|choice| restore_tool_choice_with_map(choice, map))
}

fn restore_tool_choice_with_map(choice: &mut Value, map: &NamespaceMap) -> bool {
    let Some(object) = choice.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("function")
        || object.get("namespace").and_then(Value::as_str).is_some()
    {
        return false;
    }
    let Some((namespace, name)) = object
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| map.public_member(name))
    else {
        return false;
    };
    object.insert("namespace".to_owned(), Value::String(namespace.to_owned()));
    object.insert("name".to_owned(), Value::String(name.to_owned()));
    true
}

fn restore_response_map_with_map(object: &mut Map<String, Value>, map: &NamespaceMap) -> bool {
    let mut changed = restore_response_metadata_with_map(object, map);
    if let Some(item) = object.get_mut("item") {
        changed |= restore_call_value_with_map(item, map);
    }
    for key in ["response", "payload"] {
        if let Some(nested) = object.get_mut(key) {
            changed |= restore_response_value_with_map(nested, map);
        }
    }
    if let Some(Value::Array(items)) = object.get_mut("output") {
        for item in items {
            changed |= restore_call_value_with_map(item, map);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::CodexNamespaceHandler;
    use crate::tool::codex::model_visible_namespace_member_name;
    use crate::types::event::MessageStatus;
    use crate::types::io::ToolChoice;
    use crate::types::tools::{CodexNamespaceMember, NonEmptyToolName, ResponsesTool};
    fn completed_call(name: &str, arguments: &str) -> OutputItem {
        OutputItem::FunctionCall(FunctionToolCall {
            id: "fc_1".to_string(),
            call_id: "call_1".to_string(),
            name: name.to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            status: MessageStatus::Completed,
        })
    }
    #[test]
    fn long_namespace_member_round_trips_through_shortened_name() {
        let namespace = "mcp__codex_apps__github";
        let member = "_remove_reaction_from_pr_review_comment";
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": namespace,
                "tools": [{"type": "function", "name": member}]
            }
        ]))
        .unwrap();
        let upstream_name = model_visible_namespace_member_name(namespace, member);
        let mut output = vec![completed_call(&upstream_name, "{}")];

        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        assert!(matches!(
            resolved.as_slice(),
            [ResponsesTool::Namespace(namespace)]
                if matches!(&namespace.tools[0], CodexNamespaceMember::Function(function)
                    if function.name.as_str() == upstream_name)
        ));

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        let choice = ToolChoice::Function {
            namespace: Some(namespace.to_string()),
            name: NonEmptyToolName::try_from(member).unwrap(),
        };
        assert_eq!(
            CodexNamespaceHandler.resolve_tool_choice(map.as_ref(), Some(&choice)),
            ToolChoice::Function {
                namespace: None,
                name: NonEmptyToolName::try_from(upstream_name).unwrap(),
            }
        );
        CodexNamespaceTranslator::restore_output_items(&mut output, map.as_ref());

        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert_eq!(call.namespace.as_deref(), Some(namespace));
        assert_eq!(call.name, member);
    }
    #[test]
    fn flat_namespace_member_call_preserves_tools_argument() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__agentic_fixture",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();
        let mut output = vec![completed_call(
            "agentic_ns__mcp__agentic_fixture__run",
            "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}",
        )];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        CodexNamespaceTranslator::restore_output_items(&mut output, map.as_ref());

        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert_eq!(call.namespace.as_deref(), Some("mcp__agentic_fixture"));
        assert_eq!(call.name, "run");
        assert_eq!(call.arguments, "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}");
    }
    #[test]
    fn plain_function_call_round_trip() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object"}
            }
        ]))
        .unwrap();
        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        let mut output = vec![completed_call("get_weather", "{\"city\":\"SF\"}")];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        CodexNamespaceTranslator::restore_output_items(&mut output, map.as_ref());

        assert!(matches!(
            resolved.as_slice(),
            [ResponsesTool::Function(function)] if function.name.as_str() == "get_weather"
        ));
        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert!(call.namespace.is_none());
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments, "{\"city\":\"SF\"}");
    }
    #[test]
    fn response_value_normalizes_nested_function_call_item() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__agentic_fixture",
                "tools": [{"type": "function", "name": "add_numbers"}]
            }
        ]))
        .unwrap();
        let mut value = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "agentic_ns__mcp__agentic_fixture__add_numbers",
                "call_id": "call_1",
                "arguments": "{\"numbers\":[8,0]}"
            }
        });

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        assert!(restore_response_value_with_map(&mut value, map.as_ref().unwrap()));
        assert_eq!(value["item"]["namespace"], "mcp__agentic_fixture");
        assert_eq!(value["item"]["name"], "add_numbers");
        assert_eq!(value["item"]["arguments"], "{\"numbers\":[8,0]}");
    }
    #[test]
    fn response_lifecycle_metadata_restores_public_namespace_tool_choice() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([{
            "type": "namespace",
            "name": "travel",
            "tools": [{"type": "function", "name": "get_timezone"}]
        }]))
        .unwrap();
        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        let mut wire = WireEvent::new("response.created");
        wire.rest.insert(
            "response".to_owned(),
            serde_json::json!({
                "tool_choice": {
                    "type": "function",
                    "name": "agentic_ns__travel__get_timezone"
                }
            }),
        );

        assert!(CodexNamespaceTranslator::restore_response_wire(&mut wire, map.as_ref()));
        assert_eq!(
            wire.rest["response"]["tool_choice"],
            serde_json::json!({
                "type": "function",
                "namespace": "travel",
                "name": "get_timezone"
            })
        );
    }
    #[test]
    fn namespace_wire_translation_preserves_provider_fields() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__agentic_fixture",
                "tools": [{"type": "function", "name": "add_numbers"}]
            }
        ]))
        .unwrap();
        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        let mut wire = WireEvent::new("response.output_item.done");
        wire.output_index = Some(0);
        wire.rest.insert(
            "item".to_owned(),
            serde_json::json!({
                "type": "function_call",
                "name": "agentic_ns__mcp__agentic_fixture__add_numbers",
                "call_id": "call_1",
                "arguments": "{\"numbers\":[8,0]}",
                "provider_extra": {"kept": true}
            }),
        );

        assert!(CodexNamespaceTranslator::restore_response_wire(&mut wire, map.as_ref()));

        let item = &wire.rest["item"];
        assert_eq!(item["namespace"], "mcp__agentic_fixture");
        assert_eq!(item["name"], "add_numbers");
        assert_eq!(item["arguments"], "{\"numbers\":[8,0]}");
        assert_eq!(item["provider_extra"]["kept"], true);
    }
}
