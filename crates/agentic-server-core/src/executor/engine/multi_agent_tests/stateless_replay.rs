//! Client-supplied history is used directly; stateless requests carry no tree snapshot.
use super::*;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::types::io::InOutItem;
use crate::utils::common::deserialize_from_value;

fn request(stream: bool) -> RequestPayload {
    deserialize_from_value(json!({
        "model":"test", "store":false, "stream":stream,
        "input":"simple compaction test", "multi_agent":{"enabled":true,"max_concurrent_subagents":3}
    }))
    .unwrap()
}

#[tokio::test]
async fn attributed_continuation_preserves_resolved_single_agent_history() {
    for conversation in [false, true] {
        for stream in [false, true] {
            let (exec, server) = setup().await;
            let mut seed = request(false);
            seed.store = true;
            seed.multi_agent = None;
            if conversation {
                seed.conversation = Some(exec.conv_handler.create().await.unwrap().conversation_id);
            }
            let previous = response(seed.clone(), exec.clone()).await;
            let mut continuation = request(stream);
            continuation.conversation = seed.conversation;
            if !conversation {
                continuation.previous_response_id = Some(previous.id);
            }
            continuation.input = serde_json::from_value(json!([{
                "type":"agent_message", "author":"/root", "recipient":"/root",
                "content":[{"type":"input_text", "text":"new attributed input"}]
            }]))
            .unwrap();
            let mut ctx = rehydrate_conversation(continuation.clone(), &exec).await.unwrap();
            assert!(ctx.multi_agent_tree.is_none());
            let restored = restore_agents(&mut ctx, &exec, &ExecutorResponseBudget::new()).unwrap();
            let root = restored.agents.iter().find(|agent| agent.identity.is_root()).unwrap();
            let history = serde_json::to_value(&root.history).unwrap();
            let items = history.as_array().unwrap();
            assert!(
                items
                    .iter()
                    .any(|item| has_message_text(item, "simple compaction test")),
                "attributed input must preserve the resolved user history"
            );
            assert!(items.iter().any(|item| has_message_text(item, "finished")));
            assert!(items.iter().any(|item| item["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["type"] == "input_text" && part["text"] == "new attributed input")
            })));
            let completed = response(continuation, exec.clone()).await;
            assert_eq!(completed.status, "completed");
            assert!(exec.resp_handler.retrieve(&completed.id).await.is_err());
            server.abort();
        }
    }
}

#[tokio::test]
async fn complete_history_runs_without_a_synthetic_checkpoint() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let mut request = request(stream);
        let mut history = request.input.clone().into_canonical_items();
        for _ in 0..3 {
            request.input = ResponsesInput::Items(history.clone());
            let completed = response(request.clone(), exec.clone()).await;
            assert_eq!(completed.status, "completed");
            assert!(
                !completed
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::Compaction(_)))
            );
            assert!(exec.resp_handler.retrieve(&completed.id).await.is_err());
            history.extend(completed.output.into_iter().map(InOutItem::Output));
            history.push(deserialize_from_value(json!({"role":"user","content":"continue"})).unwrap());
            let ctx = rehydrate_conversation(request.clone(), &exec).await.unwrap();
            assert!(ctx.multi_agent_tree.is_none());
        }
        server.abort();
    }
}

#[tokio::test]
async fn public_collaboration_history_is_not_a_stored_tree() {
    let (exec, server) = setup().await;
    let mut request = request(false);
    request.input = deserialize_from_value(json!([
        {"role":"user","content":"question"},
        {"type":"multi_agent_call","call_id":"spawn","action":"spawn_agent",
            "arguments":"{\"task_name\":\"review\",\"message\":\"review code\"}","agent":{"agent_name":"/root"}},
        {"type":"agent_message","author":"/root","recipient":"/root/review",
            "content":[{"type":"input_text","text":"review code"}]},
        {"type":"multi_agent_call_output","call_id":"spawn","action":"spawn_agent",
            "output":[{"type":"output_text","text":"{\"task_name\":\"/root/review\"}"}],
            "agent":{"agent_name":"/root"}}
    ]))
    .unwrap();
    let expected = serde_json::to_value(&request.input).unwrap();
    let ctx = rehydrate_conversation(request, &exec).await.unwrap();
    assert!(ctx.multi_agent_tree.is_none());
    assert_eq!(serde_json::to_value(ctx.enriched_request.input).unwrap(), expected);
    server.abort();
}

#[tokio::test]
async fn stateless_multi_agent_trigger_is_rejected_without_a_checkpoint() {
    let (exec, server) = setup().await;
    let mut request = request(false);
    request.input.push(InputItem::CompactionTrigger);
    assert!(matches!(rehydrate_conversation(request, &exec).await,
        Err(ExecutorError::InvalidRequest(message)) if message.contains("compaction_trigger")));
    server.abort();
}

pub(super) fn client_agent_message_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    if !input.iter().any(|item| {
        item["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Client agent message probe:"))
    }) {
        return None;
    }
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    let output = if guidance.contains("You are `/root/token_checker`")
        || input.iter().any(|item| {
            item["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Message Type: FINAL_ANSWER"))
        }) {
        vec![message("CLIENT_AGENT_MESSAGE_9f21")]
    } else if input
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "spawn_probe")
    {
        vec![function(
            &format!("wait_probe_{}", input.len()),
            "wait_agent",
            json!({"timeout_ms":10000}),
        )]
    } else {
        vec![function(
            "spawn_probe",
            "spawn_agent",
            json!({"task_name":"token_checker",
            "message":"Check the supplied client agent message", "fork_turns":"all"}),
        )]
    };
    Some(output)
}

