use super::{TOOL_SEARCH_NAME, ToolSearchHandler};
use crate::tool::ToolError;
use crate::types::io::{InputItem, ResponsesInput};
use crate::types::request_response::RequestPayload;
use crate::types::tools::{CodexNamespaceMember, ResponsesTool};

pub(super) fn validate_tool_search_request(
    request: &RequestPayload,
    input: &ResponsesInput,
) -> Result<bool, ToolError> {
    if !request_contains_tool_search_state(request, input) {
        return Ok(false);
    }

    let tools = request.tools.as_deref().unwrap_or_default();
    if tools
        .iter()
        .filter(|tool| matches!(tool, ResponsesTool::ToolSearch(_)))
        .count()
        > 1
    {
        return Err(ToolError::Config(
            "tool search accepts at most one tool_search declaration".to_owned(),
        ));
    }
    if request.parallel_tool_calls == Some(true) {
        return Err(ToolError::Config(
            "parallel_tool_calls must be false when tool search is active".to_owned(),
        ));
    }

    for tool in tools {
        tool.validate()?;
        if has_reserved_tool_search_name(tool) {
            return Err(ToolError::Config(
                "model-visible tool name 'tool_search' is reserved while tool search is active".to_owned(),
            ));
        }
    }

    Ok(true)
}

pub(super) fn request_contains_tool_search_state<T: ?Sized>(
    request: &RequestPayload<T>,
    input: &ResponsesInput,
) -> bool {
    input_contains_tool_search_state(input)
        || request
            .tools
            .as_deref()
            .is_some_and(|tools| tools.iter().any(tool_activates_tool_search))
}

pub(super) fn input_contains_tool_search_state(input: &ResponsesInput) -> bool {
    matches!(
        input,
        ResponsesInput::Items(items)
            if items
                .iter()
                .any(|item| matches!(item, InputItem::ToolSearchCall(_) | InputItem::ToolSearchOutput(_)))
    )
}

pub(super) fn tool_activates_tool_search(tool: &ResponsesTool) -> bool {
    matches!(tool, ResponsesTool::ToolSearch(_)) || tool_has_deferred_definition(tool)
}

pub(super) fn tool_has_deferred_definition(tool: &ResponsesTool) -> bool {
    match tool {
        ResponsesTool::Function(function) => function.defer_loading == Some(true),
        ResponsesTool::Namespace(namespace) => namespace.tools.iter().any(
            |member| matches!(member, CodexNamespaceMember::Function(function) if function.defer_loading == Some(true)),
        ),
        ResponsesTool::Mcp(mcp) => mcp.defer_loading == Some(true),
        ResponsesTool::Custom(custom) => custom.defer_loading == Some(true),
        ResponsesTool::ToolSearch(_)
        | ResponsesTool::WebSearch(_)
        | ResponsesTool::FileSearch(_)
        | ResponsesTool::CodeInterpreter(_)
        | ResponsesTool::Shell(_)
        | ResponsesTool::Unknown => false,
    }
}

pub(crate) fn ensure_request_prepared(request: &RequestPayload, prepared: bool) -> Result<(), ToolError> {
    if ToolSearchHandler::request_has_state(request) && !prepared {
        return Err(ToolError::Config(
            "tool_search requests require prepared request-scoped state before upstream conversion".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn has_reserved_tool_search_name(tool: &ResponsesTool) -> bool {
    match tool {
        ResponsesTool::Function(function) => function.name.as_str() == TOOL_SEARCH_NAME,
        ResponsesTool::Custom(custom) => custom.name.as_str() == TOOL_SEARCH_NAME,
        ResponsesTool::Namespace(namespace) => namespace.name == TOOL_SEARCH_NAME,
        ResponsesTool::ToolSearch(_)
        | ResponsesTool::Mcp(_)
        | ResponsesTool::WebSearch(_)
        | ResponsesTool::FileSearch(_)
        | ResponsesTool::CodeInterpreter(_)
        | ResponsesTool::Shell(_)
        | ResponsesTool::Unknown => false,
    }
}
