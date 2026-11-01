//! MCP list-tools lifecycle state: which servers this request listed, and what history already holds.

use super::{ToolRegistry, ToolType};
use crate::types::io::output::McpListTools;
use crate::types::io::{InOutItem, InputItem, ResponsesInput};

impl ToolRegistry {
    #[must_use]
    pub fn contains_mcp_server_label(&self, server_label: &str) -> bool {
        self.entries
            .values()
            .any(|entry| entry.tool_type == ToolType::Mcp && entry.server_label.as_deref() == Some(server_label))
    }

    pub(crate) fn cache_listed_mcp_tools(&mut self, input: &ResponsesInput) {
        if let ResponsesInput::Items(items) = input {
            for item in items.iter().filter_map(InOutItem::as_input) {
                let InputItem::McpListTools(list_tools) = &*item else {
                    continue;
                };
                // Only a currently declared MCP server can emit discovery in
                // this request. Ignore history for labels absent from the
                // current registry instead of turning it into a new candidate.
                if let Some(items) = self.mcp_list_tools_items.get_mut(&list_tools.server_label) {
                    items.push(list_tools.clone());
                }
            }
        }
    }

    /// Current discovery items without an equivalent successful historical
    /// lifecycle. Failed or changed discovery remains visible to the client
    /// when the request continues (see [`Self::failed_mcp_list_tools`]).
    pub(crate) fn mcp_list_tool_items(&self) -> impl Iterator<Item = &McpListTools> {
        self.mcp_list_tools_items.values().filter_map(|items| {
            let (current, history) = items.split_first()?;
            let already_emitted = current.error.is_none()
                && history
                    .iter()
                    .any(|previous| previous.error.is_none() && previous.tools == current.tools);
            (!already_emitted).then_some(current)
        })
    }

    /// The first declared MCP server whose current tool listing failed and
    /// whose history holds no successful listing. The Responses API does not
    /// list a server again while its listing is in context, so a continuation
    /// keeps going when such a server later fails.
    #[must_use]
    pub(crate) fn failed_mcp_list_tools(&self) -> Option<&McpListTools> {
        self.mcp_list_tools_items.values().find_map(|items| {
            let (current, history) = items.split_first()?;
            (current.error.is_some() && history.iter().all(|previous| previous.error.is_some())).then_some(current)
        })
    }

    /// Marks the current request's discovery lifecycle as consumed.
    pub(crate) fn clear_mcp_list_tool_items(&mut self) {
        self.mcp_list_tools_items.clear();
    }
}
