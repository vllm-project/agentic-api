//! Feeding a round's output and gateway results back into the next request.

use crate::executor::request::RequestContext;
use crate::tool::ToolRegistry;
use crate::types::io::{InputItem, OutputItem, ResponsesInput};

fn append_input_item(input: &mut ResponsesInput, item: InputItem) {
    match input {
        ResponsesInput::Items(items) => items.push(item),
        ResponsesInput::Text(text) => {
            let text_input = ResponsesInput::Text(std::mem::take(text));
            let mut items = Vec::<InputItem>::from(&text_input);
            items.push(item);
            *input = ResponsesInput::Items(items);
        }
    }
}

pub(in crate::executor) fn append_output_items_to_input(input: &mut ResponsesInput, output_items: &[OutputItem]) {
    for input_item in output_items.iter().filter_map(OutputItem::to_input_item) {
        append_input_item(input, input_item);
    }
}

pub(in crate::executor) fn append_tool_outputs(ctx: &mut RequestContext, tool_outputs: Vec<InputItem>) {
    for output in tool_outputs {
        ctx.new_input_items.push(output.clone());
        append_input_item(&mut ctx.enriched_request.input, output);
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
        registry
            .is_gateway_owned_name(&call.name)
            .then(|| InputItem::FunctionCall(call.clone().into()))
    }));
}
