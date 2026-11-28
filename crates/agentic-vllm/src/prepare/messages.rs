//! Adapt resolved history to the shared frontend's chat representation.

use crate::Result;
use agentic_core::types::io::input::{
    FunctionToolResultMessage, InputFunctionToolCall, ToolCallOutput, ToolOutputContent,
};
use agentic_core::types::io::{InputContent, InputItem, InputMessageContent, ResponsesInput};
use vllm_chat::{AssistantContentBlock, AssistantToolCall, ChatContent, ChatContentPart, ChatMessage};

use super::unsupported;

pub(super) fn convert(input: ResponsesInput, instructions: Option<&str>) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    if let Some(instructions) = instructions {
        messages.push(ChatMessage::system(instructions));
    }
    let mut assistant = Vec::new();
    for item in Vec::<InputItem>::from(input) {
        match &item {
            InputItem::Message(message) if message.role == "assistant" => match &message.content {
                InputMessageContent::Text(text) => assistant.push(AssistantContentBlock::Text { text: text.clone() }),
                InputMessageContent::Parts(parts) => {
                    for part in parts {
                        assistant.push(assistant_block(part)?);
                    }
                }
            },
            InputItem::Reasoning(reasoning) => {
                if reasoning.encrypted_content.is_some() || !reasoning.summary.is_empty() {
                    return Err(unsupported("opaque or summarized reasoning replay").into());
                }
                for content in &reasoning.content {
                    assistant.push(AssistantContentBlock::Reasoning {
                        text: content.text.clone(),
                    });
                }
            }
            InputItem::FunctionCall(call) => assistant.push(function_call(call)?),
            other => {
                flush_assistant(&mut messages, &mut assistant);
                messages.push(match other {
                    InputItem::Message(message) => {
                        let content = message_content(&message.content)?;
                        match message.role.as_str() {
                            "system" => ChatMessage::system(content),
                            "developer" => ChatMessage::developer(content, None),
                            "user" => ChatMessage::user(content),
                            _ => return Err(unsupported("message role").into()),
                        }
                    }
                    InputItem::FunctionCallOutput(output) => tool_output(output)?,
                    _ => return Err(unsupported("unresolved or unsupported history item").into()),
                });
            }
        }
    }
    flush_assistant(&mut messages, &mut assistant);
    Ok(messages)
}

fn assistant_block(part: &InputContent) -> Result<AssistantContentBlock> {
    let text = match part {
        InputContent::InputText(text) if text.extra.is_empty() => text,
        // A completed output_text block carries annotations and logprobs even for plain text.
        InputContent::OutputText(text)
            if text.extra.iter().all(|(key, value)| {
                matches!(key.as_str(), "annotations" | "logprobs")
                    && (value.is_null() || value.as_array().is_some_and(Vec::is_empty))
            }) =>
        {
            text
        }
        InputContent::ReasoningText(text) if text.extra.is_empty() => {
            return Ok(AssistantContentBlock::Reasoning {
                text: text.text.clone(),
            });
        }
        _ => return Err(unsupported("assistant content other than plain text or reasoning").into()),
    };
    Ok(AssistantContentBlock::Text {
        text: text.text.clone(),
    })
}

fn flush_assistant(messages: &mut Vec<ChatMessage>, blocks: &mut Vec<AssistantContentBlock>) {
    if !blocks.is_empty() {
        messages.push(ChatMessage::assistant_blocks(std::mem::take(blocks)));
    }
}

fn function_call(call: &InputFunctionToolCall) -> Result<AssistantContentBlock> {
    if call.namespace.is_some() {
        return Err(unsupported("unresolved function namespace").into());
    }
    Ok(AssistantContentBlock::ToolCall(AssistantToolCall {
        id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    }))
}

fn message_content(content: &InputMessageContent) -> Result<ChatContent> {
    match content {
        InputMessageContent::Text(text) => Ok(text.as_str().into()),
        InputMessageContent::Parts(parts) => {
            let parts = parts
                .iter()
                .map(|part| match part {
                    InputContent::InputText(text) | InputContent::OutputText(text) if text.extra.is_empty() => {
                        Ok(ChatContentPart::text(text.text.clone()))
                    }
                    _ => Err(unsupported("non-text input or content extensions").into()),
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ChatContent::Parts(parts))
        }
    }
}

fn tool_output(output: &FunctionToolResultMessage) -> Result<ChatMessage> {
    let content = match &output.output {
        ToolCallOutput::Text(text) => text.as_str().into(),
        ToolCallOutput::Content(parts) => {
            let parts = parts
                .iter()
                .map(|part| match part {
                    ToolOutputContent::InputText(text) if text.extra.is_empty() => {
                        Ok(ChatContentPart::text(text.text.clone()))
                    }
                    _ => Err(unsupported("non-text tool output or content extensions").into()),
                })
                .collect::<Result<Vec<_>>>()?;
            ChatContent::Parts(parts)
        }
    };
    Ok(ChatMessage::tool_response(content, output.call_id.clone()))
}
