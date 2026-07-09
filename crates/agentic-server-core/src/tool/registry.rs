use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::codex::NamespaceMap;
use super::{CodexNamespaceHandler, GatewayExecutor, ToolError, ToolOutput};
use crate::types::io::OutputItem;
use crate::types::io::output::FunctionToolCall;
use crate::types::tools::{CodexNamespaceMember, ResponsesTool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Function,
    CodexNamespace,
    Mcp,
    /// Internal routing discriminant. Serializes as `"web_search"`.
    /// Note: the corresponding `ResponsesTool` wire tag is `"web_search_preview"`.
    /// `ToolType` is not used in wire-facing types so the names differ intentionally.
    WebSearch,
    FileSearch,
    CodeInterpreter,
}

impl ToolType {
    #[must_use]
    pub const fn is_gateway_owned(self) -> bool {
        !matches!(self, Self::Function | Self::CodexNamespace)
    }
}

/// Per-request routing entry keyed by the tool name the model will call.
#[derive(Clone)]
pub struct ToolEntry {
    pub tool_type: ToolType,
    /// Full serialised tool param for the executor (used during dispatch).
    pub config: Value,
    /// For MCP tools: which server this tool belongs to.
    pub server_label: Option<String>,
    pub handler: Option<Arc<dyn GatewayExecutor>>,
}

impl std::fmt::Debug for ToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEntry")
            .field("tool_type", &self.tool_type)
            .field("config", &self.config)
            .field("server_label", &self.server_label)
            .field("handler", &self.handler.is_some())
            .finish()
    }
}

pub struct GatewayDispatchResult {
    pub tool_type: ToolType,
    pub output: Result<ToolOutput, ToolError>,
}

/// Request-scoped registry built from `RequestPayload.tools`.
/// Maps the name the LLM sees → routing metadata.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    entries: HashMap<String, ToolEntry>,
    /// Built once from the declared tools, so `restore_final_payload_output`
    /// and `restore_stream_event_value` — the latter called once per SSE line
    /// during streaming — don't rebuild it on every call.
    namespace_map: Option<NamespaceMap>,
}

impl ToolRegistry {
    /// Build a registry from the declared tools.
    ///
    /// Duplicate tool names result in last-write-wins, logged at `warn` level.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when Codex namespace member flattening
    /// would collide with another declared tool name.
    ///
    /// # Panics
    ///
    /// Panics if serialization of a tool param struct fails, which cannot happen
    /// for the types defined in this module (`#[derive(Serialize)]` on plain structs).
    pub fn build(tools: &[ResponsesTool]) -> Result<Self, ToolError> {
        Self::build_with_handlers(tools, |_| None)
    }

