//! Client shell continuation and public multi-agent lifecycle contracts.
use super::*;
use crate::StorageError;
use crate::executor::{prepare_non_generating_turn, rehydrate::rehydrate_conversation};
use crate::types::io::{AgentMessageContent, InOutItem};
use crate::utils::common::deserialize_from_value;

pub(super) fn shell_review_output(request: &Value) -> Option<Vec<Value>> {
    let input = request["input"].as_array().unwrap();
    // Root compaction can replace the original prompt before the child replies.
    if !input.iter().any(|item| {
        item["content"] == "local shell review"
            || item["content"].as_str().is_some_and(|text| {
                text.starts_with("Message Type: FINAL_ANSWER") && text.contains("shell result is 55")
            })
    }) {
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
            // Listing ignores model-generated wrappers and returns the actual tree.
            vec![function(
                "list_after_shell",
                "list_agents",
                json!({"parameters": {}, "function_results": {"fake_agent": "completed"}}),
            )]
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
            // Output-only history cannot be silently discarded during continuation validation.
            for item in [
                json!({"type":"web_search_call", "id":"ws_1", "status":"completed",
                    "action":{"type":"search", "query":"weather"}}),
                json!({"type":"mcp_call", "id":"mcp_1", "name":"weather",
                    "server_label":"weather", "arguments":"{}", "output":"sunny", "status":"completed"}),
            ] {
                let mut invalid = continuation.clone();
                invalid.input.push(serde_json::from_value::<InOutItem>(item).unwrap());
                let ctx = rehydrate_conversation(invalid, &exec).await.unwrap();
                assert!(matches!(
                    prepare_non_generating_turn(ctx, &exec),
                    Err(ExecutorError::InvalidRequest(message))
                        if message.contains("multi-agent continuation accepts")
                ));
            }
            // Rejected attempts must leave the original pending shell ownership intact.
            for store in [true, false] {
                let mut request = continuation.clone();
                request.store = store;
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
                if !store {
                    assert!(matches!(
                        exec.resp_handler.retrieve(&completed.id).await,
                        Err(ExecutorError::Storage(StorageError::ResponseNotFound { .. }))
                    ));
                }
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(15), work).await;
        server.abort();
        result.unwrap();
    }
}

#[tokio::test]
async fn child_shell_call_replays_full_history_without_storage() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let mut context = (*exec).clone();
        context.conv_handler = ConversationHandler::new(ConversationStore::new(pool.clone()));
        context.resp_handler = ResponseHandler::new(ResponseStore::new(pool.clone()));
        let exec = Arc::new(context);
        let request: RequestPayload = deserialize_from_value(json!({
            "model":"test", "store":false, "stream":stream, "input":"local shell review",
            "multi_agent":{"enabled":true,"max_concurrent_subagents":3},
            "tools":[{"type":"shell","environment":{"type":"local"}}]
        }))
        .unwrap();
        let first = response(request.clone(), exec.clone()).await;
        let call = first
            .output
            .iter()
            .find_map(|item| match item {
                OutputItem::ShellCall(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.agent.as_ref().unwrap().agent_name, "/root/shell_worker");
        let mut history = request.input.clone().into_canonical_items();
        history.extend(first.output.clone().into_iter().map(InOutItem::Output));
        history.push(
            deserialize_from_value(json!({
                "type":"shell_call_output", "call_id":call.call_id,
                "output":[{"stdout":"55","stderr":"","outcome":{"type":"exit","exit_code":0}}]
            }))
            .unwrap(),
        );
        // Exercise the actual public JSON input decoder, including output -> input conversion.
        let mut next = request.clone();
        next.input = deserialize_from_value(serde_json::to_value(&history).unwrap()).unwrap();
        assert!(
            !first
                .output
                .iter()
                .any(|item| matches!(item, OutputItem::Compaction(_)))
        );
        let mut triggered = next.clone();
        triggered.input.push(InputItem::CompactionTrigger);
        assert!(matches!(rehydrate_conversation(triggered, &exec).await,
            Err(ExecutorError::InvalidRequest(message)) if message.contains("compaction_trigger")));

        // Each request is independent and carries all public history, with no private checkpoint.
        for _ in 0..2 {
            let completed = response(next.clone(), exec.clone()).await;
            assert_eq!(completed.status, "completed");
            assert!(
                completed
                    .output
                    .iter()
                    .any(|item| matches!(item, OutputItem::Message(message)
                if message.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root/shell_worker")
                    && message.phase == Some(MessagePhase::FinalAnswer)))
            );
            assert!(
                serde_json::to_string(&completed.output)
                    .unwrap()
                    .contains("combined shell result is 55")
            );
        }
        let mut disabled = (*exec).clone();
        disabled.conv_handler = ConversationHandler::new(ConversationStore::disabled());
        disabled.resp_handler = ResponseHandler::new(ResponseStore::disabled());
        assert_eq!(response(next, Arc::new(disabled)).await.status, "completed");
        for table in ["responses", "items", "conversations"] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
            assert_eq!(count, 0, "store:false wrote {table}");
        }
        server.abort();
    }
}
