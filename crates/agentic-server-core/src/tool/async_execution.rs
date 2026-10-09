//! Model-visible hints for async client tools.
//!
//! An `OpenAI` model keeps answering after it calls an async tool. A model served by vLLM stops at the
//! call, so the gateway continues the response with the call still pending. Without guidance the
//! model then tends to invent the pending result or call the tool again. Two hints prevent that:
//!
//! - every async tool's model-visible description says that it runs in the background, and
//! - the upstream copy of the input carries a `developer` note after each async call whose output has
//!   not arrived.
//!
//! The texts are the `upstream_hints` in `tests/cassettes/async_tools/prompts.json`; the recorder's
//! `--sample` runs measured their effect. Notes exist only in the request sent upstream; they are never stored or
//! returned to the client. The public `async` marker itself is not sent upstream: declarations lose
//! it during normalization and replayed calls lose it in [`lower_async_calls`].

use std::borrow::Cow;
use std::collections::HashSet;

use crate::types::io::{InputItem, InputMessage, InputMessageContent, ResponsesInput};

/// Appended to the description of every async tool the model sees.
pub const DESCRIPTION_SUFFIX: &str = " Runs in the background: the result arrives in a later message. Until then, do not guess the result and do not call this tool again for the same request.";

const PENDING_NOTE_PREFIX: &str = "The ";
const PENDING_NOTE_CALL: &str = " call ";
const PENDING_NOTE_SUFFIX: &str = " is running in the background; its result has not arrived yet. Do not guess it and do not call it again. Continue with the rest of the request.";

/// The model-visible description of an async tool: its own description plus the background hint.
#[must_use]
pub fn async_description(description: Option<&str>) -> String {
    match description {
        Some(description) => format!("{description}{DESCRIPTION_SUFFIX}"),
        None => DESCRIPTION_SUFFIX.trim_start().to_owned(),
    }
}

/// The note placed after an async call that has no output yet.
#[must_use]
pub fn pending_call_note(name: &str, call_id: &str) -> String {
    format!("{PENDING_NOTE_PREFIX}{name}{PENDING_NOTE_CALL}{call_id}{PENDING_NOTE_SUFFIX}")
}

