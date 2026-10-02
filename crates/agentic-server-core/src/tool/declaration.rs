//! The tool layer's view of a declared tool, independent of the API that
//! declared it.
//!
//! The Responses API declares tools as [`ResponsesTool`], a wire type; the
//! Messages API declares them as Anthropic [`ToolParam`] blocks. Registry
//! building, declaration validation, and normalization read both through
//! [`ToolDeclarationRef`], the borrowed view every [`DeclaredTool`] yields, so
//! there is one registry-building path and no API-specific variant leaks into
//! the other API's wire enum. The Messages mapping ([`registry_tools`])
//! produces owned [`ToolDeclaration`]s; that is where the Messages-only
//! `web_fetch` declaration lives.

use crate::types::messages::request::ToolParam;
use crate::types::messages::tool_seam::{
    GatewayToolMap, NATIVE_WEB_SEARCH_TYPE, WEB_SEARCH_EXECUTOR, is_native_web_fetch_type,
};
use crate::types::tools::{
    CodeInterpreterToolParam, CodexNamespaceToolParam, CustomToolParam, DomainFilters, FileSearchToolParam,
    FunctionToolParam, McpDiscoveredToolParam, McpToolParam, ResponsesTool, ShellToolParam, ToolSearchToolParam,
    WebFetchToolParam, WebSearchToolParam, WebSearchUserLocation,
};
use crate::utils::common::deserialize_from_value_opt;

/// A declared tool as the tool layer understands it, owned.
///
/// One variant per kind the gateway implements, plus [`Unsupported`] for a
/// declaration of a kind it does not: that one registers nothing and is not
/// sent upstream. [`registry_tools`] builds these for the Messages API; the
/// Responses API reads its wire [`ResponsesTool`]s through [`DeclaredTool`]
/// and never converts, and [`From<ResponsesTool>`] is the owned conversion
/// for a caller that keeps one.
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

