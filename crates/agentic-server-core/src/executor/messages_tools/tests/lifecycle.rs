//! Deterministic permit-wait and failure tests using Tokio's paused clock.

use std::task::Poll;

use futures::poll;
use tokio::sync::Semaphore;

use super::*;
use crate::executor::messages_tools::{Admission, GATEWAY_TOOL_TIMEOUT, run};
use crate::executor::telemetry::metrics::ExecutorMetrics;
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

fn admission(name: &str) -> Admission {
    Admission::Run {
        call: tool_seam::tool_use_to_call("call", name, &json!({"query":"test"}), &GatewayToolMap::default()),
        name: name.to_owned(),
    }
}

fn exclusion(registry: &ToolRegistry) -> &Semaphore {
    let ToolOwnership::Gateway(Some(binding)) = &registry.lookup("web_search").unwrap().ownership else {
        panic!("expected gateway binding");
    };
    binding.self_exclusion.as_deref().unwrap()
}

/// The permit a call waits for, in acquisition order.
#[derive(Debug, Clone, Copy)]
enum WaitStage {
    /// The binding's same-tool exclusion.
    SameTool,
    /// The round's execution window.
    Round,
    /// The policy's shared materialization window.
    Materialization,
}

#[tokio::test(start_paused = true)]
async fn messages_scheduling_timeout_excludes_each_permit_wait_and_releases_capacity() {
    for stage in [WaitStage::SameTool, WaitStage::Round, WaitStage::Materialization] {
        let (registry, mut started, active) = registry().await;
        let policy = policy(1);
        let metrics = ExecutorMetrics::from_global();
        let slots = policy.new_round_execution_slots();
        // Hold exactly this stage's capacity so the call waits there.
        let mut materialization = Vec::new();
        let held = match stage {
            WaitStage::SameTool => Some(exclusion(&registry).try_acquire().unwrap()),
            WaitStage::Round => Some(slots.try_acquire().unwrap()),
            WaitStage::Materialization => {
                for _ in 0..MAX_CONCURRENT_MATERIALIZATIONS {
                    materialization.push(policy.acquire_materialization_permit().await);
                }
                None
            }
        };
        let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots, &metrics));
        assert!(
            poll!(call.as_mut()).is_pending(),
            "{stage:?}: the call waits for its permit"
        );
        tokio::time::advance(GATEWAY_TOOL_TIMEOUT * 2).await;
        assert!(
            poll!(call.as_mut()).is_pending(),
            "{stage:?}: a permit wait cannot time out execution"
        );
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "{stage:?}: the handler has not started"
        );
        drop(held);
        drop(materialization);
        assert!(poll!(call.as_mut()).is_pending(), "{stage:?}: the handler is running");
        let running = started
            .try_recv()
            .unwrap_or_else(|error| panic!("{stage:?}: the handler did not start: {error}"));
        tokio::time::advance(GATEWAY_TOOL_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
        assert!(
            poll!(call.as_mut()).is_pending(),
            "{stage:?}: the handler gets its full execution timeout"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let result = serde_json::to_value(call.await).unwrap();
        assert_eq!(result["tool_use_id"], "call", "{stage:?}");
        assert_eq!(result["is_error"], true, "{stage:?}");
        assert_eq!(
            result["content"], "gateway tool 'web_search' timed out after 60s",
            "{stage:?}"
        );
        assert!(
            running.finish.is_closed(),
            "{stage:?}: the timed-out handler was dropped"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0, "{stage:?}");
        assert_eq!(slots.available_permits(), 1, "{stage:?}: the round slot was released");
        assert_eq!(
            exclusion(&registry).available_permits(),
            1,
            "{stage:?}: the same-tool exclusion was released"
        );

        // A following call must be able to use all three permit layers.
        let mut next = Box::pin(run(admission("web_search"), &registry, &policy, &slots, &metrics));
        assert!(poll!(next.as_mut()).is_pending(), "{stage:?}");
        started.try_recv().unwrap().complete();
        assert!(
            !next.await.is_error,
            "{stage:?}: a later call runs on the released permits"
        );
    }
}

#[tokio::test]
async fn messages_scheduling_errors_release_permits_and_preserve_tool_failure_status() {
    let (registry, mut started, active) = registry().await;
    let policy = policy(1);
    let metrics = ExecutorMetrics::from_global();
    let slots = policy.new_round_execution_slots();
    for outcome in [
        Err(ToolError::Execution("backend failed".into())),
        Ok(ToolOutput::failure("call", "documented failure")),
        Ok(ToolOutput::success("call", "success")),
    ] {
        let expected = match &outcome {
            Err(error) => (format!("tool execution failed: {error}"), true),
            Ok(output) => (output.output.clone(), output.is_failure()),
        };
        let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots, &metrics));
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
    let metrics = ExecutorMetrics::from_global();
    let slots = policy.new_round_execution_slots();
    let _busy = slots.acquire().await.unwrap();
    let refused = Admission::Refused(tool_seam::tool_result_block("refused", "budget exceeded".into(), true));
    for admission in [refused, admission("missing")] {
        let mut call = Box::pin(run(admission, &registry, &policy, &slots, &metrics));
        let Poll::Ready(result) = poll!(call.as_mut()) else {
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
    let metrics = ExecutorMetrics::from_global();
    let slots = policy.new_round_execution_slots();
    let mut held = Vec::new();
    for _ in 0..MAX_CONCURRENT_MATERIALIZATIONS {
        held.push(policy.acquire_materialization_permit().await);
    }
    let mut call = Box::pin(run(admission("web_search"), &registry, &policy, &slots, &metrics));
    assert!(poll!(call.as_mut()).is_pending());
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(exclusion(&registry).available_permits(), 0);
    drop(call);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(exclusion(&registry).available_permits(), 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}
