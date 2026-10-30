//! Tool search in the registry: one kind, owned where its declaration runs it, and the request's tool catalog, which
//! the registry owns for the response. The registry offers the model the catalog's loaded definitions, withholds the
//! deferred ones, runs a hosted search over it, and loads what a search returns.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;

use super::super::catalog::{PriorCalls, ToolCatalog};
use super::super::declaration::{ToolDeclaration, responses_declarations};
use super::super::executors::GatewayExecutors;
use super::super::handler::{GatewayExecutor, GatewayToolEventPlan, PlannedResult, ToolOutput};
use super::super::ownership::{GatewayBinding, ToolOwnership};
use super::super::tool_search::{TOOL_SEARCH_NAME, ToolSearchHandler, offered_tools};
use super::super::{CodexNamespaceHandler, ToolError, model_visible_namespace_member_name};
use super::{CustomHandler, ToolEntry, ToolRegistry, ToolType};
use crate::types::io::OutputItem;
use crate::types::io::input::hosted_search_output_text;
use crate::types::io::output::FunctionToolCall;
use crate::types::io::{ToolSearchCall, ToolSearchOutputMessage};
use crate::types::request_response::RequestPayload;
use crate::types::tools::ResponsesTool;
use crate::types::tools::{ToolSearchStatus, ToolSearchToolParam};

/// The request's tool catalog as the registry owns it, with the synthetic search declaration the offered tools carry.
#[derive(Clone)]
pub(crate) struct RegistryToolSearch {
    catalog: Arc<ToolCatalog>,
    synthetic: Option<ToolSearchToolParam>,
    hosted: bool,
    /// Model-visible names of deferred MCP tools no search has loaded; their discovered entries stay unavailable.
    deferred_mcp: HashSet<String>,
}

impl fmt::Debug for RegistryToolSearch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryToolSearch")
            .field("definition_count", &self.catalog.definitions.len())
            .field("has_synthetic", &self.synthetic.is_some())
            .field("hosted", &self.hosted)
            .field("deferred_mcp_count", &self.deferred_mcp.len())
            .finish()
    }
}

/// The public snapshot persistence keeps after a load: the effective declarations and the loaded definitions.
pub(crate) type LoadedSnapshot = (Vec<ResponsesTool>, Vec<ResponsesTool>);

impl ToolRegistry {
    /// Takes ownership of the request's catalog: the one preparation built, or the one the registry this replaces
    /// held. A hosted search records each deferred MCP server's discovered tools in it and is bound to it.
    pub(crate) fn attach_tool_search(
        &mut self,
        mut catalog: ToolCatalog,
        synthetic: Option<ToolSearchToolParam>,
        hosted: bool,
        private_tools: &[ResponsesTool],
    ) {
        if hosted {
            catalog.discover_mcp(private_tools);
        }
        let deferred_mcp = catalog.withheld_mcp_tool_names(private_tools);
        let catalog = Arc::new(catalog);
        if hosted {
            self.bind_hosted_tool_search(Arc::clone(&catalog));
        }
        self.tool_search = Some(RegistryToolSearch {
            catalog,
            synthetic,
            hosted,
            deferred_mcp,
        });
    }

    /// The request's tool catalog, when tool search is active.
    pub(crate) fn tool_catalog(&self) -> Option<&ToolCatalog> {
        self.tool_search.as_ref().map(|search| &*search.catalog)
    }

    /// Checks the registry against the catalog: no entry takes a withheld name, and a prepared search is present.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] as [`Self::validate_tool_availability`] does.
    pub(crate) fn validate_tool_search_availability(&self) -> Result<(), ToolError> {
        let Some(search) = &self.tool_search else {
            return Ok(());
        };
        self.validate_tool_availability(&search.catalog.withheld_function_names, search.synthetic.is_some())?;
        if search.synthetic.is_some() && self.is_hosted_tool_search_name(TOOL_SEARCH_NAME) != search.hosted {
            return Err(ToolError::Config(
                "prepared tool-search declaration has invalid private registry ownership".to_owned(),
            ));
        }
        Ok(())
    }

    /// Model-visible names the model may not call yet: deferred functions, namespace members and MCP tools no search
    /// has loaded.
    pub(crate) fn withheld_names(&self) -> HashSet<String> {
        let Some(search) = &self.tool_search else {
            return HashSet::new();
        };
        let mut withheld = search.catalog.withheld_function_names.clone();
        withheld.extend(search.deferred_mcp.iter().cloned());
        withheld
    }

