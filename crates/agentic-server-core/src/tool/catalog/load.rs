//! Loading a definition into the catalog: validated insertion for a definition the catalog does not hold, and
//! comparison with the one it holds, for every source a load comes from.

use serde_json::Value;

use super::{NamespaceMemberRecord, PriorCalls, ToolCatalog, definition_record, tool_identity};
use crate::tool::handler::ToolError;
use crate::tool::model_visible_namespace_member_name;
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, FunctionToolParam, ResponsesTool};
use crate::utils::common::serialize_to_value;

impl ToolCatalog {
    /// Loads `tool`: marks a held definition (or the returned namespace members) loaded, or inserts a definition the
    /// catalog does not hold yet. A deferred MCP server's namespace loads only the MCP tool names it lists.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when `tool` conflicts with the held definition of its identity, is not a
    /// function or namespace, or would resolve one of `prior` calls.
    pub(crate) fn load(&mut self, tool: &ResponsesTool, prior: &PriorCalls) -> Result<(), ToolError> {
        if let ResponsesTool::Namespace(namespace) = tool
            && let Some(server_label) = self.mcp_loads.label(&namespace.name).map(str::to_owned)
        {
            self.load_mcp_members(&server_label, namespace);
            return Ok(());
        }
        let identity = tool_identity(tool)?.ok_or_else(|| {
            ToolError::Config("tool_search_output contains an unsupported tool definition".to_owned())
        })?;
        let canonical = canonical_definition(tool)?;
        if let Some(index) = self.definition_indexes.get(identity.name()).copied() {
            let record = &self.definitions[index];
            if record.identity != identity || record.canonical != canonical {
                return Err(ToolError::Config(format!(
                    "loaded definition for identity '{}' conflicts with its existing type, schema, description, or configuration",
                    identity.name()
                )));
            }
            if let ResponsesTool::Namespace(returned) = tool {
                return self.load_namespace_members(returned, index, prior);
            }
            let record = &mut self.definitions[index];
            if !record.loaded {
                if self.withheld_function_names.contains(record.identity.name())
                    && prior.unqualified_call_positions.contains_key(record.identity.name())
                {
                    return Err(history_call_before_load());
                }
                record.loaded = true;
                self.withheld_function_names.remove(record.identity.name());
                self.loaded_public_tools
                    .push(self.public_tools[record.public_index].clone());
            }
            return Ok(());
        }

        match tool {
            ResponsesTool::Function(function)
                if prior.unqualified_call_positions.contains_key(function.name.as_str()) =>
            {
                return Err(history_call_before_load());
            }
            ResponsesTool::Namespace(namespace) => {
                ensure_namespace_members_do_not_resolve_prior_calls(namespace, namespace.tools.iter(), prior)?;
            }
            _ => {}
        }
        let public_index = self.public_tools.len();
        self.public_tools.push(tool.clone());
        let index = self.definitions.len();
        self.definition_indexes.insert(identity.name().to_owned(), index);
        self.definitions
            .push(definition_record(tool, identity, public_index, true)?);
        self.loaded_public_tools.push(tool.clone());
        Ok(())
    }

    fn load_mcp_members(&mut self, server_label: &str, namespace: &CodexNamespaceToolParam) {
        let names = namespace.tools.iter().filter_map(|member| match member {
            CodexNamespaceMember::Function(function) => Some(function.name.as_str()),
            CodexNamespaceMember::Unknown => None,
        });
        let newly_loaded = self.mcp_loads.load(server_label, names);
        if !newly_loaded.is_empty() {
            let mut loaded_subset = namespace.clone();
            loaded_subset.tools.retain(|member| {
                matches!(member, CodexNamespaceMember::Function(function)
                    if newly_loaded.iter().any(|name| name == function.name.as_str()))
            });
            self.loaded_public_tools.push(ResponsesTool::Namespace(loaded_subset));
        }
    }