/// A borrowed view of a declared tool: what [`DeclaredTool::declaration`]
/// yields for an owned [`ToolDeclaration`] and for a wire [`ResponsesTool`]
/// alike, so the tool layer matches one shape.
#[derive(Debug, Clone, Copy)]
pub enum ToolDeclarationRef<'a> {
    Function(&'a FunctionToolParam),
    ToolSearch(&'a ToolSearchToolParam),
    Mcp(&'a McpToolParam),
    WebSearch(&'a WebSearchToolParam),
    WebFetch(&'a WebFetchToolParam),
    FileSearch(&'a FileSearchToolParam),
    CodeInterpreter(&'a CodeInterpreterToolParam),
    Shell(&'a ShellToolParam),
    Namespace(&'a CodexNamespaceToolParam),
    Custom(&'a CustomToolParam),
    Unsupported,
}

impl ToolDeclarationRef<'_> {
    /// An owned copy of the viewed declaration.
    #[must_use]
    pub fn to_declaration(self) -> ToolDeclaration {
        match self {
            Self::Function(param) => ToolDeclaration::Function(param.clone()),
            Self::ToolSearch(param) => ToolDeclaration::ToolSearch(param.clone()),
            Self::Mcp(param) => ToolDeclaration::Mcp(param.clone()),
            Self::WebSearch(param) => ToolDeclaration::WebSearch(param.clone()),
            Self::WebFetch(param) => ToolDeclaration::WebFetch(param.clone()),
            Self::FileSearch(param) => ToolDeclaration::FileSearch(param.clone()),
            Self::CodeInterpreter(param) => ToolDeclaration::CodeInterpreter(param.clone()),
            Self::Shell(param) => ToolDeclaration::Shell(param.clone()),
            Self::Namespace(param) => ToolDeclaration::Namespace(param.clone()),
            Self::Custom(param) => ToolDeclaration::Custom(param.clone()),
            Self::Unsupported => ToolDeclaration::Unsupported,
        }
    }
}

/// A declared tool the registry can build from: anything that yields the
/// tool layer's view of itself and can record what MCP discovery found.
///
/// Implemented by the owned [`ToolDeclaration`] (Messages) and by the wire
/// [`ResponsesTool`] (Responses), so `ToolRegistry::build_with_handlers` is
/// one path for both APIs and a Responses request keeps its own declarations,
/// with discovered MCP tools written into them, for storage and replay.
pub trait DeclaredTool {
    /// The tool layer's view of this declaration.
    fn declaration(&self) -> ToolDeclarationRef<'_>;

    /// Records the tools an `mcp` declaration's discovery found, so the request
    /// sends them upstream and stores them. A no-op for every other kind.
    fn set_discovered_mcp_tools(&mut self, tools: Vec<McpDiscoveredToolParam>);
}

impl DeclaredTool for ToolDeclaration {
    fn declaration(&self) -> ToolDeclarationRef<'_> {
        match self {
            Self::Function(param) => ToolDeclarationRef::Function(param),
            Self::ToolSearch(param) => ToolDeclarationRef::ToolSearch(param),
            Self::Mcp(param) => ToolDeclarationRef::Mcp(param),
            Self::WebSearch(param) => ToolDeclarationRef::WebSearch(param),
            Self::WebFetch(param) => ToolDeclarationRef::WebFetch(param),
            Self::FileSearch(param) => ToolDeclarationRef::FileSearch(param),
            Self::CodeInterpreter(param) => ToolDeclarationRef::CodeInterpreter(param),
            Self::Shell(param) => ToolDeclarationRef::Shell(param),
            Self::Namespace(param) => ToolDeclarationRef::Namespace(param),
            Self::Custom(param) => ToolDeclarationRef::Custom(param),
            Self::Unsupported => ToolDeclarationRef::Unsupported,
        }
    }

    fn set_discovered_mcp_tools(&mut self, tools: Vec<McpDiscoveredToolParam>) {
        if let Self::Mcp(param) = self {
            param.discovered_tools = tools;
        }
    }
}

impl DeclaredTool for ResponsesTool {
    fn declaration(&self) -> ToolDeclarationRef<'_> {
        match self {
            Self::Function(param) => ToolDeclarationRef::Function(param),
            Self::ToolSearch(param) => ToolDeclarationRef::ToolSearch(param),
            Self::Mcp(param) => ToolDeclarationRef::Mcp(param),
            Self::WebSearch(param) => ToolDeclarationRef::WebSearch(param),
            Self::FileSearch(param) => ToolDeclarationRef::FileSearch(param),
            Self::CodeInterpreter(param) => ToolDeclarationRef::CodeInterpreter(param),
            Self::Shell(param) => ToolDeclarationRef::Shell(param),
            Self::Namespace(param) => ToolDeclarationRef::Namespace(param),
            Self::Custom(param) => ToolDeclarationRef::Custom(param),
            Self::Unknown => ToolDeclarationRef::Unsupported,
        }
    }

    fn set_discovered_mcp_tools(&mut self, tools: Vec<McpDiscoveredToolParam>) {
        if let Self::Mcp(param) = self {
            param.discovered_tools = tools;
        }
    }
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
    if is_native_web_fetch_type(tool.type_.as_deref()) {
        return web_fetch_config(tool).map(ToolDeclaration::WebFetch);
    }
    if map.canonical_executor(&tool.name) == Some(WEB_SEARCH_EXECUTOR) {
        return Some(ToolDeclaration::WebSearch(web_search_config(tool)));
    }
    let name = tool.name.clone().try_into().ok()?;
    Some(ToolDeclaration::Function(FunctionToolParam {
        name,
        description: tool.description.clone(),
        parameters: tool.input_schema.clone(),
        strict: None,
        defer_loading: None,
        extra: std::collections::HashMap::new(),
    }))
}

/// The per-request settings of a native `web_search` declaration, read the
/// same way for the registry and for the Messages adapter.
pub(crate) fn web_search_config(tool: &ToolParam) -> WebSearchToolParam {
    if tool.type_.as_deref() != Some(NATIVE_WEB_SEARCH_TYPE) {
        return WebSearchToolParam::default();
    }

    let allowed_domains = tool
        .extra
        .get("allowed_domains")
        .cloned()
        .and_then(deserialize_from_value_opt);
    let blocked_domains = tool
        .extra
        .get("blocked_domains")
        .cloned()
        .and_then(deserialize_from_value_opt);
    let filters = (allowed_domains.is_some() || blocked_domains.is_some()).then_some(DomainFilters {
        allowed_domains,
        blocked_domains,
    });
    let user_location = tool
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
    WebFetchToolParam::from_declaration(|field| tool.extra.get(field))
        .inspect_err(|error| tracing::error!(error, "web_fetch declaration unreadable after validation"))
        .ok()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::tool::ToolType;
    use crate::types::messages::request::MessagesRequest;

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
    fn a_wire_declaration_and_its_converted_form_yield_the_same_view() {
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
        let kinds: Vec<Option<ToolType>> = tools.iter().map(|tool| tool.declaration().tool_type()).collect();
        let converted: Vec<ToolDeclaration> = tools.into_iter().map(ToolDeclaration::from).collect();
        let converted_kinds: Vec<Option<ToolType>> = converted.iter().map(ToolDeclaration::tool_type).collect();
        assert_eq!(kinds, converted_kinds);
        assert_eq!(
            kinds,
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
            converted[0].declaration().to_declaration(),
            ToolDeclaration::Function(function) if function.name.as_str() == "get_weather"
        ));
    }

    #[test]
    fn discovered_mcp_tools_are_recorded_on_both_declaration_kinds() {
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
        tools[0].set_discovered_mcp_tools(discovered.clone());
        tools[1].set_discovered_mcp_tools(discovered.clone());
        let ResponsesTool::Mcp(param) = &tools[1] else {
            panic!("mcp declaration")
        };
        assert_eq!(param.discovered_tools.len(), 1);
        assert!(
            matches!(&tools[0], ResponsesTool::Function(_)),
            "other kinds are untouched"
        );

        let mut declaration = ToolDeclaration::from(tools[1].clone());
        declaration.set_discovered_mcp_tools(Vec::new());
        let ToolDeclarationRef::Mcp(param) = declaration.declaration() else {
            panic!("mcp declaration")
        };
        assert!(param.discovered_tools.is_empty());
    }
}
