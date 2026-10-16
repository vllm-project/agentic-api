//! Deterministic permit-wait and failure tests using Tokio's paused clock.

use futures::poll;
use tokio::sync::Semaphore;

use super::*;
use crate::executor::messages_tools::{Admission, GATEWAY_TOOL_TIMEOUT, run};
use crate::tool::{GatewayExecutors, ToolDeclaration, ToolOwnership};
use crate::types::messages::tool_seam;

async fn registry() -> (ToolRegistry, mpsc::Receiver<Started>, Arc<AtomicUsize>) {
    let (sender, receiver) = mpsc::channel(4);
    let active = Arc::new(AtomicUsize::new(0));
    let mut executors = GatewayExecutors::default();
    executors.insert(Arc::new(Probe {
        started: sender,
        active: Arc::clone(&active),
        parallel: false,
    }));
    let mut tools = [ToolDeclaration::WebSearch(
        serde_json::from_value(json!({"type":"web_search_preview"})).unwrap(),
    )];
    (
        ToolRegistry::build_with_handlers(&mut tools, &mut executors)
            .await
            .unwrap(),
        receiver,
        active,
    )
}

fn admission(name: &'static str) -> Admission<'static> {
    Admission::Run {
        call: tool_seam::tool_use_to_call("call", name, &json!({"query":"test"}), &GatewayToolMap::default()),
        name,
    }
}

fn exclusion(registry: &ToolRegistry) -> &Semaphore {
    let ToolOwnership::Gateway(Some(binding)) = &registry.lookup("web_search").unwrap().ownership else {
        panic!("expected gateway binding");
    };
    binding.self_exclusion.as_deref().unwrap()
}

#[tokio::test(start_paused = true)]
async fn messages_scheduling_timeout_excludes_each_permit_wait_and_releases_capacity() {
    for wait_at in 0..3 {
        let (registry, mut started, active) = registry().await;
        let policy = policy(1);
        let slots = policy.execution_slots();
        let same_tool = (wait_at == 0).then(|| exclusion(&registry).try_acquire().unwrap());
        let round_slot = (wait_at == 1).then(|| slots.try_acquire().unwrap());
        let mut materialization = Vec::new();
        if wait_at == 2 {
            for _ in 0..MAX_CONCURRENT_MATERIALIZATIONS {
                materialization.push(policy.acquire_materialization_permit().await);
            }
        }
        let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots));
        assert!(poll!(call.as_mut()).is_pending());
        tokio::time::advance(GATEWAY_TOOL_TIMEOUT * 2).await;
        assert!(
            poll!(call.as_mut()).is_pending(),
            "permit wait cannot time out execution"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
        drop(same_tool);
        drop(round_slot);
        drop(materialization);
        assert!(poll!(call.as_mut()).is_pending());
        let running = started.try_recv().unwrap();
        tokio::time::advance(GATEWAY_TOOL_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
        assert!(
            poll!(call.as_mut()).is_pending(),
            "handler gets its full execution timeout"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = serde_json::to_value(call.await).unwrap();
        assert_eq!(result["tool_use_id"], "call");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"], "gateway tool 'web_search' timed out after 60s");
        assert!(running.finish.is_closed());
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(exclusion(&registry).available_permits(), 1);

        // A following call must be able to use all three permit layers.
        let mut next = Box::pin(run(admission("web_search"), &registry, &policy, &slots));
        assert!(poll!(next.as_mut()).is_pending());
        started.try_recv().unwrap().complete();
        assert!(!next.await.is_error);
    }
}

#[tokio::test]
async fn messages_scheduling_errors_release_permits_and_preserve_tool_failure_status() {
    let (registry, mut started, active) = registry().await;
    let policy = policy(1);
    let slots = policy.execution_slots();
    for outcome in [
        Err(ToolError::Execution("backend failed".into())),
        Ok(ToolOutput::failure("call", "documented failure")),
        Ok(ToolOutput::success("call", "success")),
    ] {
        let expected = match &outcome {
            Err(error) => (format!("tool execution failed: {error}"), true),
            Ok(output) => (output.output.clone(), output.is_failure()),
        };
        let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots));
        assert!(poll!(call.as_mut()).is_pending());
        started.try_recv().unwrap().finish.send(outcome).unwrap();
        let result = call.await;
        assert_eq!(result.content, expected.0);
        assert_eq!(result.is_error, expected.1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(exclusion(&registry).available_permits(), 1);
    }
}

#[tokio::test]
async fn messages_scheduling_unexecuted_results_do_not_wait_for_capacity() {
    let (registry, _, active) = registry().await;
    let policy = policy(1);
    let slots = policy.execution_slots();
    let _busy = slots.acquire().await.unwrap();
    let refused = Admission::Refused(tool_seam::tool_result_block("refused", "budget exceeded".into(), true));
    for admission in [refused, admission("missing")] {
        let mut call = Box::pin(run(admission, &registry, &policy, &slots));
        let std::task::Poll::Ready(result) = poll!(call.as_mut()) else {
            panic!("a call with no executable work must not wait for an execution permit");
        };
        assert!(result.is_error);
    }
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn messages_scheduling_dropping_permit_wait_releases_partial_acquisition() {
    let (registry, _, active) = registry().await;
    let policy = policy(1);
    let slots = policy.execution_slots();
    let mut held = Vec::new();
    for _ in 0..MAX_CONCURRENT_MATERIALIZATIONS {
        held.push(policy.acquire_materialization_permit().await);
    }
    let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots));
    assert!(poll!(call.as_mut()).is_pending());
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(exclusion(&registry).available_permits(), 0);
    drop(call);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(exclusion(&registry).available_permits(), 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}
