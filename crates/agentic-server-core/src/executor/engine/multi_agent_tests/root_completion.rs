//! Root finalization regression scenarios.
use super::*;

pub(super) fn root_completion_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    if !input.iter().any(|item| item["content"] == "finish while child waits") {
        return None;
    }
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    if guidance.contains("You are `/root/worker`") {
        assert!(
            !input.iter().any(|item| item["call_id"] == "worker_wait"),
            "child must not run again after root completion"
        );
        return Some(vec![
            function(
                "report",
                "send_message",
                json!({"target":"/root","message":"work done"}),
            ),
            function("worker_wait", "wait_agent", json!({"timeout_ms":120_000})),
        ]);
    }
    if input
        .iter()
        .any(|item| item["content"].as_str().is_some_and(|text| text.contains("work done")))
    {
        Some(vec![message("combined answer")])
    } else if input.iter().any(|item| item["call_id"] == "spawn_worker") {
        Some(vec![function("root_wait", "wait_agent", json!({"timeout_ms":120_000}))])
    } else {
        Some(vec![function(
            "spawn_worker",
            "spawn_agent",
            json!({"task_name":"worker","message":"do work","fork_turns":"all"}),
        )])
    }
}

#[tokio::test]
async fn root_final_answer_settles_child_mailbox_wait_json_and_sse() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = response(
            RequestPayload {
                model: "test".into(),
                store: true,
                stream,
                input: ResponsesInput::Text("finish while child waits".into()),
                multi_agent: Some(MultiAgentConfig {
                    enabled: true,
                    max_concurrent_subagents: Some(3),
                }),
                ..Default::default()
            },
            exec,
        );
        let result = tokio::time::timeout(Duration::from_secs(5), work).await;
        server.abort();
        let response = result.expect("root final answer must not wait for child's mailbox timeout");
        assert_eq!(response.status, "completed");
        assert!(response.output.iter().any(|item| matches!(item,
            OutputItem::MultiAgentCallOutput(output) if output.call_id == "worker_wait")));
        assert!(response.output.iter().any(|item| matches!(item,
            OutputItem::Message(message) if message.phase == Some(MessagePhase::FinalAnswer)
                && message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root"))));
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
