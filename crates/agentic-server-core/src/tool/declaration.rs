//! The tool layer's declaration of a tool, independent of the API that
//! declared it.
//!
//! The Responses API declares tools as [`ResponsesTool`], a wire type; the
//! Messages API declares them as Anthropic [`ToolParam`] blocks. Each adapter
//! converts its declarations into [`ToolDeclaration`] at its boundary —
//! [`responses_declarations`] for Responses, [`registry_tools`] for Messages —
//! and registry building, validation, and normalization work on that one
//! concrete type. What MCP discovery records on a declaration travels back to
//! a Responses request through [`record_discovered_mcp_tools`], so no wire
//! type carries tool-layer behaviour and no API-specific variant leaks into
//! the other API's wire enum. The Messages-only `web_fetch` declaration lives
//! here, in [`ToolDeclaration::WebFetch`].

use crate::types::messages::request::ToolParam;
use crate::types::messages::tool_seam::{GatewayToolMap, WEB_SEARCH_EXECUTOR, is_native_web_fetch_type};
use crate::types::tools::{
    CodeInterpreterToolParam, CodexNamespaceToolParam, CustomToolParam, DomainFilters, FileSearchToolParam,
    FunctionToolParam, McpToolParam, ResponsesTool, ShellToolParam, ToolSearchToolParam, WebFetchToolParam,
    WebSearchToolParam, WebSearchUserLocation,
};
use crate::utils::common::deserialize_from_value_opt;

/// A declared tool as the tool layer understands it.
///
/// One variant per kind the gateway implements, plus [`Unsupported`] for a
/// declaration of a kind it does not: that one registers nothing and is not
/// sent upstream. [`registry_tools`] builds these for the Messages API;
/// [`responses_declarations`] converts a Responses request's wire tools.
///
/// [`Unsupported`]: ToolDeclaration::Unsupported
#[derive(Debug, Clone)]
pub enum ToolDeclaration {
    Function(FunctionToolParam),
    ToolSearch(ToolSearchToolParam),
    Mcp(McpToolParam),
    WebSearch(WebSearchToolParam),
    /// Gateway-executed page fetch, declared only through the Messages seam.
    WebFetch(WebFetchToolParam),
    FileSearch(FileSearchToolParam),
    CodeInterpreter(CodeInterpreterToolParam),
    Shell(ShellToolParam),
    Namespace(CodexNamespaceToolParam),
    Custom(CustomToolParam),
    /// A declaration of a kind the gateway does not implement.
    Unsupported,
}

impl From<ResponsesTool> for ToolDeclaration {
    fn from(tool: ResponsesTool) -> Self {
        match tool {
            ResponsesTool::Function(param) => Self::Function(param),
            ResponsesTool::ToolSearch(param) => Self::ToolSearch(param),
            ResponsesTool::Mcp(param) => Self::Mcp(param),
            ResponsesTool::WebSearch(param) => Self::WebSearch(param),
            ResponsesTool::FileSearch(param) => Self::FileSearch(param),
            ResponsesTool::CodeInterpreter(param) => Self::CodeInterpreter(param),
            ResponsesTool::Shell(param) => Self::Shell(param),
            ResponsesTool::Namespace(param) => Self::Namespace(param),
            ResponsesTool::Custom(param) => Self::Custom(param),
            ResponsesTool::Unknown => Self::Unsupported,
        }
    }
}

/// The conversion for a wire declaration the request keeps: the typed
/// parameters are cloned into the internal declaration.
impl From<&ResponsesTool> for ToolDeclaration {
    fn from(tool: &ResponsesTool) -> Self {
        match tool {
            ResponsesTool::Function(param) => Self::Function(param.clone()),
            ResponsesTool::ToolSearch(param) => Self::ToolSearch(param.clone()),
            ResponsesTool::Mcp(param) => Self::Mcp(param.clone()),
            ResponsesTool::WebSearch(param) => Self::WebSearch(param.clone()),
            ResponsesTool::FileSearch(param) => Self::FileSearch(param.clone()),
            ResponsesTool::CodeInterpreter(param) => Self::CodeInterpreter(param.clone()),
            ResponsesTool::Shell(param) => Self::Shell(param.clone()),
            ResponsesTool::Namespace(param) => Self::Namespace(param.clone()),
            ResponsesTool::Custom(param) => Self::Custom(param.clone()),
            ResponsesTool::Unknown => Self::Unsupported,
        }
    }
}

