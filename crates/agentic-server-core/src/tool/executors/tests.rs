use crate::tool::ToolDeclaration;
use std::collections::HashMap;
use std::sync::Arc;

use super::{GatewayExecutorRegistration, GatewayExecutors, validate_mcp_execution_options};
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

    validate_mcp_execution_options(&param, false).unwrap();
}

#[test]
fn mcp_execution_uses_configured_never_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter"
    }));

    validate_mcp_execution_options(&param, true).unwrap();
}

#[test]
fn mcp_execution_rejects_omitted_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "server_url": "http://localhost:8000/mcp"
    }));

    let error = validate_mcp_execution_options(&param, false).unwrap_err();
    assert!(error.to_string().contains("gateway configuration or the request"));
}

#[test]
fn mcp_execution_rejects_unsupported_approval_policy() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "server_url": "http://localhost:8000/mcp",
        "require_approval": "always"
    }));

    let error = validate_mcp_execution_options(&param, false).unwrap_err();
    assert!(error.to_string().contains("approval gating is not yet supported"));
}

#[test]
fn mcp_execution_rejects_connector_id() {
    let param = mcp_param(serde_json::json!({
        "server_label": "counter",
        "connector_id": "connector_dropbox"
    }));

    let error = validate_mcp_execution_options(&param, false).unwrap_err();
    assert!(error.to_string().contains("connector_id is not supported"));
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

#[tokio::test]
async fn cached_mcp_handlers_reject_empty_final_allowed_set() {
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

    let Err(error) = executors.mcp_server_tools(&param).await else {
        panic!("expected empty allowed set to be rejected");
    };

    assert!(error.to_string().contains("empty final allowed tool set"));
}
