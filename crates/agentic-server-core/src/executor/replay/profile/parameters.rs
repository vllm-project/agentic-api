//! Request-surface validation for the exact pinned opaque profile.
//!
//! Only preflight calls this. It never normalizes or mutates public input, and
//! must run before rehydration or gateway tool discovery can do external work.

use crate::types::io::{InputContent, InputItem, InputMessageContent, ResponsesInput, ToolCallOutput, ToolChoice};
use crate::types::reasoning_profile::{
    MAX_OPAQUE_CONTENT_PARTS, MAX_OPAQUE_INPUT_ITEMS, MAX_OPAQUE_REASONING_SUMMARIES, MAX_OPAQUE_TOOLS,
    OpaqueReasoningProfile, OpaqueReplayRequestField as Field,
};
use crate::types::reasoning_replay::ReasoningReplayError;
use crate::types::request_response::RequestPayload;
use crate::types::tools::ResponsesTool;

fn unsupported(field: Field) -> ReasoningReplayError {
    ReasoningReplayError::UnsupportedParameter(field)
}

pub(in crate::executor::replay) fn validate(
    profile: OpaqueReasoningProfile,
    request: &RequestPayload,
) -> Result<(), ReasoningReplayError> {
    match profile {
        OpaqueReasoningProfile::OpenAiGpt54_20260305V1 => validate_gpt54(request),
    }
}

fn validate_gpt54(request: &RequestPayload) -> Result<(), ReasoningReplayError> {
    cover_request_fields(request);
    if !supported_input(&request.input) {
        return Err(unsupported(Field::Input));
    }
    if let Some(reasoning) = request.reasoning.as_deref() {
        if reasoning.context.is_some() {
            return Err(unsupported(Field::ReasoningContext));
        }
        if reasoning.effort.as_deref().is_some_and(|effort| effort != "low") {
            return Err(unsupported(Field::ReasoningEffort));
        }
        if reasoning.generate_summary.is_some() {
            return Err(unsupported(Field::ReasoningGenerateSummary));
        }
        if reasoning.mode.is_some() {
            return Err(unsupported(Field::ReasoningMode));
        }
        if reasoning.summary.as_deref().is_some_and(|summary| summary != "concise") {
            return Err(unsupported(Field::ReasoningSummary));
        }
    }
    validate_gpt54_optional_fields(request)
}

fn cover_request_fields(request: &RequestPayload) {
    // An added typed request field must be explicitly reviewed for this closed profile.
    let RequestPayload {
        model: _,
        input: _,
        instructions: _,
        previous_response_id: _,
        conversation_id: _,
        tools: _,
        tool_choice: _,
        stream: _,
        store: _,
        include: _,
        reasoning: _,
        text: _,
        temperature: _,
        top_p: _,
        max_output_tokens: _,
        ignore_eos: _,
        truncation: _,
        metadata: _,
        parallel_tool_calls: _,
        prompt_cache_key: _,
        cache_salt: _,
        context_management: _,
    } = request;
}

fn validate_gpt54_optional_fields(request: &RequestPayload) -> Result<(), ReasoningReplayError> {
    if request
        .include
        .as_ref()
        .is_some_and(|include| include.iter().any(|item| item != "reasoning.encrypted_content"))
    {
        return Err(unsupported(Field::Include));
    }
    if request.text.is_some() {
        return Err(unsupported(Field::Text));
    }
    if request.temperature.is_some() {
        return Err(unsupported(Field::Temperature));
    }
    if request.top_p.is_some() {
        return Err(unsupported(Field::TopP));
    }
    if request
        .max_output_tokens
        .is_some_and(|tokens| !(1..=128_000).contains(&tokens))
    {
        return Err(unsupported(Field::MaxOutputTokens));
    }
    if request.ignore_eos.is_some() {
        return Err(unsupported(Field::IgnoreEos));
    }
    if request.truncation.as_deref().is_some_and(|mode| mode != "disabled") {
        return Err(unsupported(Field::Truncation));
    }
    if request.metadata.is_some() {
        return Err(unsupported(Field::Metadata));
    }
    if request.parallel_tool_calls == Some(true) {
        return Err(unsupported(Field::ParallelToolCalls));
    }
    if request.prompt_cache_key.is_some() {
        return Err(unsupported(Field::PromptCacheKey));
    }
    if request.cache_salt.is_some() {
        return Err(unsupported(Field::CacheSalt));
    }
    if request.tools.as_ref().is_some_and(|tools| {
        tools.len() > MAX_OPAQUE_TOOLS
            || tools.iter().any(|tool| match tool {
                ResponsesTool::Function(function) => function.defer_loading == Some(true) || !function.extra.is_empty(),
                ResponsesTool::Mcp(mcp) => {
                    mcp.defer_loading == Some(true) || mcp.require_approval.as_deref() != Some("never")
                }
                _ => true,
            })
    }) {
        return Err(unsupported(Field::Tools));
    }
    if request.tool_choice.as_ref().is_some_and(|choice| {
        !matches!(
            choice,
            ToolChoice::Auto | ToolChoice::None | ToolChoice::Required | ToolChoice::Function { namespace: None, .. }
        )
    }) {
        return Err(unsupported(Field::ToolChoice));
    }
    Ok(())
}

fn supported_input(input: &ResponsesInput) -> bool {
    let ResponsesInput::Items(items) = input else {
        return true;
    };
    items.len() <= MAX_OPAQUE_INPUT_ITEMS
        && items.iter().all(|item| match item {
            InputItem::Message(message) => match (message.role.as_str(), &message.content) {
                ("user" | "assistant", InputMessageContent::Text(_)) => true,
                ("user" | "assistant", InputMessageContent::Parts(parts)) => {
                    parts.len() <= MAX_OPAQUE_CONTENT_PARTS
                        && parts.iter().all(|part| match part {
                            InputContent::InputText(text) => message.role == "user" && text.extra.is_empty(),
                            InputContent::OutputText(text) => {
                                message.role == "assistant"
                                    && text.extra.iter().all(|(key, value)| {
                                        matches!(key.as_str(), "annotations" | "logprobs")
                                            && value.as_array().is_some_and(Vec::is_empty)
                                    })
                            }
                            InputContent::InputImage(_)
                            | InputContent::InputFile(_)
                            | InputContent::Refusal(_)
                            | InputContent::ReasoningText(_)
                            | InputContent::Unknown(_) => false,
                        })
                }
                _ => false,
            },
            InputItem::Reasoning(reasoning) => {
                reasoning.content.is_empty() && reasoning.summary.len() <= MAX_OPAQUE_REASONING_SUMMARIES
            }
            InputItem::FunctionCall(call) => call.namespace.is_none(),
            InputItem::FunctionCallOutput(output) => matches!(output.output, ToolCallOutput::Text(_)),
            // Gateway-retained MCP discovery metadata is filtered before projection.
            InputItem::McpListTools(_) => true,
            InputItem::ToolSearchCall(_)
            | InputItem::ToolSearchOutput(_)
            | InputItem::CustomToolCall(_)
            | InputItem::CustomToolCallOutput(_)
            | InputItem::ShellCall(_)
            | InputItem::ShellCallOutput(_)
            | InputItem::Compaction(_)
            | InputItem::CompactionTrigger
            | InputItem::Unknown => false,
        })
}