    fn load_namespace_members(
        &mut self,
        returned: &CodexNamespaceToolParam,
        index: usize,
        prior: &PriorCalls,
    ) -> Result<(), ToolError> {
        if returned.tools.is_empty() {
            return Err(ToolError::Config(
                "tool_search_output namespaces must contain at least one function member".to_owned(),
            ));
        }
        let record = &mut self.definitions[index];
        let ResponsesTool::Namespace(public_namespace) = &mut self.public_tools[record.public_index] else {
            return Err(ToolError::Config(
                "namespace identity conflicts with an existing non-namespace definition".to_owned(),
            ));
        };
        let members = record
            .namespace_members
            .as_mut()
            .ok_or_else(|| ToolError::Config("namespace definition is missing prepared member state".to_owned()))?;
        ensure_namespace_members_do_not_resolve_prior_calls(
            returned,
            returned.tools.iter().filter(|member| match member {
                CodexNamespaceMember::Function(function) => !members.indexes.contains_key(function.name.as_str()),
                CodexNamespaceMember::Unknown => true,
            }),
            prior,
        )?;
        let mut newly_loaded = Vec::new();
        for member in &returned.tools {
            let CodexNamespaceMember::Function(returned_function) = member else {
                return Err(ToolError::Config(
                    "tool_search_output namespaces may contain only function members".to_owned(),
                ));
            };
            let member_name = returned_function.name.as_str();
            let canonical = canonical_namespace_member(returned_function)?;
            if let Some(member_index) = members.indexes.get(member_name).copied() {
                let member_record = &mut members.ordered[member_index];
                if member_record.canonical != canonical {
                    return Err(ToolError::Config(format!(
                        "loaded namespace member '{}.{member_name}' conflicts with its existing schema, description, or configuration",
                        returned.name
                    )));
                }
                if !member_record.loaded {
                    let unloaded_count = members.unloaded_count.checked_sub(1).ok_or_else(|| {
                        ToolError::Config("namespace member availability state is inconsistent".to_owned())
                    })?;
                    member_record.loaded = true;
                    members.unloaded_count = unloaded_count;
                    self.withheld_function_names
                        .remove(&model_visible_namespace_member_name(&returned.name, member_name));
                    newly_loaded.push(public_namespace.tools[member_record.public_member_index].clone());
                }
                continue;
            }

            let public_member_index = public_namespace.tools.len();
            public_namespace.tools.push(member.clone());
            members.indexes.insert(member_name.to_owned(), members.ordered.len());
            members.ordered.push(NamespaceMemberRecord {
                canonical,
                public_member_index,
                loaded: true,
            });
            newly_loaded.push(member.clone());
        }
        if !newly_loaded.is_empty() {
            let mut loaded_subset = public_namespace.clone();
            loaded_subset.tools = newly_loaded;
            self.loaded_public_tools.push(ResponsesTool::Namespace(loaded_subset));
        }
        record.loaded = members.unloaded_count == 0;
        Ok(())
    }
}

fn ensure_namespace_members_do_not_resolve_prior_calls<'a>(
    namespace: &CodexNamespaceToolParam,
    members: impl Iterator<Item = &'a CodexNamespaceMember>,
    prior: &PriorCalls,
) -> Result<(), ToolError> {
    let prior_public_members = prior.unknown_namespace_calls.get(&namespace.name);
    for member in members {
        let CodexNamespaceMember::Function(function) = member else {
            continue;
        };
        let public_match = prior_public_members.is_some_and(|members| members.contains(function.name.as_str()));
        let flat_name = model_visible_namespace_member_name(&namespace.name, function.name.as_str());
        if public_match || prior.unqualified_call_positions.contains_key(&flat_name) {
            return Err(history_call_before_load());
        }
    }
    Ok(())
}

/// A history call to a function before its definition was loaded.
pub(crate) fn history_call_before_load() -> ToolError {
    ToolError::Config("request history calls a function before its definition is loaded".to_owned())
}

pub(crate) fn canonical_definition(tool: &ResponsesTool) -> Result<Value, ToolError> {
    let projected = match tool {
        ResponsesTool::Namespace(namespace) => {
            let mut namespace = namespace.clone();
            namespace.tools.clear();
            ResponsesTool::Namespace(namespace)
        }
        other => other.clone(),
    };
    serialize_to_value(&projected)
        .map(without_null_output_schema)
        .map_err(|_| ToolError::Config("tool-search definition could not be compared safely".to_owned()))
}

pub(crate) fn canonical_namespace_member(function: &FunctionToolParam) -> Result<Value, ToolError> {
    serialize_to_value(function)
        .map(without_null_output_schema)
        .map_err(|_| ToolError::Config("namespace member definition could not be compared safely".to_owned()))
}

/// The Responses API returns loaded functions with `"output_schema": null`;
/// that means the same as an absent output schema in the declaration.
fn without_null_output_schema(mut definition: Value) -> Value {
    if let Some(object) = definition.as_object_mut()
        && object.get("output_schema").is_some_and(Value::is_null)
    {
        object.remove("output_schema");
    }
    definition
}