    /// Whether `name` is a deferred definition no search has loaded: the registry knows it, but nothing may call it.
    pub(crate) fn is_deferred(&self, name: &str) -> bool {
        self.tool_search.as_ref().is_some_and(|search| {
            search.catalog.withheld_function_names.contains(name) || search.deferred_mcp.contains(name)
        })
    }

    /// Refuses a round whose output calls a deferred definition. Ingestion refuses such a call first; this keeps the
    /// registry from running one if a call ever reaches it.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::UpstreamWithheldFunctionCall`], as ingestion does.
    pub(crate) fn ensure_loaded_calls(&self, output: &[OutputItem]) -> Result<(), ToolError> {
        // Calls are restored to their public shape by now: a namespace member's `{namespace, name}` has a withheld
        // flat name, and a searched top-level function's call names itself as namespace.
        let deferred = |call: &FunctionToolCall| {
            self.is_deferred(&call.name)
                || call.namespace.as_deref().is_some_and(|namespace| {
                    self.is_deferred(&model_visible_namespace_member_name(namespace, &call.name))
                })
        };
        if output
            .iter()
            .any(|item| matches!(item, OutputItem::FunctionCall(call) if deferred(call)))
        {
            return Err(ToolError::UpstreamWithheldFunctionCall);
        }
        Ok(())
    }

    /// Deferred top-level functions of a hosted search; their calls report their own name as namespace.
    pub(crate) fn hosted_deferred_functions(&self) -> HashSet<String> {
        self.tool_search
            .as_ref()
            .filter(|search| search.hosted)
            .map(|search| search.catalog.deferred_functions())
            .unwrap_or_default()
    }

    /// Loads the tools this round's hosted searches returned into the catalog, offers the next round the definitions
    /// now loaded (keeping MCP discovery), adds client entries for them, and returns the public snapshot to persist.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a returned definition conflicts with the catalog or a loaded name collides.
    pub(crate) async fn load_searched_tools(
        &mut self,
        loaded: &[ResponsesTool],
        request: &mut RequestPayload,
    ) -> Result<Option<LoadedSnapshot>, ToolError> {
        let Some(search) = self.tool_search.as_mut() else {
            return Ok(None);
        };
        let catalog = Arc::make_mut(&mut search.catalog);
        // A load in this response follows the calls already made in it, so none of them is a call before load.
        for tool in loaded {
            catalog.load(tool, &PriorCalls::default())?;
        }
        let mut offered = offered_tools(catalog, search.synthetic.as_ref());
        for tool in &mut offered {
            let ResponsesTool::Mcp(server) = tool else {
                continue;
            };
            if let Some(discovered) = request
                .tools
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find_map(|previous| match previous {
                    ResponsesTool::Mcp(previous) if previous.server_label == server.server_label => {
                        Some(&previous.discovered_tools)
                    }
                    _ => None,
                })
            {
                server.discovered_tools.clone_from(discovered);
            }
        }
        let snapshot = (catalog.public_tools.clone(), catalog.loaded_public_tools.clone());
        search.deferred_mcp = catalog.withheld_mcp_tool_names(&offered);
        request.tools = Some(offered);
        if search.hosted {
            let catalog = Arc::clone(&search.catalog);
            self.bind_hosted_tool_search(catalog);
        }
        self.refresh_client_tools(request.tools.as_deref().unwrap_or_default())
            .await?;
        Ok(Some(snapshot))
    }

    /// Adds client entries for tools a hosted search loaded mid-response, and
    /// rebuilds the namespace and custom maps that restore their public shape.
    /// Gateway entries, including discovered MCP tools, are kept as built, so
    /// no MCP server is listed again.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a loaded name collides with another tool.
    pub(crate) async fn refresh_client_tools(&mut self, tools: &[ResponsesTool]) -> Result<(), ToolError> {
        let declarations = responses_declarations(tools);
        let mut client_declarations: Vec<_> = declarations
            .iter()
            .filter(|declaration| {
                matches!(
                    declaration,
                    ToolDeclaration::Function(_) | ToolDeclaration::Namespace(_) | ToolDeclaration::Custom(_)
                )
            })
            .cloned()
            .collect();
        let client = Self::build_with_handlers(&mut client_declarations, &mut GatewayExecutors::default()).await?;
        for (name, entry) in client.entries {
            self.entries.entry(name).or_insert(entry);
        }
        self.namespace_map =
            CodexNamespaceHandler.build_namespace_map((!declarations.is_empty()).then_some(&declarations))?;
        self.custom_tool_map = CustomHandler::build_tool_map(&declarations);
        Ok(())
    }

