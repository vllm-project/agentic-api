//! Registry binding for the gateway-owned file search executor.

use std::collections::HashMap;

use crate::types::tools::FileSearchToolParam;

use super::{GatewayExecutors, ToolEntry, ToolError, insert_unique_tool_entries};
use crate::tool::file_search::handler::insert_file_search_entry;

pub(super) fn insert_file_search_binding(
    entries: &mut HashMap<String, ToolEntry>,
    executors: &GatewayExecutors,
    param: &FileSearchToolParam,
) -> Result<(), ToolError> {
    let handler = executors.file_search_handler();
    let include = executors.include_file_search_results;
    insert_unique_tool_entries(entries, |resolved| {
        insert_file_search_entry(resolved, param, handler, include);
    })
}
