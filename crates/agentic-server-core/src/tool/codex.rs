use crate::types::io::InOutItem;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use super::handler::{ToolError, ToolHandler};
use super::registry::{ToolEntry, ToolType};
use crate::types::io::{FunctionTool, InputFunctionToolCall, InputItem, ResponsesInput, ToolChoice};
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, NonEmptyToolName};

use super::declaration::ToolDeclaration;

// Upstream Responses-compatible backends only see flat function names. Prefix
// flattened Codex namespace members so generated names are recognizable,
// unlikely to collide with user functions, and can be restored to
// `{ namespace, name }` on the way back to the client.
pub const MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX: &str = "agentic_ns__";
pub const MAX_MODEL_VISIBLE_TOOL_NAME_LEN: usize = 64;

const HASHED_NAMESPACE_MEMBER_SUFFIX_LEN: usize = 18;

fn stable_name_hash(value: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    value.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

#[must_use]
pub fn model_visible_namespace_member_name(namespace: &str, member: &str) -> String {
    let full_name = format!("{MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX}{namespace}__{member}");
    if full_name.chars().count() <= MAX_MODEL_VISIBLE_TOOL_NAME_LEN {
        return full_name;
    }

    let hash = stable_name_hash(&full_name);
    let readable_len = MAX_MODEL_VISIBLE_TOOL_NAME_LEN - HASHED_NAMESPACE_MEMBER_SUFFIX_LEN;
    let readable_prefix = full_name.chars().take(readable_len).collect::<String>();
    format!("{readable_prefix}__{hash:016x}")
}

/// Registers one `ToolEntry` per `Function` member of `p`, keyed by the
/// member's already-flattened, model-visible name — callers must resolve
/// namespace members to those flat names first (see
/// [`CodexNamespaceHandler::resolve_namespace_members`]).
pub(crate) fn insert_namespace_entries(entries: &mut HashMap<String, ToolEntry>, p: &CodexNamespaceToolParam) {
    for member in &p.tools {
        let CodexNamespaceMember::Function(function) = member else {
            continue;
        };
        let name = function.name.as_str().to_owned();
        if entries
            .insert(
                name.clone(),
                ToolEntry::client(ToolType::CodexNamespace, Some(p.name.clone()))
                    .with_async_execution(function.is_async()),
            )
            .is_some()
        {
            tracing::warn!(name = %name, namespace = %p.name, "duplicate tool name - previous definition overwritten");
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NamespaceMemberName {
    namespace: String,
    name: String,
}

#[derive(Clone, Debug)]
pub(crate) struct NamespaceCallMapping {
    member: NamespaceMemberName,
    upstream_name: String,
}

/// A pre-built, reusable namespace rename map, computed once per request from
/// the declared tools via [`CodexNamespaceHandler::build_namespace_map`].
///
/// Shared lookup facts for request normalization and public namespace translation.
#[derive(Clone, Debug, Default)]
pub struct NamespaceMap {
    calls: HashMap<String, NamespaceCallMapping>,
    members: HashMap<NamespaceMemberName, String>,
}

impl NamespaceMap {
    pub(crate) fn public_member(&self, name: &str) -> Option<(&str, &str)> {
        self.mapping_for_call(name)
            .map(|mapping| (mapping.member.namespace.as_str(), mapping.member.name.as_str()))
    }

    fn mapping_for_call(&self, name: &str) -> Option<&NamespaceCallMapping> {
        self.calls.get(name)
    }

    pub(crate) fn mapping_for_member(&self, namespace: &str, name: &str) -> Option<&NamespaceCallMapping> {
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
    top_level_registry_keys: HashMap<String, ToolType>,
    map: NamespaceMap,
}

impl NamespaceMapBuilder {
    fn new(top_level_registry_keys: HashMap<String, ToolType>) -> Self {
        Self {
            top_level_registry_keys,
            ..Self::default()
        }
    }

    fn validate_and_record_flat_member(
        &mut self,
        namespace_name: &str,
        member_name: &str,
    ) -> Result<String, ToolError> {
        let flat_name = model_visible_namespace_member_name(namespace_name, member_name);
        if let Some(tool_kind) = self.top_level_registry_keys.get(&flat_name) {
            return Err(ToolError::Config(format!(
                "codex namespace member {namespace_name}.{member_name} generates name {flat_name}, which collides with a declared {}",
                tool_kind.description()
            )));
        }
        if let Some(existing) = self.map.calls.get(&flat_name)
            && (existing.member.namespace != namespace_name || existing.member.name != member_name)
        {
            return Err(ToolError::Config(format!(
                "codex namespace member {namespace_name}.{member_name} collides with {}.{} at generated name {flat_name}",
                existing.member.namespace, existing.member.name
            )));
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
    /// with collision detection against sibling function-call registry keys.
    ///
    /// Tools stay `ToolDeclaration::Namespace` — only the nested members'
    /// `name` fields change — so [`ToolDeclaration::to_function_tools`] and
    /// [`super::registry::ToolRegistry::build_with_handlers`] can read each
    /// member's already-flat name directly, with no further namespace logic.
    /// The input is the request's declarations as its adapter converted them;
    /// the resolved list is the tool layer's own.
    ///
    /// Request execution must handle the result so ambiguous declarations fail
    /// instead of being normalized into an irreversible flat shape.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a generated namespace member name
    /// collides with another declared function-call tool or with another
    /// namespace member.
    pub fn resolve_namespace_members(&self, tools: &[ToolDeclaration]) -> Result<Vec<ToolDeclaration>, ToolError> {
        let mut builder = NamespaceMapBuilder::new(typed_top_level_registry_keys(tools));
        tools
            .iter()
            .map(|tool| match tool {
                ToolDeclaration::Namespace(namespace) => {
                    rename_namespace_members(namespace, &mut builder).map(ToolDeclaration::Namespace)
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
    /// collides with another declared function-call tool or with another
    /// namespace member.
    pub fn build_namespace_map(&self, tools: Option<&[ToolDeclaration]>) -> Result<Option<NamespaceMap>, ToolError> {
        namespace_map_from_tools(tools)
    }

    /// Resolves the request's `tool_choice` (defaulting to `ToolChoice::Auto`
    /// when absent) and, if it's a namespaced `ToolChoice::Function {
    /// namespace, name }`, rewrites it to the flattened, model-visible name
    /// that [`ToolDeclaration::to_function_tools`] produces for the matching
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

    /// Rewrite public namespaced function-call history to the exact flat names
    /// from the request-scoped namespace map used for tool declarations, and
    /// unqualify a call to a searched top-level function, one of `functions`
    /// (see [`unqualify_or_flatten_calls`]).
    #[must_use]
    pub fn resolve_input<'a>(
        &self,
        map: Option<&NamespaceMap>,
        functions: &HashSet<&str>,
        input: Cow<'a, ResponsesInput>,
    ) -> Cow<'a, ResponsesInput> {
        let resolves = |call: &InputFunctionToolCall| {
            call.namespace
                .as_deref()
                .is_some_and(|namespace| map.is_some_and(|map| map.mapping_for_member(namespace, &call.name).is_some()))
                || is_searched_function_call(call, |name| functions.contains(name))
        };
        let should_rewrite = matches!(&*input, ResponsesInput::Items(items) if items.iter().any(|item| {
            matches!(item, InOutItem::Input(InputItem::FunctionCall(call)) if resolves(call))
        }));
        if !should_rewrite {
            return input;
        }
        let mut input = input.into_owned();
        unqualify_or_flatten_calls(&mut input, map, functions);
        Cow::Owned(input)
    }
}

impl ToolHandler for CodexNamespaceHandler {
    type ToolParams = CodexNamespaceToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::CodexNamespace
    }

    fn validate(&self, _params: &CodexNamespaceToolParam) -> Result<(), ToolError> {
        Ok(())
    }

    /// Converts an already-renamed namespace's function members straight to
    /// `FunctionTool`s. Callers must rename members to their flat, model-visible
    /// names first via [`CodexNamespaceHandler::resolve_namespace_members`] —
    /// this method has no sibling-tool context to do that itself.
    fn normalize(&self, params: &CodexNamespaceToolParam) -> Vec<FunctionTool> {
        params
            .tools
            .iter()
            .filter_map(|member| match member {
                CodexNamespaceMember::Function(function) => {
                    Some(super::function::FunctionHandler::model_visible_tool(function))
                }
                CodexNamespaceMember::Unknown => None,
            })
            .collect()
    }
}

fn namespace_map_from_tools(tools: Option<&[ToolDeclaration]>) -> Result<Option<NamespaceMap>, ToolError> {
    let Some(tools) = tools else {
        return Ok(None);
    };
    let mut builder = NamespaceMapBuilder::new(typed_top_level_registry_keys(tools));
    for tool in tools {
        if let ToolDeclaration::Namespace(namespace) = tool {
            let _ = rename_namespace_members(namespace, &mut builder)?;
        }
    }
    Ok(Some(builder.finish()))
}

/// Lowers namespaced calls to the flat names the model knows. A call whose
/// namespace is its own name, that no declared namespace maps, and whose name
/// is one of the request's top-level `functions` is to a searched function: the
/// public item mirrors the Responses API, but the model knows the function
/// unqualified. Any other unmapped call keeps its namespace, since a namespace
/// may hold a member of its own name.
fn unqualify_or_flatten_calls(input: &mut ResponsesInput, map: Option<&NamespaceMap>, functions: &HashSet<&str>) {
    let ResponsesInput::Items(items) = input else {
        return;
    };
    for item in items {
        let InOutItem::Input(InputItem::FunctionCall(call)) = item else {
            continue;
        };
        let Some(namespace) = call.namespace.as_deref() else {
            continue;
        };
        if let Some(mapping) = map.and_then(|map| map.mapping_for_member(namespace, &call.name)) {
            call.name.clone_from(&mapping.upstream_name);
            call.namespace = None;
        } else if is_searched_function_call(call, |name| functions.contains(name)) {
            call.namespace = None;
        }
    }
}

/// Whether `call` is to a searched top-level function: the Responses API names such a call's namespace after it, and
/// `is_function` confirms the name is a top-level function the request declares or a search in its history loaded.
/// The model knows the function unqualified, so every model-facing projection drops that namespace.
pub(crate) fn is_searched_function_call(call: &InputFunctionToolCall, is_function: impl Fn(&str) -> bool) -> bool {
    names_itself(call) && is_function(&call.name)
}

/// Whether `call` reports its own name as namespace, as the Responses API does for a searched top-level function.
fn names_itself(call: &InputFunctionToolCall) -> bool {
    call.namespace.as_deref() == Some(call.name.as_str())
}

/// Returns `namespace` with its function members' names rewritten to their
/// flat, model-visible form, recording each rename in `builder` along the
/// way.
///
/// # Errors
///
/// Returns [`ToolError::Config`] when a generated namespace member name
/// collides with another declared function-call tool or with another namespace
/// member.
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

fn typed_top_level_registry_keys(tools: &[ToolDeclaration]) -> HashMap<String, ToolType> {
    tools
        .iter()
        .filter_map(|tool| Some((tool.top_level_registry_key()?, tool.tool_type()?)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::declaration::responses_declarations;
    use crate::types::tools::ResponsesTool;

    #[test]
    fn unqualified_function_tool_choice_is_not_rewritten_to_namespace_member() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": "mcp__shell",
                    "tools": [{"type": "function", "name": "run"}]
                }
            ]))
            .unwrap(),
        );
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
            [ToolDeclaration::Namespace(namespace)]
                if matches!(&namespace.tools[0], CodexNamespaceMember::Function(f) if f.name.as_str() == "agentic_ns__mcp__shell__run")
        ));
    }

    #[test]
    fn namespaced_function_tool_choice_flattens_exact_member() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
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
            .unwrap(),
        );
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
    fn long_namespace_member_name_is_stable_and_within_upstream_limit() {
        let namespace = "mcp__codex_apps__github";
        let member = "_remove_reaction_from_pr_review_comment";

        let shortened = model_visible_namespace_member_name(namespace, member);

        assert_eq!(shortened.chars().count(), MAX_MODEL_VISIBLE_TOOL_NAME_LEN);
        assert_eq!(
            shortened,
            "agentic_ns__mcp__codex_apps__github___remove_r__2e989f39f22daf41"
        );
        assert_eq!(shortened, model_visible_namespace_member_name(namespace, member));
        assert_ne!(
            shortened,
            model_visible_namespace_member_name(namespace, "_remove_reaction_from_issue_comment")
        );
    }

    #[test]
    fn namespace_member_name_preserves_exact_limit_and_shortens_next_character() {
        let namespace = "n";
        let fixed_len = MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX.chars().count() + namespace.chars().count() + 2;
        let member_at_limit = "m".repeat(MAX_MODEL_VISIBLE_TOOL_NAME_LEN - fixed_len);
        let full_name_at_limit = format!("{MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX}{namespace}__{member_at_limit}");

        assert_eq!(full_name_at_limit.chars().count(), MAX_MODEL_VISIBLE_TOOL_NAME_LEN);
        assert_eq!(
            model_visible_namespace_member_name(namespace, &member_at_limit),
            full_name_at_limit
        );

        let member_over_limit = format!("{member_at_limit}m");
        let shortened = model_visible_namespace_member_name(namespace, &member_over_limit);
        assert_eq!(shortened.chars().count(), MAX_MODEL_VISIBLE_TOOL_NAME_LEN);
        assert_ne!(
            shortened,
            format!("{MODEL_VISIBLE_NAMESPACE_MEMBER_PREFIX}{namespace}__{member_over_limit}")
        );
    }

    #[test]
    fn long_unicode_namespace_member_name_stays_valid_utf8() {
        let namespace = "工具箱";
        let member = "工具".repeat(30);

        let shortened = model_visible_namespace_member_name(namespace, &member);

        assert_eq!(shortened.chars().count(), MAX_MODEL_VISIBLE_TOOL_NAME_LEN);
        assert!(shortened.starts_with("agentic_ns__工具箱__"));
    }

    #[test]
    fn resolve_namespace_members_rejects_top_level_flat_name_collision() {
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {"type": "function", "name": "agentic_ns__mcp__shell__run"},
                {
                    "type": "namespace",
                    "name": "mcp__shell",
                    "tools": [{"type": "function", "name": "run"}]
                }
            ]))
            .unwrap(),
        );

        let err = CodexNamespaceHandler.resolve_namespace_members(&tools).unwrap_err();

        assert!(err.to_string().contains("collides with a declared function tool"));
    }

    #[test]
    fn resolve_namespace_members_accepts_native_mcp_without_static_registry_key() {
        let namespace = "mcp__codex_apps__github";
        let member = "_remove_reaction_from_pr_review_comment";
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(serde_json::json!([
                {
                    "type": "namespace",
                    "name": namespace,
                    "tools": [{"type": "function", "name": member}]
                },
                {
                    "type": "mcp",
                    "server_label": "fixture",
                    "server_url": "http://127.0.0.1:1/mcp"
                }
            ]))
            .unwrap(),
        );

        CodexNamespaceHandler
            .resolve_namespace_members(&tools)
            .expect("native MCP registry keys are derived after discovery");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "namespace collisions must be validated before recording namespace members")]
    fn namespace_map_builder_debug_asserts_when_member_collision_validation_is_skipped() {
        let mut builder = NamespaceMapBuilder::new(HashMap::new());

        assert_eq!(
            builder.record_flat_member_with_flat_name("a__b", "c", "agentic_ns__a__b__c".to_owned()),
            "agentic_ns__a__b__c"
        );
        let _ = builder.record_flat_member_with_flat_name("a", "b__c", "agentic_ns__a__b__c".to_owned());
    }
}
