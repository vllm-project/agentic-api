//! Registry entries for hosted tool search: its per-response catalog binding and the tools a search loads.

use super::super::declaration::{ToolDeclaration, responses_declarations};
use super::super::executors::GatewayExecutors;
use super::super::tool_search::{HostedCatalog, TOOL_SEARCH_NAME, hosted_tool_search_entry};
use super::super::{CodexNamespaceHandler, ToolError};
use super::{CustomHandler, ToolRegistry, ToolType};
use crate::types::io::OutputItem;
use crate::types::tools::ResponsesTool;

impl ToolRegistry {
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
    pub(crate) fn bind_hosted_tool_search(&mut self, catalog: HostedCatalog) {
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
            .is_some_and(|entry| entry.tool_type == ToolType::HostedToolSearch)
    }

    /// Whether an upstream output item is recorded as the model saw it. A
    /// hosted search call is recorded through its call result instead.
    #[must_use]
    pub(crate) fn records_output_item(&self, item: &OutputItem) -> bool {
        !matches!(item, OutputItem::FunctionCall(call) if self.is_hosted_tool_search_name(&call.name))
    }
}
