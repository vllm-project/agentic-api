//! Public names for namespace members in a response's `tool_choice`. Output calls get theirs from
//! [`TranslationContext`](super::TranslationContext)'s call rewrite.

use crate::tool::NamespaceMap;
use serde_json::Value;

/// Restores a function `tool_choice` that names a shortened namespace member to its public
/// namespace and member name.
pub(super) fn restore_tool_choice(choice: &mut Value, map: &NamespaceMap) {
    let Some(object) = choice.as_object_mut() else {
        return;
    };
    if object.get("type").and_then(Value::as_str) != Some("function")
        || object.get("namespace").and_then(Value::as_str).is_some()
    {
        return;
    }
    let Some((namespace, name)) = object
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| map.public_member(name))
    else {
        return;
    };
    object.insert("namespace".to_owned(), Value::String(namespace.to_owned()));
    object.insert("name".to_owned(), Value::String(name.to_owned()));
}

#[cfg(test)]
mod tests {
    use super::super::TranslationContext;
    use crate::events::WireEvent;
    use crate::tool::ToolDeclaration;
    use crate::tool::codex::model_visible_namespace_member_name;
    use crate::tool::{CodexNamespaceHandler, responses_declarations};
    use crate::types::event::MessageStatus;
    use crate::types::io::{FunctionToolCall, OutputItem, ToolChoice};
    use crate::types::tools::{CodexNamespaceMember, NonEmptyToolName, ResponsesTool};
    fn completed_call(name: &str, arguments: &str) -> OutputItem {
        OutputItem::FunctionCall(FunctionToolCall {
            async_execution: false,
            agent: None,
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
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": namespace,
                    "tools": [{"type": "function", "name": member}]
                }
            ]))
            .unwrap(),
        );
        let upstream_name = model_visible_namespace_member_name(namespace, member);
        let mut output = [completed_call(&upstream_name, "{}")];

        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        assert!(matches!(
            resolved.as_slice(),
            [ToolDeclaration::Namespace(namespace)]
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
        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .normalize_output_item(&mut output[0]);

        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert_eq!(call.namespace.as_deref(), Some(namespace));
        assert_eq!(call.name, member);
    }
    #[test]
    fn flat_namespace_member_call_preserves_tools_argument() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": "mcp__agentic_fixture",
                    "tools": [{"type": "function", "name": "run"}]
                }
            ]))
            .unwrap(),
        );
        let mut output = [completed_call(
            "agentic_ns__mcp__agentic_fixture__run",
            "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}",
        )];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .normalize_output_item(&mut output[0]);

        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert_eq!(call.namespace.as_deref(), Some("mcp__agentic_fixture"));
        assert_eq!(call.name, "run");
        assert_eq!(call.arguments, "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}");
    }
    #[test]
    fn plain_function_call_round_trip() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "function",
                    "name": "get_weather",
                    "parameters": {"type": "object"}
                }
            ]))
            .unwrap(),
        );
        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        let mut output = [completed_call("get_weather", "{\"city\":\"SF\"}")];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .normalize_output_item(&mut output[0]);

        assert!(matches!(
            resolved.as_slice(),
            [ToolDeclaration::Function(function)] if function.name.as_str() == "get_weather"
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
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": "mcp__agentic_fixture",
                    "tools": [{"type": "function", "name": "add_numbers"}]
                }
            ]))
            .unwrap(),
        );
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
        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .normalize_wire_item(&mut value["item"]);
        assert_eq!(value["item"]["namespace"], "mcp__agentic_fixture");
        assert_eq!(value["item"]["name"], "add_numbers");
        assert_eq!(value["item"]["arguments"], "{\"numbers\":[8,0]}");
    }
    #[test]
    fn response_lifecycle_metadata_restores_public_namespace_tool_choice() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([{
                "type": "namespace",
                "name": "travel",
                "tools": [{"type": "function", "name": "get_timezone"}]
            }]))
            .unwrap(),
        );
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

        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .restore_stream_event_wire(&mut wire)
            .unwrap();
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
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": "mcp__agentic_fixture",
                    "tools": [{"type": "function", "name": "add_numbers"}]
                }
            ]))
            .unwrap(),
        );
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

        TranslationContext::default()
            .with_response_metadata(map, None, None, None)
            .restore_stream_event_wire(&mut wire)
            .unwrap();

        let item = &wire.rest["item"];
        assert_eq!(item["namespace"], "mcp__agentic_fixture");
        assert_eq!(item["name"], "add_numbers");
        assert_eq!(item["arguments"], "{\"numbers\":[8,0]}");
        assert_eq!(item["provider_extra"]["kept"], true);
    }
}
