//! Restores canonical shell function calls to the public shell lifecycle.

use super::custom::complete_json_string_prefix;
use super::{ToolEvent, ToolTranslator, ensure_function_call_size};
use crate::events::{EventFrame, SSEEventType};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::synthetic_event;
use crate::tool::{ShellHandler, shell};
use crate::types::io::{ShellCallAction, ShellCallStatus};
use serde_json::Value;

#[derive(Debug)]
struct ShellCallState {
    added: bool,
    output_index: u32,
    commands: Vec<String>,
    cursor: Option<usize>,
    command_open: bool,
    completion: ShellCommandsCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellCommandsCompletion {
    Streaming,
    ArrayDone,
    ArgumentsDone,
}

#[derive(Debug, Default)]
pub(super) struct ShellTranslator {
    state: Option<ShellCallState>,
}

impl ToolTranslator for ShellTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        match event {
            ToolEvent::Added { output_index, .. } => {
                self.state = Some(ShellCallState {
                    added: call.is_some(),
                    output_index,
                    commands: Vec::new(),
                    cursor: None,
                    command_open: false,
                    completion: ShellCommandsCompletion::Streaming,
                });
                Ok(call
                    .map(|call| shell_added_frame(&call))
                    .transpose()?
                    .into_iter()
                    .collect())
            }
            event => {
                let state = self
                    .state
                    .as_mut()
                    .ok_or_else(|| ExecutorError::StreamError("shell translator has no started call".to_owned()))?;
                let Some(call) = call else {
                    return Ok(Vec::new());
                };
                match event {
                    ToolEvent::Delta(_) => incremental_shell_commands(state, call.arguments()),
                    ToolEvent::ArgumentsDone(_) | ToolEvent::Done(_) => {
                        let mut frames = Vec::new();
                        if !state.added {
                            frames.push(shell_added_frame(&call)?);
                            state.added = true;
                        }
                        frames.extend(finish_shell_commands(state, call.arguments())?);
                        if matches!(event, ToolEvent::Done(_)) {
                            frames.push(shell_done_frame(&call)?);
                        }
                        Ok(frames)
                    }
                    ToolEvent::Added { .. } => unreachable!("handled above"),
                }
            }
        }
    }
}

fn shell_added_frame(call: &AccumulatedFunctionCall<'_>) -> ExecutorResult<EventFrame> {
    shell_event_frame(
        SSEEventType::OutputItemAdded,
        call.output_index,
        [(
            "item".to_owned(),
            serde_json::json!({
                "type": "shell_call",
                "id": shell::public_item_id(&call.item.id),
                "call_id": call.item.call_id,
                "status": "in_progress",
                "action": {"commands": [], "timeout_ms": null, "max_output_length": null}
            }),
        )],
    )
}

fn shell_done_frame(call: &AccumulatedFunctionCall<'_>) -> ExecutorResult<EventFrame> {
    shell_frame(
        SSEEventType::OutputItemDone,
        call.output_index,
        call.item.status.into(),
        call,
    )
}

fn shell_frame(
    event_type: SSEEventType,
    output_index: u32,
    status: ShellCallStatus,
    call: &AccumulatedFunctionCall<'_>,
) -> ExecutorResult<EventFrame> {
    let item = ShellHandler::output_item_with_status(call.item, status).ok_or_else(|| {
        ExecutorError::StreamError("shell function call contains invalid action arguments".to_owned())
    })?;
    let item = serde_json::to_value(item).map_err(ExecutorError::JsonError)?;
    let mut frame = synthetic_event(event_type, [("item".to_owned(), item)])?;
    frame.wire.output_index = Some(u64::from(output_index));
    Ok(frame)
}

fn shell_command_frame(
    event_type: SSEEventType,
    output_index: u32,
    command_index: usize,
    value: &str,
) -> ExecutorResult<EventFrame> {
    let field = if event_type == SSEEventType::ShellCallCommandDelta {
        "delta"
    } else {
        "command"
    };
    shell_event_frame(
        event_type,
        output_index,
        [
            ("command_index".to_owned(), Value::from(command_index)),
            (field.to_owned(), Value::String(value.to_owned())),
        ],
    )
}

