use super::{ToolEvent, ToolTranslator, ensure_function_call_size};
use crate::events::WireEvent;
use crate::events::{EventFrame, SSEEventType};
use crate::executor::accumulator::AccumulatedFunctionCall;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::synthetic_event;
use crate::tool::custom::CustomToolMap;
use crate::utils::common::serialize_to_value_or_custom_default;
use serde_json::{Map, Value};

#[derive(Debug)]
struct CustomCallState {
    public_item_id: String,
    output_index: u32,
    emitted_input: String,
    input_start: Option<usize>,
    input_cursor: usize,
    input_done: bool,
}

#[derive(Debug, Default)]
pub(super) struct CustomTranslator {
    state: Option<CustomCallState>,
}

impl ToolTranslator for CustomTranslator {
    fn translate(
        &mut self,
        event: ToolEvent<'_>,
        call: Option<AccumulatedFunctionCall<'_>>,
    ) -> ExecutorResult<Vec<EventFrame>> {
        match event {
            ToolEvent::Added {
                item_id, output_index, ..
            } => {
                let public_item_id = call.as_ref().map_or_else(
                    || crate::tool::custom::public_item_id(item_id),
                    |call| crate::tool::custom::public_item_id(&call.item.id),
                );
                self.state = Some(CustomCallState {
                    public_item_id,
                    output_index,
                    emitted_input: String::new(),
                    input_start: None,
                    input_cursor: 0,
                    input_done: false,
                });
                Ok(call
                    .map(|call| custom_added_frame(&call))
                    .transpose()?
                    .into_iter()
                    .collect())
            }
            event => {
                let state = self
                    .state
                    .as_mut()
                    .ok_or_else(|| ExecutorError::StreamError("custom translator has no started call".to_owned()))?;
                let Some(call) = call else {
                    return Ok(Vec::new());
                };
                match event {
                    ToolEvent::Delta(_) => Ok(incremental_custom_delta(state, call.arguments())?.into_iter().collect()),
                    ToolEvent::ArgumentsDone(_) => finish_custom_input(state, call.arguments()),
                    ToolEvent::Done(_) => {
                        let mut frames = finish_custom_input(state, call.arguments())?;
                        frames.push(custom_done_frame(state, &call)?);
                        Ok(frames)
                    }
                    ToolEvent::Added { .. } => unreachable!("handled above"),
                }
            }
        }
    }
}

fn custom_added_frame(call: &AccumulatedFunctionCall<'_>) -> ExecutorResult<EventFrame> {
    custom_frame(
        SSEEventType::OutputItemAdded,
        call.output_index,
        [(
            "item".to_owned(),
            serde_json::json!({
                "id": crate::tool::custom::public_item_id(&call.item.id),
                "type": "custom_tool_call",
                "status": "in_progress",
                "call_id": call.item.call_id,
                "input": "",
                "name": call.item.name,
            }),
        )],
    )
}

fn incremental_custom_delta(state: &mut CustomCallState, arguments: &str) -> ExecutorResult<Option<EventFrame>> {
    ensure_function_call_size(arguments)?;
    let Some(delta) = partial_custom_input(state, arguments)? else {
        return Ok(None);
    };
    state.emitted_input.push_str(&delta);
    custom_frame(
        SSEEventType::CustomToolCallInputDelta,
        state.output_index,
        [
            ("delta".to_owned(), Value::String(delta)),
            ("item_id".to_owned(), Value::String(state.public_item_id.clone())),
        ],
    )
    .map(Some)
}

fn finish_custom_input(state: &mut CustomCallState, arguments: &str) -> ExecutorResult<Vec<EventFrame>> {
    if state.input_done {
        return Ok(Vec::new());
    }
    ensure_function_call_size(arguments)?;
    let input = crate::tool::custom::input_from_arguments(arguments);
    let Some(remaining) = input.strip_prefix(&state.emitted_input) else {
        return Err(ExecutorError::StreamError(
            "authoritative custom tool input contradicts streamed custom tool input".to_owned(),
        ));
    };
    let remaining = (!remaining.is_empty()).then(|| remaining.to_owned());
    state.emitted_input.clone_from(&input);
    state.input_done = true;

    let mut frames = Vec::with_capacity(2);
    if let Some(delta) = remaining {
        frames.push(custom_frame(
            SSEEventType::CustomToolCallInputDelta,
            state.output_index,
            [
                ("delta".to_owned(), Value::String(delta)),
                ("item_id".to_owned(), Value::String(state.public_item_id.clone())),
            ],
        )?);
    }
    frames.push(custom_frame(
        SSEEventType::CustomToolCallInputDone,
        state.output_index,
        [
            ("input".to_owned(), Value::String(input)),
            ("item_id".to_owned(), Value::String(state.public_item_id.clone())),
        ],
    )?);
    Ok(frames)
}

