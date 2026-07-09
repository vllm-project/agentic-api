use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::types::io::{FunctionTool, FunctionToolCall, OutputItem, ToolChoice};
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, NonEmptyToolName, ResponsesTool};

use super::handler::{ToolError, ToolHandler};
use super::registry::ToolType;

// Upstream Responses-compatible backends only see flat function names. Prefix
// flattened Codex namespace members so generated names are recognizable,
// unlikely to collide with user functions, and can be restored to
// `{ namespace, name }` on the way back to the client.
pub const MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX: &str = "agentic_ns__";

#[must_use]
pub fn model_visible_namespace_member_name(namespace: &str, member: &str) -> String {
    format!("{MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX}{namespace}__{member}")
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NamespaceMemberName {
    namespace: String,
    name: String,
}

#[derive(Clone, Debug)]
struct NamespaceCallMapping {
    member: NamespaceMemberName,
    upstream_name: String,
}

/// A pre-built, reusable namespace rename map, computed once per request from
/// the declared tools via [`CodexNamespaceHandler::build_namespace_map`].
///
/// Passing this into [`CodexNamespaceHandler::restore_output_items`],
/// [`CodexNamespaceHandler::restore_response_value`], and
/// [`CodexNamespaceHandler::resolve_tool_choice`] avoids
/// rebuilding the map on every call — important for streaming responses,
/// which call the restore path once per SSE line.
#[derive(Clone, Debug, Default)]
pub struct NamespaceMap {
    calls: HashMap<String, NamespaceCallMapping>,
    members: HashMap<NamespaceMemberName, String>,
}

impl NamespaceMap {
    fn mapping_for_call(&self, name: &str) -> Option<&NamespaceCallMapping> {
        self.calls.get(name)
    }

    fn mapping_for_member(&self, namespace: &str, name: &str) -> Option<&NamespaceCallMapping> {
        let member = NamespaceMemberName {
            namespace: namespace.to_string(),
            name: name.to_string(),
        };
        self.members
            .get(&member)
            .and_then(|upstream_name| self.calls.get(upstream_name))
    }
}

#[derive(Default)]
struct NamespaceMapBuilder {
    top_level_names: HashSet<String>,
    map: NamespaceMap,
}

impl NamespaceMapBuilder {
    fn new(top_level_names: HashSet<String>) -> Self {
        Self {
            top_level_names,
            ..Self::default()
        }
    }

    fn validate_and_record_flat_member(
        &mut self,
        namespace_name: &str,
        member_name: &str,
    ) -> Result<String, ToolError> {
        let flat_name = model_visible_namespace_member_name(namespace_name, member_name);
        if self.top_level_names.contains(&flat_name) {
            return Err(ToolError::Config(format!(
                "codex namespace member {namespace_name}.{member_name} collides with top-level function {flat_name}"
            )));
        }
        if let Some(existing) = self.map.calls.get(&flat_name) {
            if existing.member.namespace != namespace_name || existing.member.name != member_name {
                return Err(ToolError::Config(format!(
                    "codex namespace member {namespace_name}.{member_name} collides with {}.{} at generated name {flat_name}",
                    existing.member.namespace, existing.member.name
                )));
            }
        }
        Ok(self.record_flat_member_with_flat_name(namespace_name, member_name, flat_name))
    }

    fn record_flat_member_with_flat_name(
        &mut self,
        namespace_name: &str,
        member_name: &str,
        flat_name: String,
    ) -> String {
        let member = NamespaceMemberName {
            namespace: namespace_name.to_string(),
            name: member_name.to_string(),
        };
        if let Some(existing) = self.map.calls.get(&flat_name) {
            debug_assert!(
                existing.member == member,
                "namespace collisions must be validated before recording namespace members"
            );
            if existing.member != member {
                tracing::warn!(
                    upstream_name = %flat_name,
                    namespace = %namespace_name,
                    member = %member_name,
                    existing_namespace = %existing.member.namespace,
                    existing_member = %existing.member.name,
                    "generated codex namespace member name collides with another namespace member"
                );
            }
        }
        let mapping = NamespaceCallMapping {
            member: member.clone(),
            upstream_name: flat_name.clone(),
        };

        self.map.members.insert(member, flat_name.clone());
        self.map.calls.insert(flat_name.clone(), mapping);
        flat_name
    }

    fn finish(self) -> NamespaceMap {
        self.map
    }
}

/// Handler for Codex `type: "namespace"` tools.
///
/// Namespace tools are client-owned, like plain function tools, but need a
/// request-scoped normalization pass to flatten members into model-visible
/// function names and restore model calls back to the public namespace shape.
#[derive(Debug)]
pub struct CodexNamespaceHandler;

impl CodexNamespaceHandler {
    /// Rewrites every `Namespace` tool's function members to their flat,
    /// model-visible names (see [`model_visible_namespace_member_name`]),
    /// given real collision detection against sibling top-level tool names.
    ///
    /// Tools stay `ResponsesTool::Namespace` — only the nested members'
    /// `name` fields change — so [`ResponsesTool::to_function_tools`] and
    /// [`super::registry::ToolRegistry::build_with_handlers`] can read each
    /// member's already-flat name directly, with no further namespace logic.
    ///
    /// Request execution must handle the result so ambiguous declarations fail
    /// instead of being normalized into an irreversible flat shape.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a generated namespace member name
    /// collides with a top-level function tool or with another namespace
    /// member.
    pub fn resolve_namespace_members(&self, tools: &[ResponsesTool]) -> Result<Vec<ResponsesTool>, ToolError> {
        let mut builder = NamespaceMapBuilder::new(typed_top_level_tool_names(tools));
        tools
            .iter()
            .map(|tool| match tool {
                ResponsesTool::Namespace(namespace) => {
                    rename_namespace_members(namespace, &mut builder).map(ResponsesTool::Namespace)
                }
                other => Ok(other.clone()),
            })
            .collect()
    }

    /// Builds a [`NamespaceMap`] once from a request's declared tools, for
    /// reuse across every subsequent restore/rewrite call on that request —
    /// see [`NamespaceMap`]'s docs for why this matters.
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a generated namespace member name
    /// collides with a top-level function tool or with another namespace
    /// member.
    pub fn build_namespace_map(&self, tools: Option<&[ResponsesTool]>) -> Result<Option<NamespaceMap>, ToolError> {
        namespace_map_from_tools(tools)
    }

    /// Rejects namespace declarations that cannot be represented
    /// unambiguously by the gateway-owned flat model-visible naming scheme.
    ///
    /// This must run before building the upstream request or request-scoped
    /// registry. Otherwise the gateway could silently drop a namespace member
    /// or restore a flat model call to the wrong public `{ namespace, name }`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a generated namespace member name
    /// collides with a top-level function tool or with another namespace
    /// member.
    pub fn validate_namespace_collisions(&self, tools: Option<&[ResponsesTool]>) -> Result<(), ToolError> {
        let Some(tools) = tools else {
            return Ok(());
        };
        let mut builder = NamespaceMapBuilder::new(typed_top_level_tool_names(tools));
        for tool in tools {
            let ResponsesTool::Namespace(namespace) = tool else {
                continue;
            };
            for member_name in typed_function_member_names(namespace) {
                builder.validate_and_record_flat_member(&namespace.name, &member_name)?;
            }
        }
        Ok(())
    }

    /// Resolves the request's `tool_choice` (defaulting to `ToolChoice::Auto`
    /// when absent) and, if it's a namespaced `ToolChoice::Function {
    /// namespace, name }`, rewrites it to the flattened, model-visible name
    /// that [`ResponsesTool::to_function_tools`] produces for the matching
    /// namespace member — so `tool_choice` agrees with the tool names
    /// actually sent upstream.
    ///
    /// A no-op for `ToolChoice` variants other than `Function`, and for a
    /// `Function` choice that doesn't match any declared namespace member.
    #[must_use]
    pub fn resolve_tool_choice(&self, map: Option<&NamespaceMap>, tool_choice: Option<&ToolChoice>) -> ToolChoice {
        let tool_choice = tool_choice.unwrap_or(&ToolChoice::Auto);
        let Some(map) = map else {
            return tool_choice.clone();
        };
        rewrite_tool_choice_with_map(tool_choice, map)
    }

    pub fn restore_output_items(&self, output: &mut [OutputItem], map: Option<&NamespaceMap>) {
        let Some(map) = map else {
            return;
        };
        for item in output {
            if let OutputItem::FunctionCall(call) = item {
                restore_function_call_with_map(call, map);
            }
        }
    }

    #[must_use]
    pub fn restore_response_value(&self, value: &mut Value, map: Option<&NamespaceMap>) -> bool {
        let Some(map) = map else {
            return false;
        };
        restore_response_value_with_map(value, map)
    }
}

impl ToolHandler for CodexNamespaceHandler {
    fn tool_type(&self) -> ToolType {
        ToolType::CodexNamespace
    }

    fn validate(&self, param: &Value) -> Result<(), ToolError> {
        serde_json::from_value::<CodexNamespaceToolParam>(param.clone())
            .map(|_| ())
            .map_err(|e| ToolError::Config(format!("invalid codex namespace tool config: {e}")))
    }

    /// Converts an already-renamed namespace's function members straight to
    /// `FunctionTool`s. Callers must rename members to their flat, model-visible
    /// names first via [`CodexNamespaceHandler::resolve_namespace_members`] —
    /// this method has no sibling-tool context to do that itself.
    fn normalize(&self, param: &Value) -> Vec<FunctionTool> {
        let Ok(namespace) = serde_json::from_value::<CodexNamespaceToolParam>(param.clone()) else {
            tracing::warn!("normalize() called with invalid codex namespace param - validate() must be called first");
            return vec![];
        };
        namespace
            .tools
            .iter()
            .filter_map(|member| match member {
                CodexNamespaceMember::Function(function) => Some(FunctionTool::from(function)),
                CodexNamespaceMember::Unknown => None,
            })
            .collect()
    }
}

fn namespace_map_from_tools(tools: Option<&[ResponsesTool]>) -> Result<Option<NamespaceMap>, ToolError> {
    let Some(tools) = tools else {
        return Ok(None);
    };
    let mut builder = NamespaceMapBuilder::new(typed_top_level_tool_names(tools));
    for tool in tools {
        if let ResponsesTool::Namespace(namespace) = tool {
            let _ = rename_namespace_members(namespace, &mut builder)?;
        }
    }
    Ok(Some(builder.finish()))
}

/// Returns `namespace` with its function members' names rewritten to their
/// flat, model-visible form, recording each rename in `builder` along the
/// way.
///
/// # Errors
///
/// Returns [`ToolError::Config`] when a generated namespace member name
/// collides with a top-level function tool or with another namespace member.
fn rename_namespace_members(
    namespace: &CodexNamespaceToolParam,
    builder: &mut NamespaceMapBuilder,
) -> Result<CodexNamespaceToolParam, ToolError> {
    let function_member_names = typed_function_member_names(namespace);
    if function_member_names.is_empty() {
        tracing::debug!(
            namespace = %namespace.name,
            "namespace tool has no function members to rename for upstream"
        );
        return Ok(namespace.clone());
    }
    let tools = namespace
        .tools
        .iter()
        .map(|member| {
            let CodexNamespaceMember::Function(function) = member else {
                return Ok(member.clone());
            };
            let flat_name_text = builder.validate_and_record_flat_member(&namespace.name, function.name.as_str())?;
            let flat_name = NonEmptyToolName::try_from(flat_name_text.clone())
                .expect("generated namespace member names include a non-empty prefix");
            tracing::debug!(
                namespace = %namespace.name,
                member = %function.name.as_str(),
                upstream_name = %flat_name_text,
                "renamed namespace tool member for upstream"
            );
            let mut function = function.clone();
            function.name = flat_name;
            Ok(CodexNamespaceMember::Function(function))
        })
        .collect::<Result<Vec<_>, ToolError>>()?;

    Ok(CodexNamespaceToolParam {
        tools,
        ..namespace.clone()
    })
}

fn typed_top_level_tool_names(tools: &[ResponsesTool]) -> HashSet<String> {
    tools
        .iter()
        .filter_map(|tool| match tool {
            ResponsesTool::Function(function) => Some(function.name.as_str().to_string()),
            ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Namespace(_)
            | ResponsesTool::Unknown => None,
        })
        .collect()
}

fn typed_function_member_names(namespace: &CodexNamespaceToolParam) -> Vec<String> {
    namespace
        .tools
        .iter()
        .filter_map(|member| match member {
            CodexNamespaceMember::Function(function) => Some(function.name.as_str().to_string()),
            CodexNamespaceMember::Unknown => None,
        })
        .collect()
}

fn restore_function_call_with_map(call: &mut FunctionToolCall, map: &NamespaceMap) -> bool {
    if call.namespace.is_some() {
        return false;
    }
    let Some(mapping) = map.mapping_for_call(&call.name) else {
        return false;
    };
    let original_name = call.name.clone();

    call.namespace = Some(mapping.member.namespace.clone());
    call.name.clone_from(&mapping.member.name);
    tracing::debug!(
        upstream_name = %original_name,
        namespace = %mapping.member.namespace,
        member = %mapping.member.name,
        "restored upstream namespace function call"
    );
    true
}

fn rewrite_tool_choice_with_map(choice: &ToolChoice, map: &NamespaceMap) -> ToolChoice {
    let ToolChoice::Function { namespace, name } = choice else {
        return choice.clone();
    };
    let mapping = namespace
        .as_deref()
        .and_then(|namespace| map.mapping_for_member(namespace, name.as_str()))
        .or_else(|| {
            namespace
                .is_none()
                .then(|| map.mapping_for_call(name.as_str()))
                .flatten()
        });
    let Some(mapping) = mapping else {
        return choice.clone();
    };
    let Ok(name) = NonEmptyToolName::try_from(mapping.upstream_name.clone()) else {
        return choice.clone();
    };

    ToolChoice::Function { namespace: None, name }
}

fn restore_response_value_with_map(value: &mut Value, map: &NamespaceMap) -> bool {
    let mut changed = false;

    if let Some(item) = value.as_object_mut().and_then(|object| object.get_mut("item")) {
        changed |= restore_call_value_with_map(item, map);
    }

    changed |= restore_call_value_with_map(value, map);

    for key in ["response", "payload"] {
        if let Some(nested) = value.as_object_mut().and_then(|object| object.get_mut(key)) {
            changed |= restore_response_value_with_map(nested, map);
        }
    }

    if let Some(Value::Array(items)) = value.as_object_mut().and_then(|object| object.get_mut("output")) {
        for item in items {
            changed |= restore_call_value_with_map(item, map);
        }
    }

    changed
}

fn restore_call_value_with_map(value: &mut Value, map: &NamespaceMap) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    if object.get("namespace").and_then(Value::as_str).is_some() {
        return false;
    }
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return false;
    };
    let Some(mapping) = map.mapping_for_call(name) else {
        return false;
    };
    let original_name = name.to_string();

    object.insert("namespace".to_string(), Value::String(mapping.member.namespace.clone()));
    object.insert("name".to_string(), Value::String(mapping.member.name.clone()));
    tracing::debug!(
        upstream_name = %original_name,
        namespace = %mapping.member.namespace,
        member = %mapping.member.name,
        "restored upstream namespace function call"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::event::MessageStatus;

    fn completed_call(name: &str, arguments: &str) -> OutputItem {
        OutputItem::FunctionCall(FunctionToolCall {
            id: "fc_1".to_string(),
            call_id: "call_1".to_string(),
            name: name.to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            status: MessageStatus::Completed,
        })
    }

    #[test]
    fn unqualified_function_tool_choice_is_not_rewritten_to_namespace_member() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__shell",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();
        let choice = ToolChoice::Function {
            namespace: None,
            name: NonEmptyToolName::try_from("run").unwrap(),
        };

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        let rewritten = CodexNamespaceHandler.resolve_tool_choice(map.as_ref(), Some(&choice));

        assert_eq!(
            rewritten,
            ToolChoice::Function {
                namespace: None,
                name: NonEmptyToolName::try_from("run").unwrap()
            }
        );
        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        assert!(matches!(
            resolved.as_slice(),
            [ResponsesTool::Namespace(namespace)]
                if matches!(&namespace.tools[0], CodexNamespaceMember::Function(f) if f.name.as_str() == "agentic_ns__mcp__shell__run")
        ));
    }

    #[test]
    fn namespaced_function_tool_choice_flattens_exact_member() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__shell",
                "tools": [{"type": "function", "name": "run"}]
            },
            {
                "type": "namespace",
                "name": "mcp__git",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();
        let choice: ToolChoice = serde_json::from_value(serde_json::json!({
            "type": "function",
            "namespace": "mcp__git",
            "name": "run"
        }))
        .unwrap();

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        let rewritten = CodexNamespaceHandler.resolve_tool_choice(map.as_ref(), Some(&choice));

        assert_eq!(
            rewritten,
            ToolChoice::Function {
                namespace: None,
                name: NonEmptyToolName::try_from("agentic_ns__mcp__git__run").unwrap()
            }
        );
    }

    #[test]
    fn validate_namespace_collisions_rejects_top_level_flat_name_collision() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {"type": "function", "name": "agentic_ns__mcp__shell__run"},
            {
                "type": "namespace",
                "name": "mcp__shell",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();

        let err = CodexNamespaceHandler
            .validate_namespace_collisions(Some(&tools))
            .unwrap_err();

        assert!(err.to_string().contains("collides with top-level function"));
    }

    #[test]
    fn resolve_namespace_members_rejects_top_level_flat_name_collision() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {"type": "function", "name": "agentic_ns__mcp__shell__run"},
            {
                "type": "namespace",
                "name": "mcp__shell",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();

        let err = CodexNamespaceHandler.resolve_namespace_members(&tools).unwrap_err();

        assert!(err.to_string().contains("collides with top-level function"));
    }

    #[test]
    fn validate_namespace_collisions_rejects_generated_name_collision_between_namespace_members() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "a__b",
                "tools": [{"type": "function", "name": "c"}]
            },
            {
                "type": "namespace",
                "name": "a",
                "tools": [{"type": "function", "name": "b__c"}]
            }
        ]))
        .unwrap();

        let err = CodexNamespaceHandler
            .validate_namespace_collisions(Some(&tools))
            .unwrap_err();

        assert!(err.to_string().contains("generated name"));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "namespace collisions must be validated before recording namespace members")]
    fn namespace_map_builder_debug_asserts_when_member_collision_validation_is_skipped() {
        let mut builder = NamespaceMapBuilder::new(HashSet::new());

        assert_eq!(
            builder.record_flat_member_with_flat_name("a__b", "c", "agentic_ns__a__b__c".to_owned()),
            "agentic_ns__a__b__c"
        );
        let _ = builder.record_flat_member_with_flat_name("a", "b__c", "agentic_ns__a__b__c".to_owned());
    }

    #[test]
    fn flat_namespace_member_call_preserves_tools_argument() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__agentic_fixture",
                "tools": [{"type": "function", "name": "run"}]
            }
        ]))
        .unwrap();
        let mut output = vec![completed_call(
            "agentic_ns__mcp__agentic_fixture__run",
            "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}",
        )];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        CodexNamespaceHandler.restore_output_items(&mut output, map.as_ref());

        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert_eq!(call.namespace.as_deref(), Some("mcp__agentic_fixture"));
        assert_eq!(call.name, "run");
        assert_eq!(call.arguments, "{\"tools\":\"legitimate\",\"cmd\":\"pwd\"}");
    }

    #[test]
    fn plain_function_call_round_trip() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "function",
                "name": "get_weather",
                "parameters": {"type": "object"}
            }
        ]))
        .unwrap();
        let resolved = CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("valid namespace members");
        let mut output = vec![completed_call("get_weather", "{\"city\":\"SF\"}")];

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        CodexNamespaceHandler.restore_output_items(&mut output, map.as_ref());

        assert!(matches!(
            resolved.as_slice(),
            [ResponsesTool::Function(function)] if function.name.as_str() == "get_weather"
        ));
        let OutputItem::FunctionCall(call) = &output[0] else {
            panic!("expected function call");
        };
        assert!(call.namespace.is_none());
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments, "{\"city\":\"SF\"}");
    }

    #[test]
    fn response_value_normalizes_nested_function_call_item() {
        let tools: Vec<ResponsesTool> = serde_json::from_value(serde_json::json!([
            {
                "type": "namespace",
                "name": "mcp__agentic_fixture",
                "tools": [{"type": "function", "name": "add_numbers"}]
            }
        ]))
        .unwrap();
        let mut value = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "agentic_ns__mcp__agentic_fixture__add_numbers",
                "call_id": "call_1",
                "arguments": "{\"numbers\":[8,0]}"
            }
        });

        let map = CodexNamespaceHandler
            .build_namespace_map(Some(&tools))
            .expect("valid namespace map");
        assert!(CodexNamespaceHandler.restore_response_value(&mut value, map.as_ref()));
        assert_eq!(value["item"]["namespace"], "mcp__agentic_fixture");
        assert_eq!(value["item"]["name"], "add_numbers");
        assert_eq!(value["item"]["arguments"], "{\"numbers\":[8,0]}");
    }
}
