//! Root finalization regression scenarios.
use super::*;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::types::agent_tree::{AgentPhase, AgentState, StoredTreeSnapshot};
use crate::types::io::{InOutItem, MultiAgentAction, MultiAgentCallOutputContent};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

pub(super) fn root_completion_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    let scenario = input.iter().find_map(|item| {
        item["content"]
            .as_str()
            .filter(|text| text.starts_with("finish while child "))
    })?;
    let client_call = scenario.ends_with("needs client output");
    let explicit_interrupt = scenario.ends_with("is explicitly interrupted");
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    let has_output = |id| {
        input
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == id)
    };
    if guidance.contains("You are `/root/worker`") {
        if has_output("worker_wait") || has_output("worker_client") {
            assert!(!explicit_interrupt, "explicitly interrupted child must stay stopped");
            return Some(vec![message("child finished")]);
        }
        return Some(vec![
            function(
                "report",
                "send_message",
                json!({"target":"/root","message":"progress update"}),
            ),
            if client_call {
                function("worker_client", "get_data", json!({}))
            } else {
                function("worker_wait", "wait_agent", json!({"timeout_ms":10_000}))
            },
        ]);
    }
    assert!(
        !input.iter().any(|item| item["phase"] == "final_answer"),
        "mail must not restart the finished root"
    );
    if input.iter().any(|item| {
        item["content"]
            .as_str()
            .is_some_and(|text| text.contains("progress update"))
    }) {
        if explicit_interrupt && !has_output("stop_worker") {
            Some(vec![function(
                "stop_worker",
                "interrupt_agent",
                json!({"target":"worker"}),
            )])
        } else {
            Some(vec![message("root finished")])
        }
    } else if has_output("spawn_worker") {
        Some(vec![function("root_wait", "wait_agent", json!({"timeout_ms":120_000}))])
    } else {
        Some(vec![function(
            "spawn_worker",
            "spawn_agent",
            json!({"task_name":"worker","message":"do work","fork_turns":"all"}),
        )])
    }
}

fn completion_request(scenario: &str, stream: bool) -> RequestPayload {
    RequestPayload {
        model: "test".into(),
        store: true,
        stream,
        input: ResponsesInput::Text(format!("finish while child {scenario}")),
        multi_agent: Some(MultiAgentConfig {
            enabled: true,
            max_concurrent_subagents: Some(3),
        }),
        tools: Some(
            serde_json::from_value(json!([{
                "type":"function","name":"get_data","parameters":{"type":"object","properties":{}}
            }]))
            .unwrap(),
        ),
        ..Default::default()
    }
}

async fn stored_tree(id: &str, exec: &ExecutionContext) -> StoredTreeSnapshot {
    rehydrate_conversation(
        RequestPayload {
            model: "test".into(),
            store: true,
            previous_response_id: Some(id.into()),
            input: ResponsesInput::Items(vec![]),
            ..Default::default()
        },
        exec,
    )
    .await
    .unwrap()
    .multi_agent_tree
    .unwrap()
    .into_snapshot()
}

fn final_position(response: &ResponsePayload, agent: &str) -> usize {
    response
        .output
        .iter()
        .position(|item| {
            matches!(item, OutputItem::Message(message)
        if message.phase == Some(MessagePhase::FinalAnswer)
        && message.agent.as_ref().is_some_and(|owner| owner.agent_name == agent))
        })
        .expect("agent final answer")
}

#[tokio::test]
async fn root_final_answer_keeps_child_mailbox_wait_and_later_round_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let completed = response(completion_request("waits", stream), exec.clone()).await;
            assert_eq!(completed.status, "completed");
            assert!(final_position(&completed, "/root") < final_position(&completed, "/root/worker"));
            let wait = completed
                .output
                .iter()
                .find_map(|item| match item {
                    OutputItem::MultiAgentCallOutput(output) if output.call_id == "worker_wait" => Some(output),
                    _ => None,
                })
                .unwrap();
            let MultiAgentCallOutputContent::OutputText(text) = &wait.output[0];
            let result: Value = serde_json::from_str(&text.text).unwrap();
            assert_eq!(result["timed_out"], true, "root completion must not interrupt the wait");
            assert!(!completed.output.iter().any(|item| matches!(item,
                OutputItem::MultiAgentCall(call) if call.action == MultiAgentAction::InterruptAgent)));
            let tree = stored_tree(&completed.id, &exec).await;
            assert!(tree.agents.iter().all(|agent| agent.state == AgentState::Idle));
            assert!(tree.agents.iter().all(|agent| agent.wait.is_none()));
        };
        let result = tokio::time::timeout(Duration::from_secs(20), work).await;
        server.abort();
        result.expect("child must finish its own turn after the 10-second mailbox timeout");
    }
}