/// Find a top-level array field, skipping complete preceding values with serde.
/// The command strings themselves use the same incremental JSON string decoder
/// as custom tool input, including split escapes and Unicode surrogate pairs.
fn shell_commands_start(arguments: &str) -> Option<usize> {
    let mut rest = arguments.trim_start().strip_prefix('{')?.trim_start();
    loop {
        let mut key = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let name = key.next()?.ok()?;
        rest = rest[key.byte_offset()..].trim_start().strip_prefix(':')?.trim_start();
        if name == "commands" {
            let array = rest.strip_prefix('[')?;
            return Some(arguments.len() - array.len());
        }
        let mut value = serde_json::Deserializer::from_str(rest).into_iter::<serde::de::IgnoredAny>();
        value.next()?.ok()?;
        rest = rest[value.byte_offset()..].trim_start().strip_prefix(',')?.trim_start();
    }
}

fn incremental_shell_commands(state: &mut ShellCallState, arguments: &str) -> ExecutorResult<Vec<EventFrame>> {
    ensure_function_call_size(arguments)?;
    if state.completion != ShellCommandsCompletion::Streaming {
        return Ok(Vec::new());
    }
    let Some(mut cursor) = state.cursor.or_else(|| shell_commands_start(arguments)) else {
        return Ok(Vec::new());
    };
    let mut frames = Vec::new();
    loop {
        if !state.command_open {
            while arguments.as_bytes().get(cursor).is_some_and(u8::is_ascii_whitespace) {
                cursor += 1;
            }
            if !state.commands.is_empty() {
                match arguments.as_bytes().get(cursor) {
                    Some(b',') => {
                        // Do not consume the separator until the next string is available.
                        let rest = arguments[cursor + 1..].trim_start();
                        if rest.is_empty() {
                            break;
                        }
                        cursor = arguments.len() - rest.len();
                    }
                    Some(b']') => {
                        state.completion = ShellCommandsCompletion::ArrayDone;
                        break;
                    }
                    None => break,
                    _ => {
                        return Err(ExecutorError::StreamError(
                            "invalid shell commands array separator".to_owned(),
                        ));
                    }
                }
            } else if arguments.as_bytes().get(cursor) == Some(&b']') {
                state.completion = ShellCommandsCompletion::ArrayDone;
                break;
            }
            match arguments.as_bytes().get(cursor) {
                Some(b'"') => {
                    frames.push(shell_command_frame(
                        SSEEventType::ShellCallCommandAdded,
                        state.output_index,
                        state.commands.len(),
                        "",
                    )?);
                    state.commands.push(String::new());
                    state.command_open = true;
                    cursor += 1;
                }
                None => break,
                _ => {
                    return Err(ExecutorError::StreamError(
                        "shell command must be a JSON string".to_owned(),
                    ));
                }
            }
        }
        let encoded = arguments
            .get(cursor..)
            .ok_or_else(|| ExecutorError::StreamError("shell arguments changed while streaming".to_owned()))?;
        let length = complete_json_string_prefix(encoded);
        if length != 0 {
            let delta: String = serde_json::from_str(&format!("\"{}\"", &encoded[..length]))
                .map_err(|error| ExecutorError::StreamError(format!("invalid shell command string: {error}")))?;
            let index = state.commands.len() - 1;
            state.commands[index].push_str(&delta);
            frames.push(shell_command_frame(
                SSEEventType::ShellCallCommandDelta,
                state.output_index,
                index,
                &delta,
            )?);
            cursor += length;
        }
        if arguments.as_bytes().get(cursor) != Some(&b'"') {
            break;
        }
        let index = state.commands.len() - 1;
        frames.push(shell_command_frame(
            SSEEventType::ShellCallCommandDone,
            state.output_index,
            index,
            &state.commands[index],
        )?);
        state.command_open = false;
        cursor += 1;
    }
    state.cursor = Some(cursor);
    Ok(frames)
}

