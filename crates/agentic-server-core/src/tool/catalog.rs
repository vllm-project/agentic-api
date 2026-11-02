//! The request-scoped catalog of model-visible tool definitions a request can load.
//!
//! Every definition enters through this one inventory, wherever it comes from: the request's declarations
//! ([`ToolCatalog::build`]), and then definitions a completed search output, a hosted search or response metadata
//! restored after compaction loads ([`ToolCatalog::load`]). The catalog checks each definition's identity and
//! namespace members, compares a returned definition with the one it already holds, keeps each function and
//! namespace member loaded or deferred, and checks the model-visible names of the whole inventory together
//! ([`ToolCatalog::validate_model_visible_names`]). Tool search adds only its timing rules, through [`PriorCalls`].
//!
//! The catalog owns the name rule for every request ([`ToolCatalog::validate_declared_names`]): the registry checks
//! each request's declarations with it before it registers or discovers anything, and with tool search active,
//! preparation checks the whole catalog with it after loading, deferred definitions included.

mod history;
mod load;
mod search;

pub(crate) use load::{canonical_definition, canonical_namespace_member, history_call_before_load};
#[cfg(test)]
pub(crate) use search::hosted_mcp_member;

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::declaration::{ToolDeclaration, responses_declarations};
use super::handler::ToolError;
use super::tool_search::TOOL_SEARCH_NAME;
use super::{CodexNamespaceHandler, model_visible_namespace_member_name};
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, ResponsesTool};

/// Stable public identity used to compare definitions accumulated from search outputs.
///
/// Equality remains type-aware while the catalog also indexes the visible name separately, so returning the same
/// name under a different supported kind is rejected.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum ToolIdentity {
    Function(String),
    Namespace(String),
}

impl ToolIdentity {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Function(name) | Self::Namespace(name) => name,
        }
    }

    const fn kind(&self) -> &'static str {
        match self {
            Self::Function(_) => "function",
            Self::Namespace(_) => "namespace",
        }
    }
}

/// One function or namespace definition, its comparison form, and whether it is loaded.
#[derive(Clone)]
pub(crate) struct DefinitionRecord {
    pub(crate) identity: ToolIdentity,
    pub(crate) canonical: Value,
    pub(crate) public_index: usize,
    pub(crate) loaded: bool,
    pub(crate) namespace_members: Option<NamespaceMemberRecords>,
}

#[derive(Clone)]
pub(crate) struct NamespaceMemberRecord {
    pub(crate) canonical: Value,
    pub(crate) public_member_index: usize,
    pub(crate) loaded: bool,
}

#[derive(Clone)]
pub(crate) struct NamespaceMemberRecords {
    pub(crate) ordered: Vec<NamespaceMemberRecord>,
    pub(crate) indexes: HashMap<String, usize>,
    pub(crate) unloaded_count: usize,
}

/// The namespace name under which hosted search lists a deferred MCP server.
pub(crate) fn mcp_namespace_name(server_label: &str) -> String {
    format!("mcp_{server_label}")
}

/// Deferred MCP servers and the tools hosted search has loaded from them.
///
/// A deferred server is searchable as the namespace `mcp_<server_label>`. Loading its members records MCP tool
/// names; it never adds a client namespace, because the gateway executes MCP calls itself.
#[derive(Clone, Debug, Default)]
pub(crate) struct McpLoads {
    /// `mcp_<server_label>` to server label, in declaration order.
    pub(crate) servers: indexmap::IndexMap<String, String>,
    /// Loaded tool names by server label, in first-load order.
    pub(crate) loaded: HashMap<String, Vec<String>>,
}

impl McpLoads {
    fn from_tools(tools: &[ResponsesTool]) -> Self {
        let servers = tools
            .iter()
            .filter_map(|tool| match tool {
                ResponsesTool::Mcp(server) if server.defer_loading == Some(true) => {
                    Some((mcp_namespace_name(&server.server_label), server.server_label.clone()))
                }
                _ => None,
            })
            .collect();
        Self {
            servers,
            loaded: HashMap::new(),
        }
    }

    pub(crate) fn label(&self, namespace: &str) -> Option<&str> {
        self.servers.get(namespace).map(String::as_str)
    }

    pub(crate) fn is_deferred(&self, server_label: &str) -> bool {
        self.servers.values().any(|label| label == server_label)
    }

    /// Records `names` as loaded and returns the ones that were not loaded yet.
    fn load<'n>(&mut self, server_label: &str, names: impl Iterator<Item = &'n str>) -> Vec<String> {
        let loaded = self.loaded.entry(server_label.to_owned()).or_default();
        let mut newly_loaded = Vec::new();
        for name in names {
            if !loaded.iter().any(|loaded| loaded == name) {
                loaded.push(name.to_owned());
                newly_loaded.push(name.to_owned());
            }
        }
        newly_loaded
    }

    pub(crate) fn loaded_names(&self, server_label: &str) -> Vec<String> {
        self.loaded.get(server_label).cloned().unwrap_or_default()
    }
}

