//! Root finalization regression scenarios.
use super::*;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::types::agent_tree::{AgentPhase, AgentState, StoredTreeSnapshot};
use crate::types::io::{MultiAgentAction, MultiAgentCallOutputContent};

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