    /// Build a registry from declared tools and attach gateway handlers for dispatchable tool types.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when Codex namespace member flattening
    /// would collide with another declared tool name.
    ///
    /// # Panics
    ///
    /// Panics if serialization of a tool param struct fails, which cannot happen
    /// for the types defined in this module (`#[derive(Serialize)]` on plain structs).
    pub fn build_with_handlers(
        tools: &[ResponsesTool],
        mut handler_for: impl FnMut(ToolType) -> Option<Arc<dyn GatewayExecutor>>,
    ) -> Result<Self, ToolError> {
        let mut entries = HashMap::with_capacity(tools.len());
        // Namespace members must be keyed by the same flat, model-visible name
        // the model will call, so resolve them first — the same pure pass used
        // to build the upstream request.
        let resolved_tools = CodexNamespaceHandler.resolve_namespace_members(tools)?;

        for tool in &resolved_tools {
            match tool {
                ResponsesTool::Function(p) => {
                    // p.name is NonEmptyToolName — empty names are impossible here
                    // (serde rejects them at deserialization time).
                    if entries
                        .insert(
                            p.name.as_str().to_owned(),
                            ToolEntry {
                                tool_type: ToolType::Function,
                                config: serde_json::to_value(p).expect("serialization of known struct is infallible"),
                                server_label: None,
                                handler: None,
                            },
                        )
                        .is_some()
                    {
                        tracing::warn!(name = %p.name, "duplicate tool name — previous definition overwritten");
                    }
                }
                ResponsesTool::Mcp(p) => {
                    // MCP tool names are discovered at request-time via `tools/list`.
                    // Without discovery, we cannot know which tool names to register —
                    // keying by server_label would cause all MCP calls to miss on lookup
                    // since gateway_owned/client_owned look up by tool name, not server.
                    // MCP entries will be populated in PR C once HttpMcpHandler
                    // implements discover() and the executor calls it before build().
                    tracing::debug!(
                        server_label = %p.server_label,
                        "MCP server declared but skipped in registry — tool names unknown until discovery (PR C)"
                    );
                }
                ResponsesTool::WebSearch(p) => {
                    entries.insert(
                        "web_search".to_owned(),
                        ToolEntry {
                            tool_type: ToolType::WebSearch,
                            config: serde_json::to_value(p).expect("serialization of known struct is infallible"),
                            server_label: None,
                            handler: handler_for(ToolType::WebSearch),
                        },
                    );
                }
                ResponsesTool::FileSearch(p) => {
                    entries.insert(
                        "file_search".to_owned(),
                        ToolEntry {
                            tool_type: ToolType::FileSearch,
                            config: serde_json::to_value(p).expect("serialization of known struct is infallible"),
                            server_label: None,
                            handler: handler_for(ToolType::FileSearch),
                        },
                    );
                }
                ResponsesTool::CodeInterpreter(p) => {
                    entries.insert(
                        "code_interpreter".to_owned(),
                        ToolEntry {
                            tool_type: ToolType::CodeInterpreter,
                            config: serde_json::to_value(p).expect("serialization of known struct is infallible"),
                            server_label: None,
                            handler: handler_for(ToolType::CodeInterpreter),
                        },
                    );
                }
                ResponsesTool::Namespace(p) => {
                    // p's members already carry their flat, model-visible names
                    // (see the `resolve_namespace_members` call above).
                    let config = serde_json::to_value(p).expect("serialization of known struct is infallible");
                    for member in &p.tools {
                        let CodexNamespaceMember::Function(function) = member else {
                            continue;
                        };
                        let name = function.name.as_str().to_owned();
                        if entries
                            .insert(
                                name.clone(),
                                ToolEntry {
                                    tool_type: ToolType::CodexNamespace,
                                    config: config.clone(),
                                    server_label: Some(p.name.clone()),
                                    handler: None,
                                },
                            )
                            .is_some()
                        {
                            tracing::warn!(name = %name, namespace = %p.name, "duplicate tool name - previous definition overwritten");
                        }
                    }
                }
                ResponsesTool::Unknown => {
                    tracing::debug!("unknown tool declared but skipped in registry");
                }
            }
        }

        let namespace_map = CodexNamespaceHandler.build_namespace_map((!tools.is_empty()).then_some(tools))?;

        Ok(Self { entries, namespace_map })
    }

    #[must_use]
    pub fn lookup(&self, tool_name: &str) -> Option<&ToolEntry> {
        self.entries.get(tool_name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn restore_final_payload_output(&self, output: &mut [OutputItem]) {
        CodexNamespaceHandler.restore_output_items(output, self.namespace_map.as_ref());
    }

    pub fn restore_stream_event_value(&self, value: &mut Value) -> bool {
        CodexNamespaceHandler.restore_response_value(value, self.namespace_map.as_ref())
    }

    /// Returns the subset of `calls` whose names map to gateway-owned tools.
    #[must_use]
    pub fn gateway_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall> {
        calls
            .iter()
            .filter(|c| {
                self.entries
                    .get(&c.name)
                    .is_some_and(|e| e.tool_type.is_gateway_owned())
            })
            .collect()
    }

    #[must_use]
    pub fn is_gateway_owned_name(&self, name: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|entry| entry.tool_type.is_gateway_owned())
    }

    /// Returns the subset of `calls` whose names map to client-owned tools
    /// (`Function`, Codex namespace members, or unknown names).
    #[must_use]
    pub fn client_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall> {
        calls
            .iter()
            .filter(|c| {
                self.entries
                    .get(&c.name)
                    .is_none_or(|e| !e.tool_type.is_gateway_owned())
            })
            .collect()
    }

    pub async fn dispatch(&self, call: &FunctionToolCall) -> Option<GatewayDispatchResult> {
        let entry = self.entries.get(&call.name)?;
        let handler = entry.handler.clone()?;
        let tool_type = entry.tool_type;
        let config = entry.config.clone();
        Some(GatewayDispatchResult {
            tool_type,
            output: handler
                .execute(&call.call_id, &call.name, &call.arguments, &config)
                .await,
        })
    }
}
