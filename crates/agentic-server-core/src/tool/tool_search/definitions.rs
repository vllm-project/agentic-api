//! Definitions a replayed search output loaded: identity, canonical comparison form, and namespace members.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::tool::handler::ToolError;
use crate::tool::model_visible_namespace_member_name;
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, FunctionToolParam, ResponsesTool};
use crate::utils::common::serialize_to_value;

use super::history::withheld_function_history_call;
use super::{
    DefinitionAccumulator, DefinitionRecord, LoadedToolIdentity, NamespaceMemberRecord, TOOL_SEARCH_NAME,
    definition_record,
};

pub(super) fn load_definition(
    tool: &ResponsesTool,
    definitions: &mut DefinitionAccumulator<'_>,
) -> Result<(), ToolError> {
    if let ResponsesTool::Namespace(namespace) = tool
        && let Some(server_label) = definitions.mcp_loads.label(&namespace.name).map(str::to_owned)
    {
        let names = namespace.tools.iter().filter_map(|member| match member {
            CodexNamespaceMember::Function(function) => Some(function.name.as_str()),
            CodexNamespaceMember::Unknown => None,
        });
        let newly_loaded = definitions.mcp_loads.load(&server_label, names);
        if !newly_loaded.is_empty() {
            let mut loaded_subset = namespace.clone();
            loaded_subset.tools.retain(|member| {
                matches!(member, CodexNamespaceMember::Function(function)
                    if newly_loaded.iter().any(|name| name == function.name.as_str()))
            });
            definitions
                .loaded_public_tools
                .push(ResponsesTool::Namespace(loaded_subset));
        }
        return Ok(());
    }
    let identity = loaded_tool_identity(tool)?
        .ok_or_else(|| ToolError::Config("tool_search_output contains an unsupported tool definition".to_owned()))?;
    let canonical = canonical_definition(tool)?;
    if let Some(index) = definitions.definition_indexes.get(identity.name()).copied() {
        let record = &mut definitions.definitions[index];
        if record.identity != identity || record.canonical != canonical {
            return Err(ToolError::Config(format!(
                "loaded definition for identity '{}' conflicts with its existing type, schema, description, or configuration",
                identity.name()
            )));
        }
        if let ResponsesTool::Namespace(returned) = tool {
            return load_namespace_members(
                returned,
                record,
                definitions.public_tools,
                definitions.loaded_public_tools,
                definitions.withheld_function_names,
                &definitions.prior_unknown_namespace_calls,
                &definitions.unqualified_call_positions,
            );
        }
        if !record.loaded {
            if definitions.withheld_function_names.contains(record.identity.name())
                && definitions
                    .unqualified_call_positions
                    .contains_key(record.identity.name())
            {
                return Err(withheld_function_history_call());
            }
            record.loaded = true;
            definitions.withheld_function_names.remove(record.identity.name());
            definitions
                .loaded_public_tools
                .push(definitions.public_tools[record.public_index].clone());
        }
        return Ok(());
    }

    match tool {
        ResponsesTool::Function(function)
            if definitions
                .unqualified_call_positions
                .contains_key(function.name.as_str()) =>
        {
            return Err(withheld_function_history_call());
        }
        ResponsesTool::Namespace(namespace) => ensure_namespace_members_do_not_resolve_prior_calls(
            namespace,
            namespace.tools.iter(),
            &definitions.prior_unknown_namespace_calls,
            &definitions.unqualified_call_positions,
        )?,
        _ => {}
    }
    let public_index = definitions.public_tools.len();
    definitions.public_tools.push(tool.clone());
    let index = definitions.definitions.len();
    definitions.definition_indexes.insert(identity.name().to_owned(), index);
    definitions
        .definitions
        .push(definition_record(tool, identity, public_index, true)?);
    definitions.loaded_public_tools.push(tool.clone());
    Ok(())
}

fn ensure_namespace_members_do_not_resolve_prior_calls<'a>(
    namespace: &CodexNamespaceToolParam,
    members: impl Iterator<Item = &'a CodexNamespaceMember>,
    prior_unknown_namespace_calls: &HashMap<String, HashSet<String>>,
    unqualified_call_positions: &HashMap<String, usize>,
) -> Result<(), ToolError> {
    let prior_public_members = prior_unknown_namespace_calls.get(&namespace.name);
    for member in members {
        let CodexNamespaceMember::Function(function) = member else {
            continue;
        };
        let public_match = prior_public_members.is_some_and(|members| members.contains(function.name.as_str()));
        let flat_name = model_visible_namespace_member_name(&namespace.name, function.name.as_str());
        if public_match || unqualified_call_positions.contains_key(&flat_name) {
            return Err(withheld_function_history_call());
        }
    }
    Ok(())
}

fn load_namespace_members(
    returned: &CodexNamespaceToolParam,
    record: &mut DefinitionRecord,
    public_tools: &mut [ResponsesTool],
    loaded_public_tools: &mut Vec<ResponsesTool>,
    withheld_function_names: &mut HashSet<String>,
    prior_unknown_namespace_calls: &HashMap<String, HashSet<String>>,
    unqualified_call_positions: &HashMap<String, usize>,
) -> Result<(), ToolError> {
    if returned.tools.is_empty() {
        return Err(ToolError::Config(
            "tool_search_output namespaces must contain at least one function member".to_owned(),
        ));
    }
    let ResponsesTool::Namespace(public_namespace) = &mut public_tools[record.public_index] else {
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
        prior_unknown_namespace_calls,
        unqualified_call_positions,
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
                withheld_function_names.remove(&model_visible_namespace_member_name(&returned.name, member_name));
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
        loaded_public_tools.push(ResponsesTool::Namespace(loaded_subset));
    }
    record.loaded = members.unloaded_count == 0;
    Ok(())
}

pub(super) fn loaded_tool_identity(tool: &ResponsesTool) -> Result<Option<LoadedToolIdentity>, ToolError> {
    let identity = match tool {
        ResponsesTool::Function(function) => LoadedToolIdentity::Function(function.name.as_str().to_owned()),
        ResponsesTool::Namespace(namespace) => LoadedToolIdentity::Namespace(namespace.name.clone()),
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
    if matches!(
        &identity,
        LoadedToolIdentity::Function(name) | LoadedToolIdentity::Namespace(name)
            if name == TOOL_SEARCH_NAME
    ) {
        return Err(ToolError::Config(
            "model-visible tool name 'tool_search' is reserved while tool search is active".to_owned(),
        ));
    }
    Ok(Some(identity))
}

pub(super) fn canonical_definition(tool: &ResponsesTool) -> Result<Value, ToolError> {
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

pub(super) fn canonical_namespace_member(function: &FunctionToolParam) -> Result<Value, ToolError> {
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