#[tokio::test]
async fn root_final_answer_preserves_child_client_pause_and_continuation_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let first = response(completion_request("needs client output", stream), exec.clone()).await;
            assert_eq!(first.status, "completed");
            final_position(&first, "/root");
            let tree = stored_tree(&first.id, &exec).await;
            let root = tree.agents.iter().find(|agent| agent.identity.is_root()).unwrap();
            assert_eq!(root.state, AgentState::Idle);
            let child = tree.agents.iter().find(|agent| !agent.identity.is_root()).unwrap();
            assert_eq!(child.state, AgentState::Active(AgentPhase::WaitingForClientOutputs));
            assert!(
                tree.client_calls
                    .iter()
                    .any(|call| call.call_id.as_str() == "worker_client"
                        && !call.resolved
                        && call.owner.agent_turn.agent == child.identity)
            );
            let completed = response(
                RequestPayload {
                    model: "test".into(),
                    store: true,
                    stream,
                    previous_response_id: Some(first.id),
                    input: serde_json::from_value(json!([{
                        "type":"function_call_output","call_id":"worker_client","output":"data"
                    }]))
                    .unwrap(),
                    ..Default::default()
                },
                exec.clone(),
            )
            .await;
            assert_eq!(completed.status, "completed");
            final_position(&completed, "/root/worker");
            assert!(
                !completed
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::Message(message)
                if message.agent.as_ref().is_some_and(|owner| owner.agent_name == "/root")))
            );
            let after = stored_tree(&completed.id, &exec).await;
            assert!(after.agents.iter().all(|agent| agent.state == AgentState::Idle));
            assert!(after.client_calls.iter().all(|call| call.resolved));
            assert_eq!(
                after.agents.iter().find(|agent| agent.identity.is_root()).unwrap().turn,
                root.turn
            );
        };
        let result = tokio::time::timeout(Duration::from_secs(10), work).await;
        server.abort();
        result.expect("client continuation must resume the child without rerunning root");
    }
}

#[tokio::test]
async fn explicit_interrupt_still_settles_child_wait_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let completed = response(completion_request("is explicitly interrupted", stream), exec.clone()).await;
            assert_eq!(completed.status, "completed");
            assert!(
                completed
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::MultiAgentCall(call)
                if call.action == MultiAgentAction::InterruptAgent && call.call_id == "stop_worker"))
            );
            let tree = stored_tree(&completed.id, &exec).await;
            assert_eq!(
                tree.agents
                    .iter()
                    .find(|agent| !agent.identity.is_root())
                    .unwrap()
                    .state,
                AgentState::Interrupted
            );
            assert!(tree.agents.iter().all(|agent| agent.wait.is_none()));
        };
        let result = tokio::time::timeout(Duration::from_secs(5), work).await;
        server.abort();
        result.expect("explicit interruption must resolve the wait without its timeout");
    }
}