fn finish_shell_commands(state: &mut ShellCallState, arguments: &str) -> ExecutorResult<Vec<EventFrame>> {
    ensure_function_call_size(arguments)?;
    let action: ShellCallAction = serde_json::from_str(arguments).map_err(|error| {
        ExecutorError::StreamError(format!(
            "shell function call contains invalid action arguments: {error}"
        ))
    })?;
    if state.commands.len() > action.commands.len()
        || (state.completion == ShellCommandsCompletion::ArgumentsDone && state.commands != action.commands)
    {
        return Err(ExecutorError::StreamError(
            "authoritative shell action contradicts streamed commands".to_owned(),
        ));
    }
    let mut frames = Vec::new();
    for (index, command) in action.commands.iter().enumerate() {
        if let Some(emitted) = state.commands.get(index) {
            let open = state.command_open && index + 1 == state.commands.len();
            if !command.starts_with(emitted) || (!open && command != emitted) {
                return Err(ExecutorError::StreamError(
                    "authoritative shell action contradicts streamed commands".to_owned(),
                ));
            }
            if !open {
                continue;
            }
        } else {
            frames.push(shell_command_frame(
                SSEEventType::ShellCallCommandAdded,
                state.output_index,
                index,
                "",
            )?);
            state.commands.push(String::new());
        }
        let remaining = &command[state.commands[index].len()..];
        if !remaining.is_empty() {
            frames.push(shell_command_frame(
                SSEEventType::ShellCallCommandDelta,
                state.output_index,
                index,
                remaining,
            )?);
        }
        frames.push(shell_command_frame(
            SSEEventType::ShellCallCommandDone,
            state.output_index,
            index,
            command,
        )?);
        state.commands[index].clone_from(command);
        state.command_open = false;
    }
    state.completion = ShellCommandsCompletion::ArgumentsDone;
    Ok(frames)
}

