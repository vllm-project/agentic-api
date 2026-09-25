use serde_json::json;

use super::*;

fn param(value: Value) -> ToolSearchToolParam {
    let ResponsesTool::ToolSearch(param) = serde_json::from_value(value).expect("valid tool_search declaration") else {
        panic!("expected tool_search");
    };
    param
}

fn assert_invalid_blocking_search(state: &ToolSearchState, case: &str, item: &Value) {
    let body = json!({"status": "completed", "output": [item]}).to_string();
    assert!(
        matches!(
            validate_blocking_response(&body, state.is_active(), state.withheld_function_names()),
            Err(ToolError::InvalidUpstreamToolSearch)
        ),
        "{case}"
    );
}

#[test]
fn handler_validates_and_normalizes_exactly_one_function() {
    let param = param(json!({
        "type": "tool_search",
        "execution": "client",
        "description": "Find matching tools",
        "parameters": {"type": "array", "items": {"type": "string"}}
    }));
    ToolSearchHandler.validate(&param).unwrap();
    assert_eq!(ToolSearchHandler.tool_type(), ToolType::ToolSearch);
    assert_eq!(
        serde_json::to_value(ToolSearchHandler.normalize(&param)).unwrap(),
        json!([{
            "type": "function",
            "name": "tool_search",
            "description": "Find matching tools",
            "parameters": {"type": "array", "items": {"type": "string"}},
            "strict": false
        }])
    );
}

#[test]
fn handler_rejects_non_object_parameter_values() {
    let param = param(json!({
        "type": "tool_search",
        "execution": "client",
        "parameters": ["not", "an", "object"]
    }));

    let error = ToolSearchHandler
        .validate(&param)
        .expect_err("private function parameters require an object");
    assert!(error.to_string().contains("parameters must be a JSON object"));
}

#[test]
fn normalization_uses_safe_defaults() {
    let param = param(json!({"type": "tool_search", "execution": "client", "description": "  "}));
    assert_eq!(
        serde_json::to_value(ToolSearchHandler.normalize(&param)).unwrap(),
        json!([{
            "type": "function",
            "name": "tool_search",
            "description": "Search the client tool catalog",
            "parameters": {
                "type": "object",
                "properties": {"query": {
                    "type": "string",
                    "description": "A concise description of the needed capabilities."
                }},
                "required": ["query"],
                "additionalProperties": false
            },
            "strict": false
        }])
    );
}

#[test]
fn synthetic_public_call_construction_is_validated_in_tool_layer() {
    let valid = FunctionToolCall {
        agent: None,
        id: "fc_search".to_owned(),
        call_id: "call_search".to_owned(),
        name: TOOL_SEARCH_NAME.to_owned(),
        namespace: None,
        arguments: r#"["weather","timezone"]"#.to_owned(),
        status: MessageStatus::Completed,
    };
    let started = started_public_call(&valid).expect("valid started call");
    assert_eq!(started.id, "tsc_search");
    assert_eq!(started.status, ToolSearchStatus::InProgress);
    let completed = completed_public_call(&valid).expect("valid completed call");
    assert_eq!(completed.arguments, json!(["weather", "timezone"]));

    for invalid in [
        FunctionToolCall {
            name: "ordinary".to_owned(),
            ..valid.clone()
        },
        FunctionToolCall {
            namespace: Some("catalog".to_owned()),
            ..valid.clone()
        },
        FunctionToolCall {
            arguments: "not valid JSON".to_owned(),
            ..valid.clone()
        },
        FunctionToolCall {
            status: MessageStatus::InProgress,
            ..valid.clone()
        },
    ] {
        assert!(completed_public_call(&invalid).is_err());
    }
}

#[test]
fn terminal_projection_preserves_incomplete_and_discards_failed_calls() {
    let synthetic = FunctionToolCall {
        agent: None,
        id: "fc_search".to_owned(),
        call_id: "call_search".to_owned(),
        name: TOOL_SEARCH_NAME.to_owned(),
        namespace: None,
        arguments: r#"{"query":"partial"#.to_owned(),
        status: MessageStatus::InProgress,
    };
    let projected = project_synthetic_call(&synthetic, ResponseStatus::Incomplete, true)
        .expect("incomplete synthetic projection")
        .expect("incomplete synthetic call remains public");
    assert_eq!(projected.id, "tsc_search");
    assert_eq!(projected.arguments, json!({}));
    assert_eq!(projected.status, ToolSearchStatus::Incomplete);
    assert!(
        project_synthetic_call(&synthetic, ResponseStatus::Error, true)
            .unwrap()
            .is_none()
    );
    assert!(project_synthetic_call(&synthetic, ResponseStatus::Completed, true).is_err());

    let native = ToolSearchCall {
        agent: None,
        id: "tsc_search".to_owned(),
        call_id: "call_search".to_owned(),
        execution: crate::types::tools::ToolSearchExecution::Client,
        arguments: json!({"query": "weather"}),
        status: ToolSearchStatus::InProgress,
    };
    let projected = project_native_call(&native, ResponseStatus::Incomplete)
        .expect("incomplete native projection")
        .expect("incomplete native call remains public");
    assert_eq!(projected.arguments, native.arguments);
    assert_eq!(projected.status, ToolSearchStatus::Incomplete);
    assert!(project_native_call(&native, ResponseStatus::Error).unwrap().is_none());
    assert!(project_native_call(&native, ResponseStatus::Completed).is_err());
}