async fn setup_interrupt_boundary(
    client_call: bool,
) -> (Arc<ExecutionContext>, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let child_rounds = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/responses",
        post({
            let started = started.clone();
            let release = release.clone();
            let child_rounds = child_rounds.clone();
            move |Json(request): Json<Value>| {
                let started = started.clone();
                let release = release.clone();
                let child_rounds = child_rounds.clone();
                async move {
                    let input = request["input"].as_array().unwrap();
                    let has_output = |id: &str| {
                        input
                            .iter()
                            .any(|item| item["type"] == "function_call_output" && item["call_id"] == id)
                    };
                    let child = input.last().unwrap()["content"]
                        .as_str()
                        .unwrap()
                        .contains("You are `/root/worker`");
                    let output = if child {
                        assert_eq!(
                            child_rounds.fetch_add(1, Ordering::SeqCst),
                            0,
                            "interrupted child must not start another round"
                        );
                        started.notify_one();
                        release.acquire().await.unwrap().forget();
                        let mut output = vec![message("retained boundary output")];
                        if client_call {
                            output.push(function("boundary_client", "get_data", json!({})));
                        }
                        output
                    } else if input
                        .iter()
                        .any(|item| has_message_text(item, "continue after interrupt"))
                    {
                        if has_output("list_after_interrupt") {
                            let result = input
                                .iter()
                                .find(|item| {
                                    item["call_id"] == "list_after_interrupt" && item["type"] == "function_call_output"
                                })
                                .unwrap();
                            assert!(result["output"].as_str().unwrap().contains("\"interrupted\""));
                            vec![message("continued without restarting child")]
                        } else {
                            vec![function("list_after_interrupt", "list_agents", json!({}))]
                        }
                    } else if has_output("stop_worker") {
                        // This request proves the coordinator has published the interrupt
                        // result before allowing the child's owned round to complete.
                        release.add_permits(1);
                        vec![message("root finished")]
                    } else if has_output("spawn_worker") {
                        started.notified().await;
                        vec![function("stop_worker", "interrupt_agent", json!({"target":"worker"}))]
                    } else {
                        vec![function(
                            "spawn_worker",
                            "spawn_agent",
                            json!({
                                "task_name":"worker", "message":"do work", "fork_turns":"all"
                            }),
                        )]
                    };
                    let payload = json!({"id":"upstream", "object":"response", "created_at":0,
                        "model":"test", "status":"completed", "output":output});
                    if request["stream"] == true {
                        ([("content-type", "text/event-stream")], upstream_events(&payload)).into_response()
                    } else {
                        Json(payload).into_response()
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (mut exec, original_server) = setup().await;
    original_server.abort();
    Arc::make_mut(&mut exec).llm_base_url = format!("http://{address}");
    (exec, server, child_rounds)
}

#[tokio::test]
async fn stateless_interrupt_retains_inflight_boundary_without_restarting_child() {
    for stream in [false, true] {
        for client_call in [false, true] {
            let (exec, server, child_rounds) = setup_interrupt_boundary(client_call).await;
            let work = async {
                let mut request = completion_request("interrupt boundary", stream);
                request.store = false;
                let mut history = request.input.clone().into_canonical_items();
                let first = response(request.clone(), exec.clone()).await;
                let interrupted = first
                    .output
                    .iter()
                    .position(|item| {
                        matches!(item,
                    OutputItem::MultiAgentCallOutput(output) if output.call_id == "stop_worker")
                    })
                    .unwrap();
                let boundary = first
                    .output
                    .iter()
                    .position(|item| {
                        matches!(item,
                    OutputItem::Message(message) if message.agent.as_ref()
                        .is_some_and(|agent| agent.agent_name == "/root/worker"))
                    })
                    .unwrap();
                assert!(
                    interrupted < boundary,
                    "the capture must contain post-interrupt boundary output"
                );
                let boundary_id = first.output[boundary].id().unwrap().to_owned();
                assert!(exec.resp_handler.retrieve(&first.id).await.is_err());
                history.extend(first.output.into_iter().map(InOutItem::Output));
                if client_call {
                    history.push(
                        serde_json::from_value(json!({
                            "type":"function_call_output", "call_id":"boundary_client", "output":"late data"
                        }))
                        .unwrap(),
                    );
                }
                history.push(
                    serde_json::from_value(json!({
                        "role":"user", "content":"continue after interrupt"
                    }))
                    .unwrap(),
                );
                request.input = ResponsesInput::Items(history);
                let mut context = rehydrate_conversation(request.clone(), &exec).await.unwrap();
                let restored = restore_agents(&mut context, &exec, &ExecutorResponseBudget::new()).unwrap();
                let child = restored.agents.iter().find(|agent| !agent.identity.is_root()).unwrap();
                assert_eq!(
                    restored.registry.get(&child.identity).unwrap().state,
                    AgentState::Interrupted
                );
                assert!(
                    child.history.iter().any(|item| item.id() == Some(boundary_id.as_str())),
                    "the interrupted round's message must remain in its owner's history"
                );
                if client_call {
                    assert!(child.history.iter().any(|item| matches!(item,
                        InputItem::FunctionCallOutput(output) if output.call_id == "boundary_client")));
                }
                let completed = response(request, exec.clone()).await;
                assert_eq!(completed.status, "completed");
                assert!(
                    completed
                        .output
                        .iter()
                        .all(|item| item.agent().is_none_or(|agent| agent.agent_name == "/root"))
                );
                assert_eq!(child_rounds.load(Ordering::SeqCst), 1);
                assert!(exec.resp_handler.retrieve(&completed.id).await.is_err());
            };
            let result = tokio::time::timeout(Duration::from_secs(10), work).await;
            server.abort();
            result.expect("interrupted boundary and its stateless continuation must finish");
        }
    }
}

// A completed upstream round with only reasoning is not a final agent answer.
pub(super) fn reasoning_only_recovery_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    if !input.iter().any(|item| item["content"] == "reasoning-only recovery") {
        return None;
    }
    if input.iter().any(|item| item["type"] == "reasoning") {
        Some(vec![message("actual final answer")])
    } else {
        Some(vec![json!({"type":"reasoning", "id":uuid7_str("rs_"),
            "content":[{"type":"reasoning_text","text":"still thinking"}],
            "summary":[],"encrypted_content":null,"status":"completed"})])
    }
}

#[tokio::test]
async fn reasoning_only_round_does_not_finish_root_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = response(
            RequestPayload {
                model: "test".into(),
                store: true,
                stream,
                input: ResponsesInput::Text("reasoning-only recovery".into()),
                multi_agent: Some(MultiAgentConfig {
                    enabled: true,
                    max_concurrent_subagents: Some(1),
                }),
                ..Default::default()
            },
            exec,
        );
        let result = tokio::time::timeout(Duration::from_secs(5), work).await;
        server.abort();
        let completed = result.expect("reasoning-only round must continue to a final answer");
        assert_eq!(completed.status, "completed");
        assert!(completed.output.iter().any(|item| matches!(item,
            OutputItem::Reasoning(reasoning) if reasoning.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root"))));
        assert!(completed.output.iter().any(|item| matches!(item,
            OutputItem::Message(message) if message.phase == Some(MessagePhase::FinalAnswer)
                && message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root"))));
    }
}
