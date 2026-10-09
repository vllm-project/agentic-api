//! Synthetic client discovery and custom-tool continuations; no recorded fixtures.
use super::*;
use crate::executor::ExecutorError;
use crate::tool::ToolError;

fn weather() -> Value {
    json!({"type":"function","name":"get_weather","defer_loading":true,
        "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}})
}

const ASYNC_POLLING_PROMPT: &str = "keep polling async jobs";

fn travel() -> Value {
    json!({"type":"namespace","name":"travel","tools":[{
        "type":"function","name":"get_timezone","defer_loading":true,
        "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
    }]})
}

pub(super) fn client_tools_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    // A model that never answers: every round starts another async job.
    if input.iter().any(|item| item["content"] == ASYNC_POLLING_PROMPT) {
        return Some(vec![function(&format!("poll_{}", input.len()), "poll_job", json!({}))]);
    }
    let root_discovery = input
        .iter()
        .any(|item| item["content"] == "discover tools before delegation");
    if !root_discovery && !input.iter().any(|item| item["content"] == "discover client tools") {
        return None;
    }
    assert!(input.iter().all(|item| !matches!(
        item["type"].as_str(),
        Some("tool_search_call" | "tool_search_output" | "custom_tool_call" | "custom_tool_call_output")
    )));
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    let result = |id: &str| {
        input
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
    };
    let names: Vec<_> = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let output = if guidance.contains("You are `/root/echo`") {
        assert_eq!(names.contains(&"get_weather"), root_discovery);
        assert_eq!(names.contains(&"agentic_ns__travel__get_timezone"), root_discovery);
        if let Some(output) = result("echo_call") {
            assert_eq!(output["output"], "CUSTOM_OK");
            vec![message("echo: CUSTOM_OK")]
        } else {
            vec![function("echo_call", "echo", json!({"input":"hello"}))]
        }
    } else if let Some((agent, name, other, text)) = [
        (
            "weather",
            "get_weather",
            "agentic_ns__travel__get_timezone",
            "weather: clear",
        ),
        (
            "timezone",
            "agentic_ns__travel__get_timezone",
            "get_weather",
            "timezone: Europe/Paris",
        ),
    ]
    .into_iter()
    .find(|(agent, _, _, _)| guidance.contains(&format!("You are `/root/{agent}`")))
    {
        assert_eq!(
            names.contains(&other),
            root_discovery,
            "children inherit parent discovery only"
        );
        let call_id = format!("lookup_{agent}");
        if let Some(output) = result(&call_id) {
            assert_eq!(output["output"], text);
            vec![message(text)]
        } else if root_discovery || result(&format!("search_{agent}")).is_some() {
            assert!(names.contains(&name));
            vec![function(&call_id, name, json!({"city":"Paris"}))]
        } else {
            assert!(!names.contains(&name));
            let mut calls = vec![function(
                &format!("search_{agent}"),
                "tool_search",
                json!({"query":agent}),
            )];
            if agent == "weather" {
                calls.push(function(
                    "search_weather_again",
                    "tool_search",
                    json!({"query":"weather"}),
                ));
            }
            calls
        }
    } else {
        root_output(input, &names, root_discovery)
    };
    Some(output)
}

fn root_output(input: &[Value], names: &[&str], root_discovery: bool) -> Vec<Value> {
    // A client-output continuation wakes the call owners, not their waiting parent.
    if let Some(index) = input.iter().rposition(|item| {
        item["type"] == "function_call_output"
            && item["output"]
                .as_str()
                .is_some_and(|text| text.contains("Agents are waiting for client tool outputs."))
    }) {
        assert!(
            input[index + 1..].iter().any(|item| {
                item["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Message Type: FINAL_ANSWER"))
            }),
            "waiting parent must receive child mail before resuming inference"
        );
    }
    if root_discovery {
        if !names.contains(&"get_weather") {
            return vec![
                function("search_weather", "tool_search", json!({"query":"weather"})),
                function("search_timezone", "tool_search", json!({"query":"timezone"})),
            ];
        }
        assert!(names.contains(&"agentic_ns__travel__get_timezone"));
    } else {
        assert!(!names.contains(&"get_weather"));
        assert!(!names.contains(&"agentic_ns__travel__get_timezone"));
    }
    if input
        .iter()
        .filter(|item| {
            item["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Message Type: FINAL_ANSWER"))
        })
        .count()
        == 3
    {
        vec![message("all three jobs finished")]
    } else if input
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "spawn_weather")
        || input.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("/root/weather:")
    {
        vec![function(
            &format!("wait_{}", input.len()),
            "wait_agent",
            json!({"timeout_ms":10000}),
        )]
    } else {
        ["weather", "timezone", "echo"]
            .into_iter()
            .map(|agent| {
                function(
                    &format!("spawn_{agent}"),
                    "spawn_agent",
                    json!({"task_name":agent,"message":format!("do {agent}"),"fork_turns":"all"}),
                )
            })
            .collect()
    }
}