#[test]
fn tool_search_requires_preparation_before_upstream_conversion() {
    let mut request: RequestPayload = serde_json::from_value(json!({
        "model": "test",
        "input": "find weather",
        "parallel_tool_calls": false,
        "tools": [{"type": "tool_search", "execution": "client"}]
    }))
    .expect("request shape");

    assert!(ensure_request_prepared(&request, false).is_err());
    let state = ToolSearchHandler::prepare_request(&mut request, &[], false)
        .expect("tool-search preparation")
        .expect("active tool-search state");
    ensure_request_prepared(&request, state.is_active()).expect("prepared request is ready for upstream conversion");
}

#[test]
fn preparation_retains_mcp_list_metadata_until_upstream_projection() {
    let mut request: RequestPayload = serde_json::from_value(json!({
        "model": "test",
        "input": [
            {
                "type": "mcp_list_tools",
                "id": "mcpl_counter",
                "server_label": "counter",
                "tools": []
            },
            {"role": "user", "content": "find weather"}
        ],
        "parallel_tool_calls": false,
        "tools": [{"type": "tool_search", "execution": "client"}]
    }))
    .expect("request shape");

    ToolSearchHandler::prepare_request(&mut request, &[], false)
        .expect("tool-search preparation")
        .expect("active tool-search state");

    let ResponsesInput::Items(prepared_items) = &request.input else {
        panic!("expected prepared item input");
    };
    assert!(
        prepared_items
            .iter()
            .any(|item| matches!(item, InputItem::McpListTools(list) if list.server_label == "counter"))
    );

    let model_input = request.input.model_input();
    let ResponsesInput::Items(model_items) = model_input.as_ref() else {
        panic!("expected model item input");
    };
    assert!(
        model_items
            .iter()
            .all(|item| !matches!(item, InputItem::McpListTools(_)))
    );
}

#[test]
fn preparation_preserves_shell_declarations_and_history() {
    let mut request: RequestPayload = serde_json::from_value(json!({
        "model": "test",
        "tools": [
            {"type": "tool_search", "execution": "client"},
            {"type": "shell", "environment": {"type": "local"}}
        ],
        "input": [
            {"type": "shell_call", "call_id": "call_shell", "action": {"commands": ["pwd"]}},
            {"type": "shell_call_output", "call_id": "call_shell", "output": [
                {"stdout": "/workspace", "outcome": {"type": "exit", "exit_code": 0}}
            ]}
        ]
    }))
    .expect("shell history with tool search");
    let original_input = serialize_to_value(&request.input).expect("input serializes");

    let state = ToolSearchHandler::prepare_request(&mut request, &[], false)
        .expect("tool-search preparation")
        .expect("active tool search");

    assert_eq!(serialize_to_value(&request.input).unwrap(), original_input);
    assert!(
        request
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|tool| matches!(tool, ResponsesTool::Shell(_)))
    );
    assert!(
        state
            .public_response_tools()
            .iter()
            .any(|tool| matches!(tool, ResponsesTool::Shell(_)))
    );
}

#[test]
fn ordinary_function_named_tool_search_does_not_require_preparation() {
    let request: RequestPayload = serde_json::from_value(json!({
        "model": "test",
        "input": "call the ordinary function",
        "tools": [{"type": "function", "name": "tool_search"}]
    }))
    .expect("ordinary function request");

    ensure_request_prepared(&request, false).expect("the reserved name applies only to active tool search");
}

