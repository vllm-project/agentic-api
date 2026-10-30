//! Replayed history: the definitions its completed search outputs load, in order, and its calls checked against
//! them.

use std::borrow::Cow;

use super::{PriorCalls, ToolCatalog, ToolIdentity, history_call_before_load};
use crate::tool::codex::is_searched_function_call;
use crate::tool::handler::ToolError;
use crate::types::io::input::{SearchHistory, SearchHistoryDefect};
use crate::types::io::{InOutItem, InputFunctionToolCall, InputItem, ResponsesInput};

impl ToolCatalog {
    /// Loads the definitions each completed search output in `input` returned, in history order, and checks each
    /// function call against them, failing at the first bad position.
    ///
    /// The pending-call ledger (`executor::pending_calls`) checks how client calls and outputs pair by `call_id`, and
    /// [`SearchHistory`] the rest of the search items (item ids, hosted pairs, output status); this applies its verdicts
    /// in order and adds the one timing rule: a call cannot use a definition before it is loaded.
    pub(crate) fn load_history(&mut self, input: &ResponsesInput) -> Result<(), ToolError> {
        let ResponsesInput::Items(items) = input else {
            return Ok(());
        };
        let items: Vec<Cow<'_, InputItem>> = items.iter().filter_map(InOutItem::as_input).collect();
        let history = SearchHistory::scan(&items);
        let mut prior = PriorCalls::default();
        for (position, item) in items.iter().enumerate() {
            if let Some(defect) = history.defect(position) {
                return Err(ToolError::Config(defect.to_string()));
            }
            match item.as_ref() {
                InputItem::ToolSearchOutput(output) => {
                    for tool in &output.tools {
                        self.load(tool, &prior)?;
                    }
                }
                InputItem::FunctionCall(call) => {
                    let call = if is_searched_function_call(call, |name| self.is_function(name)) {
                        Cow::Owned(InputFunctionToolCall {
                            namespace: None,
                            ..call.clone()
                        })
                    } else {
                        Cow::Borrowed(call)
                    };
                    ensure_history_call_is_available(&call, self, &mut prior, position)?;
                }
                _ => {}
            }
        }
        if history.has_open_hosted_call() {
            return Err(ToolError::Config(SearchHistoryDefect::OpenHostedCall.to_string()));
        }
        Ok(())
    }
}

fn ensure_history_call_is_available(
    call: &InputFunctionToolCall,
    catalog: &ToolCatalog,
    prior: &mut PriorCalls,
    position: usize,
) -> Result<(), ToolError> {
    if let Some(namespace) = call.namespace.as_deref() {
        let member = catalog
            .definition(namespace)
            .filter(|record| matches!(record.identity, ToolIdentity::Namespace(_)))
            .and_then(|record| record.namespace_members.as_ref())
            .and_then(|members| members.indexes.get(&call.name).map(|index| &members.ordered[*index]));
        match member {
            Some(member) if !member.loaded => return Err(history_call_before_load()),
            Some(_) => {}
            None => {
                prior
                    .unknown_namespace_calls
                    .entry(namespace.to_owned())
                    .or_default()
                    .insert(call.name.clone());
            }
        }
    } else {
        if catalog.withheld_function_names.contains(&call.name) {
            return Err(history_call_before_load());
        }
        prior
            .unqualified_call_positions
            .entry(call.name.clone())
            .or_insert(position);
    }
    Ok(())
}