async fn rejected_continuation(exec: &Arc<ExecutionContext>, previous: &str, input: Vec<Value>) -> ExecutorError {
    match ExecuteRequest::new(continuation(previous, false, json!(input)), exec.clone())
        .run()
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("continuation should be rejected"),
    }
}

/// Bad batches are rejected atomically, leaving the original stored response usable. Missing and
/// unknown outputs use the single-agent continuation errors.
async fn assert_invalid_batches_are_rejected(exec: &Arc<ExecutionContext>, previous: &str, outputs: &[Value]) {
    let mut wrong_kind = outputs.to_vec();
    let index = wrong_kind
        .iter()
        .position(|item| item["type"] == "custom_tool_call_output")
        .unwrap();
    wrong_kind[index]["type"] = json!("function_call_output");
    let mut duplicate = outputs.to_vec();
    duplicate.push(outputs[0].clone());
    let mut incomplete_search = outputs.to_vec();
    let index = incomplete_search
        .iter()
        .position(|item| item["type"] == "tool_search_output")
        .unwrap();
    incomplete_search[index]["status"] = json!("in_progress");
    for invalid in [wrong_kind, duplicate, incomplete_search] {
        rejected_continuation(exec, previous, invalid).await;
    }
    let mut unknown = outputs.to_vec();
    unknown.push(json!({"type":"custom_tool_call_output","call_id":"unknown","output":"bad"}));
    assert!(matches!(
        rejected_continuation(exec, previous, unknown).await,
        ExecutorError::Tool(ToolError::UnknownCallOutput { call_id }) if call_id == "unknown"
    ));
    assert!(matches!(
        rejected_continuation(exec, previous, outputs[..3].to_vec()).await,
        ExecutorError::Tool(ToolError::MissingOutput { .. })
    ));
}

/// The async continuation cap ends a multi-agent turn as it ends a single-agent response: a model
/// that only starts async jobs stops after two continuations instead of exhausting the round limit.
#[tokio::test]
async fn async_continuation_cap_finishes_the_multi_agent_turn() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let request = RequestPayload {
            model: "test".into(),
            store: true,
            stream,
            input: ResponsesInput::Text(ASYNC_POLLING_PROMPT.into()),
            tools: Some(
                serde_json::from_value(json!([
                    {"type":"function","name":"poll_job","async":true,"parameters":{"type":"object"}}
                ]))
                .unwrap(),
            ),
            multi_agent: Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(1),
            }),
            ..Default::default()
        };
        let result = tokio::time::timeout(Duration::from_secs(10), response(request, exec)).await;
        server.abort();
        let payload = result.unwrap();
        assert_eq!(payload.status, "completed", "stream={stream}");
        let calls: Vec<_> = payload
            .output
            .iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 3, "one round plus two continuations; stream={stream}");
        assert!(calls.iter().all(|call| call.async_execution));
    }
}

fn continuation(previous: &str, stream: bool, input: Value) -> RequestPayload {
    RequestPayload {
        model: "test".into(),
        store: true,
        stream,
        previous_response_id: Some(previous.into()),
        input: serde_json::from_value(input).unwrap(),
        ..Default::default()
    }
}

