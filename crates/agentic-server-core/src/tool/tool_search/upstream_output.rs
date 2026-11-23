//! The tool-search rules an upstream response's output must meet before it is folded.

use std::collections::HashSet;

use serde_json::Value;

use super::{
    TOOL_SEARCH_NAME, invalid_upstream_search_call, invalid_upstream_withheld_function_call, native_call_id,
    strict_function_call, strict_native_call,
};
use crate::tool::handler::ToolError;
use crate::types::event::{MessageStatus, ResponseStatus};
use crate::types::io::ToolSearchCall;
use crate::types::io::output::FunctionToolCall;
use crate::types::tools::ToolSearchStatus;
use crate::utils::common::deserialize_from_str;

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
    let mut rules = SearchOutputRules::new(!matches!(status, ResponseStatus::Error | ResponseStatus::Incomplete));
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
                let call = rules.native_call(item)?;
                rules.ensure_finished(call.status == ToolSearchStatus::Completed)?;
            }
            Some("function_call")
                if tool_search_enabled && item.get("name").and_then(Value::as_str) == Some(TOOL_SEARCH_NAME) =>
            {
                let call = rules.synthetic_call(item)?;
                rules.ensure_finished(call.status == MessageStatus::Completed)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn might_contain_tool_search_wire(wire: &str) -> bool {
    wire.contains(TOOL_SEARCH_NAME)
}

/// The rules an upstream response's search calls meet on every transport: a strict shape, one
/// call per `call_id`, and, when the response finished, finished calls. Which function calls count
/// as searches is the caller's classification.
pub(crate) struct SearchOutputRules {
    call_ids: HashSet<String>,
    must_finish: bool,
}

impl SearchOutputRules {
    /// Rules for one response's output; `must_finish` when it completed rather than failed or
    /// stopped incomplete.
    pub(crate) fn new(must_finish: bool) -> Self {
        Self {
            call_ids: HashSet::new(),
            must_finish,
        }
    }

    /// An upstream `tool_search` function call, strictly parsed, whose `call_id` is not repeated.
    pub(crate) fn synthetic_call(&mut self, item: &Value) -> Result<FunctionToolCall, ToolError> {
        let call = strict_function_call(item)?;
        self.claim(&call.call_id)?;
        Ok(call)
    }

    /// An upstream-native `tool_search_call`, strictly parsed, whose `call_id` is not repeated.
    pub(crate) fn native_call(&mut self, item: &Value) -> Result<ToolSearchCall, ToolError> {
        let call = strict_native_call(item.clone())?;
        self.claim(native_call_id(&call)?)?;
        Ok(call)
    }

    /// Records a search call's id, rejecting one this output already holds.
    pub(crate) fn claim(&mut self, call_id: &str) -> Result<(), ToolError> {
        if self.call_ids.insert(call_id.to_owned()) {
            Ok(())
        } else {
            Err(invalid_upstream_search_call())
        }
    }

    /// Rejects an unfinished search call in a response that must have finished it.
    pub(crate) fn ensure_finished(&self, finished: bool) -> Result<(), ToolError> {
        if self.must_finish && !finished {
            return Err(invalid_upstream_search_call());
        }
        Ok(())
    }
}