#[test]
fn prepared_state_validates_blocking_search_without_changing_inactive_functions() {
    let mut request: RequestPayload = serde_json::from_value(json!({
        "model": "test",
        "input": "find weather",
        "parallel_tool_calls": false,
        "tools": [{"type": "tool_search", "execution": "client"}]
    }))
    .expect("request shape");
    let state = ToolSearchHandler::prepare_request(&mut request, &[], false)
        .expect("tool-search preparation")
        .expect("active tool-search state");
    let native = json!({
        "type": "tool_search_call",
        "id": "tsc_1",
        "call_id": "call_search",
        "execution": "client",
        "arguments": ["weather", "timezone"],
        "status": "completed"
    });
    let synthetic = json!({
        "type": "function_call",
        "id": "fc_search",
        "call_id": "call_search",
        "name": "tool_search",
        "arguments": "[\"weather\",\"timezone\"]",
        "status": "completed"
    });

    for item in [&native, &synthetic] {
        let body = json!({"status": "completed", "output": [item]}).to_string();
        validate_blocking_response(&body, state.is_active(), state.withheld_function_names())
            .expect("native and synthetic array arguments are valid");
    }
    let malformed = [
        ("native missing id", {
            let mut item = native.clone();
            item.as_object_mut().unwrap().remove("id");
            item
        }),
        ("native missing call_id", {
            let mut item = native.clone();
            item.as_object_mut().unwrap().remove("call_id");
            item
        }),
        ("native missing arguments", {
            let mut item = native.clone();
            item.as_object_mut().unwrap().remove("arguments");
            item
        }),
        ("native namespace", {
            let mut item = native.clone();
            item["namespace"] = json!("catalog");
            item
        }),
        ("synthetic missing status", {
            let mut item = synthetic.clone();
            item.as_object_mut().unwrap().remove("status");
            item
        }),
        ("synthetic null status", {
            let mut item = synthetic.clone();
            item["status"] = Value::Null;
            item
        }),
        ("synthetic invalid JSON arguments", {
            let mut item = synthetic.clone();
            item["arguments"] = json!("not valid JSON");
            item
        }),
    ];

    for (case, item) in malformed {
        assert_invalid_blocking_search(&state, case, &item);
    }

    let partial = json!({
        "status": "incomplete",
        "output": [{
            "type": "function_call",
            "id": "fc_partial",
            "call_id": "call_partial",
            "name": "tool_search",
            "arguments": "{\"query\":",
            "status": "in_progress"
        }]
    })
    .to_string();
    validate_blocking_response(&partial, state.is_active(), state.withheld_function_names())
        .expect("unfinished search placeholder is allowed on an incomplete response");

    let ordinary = json!({
        "status": "completed",
        "output": [{"type": "function_call", "name": "tool_search", "arguments": "{}"}]
    })
    .to_string();
    validate_blocking_response(&ordinary, false, &HashSet::new())
        .expect("inactive ordinary function keeps generic compatibility defaults");
}

#[test]
fn public_tool_search_item_ids_are_stable_and_domain_separated() {
    assert_eq!(public_item_id("tsc_existing"), "tsc_existing");
    assert_eq!(public_item_id("fc_search_1"), "tsc_search_1");
    let first = public_item_id("provider-item-1");
    assert_eq!(first, public_item_id("provider-item-1"));
    assert!(first.starts_with("tsc_"));
    assert_ne!(first, crate::tool::custom::public_item_id("provider-item-1"));
}

#[test]
fn response_tools_after_search_keep_immediate_and_loaded_public_availability() {
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": "test",
        "store": false,
        "tools": [
            {
                "type": "tool_search",
                "execution": "client",
                "description": "Find tools",
                "parameters": {"type": "object"}
            },
            {"type": "function", "name": "always_ready"},
            {"type": "function", "name": "get_weather", "defer_loading": true},
            {"type": "function", "name": "not_loaded", "defer_loading": true},
            {
                "type": "namespace",
                "name": "travel",
                "tools": [
                    {"type": "function", "name": "always_ready_member"},
                    {"type": "function", "name": "get_timezone", "defer_loading": true},
                    {"type": "function", "name": "not_loaded_member", "defer_loading": true}
                ]
            }
        ],
        "input": [
            {
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "arguments": {"query": "weather and timezone"}
            },
            {
                "type": "tool_search_output",
                "call_id": "call_search_1",
                "tools": [
                    {"type": "function", "name": "get_weather", "defer_loading": true},
                    {
                        "type": "namespace",
                        "name": "travel",
                        "tools": [{"type": "function", "name": "get_timezone", "defer_loading": true}]
                    }
                ]
            }
        ]
    }))
    .expect("valid mixed-availability tool-search request");

    let state = ToolSearchState::build(&request).expect("tool-search state");
    let tools = serialize_to_value(&state.public_response_tools()).expect("response tools serialize");
    assert_eq!(
        tools,
        serde_json::json!([
            {"type": "function", "name": "always_ready"},
            {"type": "function", "name": "get_weather"},
            {
                "type": "namespace",
                "name": "travel",
                "tools": [
                    {"type": "function", "name": "always_ready_member"},
                    {"type": "function", "name": "get_timezone"}
                ]
            }
        ])
    );
}

#[test]
fn tool_search_output_rejects_mcp_definitions() {
    let request: RequestPayload = serde_json::from_value(serde_json::json!({
        "model": "test",
        "store": false,
        "parallel_tool_calls": false,
        "tools": [{
            "type": "tool_search",
            "execution": "client",
            "description": "Find a tool",
            "parameters": {"type": "object"}
        }],
        "input": [
            {
                "type": "tool_search_call",
                "id": "tsc_1",
                "call_id": "call_search_1",
                "arguments": {"query": "weather"}
            },
            {
                "type": "tool_search_output",
                "call_id": "call_search_1",
                "tools": [{
                    "type": "mcp",
                    "server_label": "weather",
                    "server_url": "https://mcp.example.test/mcp"
                }]
            }
        ]
    }))
    .expect("typed request");

    let error = ToolSearchState::build(&request).expect_err("MCP is not a client-loaded tool definition");

    assert!(matches!(
        error,
        ToolError::Config(message) if message.contains("unsupported tool definition")
    ));
}
