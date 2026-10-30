use crate::tool::{ToolDeclaration, declares_async, responses_declarations};
use std::collections::HashMap;
use std::sync::Arc;

use super::{GatewayExecutorRegistration, GatewayExecutors, validate_mcp_approval_source};
use crate::config::ToolRuntimeConfig;
use crate::tool::ToolError;
use crate::tool::mcp::McpServerEntry;
use crate::tool::mcp::{McpDiscoveredHandler, McpHandler};
use crate::types::tools::{McpDiscoveredToolParam, McpToolParam, ResponsesTool};

impl GatewayExecutors {
    pub(in crate::tool) fn with_code_interpreter(executor: super::CodeInterpreterExecutor) -> Self {
        Self {
            code_interpreter: Some(Arc::new(executor)),
            ..Self::default()
        }
    }
}

fn mcp_param(value: serde_json::Value) -> McpToolParam {
    serde_json::from_value(value).unwrap()
}

fn discovered_handler(tool_name: &str) -> McpDiscoveredHandler {
    McpDiscoveredHandler {
        param: McpDiscoveredToolParam {
            server_label: "counter".to_owned(),
            tool_name: tool_name.to_owned(),
            internal_name: format!("mcp__counter__{tool_name}"),
            tool: serde_json::from_value(serde_json::json!({
                "name": tool_name,
                "inputSchema": {"type": "object"}
            }))
            .unwrap(),
        },
        handler: Arc::new(McpHandler::discovered_tool_spec_only()),
    }
}

#[test]
fn mcp_execution_allows_explicit_never_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "server_url": "http://localhost:8000/mcp",
        "require_approval": "never"
    }));

    validate_mcp_approval_source(&param, false).unwrap();
}

#[test]
fn mcp_execution_uses_configured_never_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter"
    }));

    validate_mcp_approval_source(&param, true).unwrap();
}

#[test]
fn mcp_execution_rejects_omitted_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "server_url": "http://localhost:8000/mcp"
    }));

    let error = validate_mcp_approval_source(&param, false).unwrap_err();
    assert!(error.to_string().contains("gateway configuration or the request"));
}

/// Discovery runs the declaration's own checks, as request validation does, before it connects.
#[tokio::test]
async fn mcp_discovery_rejects_what_the_declaration_check_rejects() {
    for (param, message) in [
        (
            serde_json::json!({"server_label": "counter", "server_url": "http://localhost:8000/mcp",
                "require_approval": "always"}),
            "approval gating is not yet supported",
        ),
        (
            serde_json::json!({"server_label": "counter", "connector_id": "connector_dropbox"}),
            "connector_id is not supported",
        ),
    ] {
        let mut executors = GatewayExecutors::default();
        let Err(error) = executors.mcp_server_tools(&mcp_param(param)).await else {
            panic!("{message}");
        };
        assert!(
            matches!(&error, ToolError::Config(text) if text.contains(message)),
            "{error}"
        );
    }
}

#[test]
fn request_validation_rejects_code_interpreter_without_a_ready_executor() {
    let tools = [ToolDeclaration::from(
        serde_json::from_value::<ResponsesTool>(serde_json::json!({
            "type": "code_interpreter",
            "container": {"type": "auto"}
        }))
        .expect("valid declaration"),
    )];

    let error = GatewayExecutors::default()
        .validate_declarations(Some(&tools))
        .expect_err("an unregistered executor must fail closed");

    assert!(matches!(error, ToolError::Config(message) if message.contains("code_interpreter")));
}

fn declarations(value: serde_json::Value) -> Vec<ToolDeclaration> {
    let tools: Vec<ResponsesTool> = serde_json::from_value(value).expect("valid declarations");
    crate::tool::responses_declarations(&tools)
}

fn weather_function(extra: &serde_json::Value) -> serde_json::Value {
    let mut function = serde_json::json!({
        "type": "function",
        "name": "get_weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
    });
    function
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    function
}

#[test]
fn async_is_accepted_on_function_custom_and_namespace_member_tools() {
    let raw: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
        weather_function(&serde_json::json!({"async": true})),
        {"type": "custom", "name": "echo", "async": true},
        {
            "type": "namespace",
            "name": "weather_tools",
            "description": "Demo weather tools.",
            "tools": [weather_function(&serde_json::json!({"name": "get_forecast", "async": true}))]
        }
    ]))
    .expect("valid declarations");
    let tools = responses_declarations(&raw);

    GatewayExecutors::default()
        .validate_declarations(Some(&tools))
        .expect("async is accepted on client function, custom, and namespace member tools");
    assert!(raw.iter().all(declares_async));
}

/// Matches the recorded `OpenAI` behavior: `async` anywhere else is an unknown parameter at its
/// declared index (namespace members do not shift the index).
#[test]
fn async_on_other_tool_types_is_an_unknown_parameter_at_its_declared_index() {
    let rejected = [
        serde_json::json!({"type": "web_search", "async": true}),
        serde_json::json!({"type": "shell", "environment": {"type": "local"}, "async": true}),
        serde_json::json!({"type": "tool_search", "execution": "client", "async": true}),
        serde_json::json!({"type": "mcp", "server_label": "docs", "server_url": "https://example.test/mcp", "async": false}),
        serde_json::json!({"type": "file_search", "async": true}),
        serde_json::json!({"type": "code_interpreter", "container": {"type": "auto"}, "async": true}),
        serde_json::json!({"type": "namespace", "name": "weather_tools", "async": true, "tools": []}),
    ];
    for declaration in rejected {
        let tools = declarations(serde_json::json!([
            {
                "type": "namespace",
                "name": "first",
                "tools": [weather_function(&serde_json::json!({})), weather_function(&serde_json::json!({"name": "other"}))]
            },
            declaration.clone()
        ]));
        let error = GatewayExecutors::default()
            .validate_declarations(Some(&tools))
            .expect_err("async is rejected outside function and custom tools");
        assert!(
            matches!(&error, ToolError::UnknownParameter { param } if param == "tools[1].async"),
            "{declaration}: {error:?}"
        );
    }
}

