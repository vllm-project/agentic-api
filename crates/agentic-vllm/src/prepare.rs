//! Convert the existing model-visible request into shared-vLLM chat input.

mod messages;
#[cfg(test)]
mod tests;

use crate::Result;
use agentic_core::executor::error::ExecutorError;
use agentic_core::types::ServiceTier;
use agentic_core::types::io::{AllowedToolsMode, ToolChoice};
use agentic_core::types::request_response::{ResponseTextConfig, ResponseTextFormat, UpstreamRequest, UpstreamTool};
use http::StatusCode;
use num_traits::ToPrimitive;
use serde_json::Value;
use vllm_chat::{
    AllowedToolsMode as ChatAllowedToolsMode, ChatOptions, ChatRequest, ChatTool, ChatToolChoice, EffortValue,
    ResolvedToolContext, SamplingParams,
};
use vllm_engine_core_client::protocol::structured_outputs::{
    EngineCoreStructuredOutputsParams, StructuredOutputBackend, StructuredOutputConstraint, StructuredOutputsParams,
};
use vllm_text::TextDecodeOptions;

pub(crate) fn chat_request(
    request: UpstreamRequest<'_>,
    request_id: &str,
    model: &str,
    format_backend: StructuredOutputBackend,
) -> Result<ChatRequest> {
    if request.model != model {
        return Err(ExecutorError::InvalidRequest(format!("direct runtime serves model '{model}'")).into());
    }
    validate_options(&request)?;
    let structured_outputs = structured_outputs(request.text, format_backend)?;
    let messages = messages::convert(request.input.into_owned(), request.instructions)?;
    let tools = request
        .tools
        .unwrap_or_default()
        .into_iter()
        .map(|tool| {
            let UpstreamTool::Function(tool) = tool;
            ChatTool {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters.unwrap_or(Value::Null),
                strict: tool.strict,
                defer_loading: None,
            }
        })
        .collect();
    let choice = request.tool_choice.as_ref().map(tool_choice).transpose()?;
    let tool_context = ResolvedToolContext::new(&messages, tools, choice, request.parallel_tool_calls.unwrap_or(false))
        .map_err(|error| ExecutorError::InvalidRequest(error.to_string()))?;
    Ok(ChatRequest {
        request_id: request_id.to_owned(),
        messages,
        sampling_params: SamplingParams {
            temperature: sampling_float(request.temperature, "temperature")?,
            top_p: sampling_float(request.top_p, "top_p")?,
            max_tokens: request.max_output_tokens,
            ignore_eos: request.ignore_eos.unwrap_or(false),
            structured_outputs,
            ..SamplingParams::default()
        },
        chat_options: ChatOptions {
            reasoning_effort: request
                .reasoning
                .and_then(|reasoning| reasoning.effort)
                .map(|effort| EffortValue::String(effort.as_str().to_owned())),
            ..ChatOptions::default()
        },
        tool_context,
        decode_options: TextDecodeOptions::default(),
        intermediate: true,
        prompt_truncation: None,
        priority: 0,
        documents: None,
        cache_salt: request.cache_salt.map(str::to_owned),
        add_special_tokens: false,
        data_parallel_rank: None,
        session_id: None,
        lora_request: None,
        return_prompt_text: false,
    })
}

fn sampling_float(value: Option<f64>, field: &str) -> Result<Option<f32>> {
    value
        .map(|value| {
            value.to_f32().filter(|value| value.is_finite()).ok_or_else(|| {
                ExecutorError::InvalidRequest(format!("{field} is outside the direct runtime's numeric range")).into()
            })
        })
        .transpose()
}

