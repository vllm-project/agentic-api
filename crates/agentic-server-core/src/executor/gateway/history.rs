use super::GatewayCallResult;
use crate::executor::request::RequestContext;
use crate::tool::ToolRegistry;
use crate::types::io::{InputItem, OutputItem, ResponsesInput};

pub(in crate::executor) fn append_output_items_to_input(input: &mut ResponsesInput, output_items: &[OutputItem]) {
    for input_item in output_items.iter().filter_map(OutputItem::to_input_item) {
        input.push(input_item);
    }
}

/// Records each gateway call's history and gives the model its input for the next round.
pub(in crate::executor) fn append_gateway_results(ctx: &mut RequestContext, results: Vec<GatewayCallResult>) {
    for result in results {
        ctx.new_input_items.extend(result.input.iter().cloned().map(Into::into));
        for item in result.input {
            ctx.enriched_request.input.push(item);
        }
    }
}

pub(in crate::executor) fn append_gateway_calls_to_new_input(
    ctx: &mut RequestContext,
    output_items: &[OutputItem],
    registry: &ToolRegistry,
) {
    ctx.new_input_items.extend(output_items.iter().filter_map(|item| {
        let OutputItem::FunctionCall(call) = item else {
            return None;
        };
        (registry.is_gateway_owned_name(&call.name) && registry.records_output_item(item))
            .then(|| InputItem::FunctionCall(call.clone().into()).into())
    }));
}
