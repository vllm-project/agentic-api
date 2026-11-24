//! Adapt resolved history to the shared frontend's chat representation.

use crate::Result;
use agentic_core::types::io::input::{
    FunctionToolResultMessage, InputFunctionToolCall, ToolCallOutput, ToolOutputContent,
};
use agentic_core::types::io::{InputContent, InputItem, InputMessageContent, ResponsesInput};
use vllm_chat::{AssistantContentBlock, AssistantToolCall, ChatContent, ChatContentPart, ChatMessage};

use super::unsupported;

pub(super) fn convert(input: &ResponsesInput, instructions: Option<&str>) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    if let Some(instructions) = instructions {
        messages.push(ChatMessage::system(instructions));
    }
    let items = match input {
        ResponsesInput::Items(items) => items,
        ResponsesInput::Text(text) => {
            messages.push(ChatMessage::user(text.as_str()));
            return Ok(messages);
        }
    };
    let mut assistant = Vec::new();
    for item in items {
        match item {
            InputItem::Message(message) if message.role == "assistant" => match &message.content {
                InputMessageContent::Text(text) => assistant.push(AssistantContentBlock::Text { text: text.clone() }),
                InputMessageContent::Parts(parts) => {
                    for part in parts {
                        assistant.push(match part {
                            InputContent::InputText(text) | InputContent::OutputText(text) if text.extra.is_empty() => {
                                AssistantContentBlock::Text {
                                    text: text.text.clone(),
                                }
                            }
                            InputContent::ReasoningText(text) if text.extra.is_empty() => {
                                AssistantContentBlock::Reasoning {
                                    text: text.text.clone(),
                                }
                            }
                            _ => return Err(unsupported("assistant content other than plain text or reasoning").into()),
                        });
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
            InputItem::CustomToolCall(call) => assistant.push(function_call(&call.clone().into())?),
            InputItem::ShellCall(call) => assistant.push(function_call(&call.clone().into())?),
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
                    InputItem::CustomToolCallOutput(output) => tool_output(&output.clone().into())?,
                    InputItem::ShellCallOutput(output) => tool_output(&output.clone().into())?,
                    _ => return Err(unsupported("unresolved or unsupported history item").into()),
                });
            }
        }
    }
    flush_assistant(&mut messages, &mut assistant);
    Ok(messages)
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
