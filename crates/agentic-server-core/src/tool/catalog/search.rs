//! Hosted (`execution: "server"`) tool search over the catalog: a pure lookup over the deferred definitions it holds.
//!
//! The behavior mirrors the recorded Responses API reference
//! (`tests/hosted_tool_search_characterization_test.rs`): each requested path names a deferred function or namespace
//! (an MCP server is the namespace `mcp_<server_label>`), and an optional `query` keeps only the functions whose name
//! or description contains the whole query, ignoring case. Tools loaded before this response are not offered again;
//! a repeated search within this response returns the same tools again.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

use super::{ToolCatalog, mcp_namespace_name};
use crate::types::tools::{
    CodexNamespaceMember, CodexNamespaceToolParam, FunctionToolParam, McpDiscoveredToolParam, NonEmptyToolName,
    ResponsesTool,
};
use crate::utils::common::deserialize_from_str;

/// Arguments the model sends to the hosted `tool_search` function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct HostedSearchArguments {
    pub(crate) paths: Vec<String>,
    #[serde(default)]
    pub(crate) query: Option<String>,
}

impl HostedSearchArguments {
    /// Parses the model's JSON arguments. Invalid arguments load nothing.
    pub(crate) fn parse(arguments: &str) -> Option<Self> {
        deserialize_from_str(arguments).ok()
    }
}

/// A tool hosted search may load: a top-level function or one namespace member (an MCP tool is a member of its
/// `mcp_<server_label>` namespace).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum LoadedKey {
    Function(String),
    Member(String, String),
}

/// The keys of `tools`, loaded definitions.
pub(crate) fn loaded_keys(tools: &[ResponsesTool]) -> HashSet<LoadedKey> {
    let mut keys = HashSet::new();
    for tool in tools {
        match tool {
            ResponsesTool::Function(function) => {
                keys.insert(LoadedKey::Function(function.name.as_str().to_owned()));
            }
            ResponsesTool::Namespace(namespace) => {
                keys.extend(namespace.tools.iter().filter_map(|member| match member {
                    CodexNamespaceMember::Function(function) => Some(LoadedKey::Member(
                        namespace.name.clone(),
                        function.name.as_str().to_owned(),
                    )),
                    CodexNamespaceMember::Unknown => None,
                }));
            }
            _ => {}
        }
    }
    keys
}

impl ToolCatalog {
    /// Marks what is loaded now as loaded before this response, so hosted search does not offer it again.
    pub(crate) fn begin_response(&mut self) {
        self.loaded_before_response = loaded_keys(&self.loaded_public_tools);
    }

    /// Records the tools discovery listed for each deferred MCP server in `tools`, which hosted search may load. A
    /// server whose listing failed has no members and so offers nothing.
    pub(crate) fn discover_mcp(&mut self, tools: &[ResponsesTool]) {
        for tool in tools {
            if let ResponsesTool::Mcp(server) = tool
                && self.mcp_loads.is_deferred(&server.server_label)
            {
                let members = server.discovered_tools.iter().filter_map(hosted_mcp_member).collect();
                self.mcp_members.insert(server.server_label.clone(), members);
            }
        }
    }

    /// The deferred definitions hosted search may load in this response, in declaration order: each with only the
    /// members not loaded before it.
    fn searchable(&self) -> Vec<ResponsesTool> {
        let offered = |key: LoadedKey| !self.loaded_before_response.contains(&key);
        let mut entries = Vec::new();
        for tool in &self.public_tools {
            match tool {
                ResponsesTool::Function(function)
                    if function.defer_loading == Some(true)
                        && offered(LoadedKey::Function(function.name.as_str().to_owned())) =>
                {
                    entries.push(ResponsesTool::Function(function.clone()));
                }
                ResponsesTool::Namespace(namespace) => {
                    let tools: Vec<_> = namespace
                        .tools
                        .iter()
                        .filter(|member| {
                            matches!(member, CodexNamespaceMember::Function(function)
                                if function.defer_loading == Some(true)
                                    && offered(LoadedKey::Member(namespace.name.clone(), function.name.as_str().to_owned())))
                        })
                        .cloned()
                        .collect();
                    if !tools.is_empty() {
                        entries.push(ResponsesTool::Namespace(CodexNamespaceToolParam {
                            tools,
                            ..namespace.clone()
                        }));
                    }
                }
                ResponsesTool::Mcp(server) if self.mcp_loads.is_deferred(&server.server_label) => {
                    let name = mcp_namespace_name(&server.server_label);
                    let tools: Vec<_> = self
                        .mcp_members
                        .get(&server.server_label)
                        .into_iter()
                        .flatten()
                        .filter(|member| {
                            matches!(member, CodexNamespaceMember::Function(function)
                                if offered(LoadedKey::Member(name.clone(), function.name.as_str().to_owned())))
                        })
                        .cloned()
                        .collect();
                    if !tools.is_empty() {
                        entries.push(ResponsesTool::Namespace(CodexNamespaceToolParam {
                            name,
                            // No fallback: the Responses API leaves the description empty.
                            description: Some(server.server_description.clone().unwrap_or_default()),
                            tools,
                            unsupported_async: None,
                            extra: HashMap::new(),
                        }));
                    }
                }
                _ => {}
            }
        }
        entries
    }