/// The Responses adapter boundary: a request's declared tools as the
/// declarations the tool layer builds, validates, and normalizes from. The
/// request keeps its wire tools; the tool layer never sees the wire enum.
#[must_use]
pub fn responses_declarations(tools: &[ResponsesTool]) -> Vec<ToolDeclaration> {
    tools.iter().map(ToolDeclaration::from).collect()
}

/// Record what MCP discovery found back into a Responses request's wire
/// declarations, so the discovered tools go upstream and are stored with the
/// request. `declarations` are the request's tools as [`responses_declarations`]
/// converted them, in the same order, after the registry was built from them;
/// each `mcp` pair carries its discovered tools across and every other kind is
/// left as the client declared it.
pub fn record_discovered_mcp_tools(declarations: Vec<ToolDeclaration>, tools: &mut [ResponsesTool]) {
    debug_assert_eq!(
        declarations.len(),
        tools.len(),
        "declarations mirror the request's tools"
    );
    for (declaration, tool) in declarations.into_iter().zip(tools) {
        if let (ToolDeclaration::Mcp(discovered), ResponsesTool::Mcp(declared)) = (declaration, tool) {
            declared.discovered_tools = discovered.discovered_tools;
        }
    }
}

/// Map declared Anthropic tools to the declarations a request-scoped
/// `ToolRegistry` is built from. Gateway-owned tools (built-in or configured
/// alias) become the matching gateway kind — the registry keys the
/// `web_search` executor under its canonical name, and dispatch canonicalises
/// the call name to match (`tool_seam::tool_use_to_call`). A native
/// `web_fetch_*` declaration becomes [`ToolDeclaration::WebFetch`]. Everything
/// else is a client-owned function.
#[must_use]
pub fn registry_tools(tools: Option<&Vec<ToolParam>>, map: &GatewayToolMap) -> Vec<ToolDeclaration> {
    let Some(tools) = tools else {
        return Vec::new();
    };
    tools.iter().filter_map(|tool| map_tool(tool, map)).collect()
}

fn map_tool(tool: &ToolParam, map: &GatewayToolMap) -> Option<ToolDeclaration> {
    match tool {
        ToolParam::WebFetch(_) => web_fetch_config(tool).map(ToolDeclaration::WebFetch),
        // Keep unsupported fetch versions on the validation path; they must not
        // become client tools and bypass the existing version rejection.
        ToolParam::Provider(provider) if is_native_web_fetch_type(Some(&provider.tool_type)) => {
            web_fetch_config(tool).map(ToolDeclaration::WebFetch)
        }
        ToolParam::Function(_) | ToolParam::WebSearch(_) | ToolParam::Provider(_) => map_named_tool(tool, map),
    }
}

fn map_named_tool(tool: &ToolParam, map: &GatewayToolMap) -> Option<ToolDeclaration> {
    if map.canonical_executor(tool.name()) == Some(WEB_SEARCH_EXECUTOR) {
        return Some(ToolDeclaration::WebSearch(web_search_config(tool)));
    }
    let fields = tool.fields();
    let name = tool.name().try_into().ok()?;
    Some(ToolDeclaration::Function(FunctionToolParam {
        name,
        description: fields.description.clone(),
        parameters: fields.input_schema.clone(),
        strict: None,
        defer_loading: None,
        extra: std::collections::HashMap::new(),
    }))
}

/// The per-request settings of a native `web_search` declaration, read the
/// same way for the registry and for the Messages adapter.
pub(crate) fn web_search_config(tool: &ToolParam) -> WebSearchToolParam {
    if !matches!(tool, ToolParam::WebSearch(_)) {
        return WebSearchToolParam::default();
    }

    let fields = tool.fields();
    let allowed_domains = fields
        .extra
        .get("allowed_domains")
        .cloned()
        .and_then(deserialize_from_value_opt);
    let blocked_domains = fields
        .extra
        .get("blocked_domains")
        .cloned()
        .and_then(deserialize_from_value_opt);
    let filters = (allowed_domains.is_some() || blocked_domains.is_some()).then_some(DomainFilters {
        allowed_domains,
        blocked_domains,
    });
    let user_location = fields
        .extra
        .get("user_location")
        .cloned()
        .and_then(deserialize_from_value_opt::<WebSearchUserLocation>);

    WebSearchToolParam {
        search_context_size: None,
        filters,
        user_location,
    }
}