#[tokio::test]
async fn client_tools_preserve_ownership_discovery_and_namespace_across_continuations() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let first = response(RequestPayload {
                model: "test".into(), store: true, stream,
                parallel_tool_calls: Some(false),
                input: ResponsesInput::Text("discover client tools".into()),
                multi_agent: Some(MultiAgentConfig { enabled: true, max_concurrent_subagents: Some(3) }),
                tools: Some(serde_json::from_value(json!([
                    {"type":"tool_search","execution":"client"}, weather(), travel(), {"type":"custom","name":"echo"}
                ])).unwrap()),
                ..Default::default()
            },exec.clone()).await;
            let outputs: Vec<Value> = first.output.iter().filter_map(|item| match item {
                OutputItem::ToolSearchCall(call) => {
                    let owner = call.agent.as_ref().unwrap().agent_name.as_str();
                    assert!(matches!(owner, "/root/weather" | "/root/timezone"));
                    Some(json!({"type":"tool_search_output", "call_id":call.call_id, "execution":"client", "status":"completed",
                        "tools":[if owner == "/root/weather" { weather() } else { travel() }]}))
                }
                OutputItem::CustomToolCall(call) => {
                    assert_eq!(call.agent.as_ref().unwrap().agent_name,"/root/echo");
                    assert_eq!(call.input,"hello");
                    Some(json!({"type":"custom_tool_call_output","call_id":call.call_id,"output":"CUSTOM_OK"}))
                }
                _ => None,
            }).collect();
            assert_eq!(outputs.len(), 4, "{:#?}", first.output);
            assert_invalid_batches_are_rejected(&exec, &first.id, &outputs).await;
            // Resolve outputs in a different order from the agents' calls.
            let second = response(
                continuation(&first.id, stream, json!(outputs.into_iter().rev().collect::<Vec<_>>())),
                exec.clone(),
            )
            .await;
            let function_outputs: Vec<_> = second
                .output
                .iter()
                .filter_map(|item| match item {
                    OutputItem::FunctionCall(call) => {
                        let owner = &call.agent.as_ref().unwrap().agent_name;
                        let text = if owner == "/root/weather" {
                            assert_eq!(call.name, "get_weather");
                            "weather: clear"
                        } else {
                            assert_eq!(owner, "/root/timezone");
                            assert_eq!(call.name, "get_timezone");
                            assert_eq!(call.namespace.as_deref(), Some("travel"));
                            "timezone: Europe/Paris"
                        };
                        Some(json!({"type":"function_call_output","call_id":call.call_id,"output":text}))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(function_outputs.len(), 2);
            let final_request = continuation(&second.id, stream, json!(function_outputs));
            for request in [final_request.clone(), final_request] {
                let last = response(request, exec.clone()).await;
                assert_eq!(last.status, "completed");
                assert!(last.output.iter().any(|item| matches!(item,OutputItem::Message(message)
                    if message.agent.as_ref().unwrap().agent_name == "/root"
                    && message.content.iter().any(|content| content.text() == "all three jobs finished"))));
                assert!(!last.output.iter().any(|item| matches!(
                    item,
                    OutputItem::FunctionCall(_) | OutputItem::CustomToolCall(_) | OutputItem::ToolSearchCall(_)
                )));
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(30), work).await;
        server.abort();
        result.unwrap();
    }
}

#[tokio::test]
async fn root_discovery_is_inherited_by_children_without_repeating_execution() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let first = response(RequestPayload {
                model: "test".into(), store: true, stream,
                parallel_tool_calls: Some(false),
                input: ResponsesInput::Text("discover tools before delegation".into()),
                multi_agent: Some(MultiAgentConfig { enabled: true, max_concurrent_subagents: Some(3) }),
                tools: Some(serde_json::from_value(json!([
                    {"type":"tool_search","execution":"client"}, weather(), travel(), {"type":"custom","name":"echo"}
                ])).unwrap()),
                ..Default::default()
            }, exec.clone()).await;
            let searches: Vec<_> = first
                .output
                .iter()
                .filter_map(|item| match item {
                    OutputItem::ToolSearchCall(call) => {
                        assert_eq!(call.agent.as_ref().unwrap().agent_name, "/root");
                        Some(json!({"type":"tool_search_output", "call_id":call.call_id,
                        "execution":"client", "status":"completed",
                        "tools":[if call.call_id == "search_weather" { weather() } else { travel() }]}))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(searches.len(), 2);
            let compacted = super::explicit_compaction::compact_checkpoint(&first.id, stream, exec.clone()).await;
            let delegated = response(continuation(&compacted, stream, json!(searches)), exec.clone()).await;
            let outputs: Vec<_> = delegated
                .output
                .iter()
                .filter_map(|item| match item {
                    OutputItem::FunctionCall(call) => {
                        let owner = call.agent.as_ref().unwrap().agent_name.as_str();
                        let text = match owner {
                            "/root/weather" => {
                                assert_eq!(call.name, "get_weather");
                                "weather: clear"
                            }
                            "/root/timezone" => {
                                assert_eq!(call.name, "get_timezone");
                                assert_eq!(call.namespace.as_deref(), Some("travel"));
                                "timezone: Europe/Paris"
                            }
                            _ => panic!("only the assigned children execute discovered functions"),
                        };
                        Some(json!({"type":"function_call_output","call_id":call.call_id,"output":text}))
                    }
                    OutputItem::CustomToolCall(call) => {
                        assert_eq!(call.agent.as_ref().unwrap().agent_name, "/root/echo");
                        Some(json!({"type":"custom_tool_call_output","call_id":call.call_id,"output":"CUSTOM_OK"}))
                    }
                    OutputItem::ToolSearchCall(_) => panic!("children should inherit discovered tools"),
                    _ => None,
                })
                .collect();
            assert_eq!(outputs.len(), 3);
            let compacted = super::explicit_compaction::compact_checkpoint(&delegated.id, stream, exec.clone()).await;
            let last = response(continuation(&compacted, stream, json!(outputs)), exec.clone()).await;
            assert_eq!(last.status, "completed");
            assert!(
                last.output
                    .iter()
                    .any(|item| matches!(item, OutputItem::Message(message)
                if message.agent.as_ref().unwrap().agent_name == "/root"
                && message.content.iter().any(|content| content.text() == "all three jobs finished")))
            );
            assert!(!last.output.iter().any(|item| matches!(
                item,
                OutputItem::FunctionCall(_) | OutputItem::CustomToolCall(_) | OutputItem::ToolSearchCall(_)
            )));
        };
        let result = tokio::time::timeout(Duration::from_secs(30), work).await;
        server.abort();
        result.unwrap();
    }
}