fn custom_done_frame(state: &CustomCallState, call: &AccumulatedFunctionCall<'_>) -> ExecutorResult<EventFrame> {
    custom_frame(
        SSEEventType::OutputItemDone,
        state.output_index,
        [(
            "item".to_owned(),
            serde_json::json!({
                "id": state.public_item_id,
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call.item.call_id,
                "input": state.emitted_input,
                "name": call.item.name,
            }),
        )],
    )
}

pub(super) fn custom_frame(
    event_type: SSEEventType,
    output_index: u32,
    fields: impl IntoIterator<Item = (String, Value)>,
) -> ExecutorResult<EventFrame> {
    let mut frame = synthetic_event(event_type, fields)?;
    frame.wire.output_index = Some(u64::from(output_index));
    Ok(frame)
}

fn partial_custom_input(state: &mut CustomCallState, arguments: &str) -> ExecutorResult<Option<String>> {
    let input_start = if let Some(input_start) = state.input_start {
        input_start
    } else {
        let Some(input_start) = custom_input_start(arguments) else {
            return Ok(None);
        };
        state.input_start = Some(input_start);
        state.input_cursor = input_start;
        input_start
    };
    if state.input_cursor < input_start || state.input_cursor > arguments.len() {
        return Ok(None);
    }
    let encoded = &arguments[state.input_cursor..];
    let end = complete_json_string_prefix(encoded);
    if end == 0 {
        return Ok(None);
    }
    let candidate = format!("\"{}\"", &encoded[..end]);
    let delta = serde_json::from_str::<String>(&candidate)
        .map_err(|error| ExecutorError::StreamError(format!("invalid custom tool input string: {error}")))?;
    state.input_cursor = state.input_cursor.saturating_add(end);
    Ok((!delta.is_empty()).then_some(delta))
}

pub(super) fn complete_json_string_prefix(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return index,
            b'\\' => {
                let Some(escape) = bytes.get(index + 1) else {
                    return index;
                };
                if *escape == b'u' {
                    let unicode_end = index.saturating_add(6);
                    if unicode_end > bytes.len() {
                        return index;
                    }
                    let Some(code_unit) = json_hex_quad(&bytes[index + 2..unicode_end]) else {
                        index = unicode_end;
                        continue;
                    };
                    if (0xD800..=0xDBFF).contains(&code_unit) {
                        let pair_end = index.saturating_add(12);
                        if pair_end > bytes.len() {
                            return index;
                        }
                        index = pair_end;
                    } else {
                        index = unicode_end;
                    }
                } else {
                    index = index.saturating_add(2);
                }
            }
            _ => index = index.saturating_add(1),
        }
    }
    index
}

fn json_hex_quad(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 4 {
        return None;
    }
    bytes.iter().try_fold(0_u16, |value, byte| {
        let digit = byte.to_ascii_lowercase();
        let digit = match digit {
            b'0'..=b'9' => u16::from(digit - b'0'),
            b'a'..=b'f' => u16::from(digit - b'a' + 10),
            _ => return None,
        };
        value.checked_mul(16)?.checked_add(digit)
    })
}

fn custom_input_start(arguments: &str) -> Option<usize> {
    let original_len = arguments.len();
    let arguments = arguments.trim_start();
    let arguments = arguments.strip_prefix("{}").unwrap_or(arguments).trim_start();
    let encoded = arguments
        .strip_prefix('{')?
        .trim_start()
        .strip_prefix("\"input\"")?
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    Some(original_len.saturating_sub(encoded.len()))
}

impl CustomTranslator {
    /// Restores normalized custom-tool declarations in response lifecycle
    /// metadata before the event is emitted to the client.
    pub(super) fn restore_response_wire(wire: &mut WireEvent, map: Option<&CustomToolMap>) -> bool {
        let Some(map) = map else {
            return false;
        };
        restore_response_map(&mut wire.rest, map)
    }
}