    /// The public tools one hosted search loads, in path order. Unknown and repeated paths load nothing; a namespace
    /// with no matching member is dropped.
    pub(crate) fn search(&self, arguments: &HostedSearchArguments) -> Vec<ResponsesTool> {
        let query = arguments
            .query
            .as_deref()
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let entries = self.searchable();
        let mut indexes = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            if let Some(name) = searchable_name(entry) {
                indexes.entry(name).or_insert(index);
            }
        }
        let mut seen = HashSet::with_capacity(arguments.paths.len());
        arguments
            .paths
            .iter()
            .filter(|path| seen.insert(path.as_str()))
            .filter_map(|path| indexes.get(path.as_str()).map(|index| &entries[*index]))
            .filter_map(|entry| match entry {
                ResponsesTool::Function(function) => {
                    matches(query.as_deref(), function).then(|| ResponsesTool::Function(function.clone()))
                }
                ResponsesTool::Namespace(namespace) => {
                    let tools: Vec<_> = namespace
                        .tools
                        .iter()
                        .filter(|member| match member {
                            CodexNamespaceMember::Function(function) => matches(query.as_deref(), function),
                            CodexNamespaceMember::Unknown => false,
                        })
                        .cloned()
                        .collect();
                    (!tools.is_empty()).then(|| {
                        ResponsesTool::Namespace(CodexNamespaceToolParam {
                            tools,
                            ..namespace.clone()
                        })
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// Runs one hosted search from the model's raw JSON arguments.
    pub(crate) fn search_raw(&self, arguments: &str) -> Vec<ResponsesTool> {
        HostedSearchArguments::parse(arguments).map_or_else(Vec::new, |arguments| self.search(&arguments))
    }
}

fn searchable_name(tool: &ResponsesTool) -> Option<&str> {
    match tool {
        ResponsesTool::Function(function) => Some(function.name.as_str()),
        ResponsesTool::Namespace(namespace) => Some(&namespace.name),
        _ => None,
    }
}

/// Whether `function` matches a lowercased query: the whole query must appear in its name or description. No query
/// matches everything.
fn matches(query: Option<&str>, function: &FunctionToolParam) -> bool {
    let Some(query) = query else {
        return true;
    };
    function.name.as_str().to_lowercase().contains(query)
        || function
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(query))
}

/// One discovered MCP tool as a hosted search returns it. Like the Responses API it carries `"output_schema": null`;
/// its `"strict": null` is left out, because the typed `strict` field cannot keep a null through storage.
pub(crate) fn hosted_mcp_member(tool: &McpDiscoveredToolParam) -> Option<CodexNamespaceMember> {
    let name = NonEmptyToolName::try_from(tool.tool_name.as_str()).ok()?;
    let extra = HashMap::from([("output_schema".to_owned(), Value::Null)]);
    Some(CodexNamespaceMember::Function(FunctionToolParam {
        name,
        description: tool.tool.description.as_deref().map(str::to_owned),
        parameters: Some(Value::Object(tool.tool.input_schema.as_ref().clone())),
        strict: None,
        defer_loading: Some(true),
        async_execution: None,
        extra,
    }))
}

#[cfg(test)]
mod tests;
