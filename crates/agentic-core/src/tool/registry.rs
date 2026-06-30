use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::io::output::FunctionToolCall;
use crate::types::tools::ResponsesTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Function,
    Mcp,
    /// Internal routing discriminant. Serializes as `"web_search"`.
    /// Note: the corresponding `ResponsesTool` wire tag is `"web_search_preview"`.
    /// `ToolType` is not used in wire-facing types so the names differ intentionally.
    WebSearch,
    FileSearch,
    CodeInterpreter,
}

/// Per-request routing entry keyed by the tool name the model will call.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub tool_type: ToolType,
    /// Full serialised tool param for the executor (used during dispatch).
    pub config: Value,
    /// For MCP tools: which server this tool belongs to.
    pub server_label: Option<String>,
}

/// Request-scoped registry built from `RequestPayload.tools`.
/// Maps the name the LLM sees → routing metadata.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    entries: HashMap<String, ToolEntry>,
}

impl ToolRegistry {
    /// Build a registry from the declared tools.
    ///
    /// Function tools with empty names are skipped with a warning. Duplicate
    /// tool names result in last-write-wins, also logged at `warn` level.
    ///
    /// # Panics
    ///
    /// Panics if serialization of a tool param struct fails, which cannot happen
    /// for the types defined in this module (`#[derive(Serialize)]` on plain structs).
    #[must_use]
    pub fn build(tools: &[ResponsesTool]) -> Self {
        let mut entries = HashMap::with_capacity(tools.len());

        for tool in tools {
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
                        },
                    );
                }
            }
        }

        Self { entries }
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

    /// Returns the subset of `calls` whose names map to gateway-owned tools
    /// (i.e. everything except `ToolType::Function`).
    #[must_use]
    pub fn gateway_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall> {
        calls
            .iter()
            .filter(|c| {
                self.entries
                    .get(&c.name)
                    .is_some_and(|e| e.tool_type != ToolType::Function)
            })
            .collect()
    }

    /// Returns the subset of `calls` whose names map to client-owned function
    /// tools (i.e. `ToolType::Function` or unknown names).
    #[must_use]
    pub fn client_owned<'a>(&self, calls: &'a [FunctionToolCall]) -> Vec<&'a FunctionToolCall> {
        calls
            .iter()
            .filter(|c| {
                self.entries
                    .get(&c.name)
                    .is_none_or(|e| e.tool_type == ToolType::Function)
            })
            .collect()
    }
}
