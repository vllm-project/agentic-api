#[cfg(test)]
use crate::types::io::{InOutItem, OutputItem};
use serde_json::json;

use crate::types::tools::McpDiscoveredToolParam;
use crate::utils::common::serialize_to_value;

use super::*;
use crate::tool::catalog::hosted_mcp_member;

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
        async_execution: false,
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
        async_execution: false,
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
        call_id: Some("call_search".to_owned()),
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
        prepared_items.iter().any(
            |item| matches!(item, InOutItem::Input(InputItem::McpListTools(list)) if list.server_label == "counter")
        )
    );

    let model_input = request.input.model_input();
    let ResponsesInput::Items(model_items) = model_input.as_ref() else {
        panic!("expected model item input");
    };
    assert!(
        model_items
            .iter()
            .all(|item| !matches!(item, InOutItem::Input(InputItem::McpListTools(_))))
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

#[test]
fn restored_mcp_loads_survive_compaction_and_offer_only_the_rest() {
    let request: RequestPayload = serde_json::from_value(json!({
        "model": "m",
        "input": "continue",
        "tools": [
            {"type": "tool_search"},
            {"type": "mcp", "server_label": "stub", "server_url": "http://127.0.0.1:9/mcp", "defer_loading": true}
        ]
    }))
    .unwrap();
    let restored: Vec<ResponsesTool> = serde_json::from_value(json!([{
        "type": "namespace", "name": "mcp_stub", "description": "",
        "tools": [{"type": "function", "name": "echo", "defer_loading": true}]
    }]))
    .unwrap();
    let mut state = ToolSearchState::build_with_loaded_tools(&request, &restored, true).unwrap();
    assert!(state.is_hosted());

    let mut prepared = request.clone();
    state.prepare_inference_request(&mut prepared).unwrap();
    let Some(ResponsesTool::Mcp(server)) = prepared
        .tools
        .as_mut()
        .and_then(|tools| tools.iter_mut().find(|tool| matches!(tool, ResponsesTool::Mcp(_))))
    else {
        panic!("the deferred server stays declared");
    };
    assert_eq!(
        server.loaded_tool_names.as_deref(),
        Some(["echo".to_owned()].as_slice())
    );

    // With discovery, the catalog offers only the tool not loaded before this response.
    for name in ["echo", "reverse"] {
        server.discovered_tools.push(McpDiscoveredToolParam {
            server_label: "stub".to_owned(),
            tool_name: name.to_owned(),
            internal_name: format!("mcp__stub__{name}"),
            tool: serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"}})).unwrap(),
        });
    }
    let tools = prepared.tools.clone().unwrap();
    let mut catalog = state.catalog().expect("prepared catalog").clone();
    catalog.discover_mcp(&tools);
    let found = catalog.search_raw(r#"{"paths":["mcp_stub"]}"#);
    let ResponsesTool::Namespace(namespace) = &found[0] else {
        panic!("an MCP server loads as a namespace");
    };
    assert_eq!(namespace.tools.len(), 1);
    assert!(
        matches!(&namespace.tools[0], CodexNamespaceMember::Function(function) if function.name.as_str() == "reverse")
    );
    assert_eq!(
        state
            .catalog()
            .expect("prepared catalog")
            .withheld_mcp_tool_names(&tools),
        HashSet::from(["mcp__stub__reverse".to_owned()])
    );
    // Restored loads are kept for the next response's metadata.
    assert_eq!(state.loaded_public_tools().len(), 1);
}

#[test]
fn hosted_mcp_member_survives_a_storage_round_trip_unchanged() {
    let discovered = McpDiscoveredToolParam {
        server_label: "stub".to_owned(),
        tool_name: "echo".to_owned(),
        internal_name: "mcp__stub__echo".to_owned(),
        tool: serde_json::from_value(
            json!({"name": "echo", "description": "Echo.", "inputSchema": {"type": "object"}}),
        )
        .unwrap(),
    };
    let member = hosted_mcp_member(&discovered).expect("named MCP tool");
    let stored = serde_json::to_value(&member).unwrap();
    let reloaded: CodexNamespaceMember = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(serde_json::to_value(reloaded).unwrap(), stored);
    assert_eq!(
        stored,
        json!({"type": "function", "name": "echo", "description": "Echo.", "parameters": {"type": "object"},
            "defer_loading": true, "output_schema": null})
    );
}

/// The registry owns the catalog: a deferred function or MCP tool is known but refused until a search loads it.
#[tokio::test]
async fn the_registry_refuses_deferred_definitions_until_a_search_loads_them() {
    let weather = json!({"type": "function", "name": "weather", "defer_loading": true,
        "parameters": {"type": "object"}});
    let mut request: RequestPayload = serde_json::from_value(json!({
        "model": "m",
        "input": "weather?",
        "tools": [
            {"type": "tool_search"},
            weather,
            {"type": "mcp", "server_label": "stub", "server_url": "http://127.0.0.1:9/mcp", "defer_loading": true}
        ]
    }))
    .unwrap();
    let mut state = ToolSearchState::build(&request).unwrap();
    state.prepare_inference_request(&mut request).unwrap();
    for tool in request.tools.as_mut().unwrap() {
        if let ResponsesTool::Mcp(server) = tool {
            server.discovered_tools.push(McpDiscoveredToolParam {
                server_label: "stub".to_owned(),
                tool_name: "echo".to_owned(),
                internal_name: "mcp__stub__echo".to_owned(),
                tool: serde_json::from_value(json!({"name": "echo", "inputSchema": {"type": "object"}})).unwrap(),
            });
        }
    }
    let mut registry = crate::tool::ToolRegistry::default();
    let catalog = state.take_catalog().expect("prepared catalog");
    let tools = request.tools.clone().unwrap();
    registry.attach_tool_search(catalog, state.synthetic_tool_search().cloned(), true, &tools);

    assert!(registry.is_deferred("weather"));
    assert!(registry.is_deferred("mcp__stub__echo"));
    assert_eq!(
        registry.withheld_names(),
        HashSet::from(["weather".to_owned(), "mcp__stub__echo".to_owned()])
    );
    let call: OutputItem = serde_json::from_value(json!({"type": "function_call", "id": "fc_w", "call_id": "call_w",
        "name": "weather", "arguments": "{}", "status": "completed"}))
    .unwrap();
    assert!(matches!(
        registry.ensure_loaded_calls(std::slice::from_ref(&call)),
        Err(ToolError::UpstreamWithheldFunctionCall)
    ));
    // A hosted search's function call is restored with its own name as namespace.
    let self_named: OutputItem =
        serde_json::from_value(json!({"type": "function_call", "id": "fc_w", "call_id": "call_w",
        "name": "weather", "namespace": "weather", "arguments": "{}", "status": "completed"}))
        .unwrap();
    assert!(matches!(
        registry.ensure_loaded_calls(std::slice::from_ref(&self_named)),
        Err(ToolError::UpstreamWithheldFunctionCall)
    ));

    let loaded: ResponsesTool = serde_json::from_value(weather).unwrap();
    let snapshot = registry
        .load_searched_tools(&[loaded], &mut request)
        .await
        .expect("the search's tool loads")
        .expect("tool search is active");
    assert_eq!(snapshot.1.len(), 1);
    assert!(!registry.is_deferred("weather"));
    assert!(registry.is_deferred("mcp__stub__echo"));
    assert!(registry.ensure_loaded_calls(&[call, self_named]).is_ok());
}

/// A namespace member's call reaches the registry restored to its public `{namespace, name}`; a deferred member is
/// still refused.
#[test]
fn the_registry_refuses_a_restored_call_to_a_deferred_namespace_member() {
    let request: RequestPayload = serde_json::from_value(json!({
        "model": "m",
        "input": "time?",
        "tools": [
            {"type": "tool_search"},
            {"type": "namespace", "name": "travel", "description": "Travel tools", "tools": [
                {"type": "function", "name": "get_timezone", "defer_loading": true, "parameters": {"type": "object"}}
            ]}
        ]
    }))
    .unwrap();
    let mut state = ToolSearchState::build(&request).unwrap();
    let mut registry = crate::tool::ToolRegistry::default();
    let catalog = state.take_catalog().expect("prepared catalog");
    registry.attach_tool_search(catalog, state.synthetic_tool_search().cloned(), true, &[]);
    let call: OutputItem = serde_json::from_value(json!({"type": "function_call", "id": "fc_t", "call_id": "call_t",
        "name": "get_timezone", "namespace": "travel", "arguments": "{}", "status": "completed"}))
    .unwrap();

    assert!(matches!(
        registry.ensure_loaded_calls(&[call]),
        Err(ToolError::UpstreamWithheldFunctionCall)
    ));
}
