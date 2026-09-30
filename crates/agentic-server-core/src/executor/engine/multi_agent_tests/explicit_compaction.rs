//! Explicit compaction preserves the durable tree and outstanding client calls.
use super::*;
use crate::executor::{pending_calls::resolved_prefix_len, rehydrate::rehydrate_conversation};
use crate::types::agent_tree::StoredTreeSnapshot;

fn trigger(previous: Option<&str>, stream: bool) -> RequestPayload {
    RequestPayload {
        model: "test".into(),
        store: true,
        stream,
        previous_response_id: previous.map(str::to_owned),
        input: ResponsesInput::Items(vec![InputItem::CompactionTrigger]),
        ..Default::default()
    }
}

async fn stored_tree(previous: &str, exec: &ExecutionContext) -> StoredTreeSnapshot {
    rehydrate_conversation(trigger(Some(previous), false), exec)
        .await
        .unwrap()
        .multi_agent_tree
        .unwrap()
        .into_snapshot()
}

pub(super) async fn compact_checkpoint(previous: &str, stream: bool, exec: Arc<ExecutionContext>) -> String {
    let before = stored_tree(previous, &exec).await;
    let response = response(trigger(Some(previous), stream), exec.clone()).await;
    assert_eq!(response.status, "completed");
    assert_eq!(response.usage.unwrap().total_tokens, 3, "only one summary inference");
    assert!(matches!(response.output.as_slice(), [OutputItem::Compaction(item)]
        if item.agent.as_ref().unwrap().agent_name == "/root"));
    let after = stored_tree(&response.id, &exec).await;
    assert_eq!(
        serde_json::to_value(&before.client_calls).unwrap(),
        serde_json::to_value(&after.client_calls).unwrap()
    );
    assert_eq!(before.agents.len(), after.agents.len());
    for (mut original, compacted) in before.agents.into_iter().zip(after.agents) {
        if original.identity.is_root() {
            let suffix = &original.history[resolved_prefix_len(&original.history).unwrap()..];
            assert_eq!(
                serde_json::to_value(suffix).unwrap(),
                serde_json::to_value(&compacted.history[compacted.history.len() - suffix.len()..]).unwrap()
            );
            assert!(
                compacted
                    .history
                    .iter()
                    .any(|item| matches!(item, InputItem::Compaction(_)))
            );
            original.history.clone_from(&compacted.history);
        }
        assert_eq!(
            serde_json::to_value(original).unwrap(),
            serde_json::to_value(compacted).unwrap(),
            "only the root's resolved history may change"
        );
    }
    response.id
}

#[tokio::test]
async fn explicit_compaction_initializes_a_tree_without_running_agents() {
    for stream in [false, true] {
        let (exec, server) = setup().await;
        let mut request = trigger(None, stream);
        request.multi_agent = Some(MultiAgentConfig {
            enabled: true,
            max_concurrent_subagents: Some(3),
        });
        request.input = serde_json::from_value(json!([
            {"type":"message","role":"user","content":"remember this context"},
            {"type":"compaction_trigger"}
        ]))
        .unwrap();
        let compacted = response(request, exec.clone()).await;
        assert_eq!(compacted.usage.unwrap().total_tokens, 3);
        assert!(matches!(compacted.output.as_slice(), [OutputItem::Compaction(item)]
            if item.agent.as_ref().unwrap().agent_name == "/root"));
        let tree = stored_tree(&compacted.id, &exec).await;
        assert_eq!(tree.agents.len(), 1);
        assert_eq!(tree.agents[0].rounds, 0);
        assert!(tree.agents[0].history.iter().all(|item| !item.is_compaction_trigger()));
        server.abort();
    }
}

#[tokio::test]
async fn explicit_compaction_rejects_empty_root_context() {
    let (exec, server) = setup().await;
    let mut request = trigger(None, false);
    request.multi_agent = Some(MultiAgentConfig {
        enabled: true,
        max_concurrent_subagents: Some(3),
    });
    assert!(matches!(
        ExecuteRequest::new(request, exec).run().await,
        Err(ExecutorError::InvalidRequest(_))
    ));
    server.abort();
}