fn restore_response_map(object: &mut Map<String, Value>, map: &CustomToolMap) -> bool {
    let mut changed = restore_response_metadata(object, map);
    for key in ["response", "payload"] {
        if let Some(nested) = object.get_mut(key).and_then(Value::as_object_mut) {
            changed |= restore_response_map(nested, map);
        }
    }
    changed
}

fn restore_response_metadata(object: &mut Map<String, Value>, map: &CustomToolMap) -> bool {
    let mut changed = false;
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            changed |= restore_custom_declaration(tool, map);
        }
    }
    if let Some(tool_choice) = object.get_mut("tool_choice") {
        changed |= restore_custom_tool_choice(tool_choice, map);
    }
    changed
}

fn restore_custom_declaration(tool: &mut Value, map: &CustomToolMap) -> bool {
    let Some(name) = normalized_custom_name(tool, map) else {
        return false;
    };
    let Some(param) = map.declaration(&name) else {
        return false;
    };
    let Some(mut declaration) =
        serialize_to_value_or_custom_default(param, "custom tool metadata serialization failed", Some, None)
    else {
        return false;
    };
    let Some(object) = declaration.as_object_mut() else {
        return false;
    };
    object.insert("type".to_owned(), Value::String("custom".to_owned()));
    *tool = declaration;
    true
}

fn restore_custom_tool_choice(choice: &mut Value, map: &CustomToolMap) -> bool {
    let Some(object) = choice.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
            return false;
        };
        return tools
            .iter_mut()
            .map(|tool| restore_custom_choice_type(tool, map))
            .fold(false, |changed, restored| changed | restored);
    }
    restore_custom_choice_type(choice, map)
}

fn restore_custom_choice_type(choice: &mut Value, map: &CustomToolMap) -> bool {
    if normalized_custom_name(choice, map).is_none() {
        return false;
    }
    let Some(object) = choice.as_object_mut() else {
        return false;
    };
    object.insert("type".to_owned(), Value::String("custom".to_owned()));
    object.remove("namespace");
    true
}

fn normalized_custom_name(value: &Value, map: &CustomToolMap) -> Option<String> {
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let name = object.get("name")?.as_str()?;
    map.declaration(name).map(|_| name.to_owned())
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::tool::CustomHandler;
    use crate::types::tools::{CustomToolParam, ResponsesTool};
    #[test]
    fn response_lifecycle_metadata_restores_public_custom_tool_shape() {
        let param = serde_json::from_value::<CustomToolParam>(serde_json::json!({
            "name": "raw_echo",
            "description": "Echo raw input."
        }))
        .expect("custom tool");
        let tools = vec![ResponsesTool::Custom(param)];
        let map = CustomHandler::build_tool_map(&tools);
        let mut wire = WireEvent::new("response.created");
        wire.rest.insert(
            "response".to_owned(),
            serde_json::json!({
                "tools": [{
                    "type": "function",
                    "name": "raw_echo",
                    "description": "normalized description",
                    "parameters": {"type": "object"}
                }],
                "tool_choice": {"type": "function", "name": "raw_echo"}
            }),
        );

        assert!(CustomTranslator::restore_response_wire(&mut wire, map.as_ref()));
        let response = &wire.rest["response"];
        assert_eq!(response["tools"][0]["type"], "custom");
        assert_eq!(response["tools"][0]["description"], "Echo raw input.");
        assert!(response["tools"][0].get("parameters").is_none());
        assert_eq!(response["tool_choice"]["type"], "custom");
        assert_eq!(response["tool_choice"]["name"], "raw_echo");
    }
    #[test]
    fn allowed_tools_metadata_restores_custom_selector_type() {
        let param = serde_json::from_value::<CustomToolParam>(serde_json::json!({
            "name": "raw_echo"
        }))
        .expect("custom tool");
        let tools = vec![ResponsesTool::Custom(param)];
        let map = CustomHandler::build_tool_map(&tools);
        let mut wire = WireEvent::new("response.in_progress");
        wire.rest.insert(
            "response".to_owned(),
            serde_json::json!({
                "tool_choice": {
                    "type": "allowed_tools",
                    "mode": "required",
                    "tools": [
                        {"type": "function", "name": "ordinary"},
                        {"type": "function", "name": "raw_echo"}
                    ]
                }
            }),
        );

        assert!(CustomTranslator::restore_response_wire(&mut wire, map.as_ref()));
        let tools = &wire.rest["response"]["tool_choice"]["tools"];
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[1]["type"], "custom");
    }
}
