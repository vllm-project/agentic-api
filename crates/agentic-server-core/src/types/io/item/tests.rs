use super::*;
use crate::types::conversations::CreateItemRequest;
use crate::types::event::MessageStatus;
use crate::types::io::output::{McpListTools, MessagePhase};
use crate::types::io::{
    FunctionToolCall, InputContent, InputMessage, InputMessageContent, OutputMessage, OutputTextContent,
    ReasoningOutput, ReasoningTextContent, ResponsesInput, ShellCall, ShellCallAction, ShellCallLimit, ShellCallStatus,
};
use serde_json::json;

#[test]
fn test_inout_item_from_input() {
    let input = InputItem::Message(InputMessage {
        role: "user".to_string(),
        content: InputMessageContent::Text("hello".to_string()),
        ..Default::default()
    });
    let item: InOutItem = input.into();
    assert!(matches!(item, InOutItem::Input(_)));
}

#[test]
fn test_inout_item_from_output() {
    let output = OutputItem::Message(OutputMessage::new("msg_1", MessageStatus::Completed));
    let item: InOutItem = output.into();
    assert!(matches!(item, InOutItem::Output(_)));
}

#[test]
fn test_into_input_items_converts_output_messages() {
    let mut output = OutputMessage::new("out1", MessageStatus::Completed);
    output.phase = Some(MessagePhase::FinalAnswer);
    output.content.push(OutputTextContent::new("answer").into());
    let items = vec![
        InOutItem::Input(InputItem::Message(InputMessage {
            role: "user".to_string(),
            content: InputMessageContent::Text("msg1".to_string()),
            ..Default::default()
        })),
        InOutItem::Output(OutputItem::Message(output)),
        InOutItem::Input(InputItem::Message(InputMessage {
            role: "user".to_string(),
            content: InputMessageContent::Text("msg2".to_string()),
            ..Default::default()
        })),
    ];

    let inputs = InOutItem::into_input_items(items);
    assert_eq!(inputs.len(), 3);
    match &inputs[1] {
        InputItem::Message(message) => {
            assert_eq!(message.role, "assistant");
            assert_eq!(message.phase, Some(MessagePhase::FinalAnswer));
            match &message.content {
                InputMessageContent::Parts(parts) => {
                    assert_eq!(parts.len(), 1);
                    match &parts[0] {
                        InputContent::OutputText(t) => {
                            assert_eq!(t.text, "answer");
                        }
                        _ => panic!("expected OutputText part"),
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
        InOutItem::Output(OutputItem::Message(OutputMessage::new(
            "msg_1",
            MessageStatus::Completed,
        ))),
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
fn test_into_input_items_preserves_function_calls() {
    use crate::types::event::MessageStatus;
    let fc = FunctionToolCall {
        async_execution: false,
        agent: None,
        id: "fc_1".to_string(),
        call_id: "call_abc".to_string(),
        name: "my_tool".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        status: MessageStatus::Completed,
    };
    let items = vec![InOutItem::Output(OutputItem::FunctionCall(fc))];
    let inputs = InOutItem::into_input_items(items);
    assert_eq!(inputs.len(), 1);
    assert!(matches!(inputs[0], InputItem::FunctionCall(_)));
    if let InputItem::FunctionCall(f) = &inputs[0] {
        assert_eq!(f.name, "my_tool");
    }
}

#[test]
fn test_into_input_items_preserves_shell_calls() {
    let call = ShellCall {
        agent: None,
        id: Some("sh_1".to_owned()),
        call_id: "call_shell".to_owned(),
        action: ShellCallAction {
            commands: vec!["pwd".to_owned()],
            timeout_ms: Some(ShellCallLimit::Value(1_000)),
            max_output_length: Some(ShellCallLimit::Value(4_096)),
            extra: std::collections::HashMap::new(),
        },
        status: Some(ShellCallStatus::Completed),
        extra: std::collections::HashMap::new(),
    };

    let inputs = InOutItem::into_input_items(vec![InOutItem::Output(OutputItem::ShellCall(call))]);
    assert!(
        matches!(inputs.as_slice(), [InputItem::FunctionCall(call)] if call.call_id == "call_shell" && call.name == "shell")
    );
}

#[test]
fn input_items_preserve_mcp_list_tools_until_model_input() {
    let history = vec![InOutItem::Output(OutputItem::McpListTools(McpListTools::new(
        "mcpl_1",
        "counter",
        Vec::new(),
    )))];

    let input_items = InOutItem::into_input_items(history);

    assert!(matches!(
        input_items.as_slice(),
        [InputItem::McpListTools(list_tools)] if list_tools.server_label == "counter"
    ));
    let model_input = ResponsesInput::from(input_items).model_input().into_owned();
    let ResponsesInput::Items(model_items) = model_input else {
        panic!("model input should contain items");
    };
    assert!(model_items.is_empty());
}

#[test]
fn shared_history_rejects_partial_upstream_calls() {
    for value in [
        serde_json::json!({"type": "function_call", "name": "weather"}),
        serde_json::json!({"type": "function_call", "name": "weather", "call_id": "call_1"}),
    ] {
        // Upstream decoding permits shapes that request replay must not accept.
        assert!(serde_json::from_value::<OutputItem>(value.clone()).is_ok());
        assert!(serde_json::from_value::<InOutItem>(value.clone()).is_err());
        assert!(serde_json::from_value::<ResponsesInput>(serde_json::json!([value.clone()])).is_err());
        assert!(serde_json::from_value::<CreateItemRequest>(serde_json::json!({"items": [value]})).is_err());
    }
}

#[test]
fn output_history_is_preserved_until_model_projection() {
    let call = FunctionToolCall {
        id: "fc_1".to_owned(),
        call_id: "call_1".to_owned(),
        name: "weather".to_owned(),
        arguments: "{}".to_owned(),
        status: MessageStatus::Completed,
        async_execution: true,
        agent: None,
        namespace: None,
    };
    let history = ResponsesInput::from(vec![InOutItem::Output(OutputItem::FunctionCall(call))]);
    let original = serde_json::to_value(&history).unwrap();
    assert!(history.has_async_call());
    let ResponsesInput::Items(projected) = history.model_input().into_owned() else {
        panic!("item history")
    };
    assert!(matches!(projected.as_slice(), [InOutItem::Input(InputItem::FunctionCall(call))] if call.async_execution));
    assert_eq!(serde_json::to_value(&history).unwrap(), original);
    assert!(matches!(
        history.into_canonical_items().as_slice(),
        [InOutItem::Output(OutputItem::FunctionCall(_))]
    ));
}

#[test]
fn preserves_gateway_call_input_and_rejects_unknown_types() {
    for wire in [
        json!({"type":"web_search_call", "id":"ws_1", "status":"completed", "action":{"type":"search", "query":"weather", "queries":["weather"], "sources":[{"url":"https://example.com"}]}}),
        json!({"type":"mcp_call", "id":"mcp_1", "name":"weather", "server_label":"weather", "arguments":"{}", "output":"sunny", "status":"completed"}),
    ] {
        let item: InOutItem = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(item, InOutItem::Input(_)));
        let actual = serde_json::to_value(item).unwrap();
        for (key, value) in wire.as_object().unwrap() {
            assert_eq!(&actual[key], value, "{key}");
        }
    }
    assert!(serde_json::from_value::<InOutItem>(json!({"type":"future_item", "payload":42})).is_err());
    assert!(serde_json::from_value::<InOutItem>(json!({"type":"function_call_output"})).is_err());
}

#[test]
fn item_resource_has_one_id_and_flattened_content() {
    let item: InOutItem =
        serde_json::from_value(json!({"type":"message", "id":"upstream_id", "role":"user", "content":"hello"}))
            .unwrap();
    let resource = item.into_public("item_stored".into());
    let text = serde_json::to_string(&resource).unwrap();
    assert_eq!(text.matches("\"id\"").count(), 1);
    let actual: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(actual["id"], "item_stored");
    assert_eq!(actual["type"], "message");
    assert_eq!(actual["status"], "completed");
    assert_eq!(actual["content"], json!([{"type":"input_text", "text":"hello"}]));
    assert!(actual.get("item").is_none());
}

#[test]
fn function_call_history_status_is_preserved() {
    for wire in [
        json!({"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}),
        json!({"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}","status":"completed"}),
    ] {
        let item: InOutItem = serde_json::from_value(wire.clone()).unwrap();
        let public = serde_json::to_value(item.into_public("fc_history".into())).unwrap();
        assert_eq!(public.get("status"), wire.get("status"));
        assert_eq!(public["call_id"], "call_1");
    }
}

#[test]
fn manually_added_assistant_shorthand_remains_input_text() {
    let item: InOutItem = serde_json::from_value(json!({"type":"message", "role":"assistant", "content":"9"})).unwrap();
    let resource = item.into_public("msg_manual".into());
    let actual = serde_json::to_value(resource).unwrap();
    assert_eq!(actual["role"], "assistant");
    assert_eq!(actual["content"], json!([{"type":"input_text", "text":"9"}]));
}

#[test]
fn public_item_ids_are_typed_and_preserve_tool_and_orchestration_payloads() {
    for wire in [
        json!({"type":"custom_tool_call_output", "id":"old", "call_id":"call_1", "output":"done", "status":"in_progress"}),
        json!({"type":"tool_search_output", "id":"old", "call_id":"call_1", "tools":[]}),
        json!({"type":"web_search_call", "id":"old", "status":"completed", "action":{"type":"search", "query":"weather"}}),
        json!({"type":"mcp_list_tools", "id":"old", "server_label":"weather", "tools":[]}),
        json!({"type":"compaction", "id":"old", "encrypted_content":"opaque"}),
    ] {
        let item: InOutItem = serde_json::from_value(wire).unwrap();
        assert_eq!(item.id(), Some("old"));
        let mut expected = serde_json::to_value(&item).unwrap();
        expected["id"] = json!("stored");
        let public = item.into_public("stored".into());
        assert_eq!(public.id(), Some("stored"));
        assert_eq!(serde_json::to_value(public).unwrap(), expected);
    }

    let item = InOutItem::Output(OutputItem::Message(OutputMessage::new(
        "upstream",
        MessageStatus::Completed,
    )));
    let mut expected = serde_json::to_value(&item).unwrap();
    expected["id"] = json!("stored");
    let public = item.into_public("stored".into());
    assert!(matches!(public, InOutItem::Output(_)));
    assert_eq!(serde_json::to_value(public).unwrap(), expected);
}

#[test]
fn item_limit_serde_preserves_bounds() {
    for value in [0_i64, 20, 100] {
        let limit: ItemLimit = serde_json::from_value(json!(value)).unwrap();
        assert_eq!(i64::from(limit), value);
        assert_eq!(serde_json::to_value(limit).unwrap(), value);
    }
    for value in [json!(-1), json!(101), json!(1.5)] {
        assert!(serde_json::from_value::<ItemLimit>(value).is_err());
    }
}

#[test]
fn empty_pages_match_wire_contract() {
    assert_eq!(
        serde_json::to_value(ListItemsResponse::new(vec![], false)).unwrap(),
        json!({"object":"list", "data":[], "has_more":false, "first_id":null, "last_id":null})
    );
}

#[test]
fn public_items_preserve_failed_and_incomplete_tool_states() {
    for wire in [
        json!({"type":"code_interpreter_call", "id":"ci_1", "container_id":"cntr_1", "code":"1/0", "outputs":[], "status":"failed"}),
        json!({"type":"web_search_call", "id":"ws_1", "action":{"type":"search", "query":"weather"}, "status":"failed"}),
        json!({"type":"tool_search_call", "id":"tsc_1", "call_id":"call_1", "arguments":{}, "status":"incomplete"}),
        json!({"type":"tool_search_output", "id":"tso_1", "call_id":"call_1", "tools":[], "status":"incomplete"}),
        json!({"type":"mcp_call", "id":"mcp_1", "server_label":"weather", "name":"forecast", "arguments":"{}", "error":"unavailable", "status":"failed"}),
        json!({"type":"shell_call_output", "id":"sho_1", "call_id":"call_1", "output":[], "status":"incomplete"}),
        json!({"type":"function_call_output", "id":"fc_1", "call_id":"call_1", "output":"partial", "status":"in_progress"}),
        json!({"type":"custom_tool_call_output", "id":"ctco_1", "call_id":"call_1", "output":"partial", "status":"in_progress"}),
    ] {
        let item: InOutItem = serde_json::from_value(wire).unwrap();
        let mut expected = serde_json::to_value(&item).unwrap();
        expected["id"] = json!("stored");
        assert_eq!(
            serde_json::to_value(item.into_public("stored".into())).unwrap(),
            expected
        );
    }
}
