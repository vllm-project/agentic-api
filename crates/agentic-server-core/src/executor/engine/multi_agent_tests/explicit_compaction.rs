//! Automatic and explicit compaction preserve agent state and client-call ownership.
use super::*;
use crate::executor::{pending_calls::resolved_prefix_len, rehydrate::rehydrate_conversation};
use crate::types::agent_tree::StoredTreeSnapshot;
use crate::utils::common::deserialize_from_value;

fn trigger(previous: Option<&str>, stream: bool) -> RequestPayload {
    RequestPayload {
        model: "test".into(),
        store: true,
        stream,
        previous_response_id: previous.map(str::to_owned),
        input: ResponsesInput::from(vec![InputItem::CompactionTrigger]),
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

#[tokio::test]
async fn automatic_compaction_is_attributed_and_usage_is_counted_once() {
    let (exec, server) = setup().await;
    for (store, stream) in [(true, false), (true, true), (false, false), (false, true)] {
        let request = RequestPayload {
            model: "test".into(),
            store,
            stream,
            input: ResponsesInput::Text("simple compaction test".into()),
            multi_agent: Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(3),
            }),
            context_management: Some(vec![ContextManagement {
                type_: "compaction".into(),
                compact_threshold: Some(1),
            }]),
            ..Default::default()
        };
        let result = tokio::time::timeout(Duration::from_secs(10), response(request.clone(), exec.clone())).await;
        let result = result.unwrap();
        assert_eq!(result.usage.unwrap().total_tokens, 6);
        assert!(matches!(result.output.first(), Some(OutputItem::Compaction(item))
        if item.agent.as_ref().is_some_and(|agent| agent.agent_name == "/root")));
        assert_eq!(result.output.len(), if store { 2 } else { 3 });
        if !store {
            let mut history = request.input.clone().into_canonical_items();
            history.extend(result.output.into_iter().map(Into::into));
            let mut continuation = request;
            continuation.input = deserialize_from_value(serde_json::to_value(history).unwrap()).unwrap();
            let ctx = rehydrate_conversation(continuation, &exec).await.unwrap();
            let tree = ctx.multi_agent_tree.unwrap().into_snapshot();
            assert!(
                tree.agents[0]
                    .history
                    .iter()
                    .any(|item| matches!(item, InputItem::Compaction(_)))
            );
            assert!(exec.resp_handler.retrieve(&result.id).await.is_err());
        }
    }
    server.abort();
}
