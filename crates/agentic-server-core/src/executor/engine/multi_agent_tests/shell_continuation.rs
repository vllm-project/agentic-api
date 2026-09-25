//! Client shell continuation and public multi-agent lifecycle contracts.
use super::*;
use crate::types::io::AgentMessageContent;

pub(super) fn shell_review_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    if !input.iter().any(|item| item["content"] == "local shell review") {
        return None;
    }
    let guidance = input.last().unwrap()["content"].as_str().unwrap();
    let output = if guidance.contains("You are `/root/shell_worker`") {
        if let Some(output) = input
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "shell_sum")
        {
            assert!(output["output"].as_str().unwrap().contains("55"));
            vec![message("shell result is 55")]
        } else {
            vec![function("shell_sum", "shell", json!({"commands":["printf 55"]}))]
        }
    } else if input.iter().any(|item| {
        item["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Message Type: FINAL_ANSWER"))
    }) {
        if let Some(result) = input
            .iter()
            .find(|item| item["call_id"] == "list_after_shell" && item["type"] == "function_call_output")
        {
            let listing: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            let agents = listing["agents"].as_array().unwrap();
            assert_eq!(agents.len(), 2);
            assert!(agents.contains(&json!({"agent_name":"/root", "agent_status":"running"})));
            assert!(agents.contains(&json!({"agent_name":"/root/shell_worker",
                "agent_status":{"completed":"shell result is 55"}})));
            vec![message("combined shell result is 55")]
        } else {
            vec![function("list_after_shell", "list_agents", json!({}))]
        }
    } else if input
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "spawn_shell")
    {
        vec![function(
            &format!("wait_shell_{}", input.len()),
            "wait_agent",
            json!({"timeout_ms":10000}),
        )]
    } else {
        vec![function(
            "spawn_shell",
            "spawn_agent",
            json!({"task_name":"shell_worker","message":"run printf 55","fork_turns":"all"}),
        )]
    };
    Some(output)
}

#[tokio::test]
async fn child_shell_call_survives_checkpoint_and_client_continuation() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let work = async {
            let first = response(
                RequestPayload {
                    model: "test".into(),
                    store: true,
                    stream,
                    input: ResponsesInput::Text("local shell review".into()),
                    multi_agent: Some(MultiAgentConfig {
                        enabled: true,
                        max_concurrent_subagents: Some(3),
                    }),
                    tools: Some(
                        serde_json::from_value(json!([{"type":"shell","environment":{"type":"local"}}])).unwrap(),
                    ),
                    ..Default::default()
                },
                exec.clone(),
            )
            .await;
            let assignments: Vec<_> = first
                .output
                .iter()
                .filter_map(|item| match item {
                    OutputItem::AgentMessage(message) if message.author == "/root" => Some(message),
                    _ => None,
                })
                .collect();
            assert_eq!(assignments.len(), 1);
            let assignment = assignments[0];
            assert_eq!(assignment.recipient, "/root/shell_worker");
            assert_eq!(assignment.agent.as_ref().unwrap().agent_name, "/root/shell_worker");
            assert!(matches!(assignment.content.as_slice(),
                [AgentMessageContent::EncryptedContent { encrypted_content }]
                if !encrypted_content.is_empty() && encrypted_content != "run printf 55"));
            let call = first
                .output
                .iter()
                .find_map(|item| match item {
                    OutputItem::ShellCall(call) => Some(call),
                    _ => None,
                })
                .expect("pending shell call");
            assert_eq!(call.agent.as_ref().unwrap().agent_name, "/root/shell_worker");
            let continuation = RequestPayload {
                model: "test".into(),
                store: true,
                stream,
                previous_response_id: Some(first.id),
                input: serde_json::from_value(json!([{
                    "type":"shell_call_output", "call_id":call.call_id,
                    "output":[{"stdout":"55","stderr":"","outcome":{"type":"exit","exit_code":0}}]
                }]))
                .unwrap(),
                ..Default::default()
            };
            // Both continuations restore the original pending shell ownership.
            for request in [continuation.clone(), continuation] {
                let completed = response(request, exec.clone()).await;
                assert_eq!(completed.status, "completed");
                assert!(
                    completed
                        .output
                        .iter()
                        .any(|item| matches!(item, OutputItem::Message(message)
                    if message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root/shell_worker")
                        && message.phase == Some(MessagePhase::FinalAnswer)))
                );
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(15), work).await;
        server.abort();
        result.unwrap();
    }
}
