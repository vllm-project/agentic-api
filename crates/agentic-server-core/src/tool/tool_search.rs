mod validation;
pub(crate) use validation::ensure_request_prepared;
use validation::{input_contains_tool_search_state, request_contains_tool_search_state, validate_tool_search_request};

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::event::{MessageStatus, ResponseStatus};
pub(crate) use crate::types::io::input::TOOL_SEARCH_NAME;
use crate::types::io::output::FunctionToolCall;
use crate::types::io::{FunctionTool, InOutItem, InputItem, ResponsesInput, ToolChoice, ToolSearchCall};
use crate::types::request_response::RequestPayload;
use crate::types::tools::{
    CodexNamespaceMember, CodexNamespaceToolParam, ResponsesTool, ToolSearchExecution, ToolSearchStatus,
    ToolSearchToolParam,
};
use crate::utils::common::{deserialize_from_str, deserialize_from_value};

pub(crate) use super::catalog::mcp_namespace_name;
use super::catalog::{McpLoads, NamespaceMemberRecords, PriorCalls, ToolCatalog, tool_identity};
use super::handler::{ToolError, ToolHandler};
use super::registry::ToolType;
pub(crate) use crate::types::io::output::tool_search_call::public_tool_search_item_id as public_item_id;

const DEFAULT_DESCRIPTION: &str = "Search the client tool catalog";
const DEFAULT_QUERY_DESCRIPTION: &str = "A concise description of the needed capabilities.";
const HOSTED_DESCRIPTION: &str = "Load deferred tools before calling them. Pass `paths` with the names of \
catalog entries to load: a function, a namespace, or an MCP server (`mcp_<server_label>`). A namespace or MCP \
server path loads all of its tools. `query` is optional and only narrows the load to tools whose own name or \
description contains it; it does not search documents or answer questions, so omit it unless you know part of a \
tool's name";

/// Handler for client-executed `type: "tool_search"` declarations.
///
/// The declaration remains a first-class tool-search type throughout request
/// preparation and registry construction. This handler performs the one
/// provider-specific lowering step to the ordinary function shape understood
/// by upstreams without native tool-search support.
#[derive(Debug)]
pub struct ToolSearchHandler;

impl ToolSearchHandler {
    /// Whether this request carries a tool-search declaration, deferred
    /// definition, or replayed tool-search item and therefore needs the
    /// executor's preparation path.
    #[must_use]
    pub fn request_has_state<T: ?Sized, C>(request: &RequestPayload<T, C>) -> bool {
        request_contains_tool_search_state(request, &request.input)
    }

    /// Prepare the private inference view from fully rehydrated public state.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the public tool-search history or
    /// effective tool selection is invalid.
    pub(crate) fn prepare_request(
        request: &mut RequestPayload,
        restored_loaded_tools: &[ResponsesTool],
        restore_only_declared: bool,
    ) -> Result<Option<ToolSearchState>, ToolError> {
        let mut state =
            ToolSearchState::build_with_loaded_tools(request, restored_loaded_tools, restore_only_declared)?;
        if !state.is_active() {
            return Ok(None);
        }
        state.prepare_inference_request(request)?;
        Ok(Some(state))
    }

    #[must_use]
    pub(crate) fn normalized_param(param: &ToolSearchToolParam) -> ToolSearchToolParam {
        let mut normalized = param.clone();
        normalized.description = Some(
            param
                .description
                .as_deref()
                .filter(|description| !description.trim().is_empty())
                .unwrap_or(DEFAULT_DESCRIPTION)
                .to_owned(),
        );
        normalized.parameters = Some(
            param
                .parameters
                .clone()
                .unwrap_or_else(|| Value::Object(default_parameters())),
        );
        normalized
    }

    #[must_use]
    fn function_tool(param: &ToolSearchToolParam) -> FunctionTool {
        let normalized = Self::normalized_param(param);
        FunctionTool {
            type_: "function".to_owned(),
            name: TOOL_SEARCH_NAME.to_owned(),
            description: normalized.description,
            parameters: normalized.parameters,
            strict: Some(false),
        }
    }
}

impl ToolHandler for ToolSearchHandler {
    type ToolParams = ToolSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::ToolSearch
    }

    fn validate(&self, param: &ToolSearchToolParam) -> Result<(), ToolError> {
        if param
            .parameters
            .as_ref()
            .is_some_and(|parameters| !parameters.is_object())
        {
            return Err(ToolError::Config(
                "tool_search parameters must be a JSON object for private function lowering".to_owned(),
            ));
        }
        Ok(())
    }

    fn normalize(&self, param: &ToolSearchToolParam) -> Vec<FunctionTool> {
        vec![Self::function_tool(param)]
    }
}