/// Calls a history made before a definition loads: a load must not resolve them, since the model could not have
/// called a definition it did not have.
#[derive(Default)]
pub(crate) struct PriorCalls {
    /// Namespace member calls whose namespace the catalog did not hold yet, by namespace.
    pub(crate) unknown_namespace_calls: HashMap<String, HashSet<String>>,
    /// Unqualified calls by function name, at their first history position.
    pub(crate) unqualified_call_positions: HashMap<String, usize>,
}

/// The request's model-visible definitions, in declaration and then first-load order.
#[derive(Clone, Default)]
pub struct ToolCatalog {
    /// Public declarations, followed by definitions a load introduced.
    pub(crate) public_tools: Vec<ResponsesTool>,
    pub(crate) definitions: Vec<DefinitionRecord>,
    pub(crate) definition_indexes: HashMap<String, usize>,
    /// Public definitions loads resolved, in first-load order.
    pub(crate) loaded_public_tools: Vec<ResponsesTool>,
    /// Model-visible names of deferred functions and namespace members not loaded yet.
    pub(crate) withheld_function_names: HashSet<String>,
    pub(crate) mcp_loads: McpLoads,
    /// The tools discovery listed for each deferred MCP server, as hosted search returns them.
    pub(crate) mcp_members: HashMap<String, Vec<CodexNamespaceMember>>,
    /// Tools loaded before this response; hosted search does not offer them again.
    pub(crate) loaded_before_response: HashSet<search::LoadedKey>,
}

impl ToolCatalog {
    /// Indexes the request's declarations: each function and namespace once by identity, namespace members by name,
    /// and deferred definitions as withheld.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] for a blank, reserved or duplicate identity, an invalid namespace member, or a
    /// deferred MCP server whose search namespace another tool uses.
    pub(crate) fn build(public_tools: Vec<ResponsesTool>) -> Result<Self, ToolError> {
        let mut catalog = Self {
            definitions: Vec::with_capacity(public_tools.len()),
            definition_indexes: HashMap::with_capacity(public_tools.len()),
            mcp_loads: McpLoads::from_tools(&public_tools),
            ..Self::default()
        };
        for (public_index, tool) in public_tools.iter().enumerate() {
            let Some(identity) = tool_identity(tool)? else {
                continue;
            };
            if catalog.definition_indexes.contains_key(identity.name()) {
                return Err(ToolError::Config(format!(
                    "duplicate tool-search definition identity '{}'",
                    identity.name()
                )));
            }
            let index = catalog.definitions.len();
            catalog.definition_indexes.insert(identity.name().to_owned(), index);
            catalog
                .definitions
                .push(definition_record(tool, identity, public_index, false)?);
        }
        if let Some((name, label)) = catalog
            .mcp_loads
            .servers
            .iter()
            .find(|(name, _)| catalog.definition_indexes.contains_key(name.as_str()))
        {
            return Err(ToolError::Config(format!(
                "deferred MCP server '{label}' is searched as '{name}', which another tool already uses"
            )));
        }
        catalog.public_tools = public_tools;
        catalog.withheld_function_names = catalog.initial_withheld_function_names()?;
        Ok(catalog)
    }

    /// The record for `name`, a function or namespace identity.
    pub(crate) fn definition(&self, name: &str) -> Option<&DefinitionRecord> {
        self.definition_indexes
            .get(name)
            .and_then(|index| self.definitions.get(*index))
    }

    /// Whether `name` is a top-level function the catalog holds.
    pub(crate) fn is_function(&self, name: &str) -> bool {
        self.definition(name)
            .is_some_and(|record| matches!(record.identity, ToolIdentity::Function(_)))
    }

    /// Checks the model-visible names of the whole inventory together, deferred and loaded definitions included,
    /// with [`Self::validate_declared_names`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] naming the first colliding member.
    pub(crate) fn validate_model_visible_names(&self) -> Result<(), ToolError> {
        Self::validate_declared_names(&responses_declarations(&self.public_tools))
    }

    /// The name rule for a request's tools, the one codex's namespace map applies: a flattened namespace member name
    /// must not repeat a top-level tool's name or another member's.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] naming the first colliding member.
    pub(crate) fn validate_declared_names(declarations: &[ToolDeclaration]) -> Result<(), ToolError> {
        CodexNamespaceHandler.build_namespace_map(Some(declarations)).map(drop)
    }