fn shell_event_frame(
    event_type: SSEEventType,
    output_index: u32,
    fields: impl IntoIterator<Item = (String, Value)>,
) -> ExecutorResult<EventFrame> {
    let mut frame = synthetic_event(event_type, fields)?;
    frame.wire.output_index = Some(u64::from(output_index));
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::SseLine;
    use crate::executor::accumulator::ResponseAccumulator;
    use crate::executor::pipeline::RoundIngestion;
    use crate::executor::translate::tests::{sse, test_context, translate};
    use crate::executor::translate::{MAX_PENDING_FUNCTION_BYTES, TranslationDispatcher};
    use crate::tool::ToolType;
    use std::collections::HashMap;
    #[test]
    fn shell_function_arguments_restore_openai_shell_lifecycle() {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let registry = test_context(HashMap::from([("shell".to_owned(), ToolType::Shell)]));
        let mut translator = TranslationDispatcher::new(registry);
        let events = [
            serde_json::json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"id": "fc_shell", "type": "function_call", "call_id": "call_shell",
                    "name": "shell", "arguments": "", "status": "in_progress"}
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_shell", "call_id": "call_shell",
                "delta": "{\"commands\":[\"pwd\"],\"timeout_ms\":1000}"
            }),
            serde_json::json!({
                "type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_shell", "call_id": "call_shell", "name": "shell",
                "arguments": "{\"commands\":[\"pwd\"],\"timeout_ms\":1000}"
            }),
            serde_json::json!({
                "type": "response.output_item.done", "output_index": 0,
                "item": {"id": "fc_shell", "type": "function_call", "call_id": "call_shell",
                    "name": "shell", "arguments": "{\"commands\":[\"pwd\"],\"timeout_ms\":1000}",
                    "status": "completed"}
            }),
        ];

        let frames = events
            .iter()
            .flat_map(|event| translate(&mut accumulator, &mut translator, event).frames)
            .collect::<Vec<_>>();

        assert_eq!(
            frames.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
            [
                SSEEventType::OutputItemAdded,
                SSEEventType::ShellCallCommandAdded,
                SSEEventType::ShellCallCommandDelta,
                SSEEventType::ShellCallCommandDone,
                SSEEventType::OutputItemDone
            ]
        );
        assert_eq!(frames[0].wire.rest["item"]["type"], "shell_call");
        assert_eq!(frames[0].wire.rest["item"]["id"], "sh_shell");
        assert_eq!(frames[0].wire.rest["item"]["status"], "in_progress");
        assert_eq!(frames[0].wire.rest["item"]["action"]["commands"], serde_json::json!([]));
        assert_eq!(frames[3].wire.rest["command"], "pwd");
        assert_eq!(frames[4].wire.rest["item"]["status"], "completed");
    }

    #[test]
    fn shell_commands_stream_before_arguments_done_with_split_escapes_and_reordered_fields() {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let registry = test_context(HashMap::from([("shell".to_owned(), ToolType::Shell)]));
        let mut translator = TranslationDispatcher::new(registry);
        translate(
            &mut accumulator,
            &mut translator,
            &serde_json::json!({
                "type": "response.output_item.added", "output_index": 2,
                "item": {"id": "fc_shell", "type": "function_call", "call_id": "call_shell",
                    "name": "shell", "arguments": "", "status": "in_progress"}
            }),
        );
        let arguments = r#"{"timeout_ms":1000,"metadata":{"commands":["ignored"]},"commands":["echo \"hi\"\n\uD83D\uDE00","","pwd"],"max_output_length":4096}"#;
        let mut frames = Vec::new();
        for ch in arguments.chars() {
            frames.extend(
                translate(
                    &mut accumulator,
                    &mut translator,
                    &serde_json::json!({
                        "type": "response.function_call_arguments.delta", "output_index": 2,
                        "item_id": "fc_shell", "call_id": "call_shell", "delta": ch.to_string()
                    }),
                )
                .frames,
            );
        }
        let commands = frames
            .iter()
            .filter(|frame| frame.event_type == SSEEventType::ShellCallCommandDone)
            .map(|frame| frame.wire.rest["command"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(commands, ["echo \"hi\"\n😀", "", "pwd"]);
        assert!(frames.iter().all(|frame| frame.wire.output_index == Some(2)));
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.event_type == SSEEventType::ShellCallCommandAdded)
                .count(),
            3
        );
        let done = translate(
            &mut accumulator,
            &mut translator,
            &serde_json::json!({
                "type": "response.function_call_arguments.done", "output_index": 2,
                "item_id": "fc_shell", "call_id": "call_shell", "name": "shell", "arguments": arguments
            }),
        );
        assert!(done.frames.is_empty(), "don't repeat completed command events");
    }

    #[test]
    fn shell_authoritative_arguments_complete_partial_commands_and_reject_changes() {
        let mut state = ShellCallState {
            added: true,
            output_index: 0,
            commands: Vec::new(),
            cursor: None,
            command_open: false,
            completion: ShellCommandsCompletion::Streaming,
        };
        incremental_shell_commands(&mut state, r#"{"commands":["ec"#).unwrap();
        let frames = finish_shell_commands(&mut state, r#"{"commands":["echo","pwd"]}"#).unwrap();
        assert_eq!(frames[0].wire.rest["delta"], "ho");
        assert_eq!(state.commands, ["echo", "pwd"]);
        assert!(finish_shell_commands(&mut state, r#"{"commands":["changed"]}"#).is_err());
        assert!(finish_shell_commands(&mut state, r#"{"commands":["echo","pwd","extra"]}"#).is_err());
        assert!(incremental_shell_commands(&mut state, &"x".repeat(MAX_PENDING_FUNCTION_BYTES + 1)).is_err());
    }

    #[test]
    fn malformed_shell_arguments_fail_closed() {
        let mut accumulator = ResponseAccumulator::new("resp_1".to_owned(), None);
        let registry = test_context(HashMap::from([("shell".to_owned(), ToolType::Shell)]));
        let mut translator = TranslationDispatcher::new(registry);
        let added = serde_json::json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_shell", "type": "function_call", "call_id": "call_shell",
                "name": "shell", "arguments": "", "status": "in_progress"}
        });
        translate(&mut accumulator, &mut translator, &added);
        let done = serde_json::json!({
            "type": "response.function_call_arguments.done", "output_index": 0,
            "item_id": "fc_shell", "call_id": "call_shell", "name": "shell",
            "arguments": "not-json"
        });

        let error = RoundIngestion::translate_line(&mut accumulator, SseLine::parse(&sse(&done)), &mut translator)
            .expect_err("invalid shell action must fail");
        assert!(error.to_string().contains("invalid action arguments"));
    }
}
