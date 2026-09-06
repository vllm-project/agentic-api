//! Aggregate retention is charged per immutable object, not per Arc reference.

use super::*;
use crate::types::io::ResponsesInput;

fn group(limit: usize) -> ResponseSessionGroup {
    ResponseSessionGroup::new(
        NonZeroUsize::new(32).unwrap(),
        NonZeroUsize::new(100).unwrap(),
        NonZeroUsize::new(100_000).unwrap(),
        NonZeroUsize::new(limit).unwrap(),
    )
}

fn capture(lease: &ResponseContinuation, id: &str) -> ExecutorResult<RetainedCheckpoint> {
    let items = Vec::from(&ResponsesInput::Text("retained prompt".to_owned()))
        .into_iter()
        .map(InOutItem::Input)
        .collect::<Vec<_>>();
    lease.checkpoint(id.to_owned(), None, &ResponseMetadata::default(), &items, false)
}

fn size(checkpoint: &RetainedCheckpoint) -> usize {
    serialized_size_up_to(checkpoint, usize::MAX).unwrap().unwrap()
}

fn root_size() -> usize {
    let session = ResponseSession::new(NonZeroUsize::new(100).unwrap(), NonZeroUsize::new(100_000).unwrap());
    size(&capture(&session.begin(None).unwrap(), "resp_1").unwrap())
}

fn used(group: &ResponseSessionGroup) -> usize {
    group.budget.used.load(Ordering::Acquire)
}

fn complete(session: &ResponseSession, id: &str) {
    let lease = session.begin(None).unwrap();
    let checkpoint = capture(&lease, id).unwrap();
    lease.publish(checkpoint).unwrap();
}

#[test]
fn aggregate_budget_reserves_before_publication_and_releases_abandoned_candidates() {
    let bytes = root_size();
    let group = group(bytes);
    let first = group.new_session().unwrap();
    let second = group.new_session().unwrap();
    let first_lease = first.begin(None).unwrap();
    let checkpoint = capture(&first_lease, "resp_1").unwrap();
    assert_eq!(used(&group), bytes);
    assert!(first.state.lock().unwrap().latest.is_none(), "not yet published");
    let second_lease = second.begin(None).unwrap();
    assert!(matches!(
        capture(&second_lease, "resp_2"),
        Err(ExecutorError::PayloadTooLarge(_))
    ));
    assert_eq!(used(&group), bytes, "failed reservation must not change the counter");
    drop(checkpoint);
    assert_eq!(used(&group), 0);
    let checkpoint = capture(&second_lease, "resp_2").unwrap();
    second_lease.publish(checkpoint).unwrap();
    assert_eq!(used(&group), bytes);
    drop(second);
    assert_eq!(used(&group), 0);
}

#[test]
fn aggregate_budget_counts_shared_parent_once_and_replaced_pinned_parent_until_last_release() {
    let bytes = root_size();
    let group = group(3 * bytes);
    let source = group.new_session().unwrap();
    let first = group.new_session().unwrap();
    let second = group.new_session().unwrap();
    complete(&source, "resp_1");
    let fork = first.begin(Some("resp_1")).unwrap();
    let other_fork = second.begin(Some("resp_1")).unwrap();
    assert!(Arc::ptr_eq(
        fork.parent.as_ref().unwrap(),
        other_fork.parent.as_ref().unwrap()
    ));
    assert_eq!(used(&group), bytes, "fork handles share the original charge");
    complete(&source, "resp_2");
    assert_eq!(
        used(&group),
        2 * bytes,
        "source replacement cannot release a pinned parent"
    );
    drop(source);
    assert_eq!(used(&group), bytes);
    drop(fork);
    assert_eq!(used(&group), bytes, "the second fork is still using the old parent");
    drop(other_fork);
    assert_eq!(used(&group), 0);
}

#[test]
fn aggregate_budget_replacement_requires_headroom_without_premature_eviction() {
    let bytes = root_size();
    let group = group(bytes);
    let session = group.new_session().unwrap();
    complete(&session, "resp_1");
    let lease = session.begin(None).unwrap();
    assert!(matches!(
        capture(&lease, "resp_2"),
        Err(ExecutorError::PayloadTooLarge(_))
    ));
    assert_eq!(used(&group), bytes);
    assert_eq!(
        session.state.lock().unwrap().latest.as_ref().unwrap().response_id,
        "resp_1"
    );
    drop(lease); // Failed fresh root does not reference/evict the old checkpoint.
    session.discard_cached_response("resp_1").unwrap();
    assert_eq!(used(&group), 0);
    complete(&session, "resp_2");
    assert_eq!(used(&group), bytes);
}