    fn initial_withheld_function_names(&self) -> Result<HashSet<String>, ToolError> {
        let mut withheld = HashSet::new();
        for tool in &self.public_tools {
            if let ResponsesTool::Function(function) = tool {
                if function.defer_loading == Some(true) {
                    withheld.insert(function.name.as_str().to_owned());
                }
                continue;
            }
            let ResponsesTool::Namespace(namespace) = tool else {
                continue;
            };
            let members = self
                .definition(&namespace.name)
                .and_then(|record| record.namespace_members.as_ref())
                .ok_or_else(|| ToolError::Config("namespace availability state is inconsistent".to_owned()))?;
            for member in &namespace.tools {
                let CodexNamespaceMember::Function(function) = member else {
                    continue;
                };
                let member_index = members
                    .indexes
                    .get(function.name.as_str())
                    .ok_or_else(|| ToolError::Config("namespace availability state is inconsistent".to_owned()))?;
                if !members.ordered[*member_index].loaded {
                    withheld.insert(model_visible_namespace_member_name(
                        &namespace.name,
                        function.name.as_str(),
                    ));
                }
            }
        }
        Ok(withheld)
    }
}

/// The identity of a function or namespace definition; other tools have none.
///
/// # Errors
///
/// Returns [`ToolError::Config`] for a blank identity or the reserved `tool_search` name.
pub(crate) fn tool_identity(tool: &ResponsesTool) -> Result<Option<ToolIdentity>, ToolError> {
    let identity = match tool {
        ResponsesTool::Function(function) => ToolIdentity::Function(function.name.as_str().to_owned()),
        ResponsesTool::Namespace(namespace) => ToolIdentity::Namespace(namespace.name.clone()),
        ResponsesTool::ToolSearch(_)
        | ResponsesTool::Mcp(_)
        | ResponsesTool::WebSearch(_)
        | ResponsesTool::FileSearch(_)
        | ResponsesTool::CodeInterpreter(_)
        | ResponsesTool::Shell(_)
        | ResponsesTool::Custom(_)
        | ResponsesTool::Unknown => return Ok(None),
    };
    if identity.name().trim().is_empty() {
        return Err(ToolError::Config(format!(
            "{} definition identity must not be blank",
            identity.kind()
        )));
    }
    if identity.name() == TOOL_SEARCH_NAME {
        return Err(ToolError::Config(
            "model-visible tool name 'tool_search' is reserved while tool search is active".to_owned(),
        ));
    }
    Ok(Some(identity))
}

fn definition_record(
    tool: &ResponsesTool,
    identity: ToolIdentity,
    public_index: usize,
    dynamically_loaded: bool,
) -> Result<DefinitionRecord, ToolError> {
    let namespace_members = match tool {
        ResponsesTool::Namespace(namespace) => Some(namespace_member_records(namespace, dynamically_loaded)?),
        ResponsesTool::Function(_) => None,
        ResponsesTool::ToolSearch(_)
        | ResponsesTool::Mcp(_)
        | ResponsesTool::WebSearch(_)
        | ResponsesTool::FileSearch(_)
        | ResponsesTool::CodeInterpreter(_)
        | ResponsesTool::Shell(_)
        | ResponsesTool::Custom(_)
        | ResponsesTool::Unknown => {
            return Err(ToolError::Config(
                "tool-search definition record received an unsupported tool".to_owned(),
            ));
        }
    };
    let loaded = namespace_members
        .as_ref()
        .map_or(dynamically_loaded, |members| members.unloaded_count == 0);
    Ok(DefinitionRecord {
        identity,
        canonical: canonical_definition(tool)?,
        public_index,
        loaded,
        namespace_members,
    })
}

fn namespace_member_records(
    namespace: &CodexNamespaceToolParam,
    dynamically_loaded: bool,
) -> Result<NamespaceMemberRecords, ToolError> {
    if namespace.tools.is_empty() {
        return Err(ToolError::Config(
            "tool-search namespaces must contain at least one function member".to_owned(),
        ));
    }
    let mut ordered = Vec::with_capacity(namespace.tools.len());
    let mut indexes = HashMap::with_capacity(namespace.tools.len());
    let mut unloaded_count = 0;
    for (public_member_index, member) in namespace.tools.iter().enumerate() {
        let CodexNamespaceMember::Function(function) = member else {
            return Err(ToolError::Config(
                "tool-search namespaces may contain only function members".to_owned(),
            ));
        };
        let name = function.name.as_str();
        if indexes.insert(name.to_owned(), ordered.len()).is_some() {
            return Err(ToolError::Config(format!(
                "duplicate namespace member identity '{}.{name}'",
                namespace.name
            )));
        }
        let loaded = dynamically_loaded || function.defer_loading != Some(true);
        unloaded_count += usize::from(!loaded);
        ordered.push(NamespaceMemberRecord {
            canonical: canonical_namespace_member(function)?,
            public_member_index,
            loaded,
        });
    }
    Ok(NamespaceMemberRecords {
        ordered,
        indexes,
        unloaded_count,
    })
}

#[cfg(test)]
mod tests;
