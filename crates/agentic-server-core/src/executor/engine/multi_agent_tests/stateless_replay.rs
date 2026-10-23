//! Stateless compaction authenticates agent state and applies only new work.
use super::*;
use crate::executor::rehydrate::rehydrate_conversation;
use crate::types::io::InOutItem;
use crate::utils::common::deserialize_from_value;

fn request(stream: bool, shell: bool) -> RequestPayload {
    deserialize_from_value(json!({
        "model":"test", "store":false, "stream":stream,
        "input":if shell { "local shell review" } else { "simple compaction test" },
        "multi_agent":{"enabled":true,"max_concurrent_subagents":3},
        "tools":if shell { json!([{"type":"shell","environment":{"type":"local"}}]) } else { json!([]) }
    }))
    .unwrap()
}

#[tokio::test]
async fn replay_compaction_uses_public_fields_and_supersedes_its_prefix() {
    let (exec, server) = setup().await;
    let mut request = request(false, false);
    let first = response(request.clone(), exec.clone()).await;
    let checkpoint = serde_json::to_value(first.output.last().unwrap()).unwrap();
    assert_eq!(checkpoint["type"], "compaction");
    assert_eq!(checkpoint["agent"]["agent_name"], "/root");
    assert!(checkpoint["id"].as_str().unwrap().starts_with("cmp_"));
    assert!(checkpoint["encrypted_content"].is_string());
    assert_eq!(checkpoint.as_object().unwrap().len(), 4);
    let mut history = request.input.clone().into_canonical_items();
    history.extend(first.output.into_iter().map(InOutItem::Output));
    history.insert(
        0,
        deserialize_from_value(json!({"role":"user","content":"superseded history"})).unwrap(),
    );
    request.input = deserialize_from_value(serde_json::to_value(history).unwrap()).unwrap();
    let full = rehydrate_conversation(request.clone(), &exec).await.unwrap();
    let expected = serde_json::to_value(full.multi_agent_tree.unwrap().snapshot()).unwrap();
    for keep_attribution in [true, false] {
        let mut item = checkpoint.clone();
        if !keep_attribution {
            item.as_object_mut().unwrap().remove("agent");
        }
        request.input = deserialize_from_value(json!([item])).unwrap();
        let pruned = rehydrate_conversation(request.clone(), &exec).await.unwrap();
        assert_eq!(
            expected,
            serde_json::to_value(pruned.multi_agent_tree.unwrap().snapshot()).unwrap()
        );
    }
    let mut item = checkpoint;
    item["agent"]["agent_name"] = json!("/root/other");
    request.input = deserialize_from_value(json!([item])).unwrap();
    assert!(matches!(rehydrate_conversation(request, &exec).await,
        Err(ExecutorError::InvalidRequest(message)) if message.contains("root compaction item")));
    server.abort();
}

#[tokio::test]
async fn replay_requires_state_even_without_explicit_multi_agent_config() {
    let (exec, server) = setup().await;
    let mut request = request(false, true);
    let first = response(request.clone(), exec.clone()).await;
    let mut history = request.input.clone().into_canonical_items();
    history.extend(first.output.into_iter().map(InOutItem::Output));
    history.pop(); // Remove the checkpoint, retaining the public collaboration transcript.
    request.multi_agent = None;
    request.input = deserialize_from_value(serde_json::to_value(history).unwrap()).unwrap();
    assert!(matches!(rehydrate_conversation(request, &exec).await,
        Err(ExecutorError::InvalidRequest(message)) if message.contains("final compaction item")));
    server.abort();
}

#[tokio::test]
async fn replay_can_prune_before_latest_compaction_in_long_sessions() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let mut request = request(stream, false);
        let mut history = request.input.clone().into_canonical_items();
        for turn in 0..60 {
            let message: InOutItem = deserialize_from_value(json!({
                "role":"user", "content":format!("Turn {turn}: {}", "x".repeat(20_000))
            }))
            .unwrap();
            history.push(message);
            request.input = deserialize_from_value(serde_json::to_value(&history).unwrap()).unwrap();
            let mut completed = response(request.clone(), exec.clone()).await;
            assert_eq!(completed.status, "completed", "turn {turn}");
            let checkpoint = completed.output.pop().unwrap();
            assert!(matches!(checkpoint, OutputItem::Compaction(_)));
            history = vec![InOutItem::Output(checkpoint)];
            assert!(exec.resp_handler.retrieve(&completed.id).await.is_err());
        }
        server.abort();
    }
}

