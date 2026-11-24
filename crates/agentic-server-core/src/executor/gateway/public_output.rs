//! A round's public output items and the lifecycle plans of the items the gateway reports itself.

use super::{GatewayCallResult, GatewayEventPlan};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::mcp::handler::started_list_tools_output_item;
use crate::tool::{CustomHandler, ShellHandler, ToolRegistry};
use crate::types::io::OutputItem;

/// Supplies the public output that completes a gateway event plan.
pub(in crate::executor) trait GatewayPublicOutputSource {
    fn public_output(&self) -> Option<&OutputItem>;
}

impl GatewayPublicOutputSource for GatewayCallResult {
    fn public_output(&self) -> Option<&OutputItem> {
        self.public_output.as_ref()
    }
}

impl GatewayPublicOutputSource for OutputItem {
    fn public_output(&self) -> Option<&OutputItem> {
        Some(self)
    }
}

pub(in crate::executor) fn public_output_items(
    output_items: &[OutputItem],
    registry: &ToolRegistry,
    gateway_results: &[GatewayCallResult],
) -> ExecutorResult<Vec<OutputItem>> {
    output_items
        .iter()
        .enumerate()
        .filter(|(item_index, _)| {
            !gateway_results
                .iter()
                .any(|result| result.omitted && result.item_index == *item_index)
        })
        .map(|(item_index, item)| {
            Ok(match item {
                OutputItem::FunctionCall(call) if registry.is_client_custom_name(&call.name) => {
                    CustomHandler::output_item(call)
                }
                OutputItem::FunctionCall(call) if registry.is_client_shell_name(&call.name) => {
                    ShellHandler::output_item(call).ok_or_else(|| {
                        ExecutorError::ParseError("shell function call contains invalid action arguments".to_owned())
                    })?
                }
                OutputItem::FunctionCall(call) if registry.is_gateway_owned_name(&call.name) => {
                    let result = gateway_results.iter().find(|result| result.item_index == item_index);
                    let public = result
                        .and_then(|result| result.public_output.clone())
                        .unwrap_or_else(|| OutputItem::FunctionCall(call.clone()));
                    return Ok([Some(public), result.and_then(|result| result.follow_up_output.clone())]);
                }
                other => other.clone(),
            })
            .map(|item| [Some(item), None])
        })
        .collect::<ExecutorResult<Vec<_>>>()
        .map(|items| items.into_iter().flatten().flatten().collect())
}

pub(in crate::executor) fn mcp_list_tools_event_plans(
    public_output_items: &[OutputItem],
    output_offset: usize,
) -> Vec<GatewayEventPlan> {
    public_output_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let OutputItem::McpListTools(list_tools) = item else {
                return None;
            };
            Some(GatewayEventPlan {
                output_index: u32::try_from(output_offset.saturating_add(index)).unwrap_or(u32::MAX),
                started_output: Some(started_list_tools_output_item(list_tools)),
                completed_output: Some(item.clone()),
                follow_up_output: None,
                arguments: None,
            })
        })
        .collect()
}

pub(in crate::executor) fn compaction_event_plans(
    public_output_items: &[OutputItem],
    output_offset: usize,
) -> Vec<GatewayEventPlan> {
    public_output_items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, OutputItem::Compaction(_)))
        .map(|(index, item)| GatewayEventPlan {
            output_index: u32::try_from(output_offset.saturating_add(index)).unwrap_or(u32::MAX),
            started_output: Some(item.clone()),
            completed_output: Some(item.clone()),
            follow_up_output: None,
            arguments: None,
        })
        .collect()
}

#[cfg(test)]
pub(super) fn complete_gateway_event_plans<T: GatewayPublicOutputSource>(
    plans: &mut [GatewayEventPlan],
    completed: &[T],
) {
    for (plan, source) in plans.iter_mut().zip(completed) {
        plan.completed_output = source.public_output().cloned();
    }
}