/// The per-request settings of a native `web_fetch` declaration, read through
/// the same parser the Messages adapter validated it with. The adapter
/// rejected an unreadable declaration with HTTP 400 before any registry is
/// built, so a failure here is a desynchronized parser: the declaration is
/// dropped rather than registered with weaker filters, and logged.
fn web_fetch_config(tool: &ToolParam) -> Option<WebFetchToolParam> {
    WebFetchToolParam::from_declaration(|field| tool.fields().extra.get(field))
        .inspect_err(|error| tracing::error!(error, "web_fetch declaration unreadable after validation"))
        .ok()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::tool::ToolType;
    use crate::types::messages::request::MessagesRequest;
    use crate::types::tools::McpDiscoveredToolParam;

    fn tools_of(json_req: Value) -> Option<Vec<ToolParam>> {
        serde_json::from_value::<MessagesRequest>(json_req).unwrap().tools
    }

    fn wire(tools: Value) -> Vec<ResponsesTool> {
        serde_json::from_value(tools).unwrap()
    }

    #[test]
    fn messages_declarations_map_to_the_kind_the_gateway_executes_or_the_client_owns() {
        let map = GatewayToolMap::default();
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [
                {"name": "web_search", "input_schema": {"type": "object"}},
                {"name": "get_weather", "description": "local", "input_schema": {"type": "object"}}
            ]
        }));
        let mapped = registry_tools(tools.as_ref(), &map);
        assert!(
            matches!(mapped.as_slice(), [ToolDeclaration::WebSearch(_), ToolDeclaration::Function(function)]
            if function.name.as_str() == "get_weather" && function.description.as_deref() == Some("local"))
        );
        assert!(registry_tools(None, &map).is_empty());

        // Claude Code's `WebSearch` is a client function unless an operator alias
        // points it at the executor.
        let claude_code = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "WebSearch", "input_schema": {"type": "object"}}]
        }));
        assert!(matches!(
            registry_tools(claude_code.as_ref(), &map).as_slice(),
            [ToolDeclaration::Function(_)]
        ));
        let aliased = GatewayToolMap::from_pairs([("WebSearch", "web_search")]);
        assert!(matches!(
            registry_tools(claude_code.as_ref(), &aliased).as_slice(),
            [ToolDeclaration::WebSearch(_)]
        ));
    }

    #[test]
    fn a_native_web_search_declaration_carries_its_settings() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"type": "web_search_20250305", "name": "web_search",
                       "allowed_domains": ["example.com"], "user_location": {"type": "approximate", "city": "Paris"}}]
        }));
        let mapped = registry_tools(tools.as_ref(), &GatewayToolMap::default());
        let [ToolDeclaration::WebSearch(param)] = mapped.as_slice() else {
            panic!("expected one WebSearch declaration, got {mapped:?}");
        };
        assert_eq!(
            param
                .filters
                .as_ref()
                .and_then(|filters| filters.allowed_domains.clone()),
            Some(vec!["example.com".to_owned()])
        );
        assert_eq!(
            param.user_location.as_ref().and_then(|location| location.city.clone()),
            Some("Paris".to_owned())
        );
    }

    #[test]
    fn a_native_web_fetch_declaration_becomes_the_web_fetch_declaration() {
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 2,
                       "allowed_domains": ["example.com"], "max_content_tokens": 5000}]
        }));
        let mapped = registry_tools(tools.as_ref(), &GatewayToolMap::default());
        let [ToolDeclaration::WebFetch(param)] = mapped.as_slice() else {
            panic!("expected one WebFetch declaration, got {mapped:?}");
        };
        assert_eq!(
            param
                .filters
                .as_ref()
                .and_then(|filters| filters.allowed_domains.clone()),
            Some(vec!["example.com".to_owned()])
        );
        assert_eq!(param.max_content_tokens.map(std::num::NonZeroU32::get), Some(5000));

        // A plain function that happens to be named web_fetch is the client's.
        let plain = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "web_fetch", "input_schema": {"type": "object"}}]
        }));
        assert!(matches!(
            registry_tools(plain.as_ref(), &GatewayToolMap::default()).as_slice(),
            [ToolDeclaration::Function(_)]
        ));
    }

    #[test]
    fn an_unreadable_web_fetch_declaration_is_dropped_not_weakened() {
        // Unreachable after the adapter validated the request; if the two
        // parsers ever diverge, the declaration must not register with its
        // filters silently dropped.
        let tools = tools_of(json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"type": "web_fetch_20250910", "name": "web_fetch", "allowed_domains": "example.com"}]
        }));
        assert!(registry_tools(tools.as_ref(), &GatewayToolMap::default()).is_empty());
    }

    #[test]
    fn a_wire_declaration_converts_to_the_same_kind_by_reference_and_by_value() {
        let tools = wire(json!([
            {"type": "function", "name": "get_weather"},
            {"type": "tool_search", "execution": "client"},
            {"type": "mcp", "server_label": "docs", "server_url": "http://127.0.0.1:1/mcp"},
            {"type": "web_search_preview"},
            {"type": "file_search", "vector_store_ids": ["vs_1"]},
            {"type": "code_interpreter", "container": {"type": "auto"}},
            {"type": "shell", "environment": {"type": "local"}},
            {"type": "namespace", "name": "tools"},
            {"type": "custom", "name": "raw"},
            {"type": "something_new"}
        ]));
        let by_reference: Vec<Option<ToolType>> = responses_declarations(&tools)
            .iter()
            .map(ToolDeclaration::tool_type)
            .collect();
        let converted: Vec<ToolDeclaration> = tools.into_iter().map(ToolDeclaration::from).collect();
        let by_value: Vec<Option<ToolType>> = converted.iter().map(ToolDeclaration::tool_type).collect();
        assert_eq!(by_reference, by_value);
        assert_eq!(
            by_value,
            vec![
                Some(ToolType::Function),
                Some(ToolType::ToolSearch),
                Some(ToolType::Mcp),
                Some(ToolType::WebSearch),
                Some(ToolType::FileSearch),
                Some(ToolType::CodeInterpreter),
                Some(ToolType::Shell),
                Some(ToolType::CodexNamespace),
                Some(ToolType::Custom),
                None,
            ]
        );
        assert!(matches!(converted.last(), Some(ToolDeclaration::Unsupported)));
        assert!(matches!(
            &converted[0],
            ToolDeclaration::Function(function) if function.name.as_str() == "get_weather"
        ));
    }

    #[test]
    fn discovered_mcp_tools_are_recorded_back_into_the_responses_declarations() {
        let discovered = vec![McpDiscoveredToolParam {
            server_label: "docs".to_owned(),
            tool_name: "lookup".to_owned(),
            internal_name: "mcp__docs__lookup".to_owned(),
            tool: serde_json::from_value(
                json!({"name": "lookup", "description": "d", "inputSchema": {"type": "object"}}),
            )
            .expect("discovered MCP tool"),
        }];
        let mut tools = wire(json!([
            {"type": "function", "name": "get_weather"},
            {"type": "mcp", "server_label": "docs", "server_url": "http://127.0.0.1:1/mcp"}
        ]));
        let mut declarations = responses_declarations(&tools);
        let ToolDeclaration::Mcp(param) = &mut declarations[1] else {
            panic!("expected an mcp declaration, got {declarations:?}");
        };
        param.discovered_tools = discovered;

        record_discovered_mcp_tools(declarations, &mut tools);

        let ResponsesTool::Mcp(recorded) = &tools[1] else {
            panic!("expected the mcp wire declaration, got {tools:?}");
        };
        assert_eq!(recorded.discovered_tools.len(), 1);
        assert_eq!(recorded.discovered_tools[0].internal_name, "mcp__docs__lookup");
        assert!(matches!(
            &tools[0],
            ResponsesTool::Function(function) if function.name.as_str() == "get_weather"
        ));
    }
}