#[tokio::test]
async fn recorded_client_authored_agent_message_is_accepted() {
    for (provider, model) in [("openai-reference", "gpt-5.6-sol"), ("gateway", "Qwen-Qwen3.8-27B-FP8")] {
        for stream in [false, true] {
            let (exec, server) = setup().await;
            let mode = if stream { "streaming" } else { "nonstreaming" };
            let path = format!(
                "{}/tests/cassettes/store_false/multi-agent-store-false-{provider}-client-agent-message-{model}-{mode}.yaml",
                env!("CARGO_MANIFEST_DIR")
            );
            let cassette: Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let request: RequestPayload =
                serde_json::from_value(cassette["turns"][0]["request"]["body"].clone()).unwrap();
            assert!(!request.store);
            assert_eq!(request.stream, stream);
            let result = response(request, exec.clone()).await;
            assert_eq!(result.status, "completed");
            assert!(result.output.iter().any(|item| {
                item.agent()
                    .is_some_and(|agent| agent.agent_name == "/root/token_checker")
            }));
            // Both captures accept and spawn. Their model answers differ, so token
            // delivery is not asserted as a deterministic provider contract.
            assert!(
                !result
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::Compaction(_)))
            );
            assert!(exec.resp_handler.retrieve(&result.id).await.is_err());
            server.abort();
        }
    }
}

#[tokio::test]
async fn stateless_partial_namespace_discovery_reaches_forked_children() {
    let loaded: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/tool_search/returned_tools.json"
    )))
    .unwrap();
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let mut namespace = loaded[1].clone();
        for name in ["get_coordinates", "calculate_distance"] {
            let mut member = namespace["tools"][0].clone();
            member["name"] = json!(name);
            namespace["tools"].as_array_mut().unwrap().push(member);
        }
        let mut request = RequestPayload {
            model: "test".into(),
            store: false,
            stream,
            input: ResponsesInput::Text("discover tools before delegation".into()),
            multi_agent: Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(3),
            }),
            tools: Some(
                serde_json::from_value(json!([
                    {"type":"tool_search","execution":"client"}, loaded[0], namespace, {"type":"custom","name":"echo"}
                ]))
                .unwrap(),
            ),
            ..Default::default()
        };
        let mut history = request.input.clone().into_canonical_items();
        let mut saw_timezone = false;
        let mut finished = false;
        for _ in 0..3 {
            request.input = ResponsesInput::Items(history.clone());
            let completed = response(request.clone(), exec.clone()).await;
            assert!(exec.resp_handler.retrieve(&completed.id).await.is_err());
            let mut outputs = Vec::new();
            for item in &completed.output {
                let output = match item {
                    OutputItem::ToolSearchCall(call) => Some(json!({
                        "type":"tool_search_output", "call_id":call.call_id,
                        "execution":"client", "status":"completed", "tools":loaded
                    })),
                    OutputItem::FunctionCall(call) => {
                        let result = match call.name.as_str() {
                            "get_weather" => "weather: clear",
                            "get_timezone" => {
                                assert_eq!(call.namespace.as_deref(), Some("travel"));
                                assert_eq!(call.agent.as_ref().unwrap().agent_name, "/root/timezone");
                                saw_timezone = true;
                                "timezone: Europe/Paris"
                            }
                            other => panic!("unexpected client function {other}"),
                        };
                        Some(json!({"type":"function_call_output", "call_id":call.call_id, "output":result}))
                    }
                    OutputItem::CustomToolCall(call) => Some(json!({
                        "type":"custom_tool_call_output", "call_id":call.call_id, "output":"CUSTOM_OK"
                    })),
                    OutputItem::Message(message) => {
                        finished |= message.agent.as_ref().unwrap().agent_name == "/root"
                            && message
                                .content
                                .iter()
                                .any(|part| part.text() == "all three jobs finished");
                        None
                    }
                    _ => None,
                };
                outputs.extend(output.map(|item| serde_json::from_value(item).unwrap()));
            }
            history.extend(completed.output.into_iter().map(InOutItem::Output));
            history.extend(outputs);
        }
        assert!(
            saw_timezone && finished,
            "the loaded namespace must remain callable in children"
        );
        // Discovery metadata may be returned for an idle agent. It does not start a new turn.
        for name in ["weather", "timezone", "echo"] {
            history.push(
                serde_json::from_value(json!({
                    "type":"mcp_list_tools", "id":format!("mcp_list_{name}"),
                    "server_label":"docs", "tools":[], "agent":{"agent_name":format!("/root/{name}")}
                }))
                .unwrap(),
            );
        }
        history.push(serde_json::from_value(json!({"role":"user", "content":"Start a new task"})).unwrap());
        request.input = ResponsesInput::Items(history);
        let mut ctx = rehydrate_conversation(request, &exec).await.unwrap();
        let restored = restore_agents(&mut ctx, &exec, &ExecutorResponseBudget::new()).unwrap();
        assert_eq!(
            restored.registry.active_subagents(),
            0,
            "discovery must not wake completed children"
        );
        server.abort();
    }
}