/// Lowers async calls in the model input.
///
/// Removes the public `async` marker, which the model server does not know (declarations reach it
/// without `async` too), and adds a `developer` note for every async call that has no output. A note
/// follows the run of consecutive calls that contains its call, and any outputs directly after that
/// run, so parallel calls stay together in one assistant turn with their results. The input is
/// borrowed unchanged when it has no async call.
#[must_use]
pub fn lower_async_calls(input: Cow<'_, ResponsesInput>) -> Cow<'_, ResponsesInput> {
    let ResponsesInput::Items(items) = &*input else {
        return input;
    };
    if !items
        .iter()
        .any(|item| matches!(item, InputItem::FunctionCall(call) if call.async_execution))
    {
        return input;
    }
    let answered: HashSet<&str> = items
        .iter()
        .filter_map(|item| match item {
            InputItem::FunctionCallOutput(output) => Some(output.call_id.as_str()),
            _ => None,
        })
        .collect();

    let mut lowered = Vec::with_capacity(items.len() + 1);
    let mut notes = Vec::new();
    for item in items {
        let InputItem::FunctionCall(call) = item else {
            // Outputs directly after the calls stay with them: chat templates expect tool results
            // to follow the assistant's tool calls.
            if !matches!(item, InputItem::FunctionCallOutput(_)) {
                lowered.append(&mut notes);
            }
            lowered.push(item.clone());
            continue;
        };
        if call.async_execution && !answered.contains(call.call_id.as_str()) {
            notes.push(InputItem::Message(InputMessage {
                role: "developer".into(),
                content: InputMessageContent::Text(pending_call_note(&call.name, &call.call_id)),
                ..Default::default()
            }));
        }
        let mut call = call.clone();
        call.async_execution = false;
        lowered.push(InputItem::FunctionCall(call));
    }
    lowered.append(&mut notes);
    Cow::Owned(ResponsesInput::Items(lowered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::InputFunctionToolCall;
    use crate::types::io::input::FunctionToolResultMessage;

    const RECORDED_HINTS: &str = include_str!("../../tests/cassettes/async_tools/prompts.json");

    fn call(call_id: &str, async_execution: bool) -> InputItem {
        InputItem::FunctionCall(InputFunctionToolCall {
            agent: None,
            id: None,
            call_id: call_id.to_owned(),
            name: "get_weather".to_owned(),
            namespace: None,
            arguments: "{}".to_owned(),
            status: None,
            async_execution,
        })
    }

    fn output(call_id: &str) -> InputItem {
        InputItem::FunctionCallOutput(
            serde_json::from_value::<FunctionToolResultMessage>(
                serde_json::json!({"call_id": call_id, "output": "{}"}),
            )
            .unwrap(),
        )
    }

    fn user(text: &str) -> InputItem {
        InputItem::Message(InputMessage {
            role: "user".into(),
            content: InputMessageContent::Text(text.to_owned()),
            ..Default::default()
        })
    }

    /// Item kinds after lowering: `call:<id>`, `output:<id>`, `user`, or `note:<id>`.
    fn shape(input: &ResponsesInput) -> Vec<String> {
        let ResponsesInput::Items(items) = input else {
            panic!("items");
        };
        items
            .iter()
            .map(|item| match item {
                InputItem::FunctionCall(call) => {
                    assert!(!call.async_execution, "the marker is not sent upstream");
                    format!("call:{}", call.call_id)
                }
                InputItem::FunctionCallOutput(output) => format!("output:{}", output.call_id),
                InputItem::Message(message) if message.role == "developer" => {
                    let InputMessageContent::Text(text) = &message.content else {
                        panic!("text note");
                    };
                    let call_id = text
                        .split(" call ")
                        .nth(1)
                        .and_then(|rest| rest.split(' ').next())
                        .expect("note names its call");
                    assert_eq!(text, &pending_call_note("get_weather", call_id));
                    format!("note:{call_id}")
                }
                InputItem::Message(_) => "user".to_owned(),
                other => panic!("unexpected item {other:?}"),
            })
            .collect()
    }

    #[test]
    fn hint_texts_match_the_recorded_fixture() {
        let recorded: serde_json::Value = serde_json::from_str(RECORDED_HINTS).unwrap();
        let hints = &recorded["upstream_hints"];
        assert_eq!(hints["tool_description_suffix"], DESCRIPTION_SUFFIX);
        assert_eq!(hints["pending_note"], pending_call_note("{name}", "{call_id}"));
        assert_eq!(
            async_description(Some("Weather.")),
            format!("Weather.{DESCRIPTION_SUFFIX}")
        );
        assert_eq!(async_description(None), DESCRIPTION_SUFFIX.trim_start());
    }

    #[test]
    fn input_without_async_calls_is_borrowed_unchanged() {
        let input = ResponsesInput::Items(vec![user("hi"), call("call_sync", false)]);
        assert!(matches!(lower_async_calls(Cow::Borrowed(&input)), Cow::Borrowed(_)));
    }

    /// A note never separates a call run from the outputs that directly follow it.
    #[test]
    fn notes_follow_the_outputs_of_their_call_run() {
        let input = ResponsesInput::Items(vec![
            user("start"),
            call("call_async", true),
            call("call_sync", false),
            output("call_sync"),
            user("follow up"),
        ]);

        assert_eq!(
            shape(&lower_async_calls(Cow::Borrowed(&input))),
            [
                "user",
                "call:call_async",
                "call:call_sync",
                "output:call_sync",
                "note:call_async",
                "user"
            ]
        );
    }

    #[test]
    fn notes_follow_the_call_run_and_only_for_pending_async_calls() {
        let input = ResponsesInput::Items(vec![
            user("start"),
            call("call_a", true),
            call("call_b", false),
            call("call_c", true),
            user("follow up"),
            call("call_answered", true),
            output("call_answered"),
            call("call_last", true),
        ]);

        let lowered = lower_async_calls(Cow::Borrowed(&input));

        assert_eq!(
            shape(&lowered),
            [
                "user",
                "call:call_a",
                "call:call_b",
                "call:call_c",
                "note:call_a",
                "note:call_c",
                "user",
                "call:call_answered",
                "output:call_answered",
                "call:call_last",
                "note:call_last",
            ]
        );
    }

    /// The upstream request for a continuation with a pending async call: the async tool's
    /// description carries the suffix, neither the tool nor the replayed call carries the marker, and
    /// the note follows the call. The gateway recordings show the model server accepting this shape.
    #[test]
    fn upstream_request_carries_both_hints_and_no_marker() {
        let request: crate::types::request_response::RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "store": false,
            "input": [
                {"type": "message", "role": "user", "content": "Check the Paris weather."},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                    "arguments": "{\"city\":\"Paris\"}", "async": true}
            ],
            "tools": [{"type": "function", "name": "get_weather", "description": "Read the weather.",
                "parameters": {"type": "object"}, "async": true}]
        }))
        .unwrap();

        let upstream = serde_json::to_value(request.to_upstream_request(false).unwrap()).unwrap();

        let tool = &upstream["tools"][0];
        assert_eq!(tool["description"], async_description(Some("Read the weather.")));
        assert!(
            tool.get("async").is_none(),
            "declarations reach the model server without async"
        );
        let input = upstream["input"].as_array().unwrap();
        assert_eq!(input.len(), 3, "{input:?}");
        assert_eq!(input[1]["type"], "function_call");
        assert!(input[1].get("async").is_none(), "the marker is not sent upstream");
        assert_eq!(input[2]["role"], "developer");
        assert_eq!(input[2]["content"], pending_call_note("get_weather", "call_1"));
    }
}