#[tokio::test]
async fn replay_accepts_full_or_partially_pruned_history_before_compaction() {
    let (exec, server) = setup().await;
    let mut request = request(false, false);
    let mut history = request.input.clone().into_canonical_items();
    for _ in 0..3 {
        request.input = deserialize_from_value(serde_json::to_value(&history).unwrap()).unwrap();
        let completed = response(request.clone(), exec.clone()).await;
        assert_eq!(completed.status, "completed");
        history.extend(completed.output.into_iter().map(InOutItem::Output));
        history.push(deserialize_from_value(json!({"role":"user","content":"continue"})).unwrap());
    }
    // Keeping all old checkpoint envelopes remains supported.
    request.input = deserialize_from_value(serde_json::to_value(&history).unwrap()).unwrap();
    let restored = rehydrate_conversation(request.clone(), &exec).await.unwrap();
    let expected = serde_json::to_value(restored.multi_agent_tree.unwrap().snapshot()).unwrap();
    for mutation in ["delete", "reorder", "alter"] {
        let mut damaged = history.clone();
        match mutation {
            "delete" => {
                damaged.remove(0);
            }
            "reorder" => damaged.swap(0, 1),
            "alter" => damaged[0] = deserialize_from_value(json!({"role":"user","content":"altered"})).unwrap(),
            _ => unreachable!(),
        }
        request.input = deserialize_from_value(serde_json::to_value(damaged).unwrap()).unwrap();
        let restored = rehydrate_conversation(request.clone(), &exec).await.unwrap();
        assert_eq!(
            serde_json::to_value(restored.multi_agent_tree.unwrap().snapshot()).unwrap(),
            expected,
            "{mutation}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn replay_to_stored_compaction_applies_only_new_tool_outputs() {
    for (stream, trigger_first) in [(false, false), (false, true), (true, false), (true, true)] {
        let (exec, server) = setup().await;
        let mut request = request(stream, true);
        let first = response(request.clone(), exec.clone()).await;
        let mut history = request.input.clone().into_canonical_items();
        history.extend(first.output.into_iter().map(InOutItem::Output));
        let result = deserialize_from_value(json!({
            "type":"shell_call_output", "call_id":"shell_sum",
            "output":[{"stdout":"55","stderr":"","outcome":{"type":"exit","exit_code":0}}]
        }))
        .unwrap();
        let trigger = InOutItem::Input(InputItem::CompactionTrigger);
        history.extend(if trigger_first {
            [trigger, result]
        } else {
            [result, trigger]
        });
        request.input = deserialize_from_value(serde_json::to_value(history).unwrap()).unwrap();
        request.multi_agent = None; // Restore the mode from the client-carried checkpoint.
        request.store = true;
        let compacted = response(request.clone(), exec.clone()).await;
        assert_eq!(compacted.status, "completed");
        assert!(matches!(compacted.output.as_slice(), [OutputItem::Compaction(_)]));
        assert_eq!(compacted.usage.unwrap().total_tokens, 3);

        request.input = ResponsesInput::Items(Vec::new());
        request.previous_response_id = Some(compacted.id);
        let restored = rehydrate_conversation(request.clone(), &exec).await.unwrap();
        let tree = restored.multi_agent_tree.unwrap().into_snapshot();
        let child = tree.agents.iter().find(|agent| !agent.identity.is_root()).unwrap();
        assert_eq!(
            child
                .history
                .iter()
                .filter(|item| matches!(item,
                InputItem::ShellCallOutput(output) if output.call_id == "shell_sum"))
                .count(),
            1,
            "the new tool output is delivered to its owning child exactly once"
        );
        let completed = response(request, exec.clone()).await;
        assert_eq!(completed.status, "completed");
        assert!(
            serde_json::to_string(&completed.output)
                .unwrap()
                .contains("combined shell result is 55")
        );
        server.abort();
    }
}