fn default_parameters() -> Map<String, Value> {
    let query = Map::from_iter([
        ("type".to_owned(), Value::String("string".to_owned())),
        (
            "description".to_owned(),
            Value::String(DEFAULT_QUERY_DESCRIPTION.to_owned()),
        ),
    ]);
    let properties = Map::from_iter([("query".to_owned(), Value::Object(query))]);
    Map::from_iter([
        ("type".to_owned(), Value::String("object".to_owned())),
        ("properties".to_owned(), Value::Object(properties)),
        (
            "required".to_owned(),
            Value::Array(vec![Value::String("query".to_owned())]),
        ),
        ("additionalProperties".to_owned(), Value::Bool(false)),
    ])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolSearchActivity {
    Inactive,
    Active,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CatalogEntry {
    Function {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Namespace {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

impl CatalogEntry {
    fn display_name(&self) -> &str {
        match self {
            Self::Function { name, .. } | Self::Namespace { name, .. } => name,
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Function { description, .. } | Self::Namespace { description, .. } => description.as_deref(),
        }
    }
}

/// Pure, request-scoped state derived from fully rehydrated public history.
///
/// Preparation builds the request's [`ToolCatalog`] here; the response's tool registry then takes it and owns it for
/// the rest of the response. What remains is the public snapshot persistence needs (the effective declarations and
/// the loaded definitions, which the registry updates after each load) and preparation's private request views.
///
/// The state deliberately has no `Serialize` implementation and its `Debug`
/// output contains counts only.
#[derive(Clone)]
pub struct ToolSearchState {
    activity: ToolSearchActivity,
    has_completed_search: bool,
    /// Public effective declarations, for persistence and response metadata.
    effective_tools: Option<Vec<ResponsesTool>>,
    /// Public definitions loads resolved, in first-load order, for persistence.
    loaded_tools: Vec<ResponsesTool>,
    private_upstream_tools: Option<Vec<ResponsesTool>>,
    inference_input: Option<ResponsesInput>,
    synthetic_tool_search: Option<ToolSearchToolParam>,
    /// Whether the gateway runs this request's search (`execution: "server"`).
    hosted: bool,
    /// The request's definitions until the registry takes them.
    catalog: Option<ToolCatalog>,
}

/// Public tool-search state retained only for response persistence.
pub(crate) struct ToolSearchMetadata {
    pub(crate) effective_tools: Option<Vec<ResponsesTool>>,
    pub(crate) loaded_tools: Vec<ResponsesTool>,
}

impl fmt::Debug for ToolSearchState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolSearchState")
            .field("activity", &self.activity)
            .field("active", &self.is_active())
            .field("has_completed_search", &self.has_completed_search)
            .field(
                "public_effective_tool_count",
                &self.effective_tools.as_ref().map_or(0, Vec::len),
            )
            .field("loaded_public_tool_count", &self.loaded_tools.len())
            .field(
                "private_upstream_tool_count",
                &self.private_upstream_tools.as_ref().map_or(0, Vec::len),
            )
            .field("has_inference_input", &self.inference_input.is_some())
            .field("has_synthetic_tool_search", &self.synthetic_tool_search.is_some())
            .field("hosted", &self.hosted)
            .field("holds_catalog", &self.catalog.is_some())
            .finish()
    }
}

impl Default for ToolSearchState {
    fn default() -> Self {
        Self {
            activity: ToolSearchActivity::Inactive,
            has_completed_search: false,
            effective_tools: None,
            loaded_tools: Vec::new(),
            private_upstream_tools: None,
            inference_input: None,
            synthetic_tool_search: None,
            hosted: false,
            catalog: None,
        }
    }
}

impl ToolSearchState {
    /// Build deterministic public/private views from ordered public history.
    ///
    /// This function performs no network, storage, clock, random-ID, or
    /// transport work. It runs in linear time in input items and definitions;
    /// vectors retain declaration/history order and maps are lookup-only.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] for an invalid public declaration,
    /// call/output ordering or linkage error, duplicate/conflicting definition,
    /// or normalized-name collision.
    pub fn build(request: &RequestPayload) -> Result<Self, ToolError> {
        Self::build_with_loaded_tools(request, &[], false)
    }

    /// Build state with public loaded definitions restored from typed response
    /// metadata when compaction has removed the original search pair.
    ///
    /// The restored definitions pass through the same validation and loading
    /// logic as definitions in a public `tool_search_output`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] under the same conditions as [`Self::build`].
    pub fn build_with_loaded_tools(
        request: &RequestPayload,
        restored_loaded_tools: &[ResponsesTool],
        restore_only_declared: bool,
    ) -> Result<Self, ToolError> {
        let active_input = active_model_input(&request.input);
        if !validate_tool_search_request(request, &active_input)? {
            return Ok(Self::default());
        }

        let has_search_history = input_contains_tool_search_state(&active_input);
        let has_completed_search = !restored_loaded_tools.is_empty()
            || active_input
                .input_items()
                .any(|item| matches!(&*item, InputItem::ToolSearchOutput(_)));
        let declaration = request
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find_map(|tool| match tool {
                ResponsesTool::ToolSearch(declaration) => Some(declaration),
                _ => None,
            });
        if declaration.is_none() && !has_search_history {
            return Err(ToolError::Config(
                "defer_loading requires a tool_search declaration or replayed tool-search history".to_owned(),
            ));
        }

        let tools_were_present = request.tools.is_some();
        let mut catalog = ToolCatalog::build(request.tools.clone().unwrap_or_default())?;
        let hosted = declaration.is_some_and(ToolSearchToolParam::is_hosted);
        validate_search_mode(&catalog.public_tools, &catalog.mcp_loads, declaration)?;
        restore_loaded_definitions(restored_loaded_tools, &mut catalog, restore_only_declared)?;
        catalog.load_history(&active_input)?;
        catalog.validate_model_visible_names()?;

        let entries = catalog_entries(&catalog);
        let synthetic_tool_search = declaration.map(|declaration| synthetic_tool_search(declaration, &entries));
        let private_tools = offered_tools(&catalog, synthetic_tool_search.as_ref());
        catalog.begin_response();
        let effective_tools =
            (tools_were_present || !catalog.public_tools.is_empty()).then(|| catalog.public_tools.clone());
        let private_upstream_tools = (tools_were_present || !private_tools.is_empty()).then_some(private_tools);

        Ok(Self {
            activity: ToolSearchActivity::Active,
            has_completed_search,
            effective_tools,
            loaded_tools: catalog.loaded_public_tools.clone(),
            private_upstream_tools,
            inference_input: Some(active_input),
            synthetic_tool_search,
            hosted,
            catalog: Some(catalog),
        })
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.activity, ToolSearchActivity::Active)
    }

    #[must_use]
    pub fn public_effective_tools(&self) -> Option<&[ResponsesTool]> {
        self.effective_tools.as_deref()
    }

    /// Public declarations available for selection in response metadata.
    /// Before search completes, the response echoes the declared catalog. Once
    /// search resolves availability, it exposes only initially available and
    /// loaded definitions while preserving their public namespace shape.
    #[must_use]
    pub(crate) fn public_response_tools(&self) -> Vec<ResponsesTool> {
        let public_tools = self.public_effective_tools().unwrap_or_default();
        // A hosted response echoes the declared tools unchanged, as the Responses API does.
        if self.hosted || !self.has_completed_search {
            return public_tools.to_vec();
        }
        available_public_tools(public_tools, &self.loaded_tools)
    }

    /// Public definitions resolved by completed search outputs, in first-load order.
    ///
    /// This remains separate from `public_effective_tools`: an initially
    /// deferred definition stays deferred publicly even after becoming loaded.
    #[must_use]
    pub fn loaded_public_tools(&self) -> &[ResponsesTool] {
        &self.loaded_tools
    }

    /// Private tool-search declaration used by request-scoped registry and upstream normalization.
    #[must_use]
    pub const fn synthetic_tool_search(&self) -> Option<&ToolSearchToolParam> {
        self.synthetic_tool_search.as_ref()
    }

    /// The request's catalog, until the registry takes it.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn catalog(&self) -> Option<&ToolCatalog> {
        self.catalog.as_ref()
    }

    /// Hands the request's catalog to the response's tool registry, its owner from then on.
    pub(crate) const fn take_catalog(&mut self) -> Option<ToolCatalog> {
        self.catalog.take()
    }

    /// Records the public snapshot after the registry loaded searched tools.
    pub(crate) fn record_loads(&mut self, effective_tools: Vec<ResponsesTool>, loaded_tools: Vec<ResponsesTool>) {
        self.effective_tools = Some(effective_tools);
        self.loaded_tools = loaded_tools;
        self.has_completed_search = true;
    }

    /// Model-visible names preparation withholds, while this state still holds the catalog.
    #[must_use]
    pub(crate) fn withheld_function_names(&self) -> &HashSet<String> {
        static NONE: std::sync::LazyLock<HashSet<String>> = std::sync::LazyLock::new(HashSet::new);
        self.catalog
            .as_ref()
            .map_or(&*NONE, |catalog| &catalog.withheld_function_names)
    }

    /// Replace the request's public tool-search views with the prepared input, whose search items the upstream
    /// projection lowers, and the private tools used for inference. The retained state then contains only public
    /// metadata needed after inference.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the effective tool choice conflicts
    /// with the prepared private tool set.
    pub fn prepare_inference_request(&mut self, request: &mut RequestPayload) -> Result<(), ToolError> {
        validate_effective_tool_choice(request.tool_choice.as_ref(), self.withheld_function_names())?;
        let input = self
            .inference_input
            .take()
            .ok_or_else(|| ToolError::Config("tool-search inference input has already been consumed".to_owned()))?;
        request.input = input;
        request.tools = self.private_upstream_tools.take();
        Ok(())
    }

    /// Whether the gateway runs this request's search.
    #[must_use]
    pub(crate) const fn is_hosted(&self) -> bool {
        self.hosted
    }

    pub(crate) fn into_public_metadata(self) -> ToolSearchMetadata {
        ToolSearchMetadata {
            effective_tools: self.effective_tools,
            loaded_tools: self.loaded_tools,
        }
    }
}

impl ToolCatalog {
    /// Deferred top-level functions; their calls report their own name as namespace.
    #[must_use]
    pub(crate) fn deferred_functions(&self) -> HashSet<String> {
        self.public_tools
            .iter()
            .filter_map(|tool| match tool {
                ResponsesTool::Function(function) if function.defer_loading == Some(true) => {
                    Some(function.name.as_str().to_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// Model-visible names of deferred MCP tools that are not loaded yet, given the discovery recorded on
    /// `private_tools`. Kept apart from `withheld_function_names`: discovered MCP tools are registry entries.
    #[must_use]
    pub(crate) fn withheld_mcp_tool_names(&self, private_tools: &[ResponsesTool]) -> HashSet<String> {
        private_tools
            .iter()
            .filter_map(|tool| match tool {
                ResponsesTool::Mcp(server) if self.mcp_loads.is_deferred(&server.server_label) => Some(server),
                _ => None,
            })
            .flat_map(|server| {
                let loaded = self.mcp_loads.loaded.get(&server.server_label);
                server
                    .discovered_tools
                    .iter()
                    .filter(move |tool| loaded.is_none_or(|loaded| !loaded.contains(&tool.tool_name)))
                    .map(|tool| tool.internal_name.clone())
            })
            .collect()
    }

    /// Rejects a `tool_choice` naming a deferred MCP tool that is not loaded; those names are known only once MCP
    /// discovery has run.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when the choice targets such a tool.
    pub(crate) fn validate_mcp_tool_choice(
        &self,
        tool_choice: Option<&ToolChoice>,
        private_tools: &[ResponsesTool],
    ) -> Result<(), ToolError> {
        let withheld = self.withheld_mcp_tool_names(private_tools);
        if withheld.is_empty() {
            return Ok(());
        }
        validate_effective_tool_choice(tool_choice, &withheld)
    }
}

/// Model-visible history (search items kept for definition loading) plus MCP list records.
fn active_model_input(input: &ResponsesInput) -> ResponsesInput {
    let mut active_input = input.model_input_keeping_search_items().into_owned();
    if let (ResponsesInput::Items(public_items), ResponsesInput::Items(active_items)) = (input, &mut active_input) {
        active_items.extend(
            public_items
                .iter()
                .filter(|item| matches!(item, InOutItem::Input(InputItem::McpListTools(_))))
                .cloned(),
        );
    }
    active_input
}

fn validate_effective_tool_choice(
    tool_choice: Option<&ToolChoice>,
    withheld_function_names: &HashSet<String>,
) -> Result<(), ToolError> {
    let targets_withheld = match tool_choice {
        Some(ToolChoice::Function { namespace, name }) => {
            let model_name = namespace.as_deref().map_or_else(
                || name.as_str().to_owned(),
                |namespace| super::model_visible_namespace_member_name(namespace, name.as_str()),
            );
            withheld_function_names.contains(&model_name)
        }
        Some(ToolChoice::AllowedTools { tools, .. }) => tools
            .iter()
            .any(|tool| tool.type_.as_str() == "function" && withheld_function_names.contains(tool.name.as_str())),
        _ => false,
    };
    if targets_withheld {
        return Err(ToolError::Config(
            "tool_choice targets a function before its definition is loaded".to_owned(),
        ));
    }
    Ok(())
}

fn restore_loaded_definitions(
    restored_loaded_tools: &[ResponsesTool],
    catalog: &mut ToolCatalog,
    restore_only_declared: bool,
) -> Result<(), ToolError> {
    for tool in restored_loaded_tools {
        let Some(tool) = restored_definition_for_load(tool, catalog, restore_only_declared)? else {
            continue;
        };
        catalog.load(&tool, &PriorCalls::default())?;
    }
    Ok(())
}

fn restored_definition_for_load(
    restored: &ResponsesTool,
    catalog: &ToolCatalog,
    restore_only_declared: bool,
) -> Result<Option<ResponsesTool>, ToolError> {
    let identity = tool_identity(restored)?.ok_or_else(|| {
        ToolError::Config("stored tool-search availability contains an unsupported definition".to_owned())
    })?;
    if restore_only_declared
        && catalog.definition(identity.name()).is_none()
        && catalog.mcp_loads.label(identity.name()).is_none()
    {
        return Ok(None);
    }
    Ok(Some(restored.clone()))
}

pub(crate) fn invalid_upstream_search_call() -> ToolError {
    ToolError::InvalidUpstreamToolSearch
}

pub(crate) fn invalid_upstream_withheld_function_call() -> ToolError {
    ToolError::UpstreamWithheldFunctionCall
}

pub(crate) fn started_public_call(call: &FunctionToolCall) -> Result<ToolSearchCall, ToolError> {
    if call.id.trim().is_empty()
        || call.call_id.trim().is_empty()
        || call.name != TOOL_SEARCH_NAME
        || call.namespace.is_some()
    {
        return Err(invalid_upstream_search_call());
    }
    Ok(ToolSearchCall {
        agent: call.agent.clone(),
        id: public_item_id(&call.id),
        call_id: Some(call.call_id.clone()),
        execution: crate::types::tools::ToolSearchExecution::Client,
        arguments: Value::Object(Map::new()),
        status: ToolSearchStatus::InProgress,
    })
}

pub(crate) fn completed_public_call(call: &FunctionToolCall) -> Result<ToolSearchCall, ToolError> {
    if call.status != MessageStatus::Completed {
        return Err(invalid_upstream_search_call());
    }
    let mut public = started_public_call(call)?;
    public.arguments = json_value(&call.arguments)?;
    public.status = ToolSearchStatus::Completed;
    Ok(public)
}

pub(crate) fn project_synthetic_call(
    call: &FunctionToolCall,
    response_status: ResponseStatus,
    unfinished_stream_call: bool,
) -> Result<Option<ToolSearchCall>, ToolError> {
    if !unfinished_stream_call && call.status == MessageStatus::Completed {
        return completed_public_call(call).map(Some);
    }
    match response_status {
        ResponseStatus::Incomplete => {
            let mut public = started_public_call(call)?;
            public.status = ToolSearchStatus::Incomplete;
            Ok(Some(public))
        }
        ResponseStatus::Error => Ok(None),
        ResponseStatus::InProgress | ResponseStatus::Completed => Err(invalid_upstream_search_call()),
    }
}

pub(crate) fn project_native_call(
    call: &ToolSearchCall,
    response_status: ResponseStatus,
) -> Result<Option<ToolSearchCall>, ToolError> {
    if call.status == ToolSearchStatus::Completed {
        return Ok(Some(call.clone()));
    }
    match response_status {
        ResponseStatus::Incomplete => {
            let mut public = call.clone();
            public.status = ToolSearchStatus::Incomplete;
            Ok(Some(public))
        }
        ResponseStatus::Error => Ok(None),
        ResponseStatus::InProgress | ResponseStatus::Completed => Err(invalid_upstream_search_call()),
    }
}

pub(crate) fn ensure_function_is_available(is_withheld: bool) -> Result<(), ToolError> {
    if is_withheld {
        return Err(invalid_upstream_withheld_function_call());
    }
    Ok(())
}

pub(crate) fn validate_public_arguments(arguments: &str) -> Result<(), ToolError> {
    json_value(arguments).map(|_| ())
}

pub(crate) fn strict_started_function(item: &Value) -> Result<FunctionToolCall, ToolError> {
    let function = strict_function_call(item)?;
    if function.status != MessageStatus::InProgress || !function.arguments.is_empty() {
        return Err(invalid_upstream_search_call());
    }
    Ok(function)
}

#[derive(Debug, Deserialize)]
struct StrictFunctionToolCall {
    #[serde(default)]
    agent: Option<crate::types::io::AgentAttribution>,
    id: String,
    call_id: String,
    name: String,
    #[serde(default)]
    namespace: Option<Value>,
    arguments: String,
    status: MessageStatus,
}

pub(crate) fn strict_function_call(item: &Value) -> Result<FunctionToolCall, ToolError> {
    let call: StrictFunctionToolCall =
        deserialize_from_value(item.clone()).map_err(|_| invalid_upstream_search_call())?;
    if call.namespace.is_some() {
        return Err(invalid_upstream_search_call());
    }
    let call = FunctionToolCall {
        async_execution: false,
        agent: call.agent,
        id: call.id,
        call_id: call.call_id,
        name: call.name,
        namespace: None,
        arguments: call.arguments,
        status: call.status,
    };
    started_public_call(&call)?;
    if call.status == MessageStatus::Completed {
        json_value(&call.arguments)?;
    }
    Ok(call)
}

pub(crate) fn strict_native_call(item: Value) -> Result<ToolSearchCall, ToolError> {
    if item.get("namespace").is_some_and(|namespace| !namespace.is_null()) {
        return Err(invalid_upstream_search_call());
    }
    let call: ToolSearchCall = deserialize_from_value(item).map_err(|_| invalid_upstream_search_call())?;
    native_call_id(&call)?;
    Ok(call)
}

/// The call id of an upstream-native search, which is always client-executed:
/// the gateway lowers every declaration it sends upstream to a function.
pub(crate) fn native_call_id(call: &ToolSearchCall) -> Result<&str, ToolError> {
    match (call.execution, call.call_id.as_deref()) {
        (ToolSearchExecution::Client, Some(call_id)) => Ok(call_id),
        _ => Err(invalid_upstream_search_call()),
    }
}

fn json_value(arguments: &str) -> Result<Value, ToolError> {
    deserialize_from_str(arguments).map_err(|_| invalid_upstream_search_call())
}

pub(crate) fn validate_blocking_response(
    body: &str,
    tool_search_enabled: bool,
    withheld_function_names: &HashSet<String>,
) -> Result<(), ToolError> {
    if !tool_search_enabled && withheld_function_names.is_empty() && !might_contain_tool_search_wire(body) {
        return Ok(());
    }
    let value: Value = match deserialize_from_str(body) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map_or(ResponseStatus::Completed, |status| status.parse().unwrap_or_default());
    let allow_unfinished = matches!(status, ResponseStatus::Error | ResponseStatus::Incomplete);
    let mut call_ids = HashSet::new();
    for item in value.get("output").and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) == Some("function_call")
            && item
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| withheld_function_names.contains(name))
        {
            return Err(invalid_upstream_withheld_function_call());
        }
        match item.get("type").and_then(Value::as_str) {
            Some("tool_search_call") => {
                let call = strict_native_call(item.clone())?;
                if !call_ids.insert(native_call_id(&call)?.to_owned()) {
                    return Err(invalid_upstream_search_call());
                }
                if !allow_unfinished && call.status != ToolSearchStatus::Completed {
                    return Err(invalid_upstream_search_call());
                }
            }
            Some("function_call")
                if tool_search_enabled && item.get("name").and_then(Value::as_str) == Some(TOOL_SEARCH_NAME) =>
            {
                let call = strict_function_call(item)?;
                if !call_ids.insert(call.call_id.clone()) {
                    return Err(invalid_upstream_search_call());
                }
                if !allow_unfinished && call.status != MessageStatus::Completed {
                    return Err(invalid_upstream_search_call());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn might_contain_tool_search_wire(wire: &str) -> bool {
    wire.contains(TOOL_SEARCH_NAME)
}

/// The deferred entries the synthetic search declaration lists: functions and namespaces not fully loaded, then
/// deferred MCP servers.
fn catalog_entries(catalog: &ToolCatalog) -> Vec<CatalogEntry> {
    let public_tools = &catalog.public_tools;
    public_tools
        .iter()
        .filter_map(|tool| {
            let identity = tool_identity(tool).ok().flatten()?;
            let record = catalog.definition(identity.name())?;
            if record.loaded {
                return None;
            }
            match tool {
                ResponsesTool::Function(function) if function.defer_loading == Some(true) => {
                    Some(CatalogEntry::Function {
                        name: function.name.as_str().to_owned(),
                        description: function.description.clone(),
                    })
                }
                ResponsesTool::Namespace(namespace)
                    if namespace_has_withheld_member(record.namespace_members.as_ref()) =>
                {
                    Some(CatalogEntry::Namespace {
                        name: namespace.name.clone(),
                        description: namespace.description.clone(),
                    })
                }
                ResponsesTool::Function(_)
                | ResponsesTool::Namespace(_)
                | ResponsesTool::Mcp(_)
                | ResponsesTool::ToolSearch(_)
                | ResponsesTool::WebSearch(_)
                | ResponsesTool::FileSearch(_)
                | ResponsesTool::CodeInterpreter(_)
                | ResponsesTool::Shell(_)
                | ResponsesTool::Custom(_)
                | ResponsesTool::Unknown => None,
            }
        })
        .chain(public_tools.iter().filter_map(|tool| match tool {
            ResponsesTool::Mcp(server) if server.defer_loading == Some(true) => Some(CatalogEntry::Namespace {
                name: mcp_namespace_name(&server.server_label),
                description: server.server_description.clone(),
            }),
            _ => None,
        }))
        .collect()
}

/// Catalog prose deliberately follows the provider-characterization shape:
/// declaration text, then one ordered semicolon-delimited list of `name —
/// description` entries. It never uses schemas or execution configuration.
fn synthetic_description(description: &str, catalog: &[CatalogEntry]) -> String {
    if catalog.is_empty() {
        return description.to_owned();
    }
    let entries = catalog
        .iter()
        .map(|entry| {
            entry.description().map_or_else(
                || entry.display_name().to_owned(),
                |description| {
                    let description = description.trim();
                    if description.is_empty() {
                        entry.display_name().to_owned()
                    } else {
                        format!("{} — {description}", entry.display_name())
                    }
                },
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let noun = if catalog.len() == 1 { "entry" } else { "entries" };
    format!(
        "{}. Available catalog {noun}: {entries}.",
        description.trim().trim_end_matches('.')
    )
}

/// The `paths`/`query` arguments the gateway accepts for a hosted search.
fn hosted_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "paths": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Names of the catalog entries to load."
            },
            "query": {
                "type": "string",
                "description": "Optional part of a tool name or description to load only matching tools. Omit it to load every tool in the paths."
            }
        },
        "required": ["paths"],
        "additionalProperties": false
    })
}

/// Rejects a deferred catalog the declared search mode cannot search.
fn validate_search_mode(
    public_tools: &[ResponsesTool],
    mcp_loads: &McpLoads,
    declaration: Option<&ToolSearchToolParam>,
) -> Result<(), ToolError> {
    let Some(declaration) = declaration else {
        return Ok(());
    };
    if !declaration.is_hosted() {
        if !mcp_loads.servers.is_empty() {
            return Err(ToolError::Config(
                "deferred MCP servers require hosted tool_search; omit execution or set it to \"server\"".to_owned(),
            ));
        }
        return Ok(());
    }
    if !public_tools.iter().any(validation::tool_has_deferred_definition) {
        return Err(ToolError::Config(
            "Invalid Value: 'tools.tool_search'. tools.tool_search requires at least one deferred tool.".to_owned(),
        ));
    }
    Ok(())
}

fn synthetic_tool_search(declaration: &ToolSearchToolParam, catalog: &[CatalogEntry]) -> ToolSearchToolParam {
    let mut normalized = if declaration.is_hosted() {
        let description = declaration
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .unwrap_or(HOSTED_DESCRIPTION);
        ToolSearchToolParam {
            execution: ToolSearchExecution::Server,
            description: Some(description.to_owned()),
            parameters: Some(hosted_parameters()),
            unsupported_async: None,
        }
    } else {
        ToolSearchHandler::normalized_param(declaration)
    };
    let description = normalized.description.as_deref().unwrap_or_default();
    normalized.description = Some(synthetic_description(description, catalog));
    normalized
}

/// The tools offered upstream: loaded and eager definitions without `defer_loading`, the synthetic search
/// declaration, and deferred MCP servers with the names loaded from them.
pub(crate) fn offered_tools(
    catalog: &ToolCatalog,
    synthetic_tool_search: Option<&ToolSearchToolParam>,
) -> Vec<ResponsesTool> {
    let mcp_loads = &catalog.mcp_loads;
    catalog
        .public_tools
        .iter()
        .filter_map(|tool| match tool {
            ResponsesTool::ToolSearch(_) => synthetic_tool_search.cloned().map(ResponsesTool::ToolSearch),
            ResponsesTool::Function(_) | ResponsesTool::Namespace(_) => private_definition(tool, catalog),
            // A deferred server stays declared for discovery and execution;
            // upstream normalization exposes only its loaded tools.
            ResponsesTool::Mcp(server) if mcp_loads.is_deferred(&server.server_label) => {
                let mut server = server.clone();
                server.loaded_tool_names = Some(mcp_loads.loaded_names(&server.server_label));
                Some(ResponsesTool::Mcp(server))
            }
            ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Shell(_)
            | ResponsesTool::Custom(_)
            | ResponsesTool::Unknown => Some(tool.clone()),
        })
        .collect()
}

fn available_public_tools(public_tools: &[ResponsesTool], loaded_tools: &[ResponsesTool]) -> Vec<ResponsesTool> {
    let mut loaded_functions = HashSet::new();
    let mut loaded_namespace_members = HashMap::<&str, HashSet<&str>>::new();
    for tool in loaded_tools {
        match tool {
            ResponsesTool::Function(function) => {
                loaded_functions.insert(function.name.as_str());
            }
            ResponsesTool::Namespace(namespace) => {
                let members = loaded_namespace_members.entry(namespace.name.as_str()).or_default();
                members.extend(namespace.tools.iter().filter_map(|member| match member {
                    CodexNamespaceMember::Function(function) => Some(function.name.as_str()),
                    CodexNamespaceMember::Unknown => None,
                }));
            }
            ResponsesTool::ToolSearch(_)
            | ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Shell(_)
            | ResponsesTool::Custom(_)
            | ResponsesTool::Unknown => {}
        }
    }

    public_tools
        .iter()
        .filter_map(|tool| match tool {
            ResponsesTool::Function(function) => {
                let loaded = loaded_functions.contains(function.name.as_str());
                if function.defer_loading == Some(true) && !loaded {
                    return None;
                }
                let mut available = function.clone();
                if loaded {
                    available.defer_loading = None;
                }
                Some(ResponsesTool::Function(available))
            }
            ResponsesTool::Namespace(namespace) => {
                let loaded_members = loaded_namespace_members.get(namespace.name.as_str());
                let mut available = namespace.clone();
                available.tools = available
                    .tools
                    .into_iter()
                    .filter_map(|member| match member {
                        CodexNamespaceMember::Function(mut function) => {
                            let loaded = loaded_members.is_some_and(|members| members.contains(function.name.as_str()));
                            if function.defer_loading == Some(true) && !loaded {
                                return None;
                            }
                            if loaded {
                                function.defer_loading = None;
                            }
                            Some(CodexNamespaceMember::Function(function))
                        }
                        CodexNamespaceMember::Unknown => None,
                    })
                    .collect();
                (!available.tools.is_empty()).then_some(ResponsesTool::Namespace(available))
            }
            ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Shell(_)
            | ResponsesTool::Custom(_)
            | ResponsesTool::Unknown => Some(tool.clone()),
            ResponsesTool::ToolSearch(_) => None,
        })
        .collect()
}

fn private_definition(tool: &ResponsesTool, catalog: &ToolCatalog) -> Option<ResponsesTool> {
    let identity = tool_identity(tool).ok().flatten()?;
    let record = catalog.definition(identity.name())?;
    let loaded = record.loaded;
    match tool {
        ResponsesTool::Function(function) if loaded || function.defer_loading != Some(true) => {
            let mut function = function.clone();
            function.defer_loading = None;
            Some(ResponsesTool::Function(function))
        }
        ResponsesTool::Namespace(namespace) => {
            private_namespace(namespace, record.namespace_members.as_ref()).map(ResponsesTool::Namespace)
        }
        ResponsesTool::Function(_)
        | ResponsesTool::Mcp(_)
        | ResponsesTool::ToolSearch(_)
        | ResponsesTool::WebSearch(_)
        | ResponsesTool::FileSearch(_)
        | ResponsesTool::CodeInterpreter(_)
        | ResponsesTool::Shell(_)
        | ResponsesTool::Custom(_)
        | ResponsesTool::Unknown => None,
    }
}

fn private_namespace(
    namespace: &CodexNamespaceToolParam,
    member_records: Option<&NamespaceMemberRecords>,
) -> Option<CodexNamespaceToolParam> {
    let member_records = member_records?;
    let tools = namespace
        .tools
        .iter()
        .filter_map(|member| match member {
            CodexNamespaceMember::Function(function)
                if member_records
                    .indexes
                    .get(function.name.as_str())
                    .is_some_and(|index| member_records.ordered[*index].loaded) =>
            {
                let mut function = function.clone();
                function.defer_loading = None;
                Some(CodexNamespaceMember::Function(function))
            }
            CodexNamespaceMember::Function(_) => None,
            CodexNamespaceMember::Unknown => Some(CodexNamespaceMember::Unknown),
        })
        .collect::<Vec<_>>();
    (!tools.is_empty()).then(|| CodexNamespaceToolParam {
        tools,
        ..namespace.clone()
    })
}

fn namespace_has_withheld_member(member_records: Option<&NamespaceMemberRecords>) -> bool {
    member_records.is_some_and(|members| members.unloaded_count != 0)
}

#[cfg(test)]
mod tests;