#[test]
fn aggregate_budget_successful_continuation_releases_replaced_parent() {
    let group = group(100_000);
    let session = group.new_session().unwrap();
    complete(&session, "resp_1");
    let before = used(&group);
    let lease = session.begin(Some("resp_1")).unwrap();
    let checkpoint = capture(&lease, "resp_2").unwrap();
    let after = size(&checkpoint);
    assert_eq!(used(&group), before + after, "both survive until publication");
    lease.publish(checkpoint).unwrap();
    assert_eq!(used(&group), after);
    session.discard_cached_response("resp_2").unwrap();
    assert_eq!(used(&group), 0);
}

#[test]
fn aggregate_budget_group_drop_keeps_only_live_parent_and_pending_candidate_charges() {
    let group = group(100_000);
    let budget = Arc::clone(&group.budget);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_1");
    let parent_size = used(&group);
    let fork = target.begin(Some("resp_1")).unwrap();
    let checkpoint = capture(&fork, "resp_2").unwrap();
    let total = parent_size + size(&checkpoint);
    drop(group);
    assert_eq!(budget.used.load(Ordering::Acquire), total);
    assert!(fork.publish(checkpoint).is_err());
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn aggregate_budget_charges_canonical_durable_fallback_before_inference() {
    let group = group(100_000);
    let session = group.new_session().unwrap();
    let lease = session.begin(None).unwrap();
    let checkpoint = ResponseCheckpoint {
        response_id: "durable".to_owned(),
        conversation_id: None,
        history: Vec::from(&ResponsesInput::Text("durable history".to_owned())),
        metadata: ResponseMetadata {
            effective_instructions: Some("metadata".repeat(10)),
            ..ResponseMetadata::default()
        },
        durable: true,
    };
    let expected = serialized_size_up_to(&checkpoint, usize::MAX).unwrap().unwrap();
    let retained = lease.retain_parent(checkpoint).unwrap();
    assert_eq!(used(&group), expected);
    assert_eq!(
        size(&retained),
        expected,
        "budget metadata is not part of serialized history"
    );
    drop(retained);
    assert_eq!(used(&group), 0);
}

#[test]
fn aggregate_budget_simultaneous_candidates_cannot_oversubscribe_or_leak() {
    let bytes = root_size();
    let group = group(4 * bytes);
    let sessions = (0..16).map(|_| group.new_session().unwrap()).collect::<Vec<_>>();
    let barrier = std::sync::Barrier::new(sessions.len());
    let candidates = std::thread::scope(|scope| {
        let tasks = sessions
            .iter()
            .map(|session| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let lease = session.begin(None).unwrap();
                    barrier.wait();
                    capture(&lease, "resp_1")
                })
            })
            .collect::<Vec<_>>();
        tasks.into_iter().map(|task| task.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(candidates.iter().filter(|candidate| candidate.is_ok()).count(), 4);
    assert!(
        candidates
            .iter()
            .filter_map(|candidate| candidate.as_ref().err())
            .all(|error| matches!(error, ExecutorError::PayloadTooLarge(_)))
    );
    assert_eq!(used(&group), 4 * bytes);
    drop(candidates);
    assert_eq!(used(&group), 0);
    complete(&sessions[0], "resp_1");
    assert_eq!(used(&group), bytes);
}

#[test]
fn aggregate_budget_integer_overflow_is_rejected_without_changing_usage() {
    let group = group(usize::MAX);
    let reservation = group.budget.reserve(usize::MAX).unwrap();
    assert!(group.budget.reserve(1).is_err());
    assert_eq!(used(&group), usize::MAX);
    drop(reservation);
    assert_eq!(used(&group), 0);
}

#[tokio::test]
async fn aggregate_budget_cancellation_drops_pending_candidate_and_failed_parent() {
    let group = group(100_000);
    let session = Arc::new(group.new_session().unwrap());
    complete(&session, "resp_1");
    let worker_session = Arc::clone(&session);
    let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        let lease = worker_session.begin(Some("resp_1")).unwrap();
        let checkpoint = capture(&lease, "resp_2").unwrap();
        prepared_tx.send(()).unwrap();
        std::future::pending::<()>().await;
        lease.publish(checkpoint).unwrap();
    });
    prepared_rx.await.unwrap();
    assert!(used(&group) > root_size());
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    session.wait_until_idle().await.unwrap();
    assert_eq!(used(&group), 0);
    complete(&session, "resp_3");
    assert_eq!(used(&group), root_size());
}