fn validate_options(request: &UpstreamRequest<'_>) -> Result<()> {
    if request.top_logprobs.as_ref().is_some() {
        return Err(unsupported("top_logprobs").into());
    }
    if request.prompt_cache_options.is_some() {
        return Err(unsupported("prompt_cache_options").into());
    }
    if request
        .service_tier
        .is_some_and(|tier| !matches!(tier, ServiceTier::Auto | ServiceTier::Default))
    {
        return Err(unsupported("service_tier").into());
    }
    if !request.extra.is_empty() {
        return Err(unsupported("upstream extension fields").into());
    }

    if request.include.is_some_and(|include| !include.is_empty()) {
        return Err(unsupported("include").into());
    }
    if request.truncation.is_some_and(|truncation| truncation != "disabled") {
        return Err(unsupported("truncation").into());
    }
    if let Some(reasoning) = request.reasoning
        && (reasoning.context.is_some()
            || reasoning.mode.is_some()
            || reasoning.summary.is_some()
            || reasoning.generate_summary.is_some())
    {
        return Err(unsupported("reasoning controls other than effort").into());
    }
    Ok(())
}

fn structured_outputs(
    text: Option<&ResponseTextConfig>,
    backend: StructuredOutputBackend,
) -> Result<Option<StructuredOutputsParams>> {
    let Some(text) = text else { return Ok(None) };
    if text.verbosity.is_some() || !text.extra.is_empty() {
        return Err(unsupported("text extensions or verbosity").into());
    }
    let params = match &text.format {
        None => return Ok(None),
        Some(ResponseTextFormat::Text { extra }) if extra.is_empty() => return Ok(None),
        Some(ResponseTextFormat::JsonObject { extra }) if extra.is_empty() => StructuredOutputsParams::json_object(),
        Some(ResponseTextFormat::JsonSchema { schema, extra, .. }) if extra.is_empty() => {
            StructuredOutputsParams::json(Value::Object(schema.clone()))
        }
        Some(_) => return Err(unsupported("text format or format extensions").into()),
    };
    // Validate the user's constraint before a tool or reasoning parser can wrap it in another grammar.
    let params = EngineCoreStructuredOutputsParams::new(params, backend)
        .map_err(invalid_text_format)?
        .params;
    if let StructuredOutputConstraint::Json(schema) = &params.constraint {
        jsonschema::meta::try_validate(schema)
            .map_err(invalid_text_format)?
            .map_err(|error| invalid_text_format(error.to_owned()))?;
    }
    Ok(Some(params))
}

fn invalid_text_format(error: impl std::error::Error + Send + Sync + 'static) -> ExecutorError {
    ExecutorError::DirectInference {
        status: StatusCode::BAD_REQUEST,
        param: Some("text.format"),
        source: Box::new(error),
    }
}

fn tool_choice(choice: &ToolChoice) -> Result<ChatToolChoice> {
    match choice.normalized_for_upstream() {
        ToolChoice::Auto => Ok(ChatToolChoice::Auto),
        ToolChoice::None => Ok(ChatToolChoice::None),
        ToolChoice::Required => Ok(ChatToolChoice::Required),
        ToolChoice::Function { namespace: None, name } => Ok(ChatToolChoice::Function {
            name: name.as_str().to_owned(),
        }),
        ToolChoice::AllowedTools { mode, tools } => {
            let names = tools
                .into_iter()
                .map(|tool| {
                    if tool.type_.as_str() != "function" {
                        return Err(unsupported("non-function allowed tool").into());
                    }
                    Ok(tool.name.as_str().to_owned())
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ChatToolChoice::AllowedTools {
                mode: match mode {
                    AllowedToolsMode::Auto => ChatAllowedToolsMode::Auto,
                    AllowedToolsMode::Required => ChatAllowedToolsMode::Required,
                },
                names,
            })
        }
        ToolChoice::Function { .. }
        | ToolChoice::Custom { .. }
        | ToolChoice::Shell
        | ToolChoice::FileSearch
        | ToolChoice::Mcp { .. } => Err(unsupported("unresolved tool choice").into()),
    }
}

fn unsupported(field: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(format!("direct inference does not support {field}"))
}