#[test]
fn from_env_never_registers_the_operator_gated_code_interpreter() {
    let executors = GatewayExecutors::from_env(Arc::new(reqwest::Client::new()));
    assert!(executors.code_interpreter_executor().is_none());
}

#[test]
fn web_fetch_executor_follows_the_operator_switch() {
    let client = Arc::new(reqwest::Client::new());
    assert!(
        GatewayExecutors::from_env(Arc::clone(&client))
            .require_web_fetch()
            .is_ok()
    );
    let Err(error) = GatewayExecutors::default().require_web_fetch() else {
        panic!("a default registry has no web_fetch executor");
    };
    assert!(matches!(&error, ToolError::Config(message) if message == super::WEB_FETCH_UNAVAILABLE));
    let enabled = GatewayExecutors::from_config(Arc::clone(&client), &ToolRuntimeConfig::default()).unwrap();
    assert!(enabled.require_web_fetch().is_ok());
    let config = ToolRuntimeConfig {
        web_fetch: crate::config::WebFetchConfig::default().with_enabled(false),
        ..ToolRuntimeConfig::default()
    };
    let disabled = GatewayExecutors::from_config(client, &config).unwrap();
    assert!(disabled.require_web_fetch().is_err());
    assert!(format!("{disabled:?}").contains("web_fetch: false"));
}

#[tokio::test]
async fn configured_mcp_server_rejects_request_connection_override() {
    let mut executors = GatewayExecutors::default();
    executors.insert(GatewayExecutorRegistration::Mcp {
        server_label: "counter".to_owned(),
        handlers: vec![discovered_handler("read")],
    });
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "server_url": "http://localhost:8000/mcp",
        "require_approval": "never"
    }));

    let Err(error) = executors.mcp_server_tools(&param).await else {
        panic!("request connection override must fail");
    };
    assert!(error.to_string().contains("configured by the gateway"));
    assert!(error.to_string().contains("omit server_url"));
}

#[tokio::test]
async fn unavailable_configured_mcp_server_does_not_block_startup() {
    let mut servers = HashMap::new();
    servers.insert(
        "unavailable".to_owned(),
        McpServerEntry::Http {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers: None,
            allowed_tools: Some(vec!["read".to_owned()]),
            require_approval: Some("never".to_owned()),
            description: None,
            defer_loading: None,
        },
    );
    let config = ToolRuntimeConfig {
        mcp_servers: servers,
        ..ToolRuntimeConfig::default()
    };

    let executors = GatewayExecutors::from_config(Arc::new(reqwest::Client::new()), &config);

    assert!(executors.is_ok());
}

#[tokio::test]
async fn configured_allowed_tools_cannot_be_expanded_by_request() {
    let mut executors = GatewayExecutors::default();
    executors.insert(GatewayExecutorRegistration::Mcp {
        server_label: "counter".to_owned(),
        handlers: vec![discovered_handler("read")],
    });
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "allowed_tools": ["read", "delete"]
    }));

    let tools = executors.mcp_server_tools(&param).await.unwrap();

    assert_eq!(tools.discovered_handlers.len(), 1);
    assert_eq!(tools.discovered_handlers[0].param.tool_name, "read");
}

#[tokio::test]
async fn cached_mcp_server_tools_apply_request_allowed_tools_with_fresh_output_id() {
    let mut executors = GatewayExecutors::default();
    executors.insert(GatewayExecutorRegistration::Mcp {
        server_label: "counter".to_owned(),
        handlers: vec![discovered_handler("read"), discovered_handler("delete")],
    });
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "allowed_tools": ["read"],
        "require_approval": "never"
    }));

    let first = executors.mcp_server_tools(&param).await.unwrap();
    let first_output_id = first.list_tools_item.id.clone();
    let second = executors.mcp_server_tools(&param).await.unwrap();

    assert_eq!(first.discovered_handlers.len(), 1);
    assert_eq!(first.discovered_handlers[0].param.tool_name, "read");
    assert_eq!(first.list_tools_item.tools.len(), 1);
    assert_eq!(first.list_tools_item.tools[0].name, "read");
    assert_ne!(first_output_id, second.list_tools_item.id);
}

/// Recorded reference (`mcp-failures`): an `allowed_tools` filter that matches nothing lists no tools and the
/// request completes.
#[tokio::test]
async fn cached_mcp_handlers_accept_an_empty_final_allowed_set() {
    let mut executors = GatewayExecutors::default();
    executors.insert(GatewayExecutorRegistration::Mcp {
        server_label: "counter".to_owned(),
        handlers: vec![discovered_handler("delete")],
    });
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "allowed_tools": ["read"],
        "require_approval": "never"
    }));

    let tool_set = executors
        .mcp_server_tools(&param)
        .await
        .expect("an empty allowed set is not an error");

    assert!(tool_set.discovered_handlers.is_empty());
    assert!(tool_set.list_tools_item.tools.is_empty());
    assert!(tool_set.list_tools_item.error.is_none());
}