    /// Binds the hosted `tool_search` entry to this response's catalog.
    pub(crate) fn bind_hosted_tool_search(&mut self, catalog: Arc<ToolCatalog>) {
        if self.is_hosted_tool_search_name(TOOL_SEARCH_NAME) {
            self.entries
                .insert(TOOL_SEARCH_NAME.to_owned(), hosted_tool_search_entry(catalog));
        }
    }

    /// Whether `name` is the hosted `tool_search` the gateway runs. Its model
    /// call is replaced by the call result's own model input and history.
    #[must_use]
    pub fn is_hosted_tool_search_name(&self, name: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|entry| entry.tool_type == ToolType::ToolSearch && entry.ownership.is_gateway())
    }

    /// Whether an upstream output item is recorded as the model saw it. A
    /// hosted search call is recorded through its call result instead.
    #[must_use]
    pub(crate) fn records_output_item(&self, item: &OutputItem) -> bool {
        !matches!(item, OutputItem::FunctionCall(call) if self.is_hosted_tool_search_name(&call.name))
    }
}

/// A hosted search is bound to an empty catalog here; the agent turn binds the
/// response's catalog once MCP discovery has run.
pub(crate) fn insert_tool_search_entry(entries: &mut HashMap<String, ToolEntry>, param: &ToolSearchToolParam) {
    let entry = if param.is_hosted() {
        hosted_tool_search_entry(Arc::new(ToolCatalog::default()))
    } else {
        tool_search_entry(ToolOwnership::Client)
    };
    if entries.insert(TOOL_SEARCH_NAME.to_owned(), entry).is_some() {
        tracing::warn!(
            name = TOOL_SEARCH_NAME,
            "duplicate tool name — previous definition overwritten"
        );
    }
}

/// The gateway runs a hosted search with [`ToolSearchHandler`] over this response's catalog.
pub(crate) fn hosted_tool_search_entry(catalog: Arc<ToolCatalog>) -> ToolEntry {
    tool_search_entry(ToolOwnership::Gateway(Some(GatewayBinding::new(
        Arc::new(ToolSearchHandler),
        catalog,
    ))))
}

/// The one tool-search kind; `ownership` comes from the declaration's `execution`.
fn tool_search_entry(ownership: ToolOwnership) -> ToolEntry {
    ToolEntry {
        tool_type: ToolType::ToolSearch,
        server_label: None,
        ownership,
        async_execution: false,
    }
}

/// A hosted search runs in the gateway: a pure lookup over the response's catalog, so the public
/// `tool_search_output` is already known when the call is planned. Validation and lowering are the handler's own.
impl GatewayExecutor for ToolSearchHandler {
    type ExecutionParams = Arc<ToolCatalog>;

    /// The planned result answers a hosted search (see [`Self::plan_gateway_events`]); this runs the same search for
    /// the trait.
    fn execute(
        &self,
        call_id: &str,
        _tool_name: &str,
        arguments: &str,
        params: &Self::ExecutionParams,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let output = hosted_search_output_text(&params.search_raw(arguments));
        Box::pin(ready(Ok(ToolOutput::success(call_id, output))))
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }

    /// The search is pure, so its result is known now: the gateway records the public pair without executing the
    /// call, and the upstream projection shows the model its `tool_search` call and output.
    fn plan_gateway_events(&self, call: &FunctionToolCall, params: &Self::ExecutionParams) -> GatewayToolEventPlan {
        let output = ToolSearchOutputMessage::hosted(call, params.search_raw(&call.arguments));
        GatewayToolEventPlan::new(Some(OutputItem::ToolSearchCall(ToolSearchCall::hosted(
            call,
            ToolSearchStatus::InProgress,
        ))))
        .with_planned_result(PlannedResult::search_pair(
            ToolSearchCall::hosted(call, ToolSearchStatus::Completed),
            output,
        ))
    }
}
