use super::*;
use crate::types::io::ResponsesInput;
use crate::utils::common::deserialize_from_value;
use serde_json::json;

pub(super) fn session(items: usize, bytes: usize) -> ResponseSession {
    ResponseSession::new(NonZeroUsize::new(items).unwrap(), NonZeroUsize::new(bytes).unwrap())
}

pub(super) fn new_checkpoint(lease: &ResponseContinuation, id: &str) -> RetainedCheckpoint {
    let input = ResponsesInput::Text("private prompt".to_owned());
    let items = Vec::from(&input).into_iter().map(InOutItem::Input).collect::<Vec<_>>();
    lease
        .checkpoint(id.to_owned(), None, &ResponseMetadata::default(), &items, false)
        .unwrap()
}

pub(super) fn complete(session: &ResponseSession, id: &str) {
    let lease = session.begin(None).unwrap();
    let checkpoint = new_checkpoint(&lease, id);
    lease.publish(checkpoint).unwrap();
}

fn group(count: usize) -> ResponseSessionGroup {
    ResponseSessionGroup::new(
        NonZeroUsize::new(count).unwrap(),
        NonZeroUsize::new(10).unwrap(),
        NonZeroUsize::new(10_000).unwrap(),
        NonZeroUsize::new(100_000).unwrap(),
    )
}

#[test]
fn pinned_response_survives_failed_lease_and_retries_transient_parent() {
    let session = session(10, 10_000);
    complete(&session, "resp_1");
    let pin = session.pin_response("resp_1").unwrap().unwrap();
    assert_eq!(pin.response_id(), "resp_1");
    assert_eq!(pin.history().len(), 1);
    assert!(!pin.checkpoint.durable);
    let weak = Arc::downgrade(&pin.checkpoint);
    let failed = session.begin_pinned(Some("resp_1"), &pin).unwrap();
    assert!(Arc::ptr_eq(failed.parent.as_ref().unwrap(), &pin.checkpoint));
    assert!(session.begin_pinned(Some("resp_1"), &pin).is_err());
    drop(failed);
    assert!(
        session.state.lock().unwrap().latest.is_none(),
        "ordinary failure still evicts cache"
    );
    assert!(!session.state.lock().unwrap().active);

    let retry = session.begin_pinned(Some("resp_1"), &pin).unwrap();
    assert!(Arc::ptr_eq(retry.parent.as_ref().unwrap(), &pin.checkpoint));
    let replacement = new_checkpoint(&retry, "resp_2");
    assert_eq!(replacement.history.len(), 2);
    retry.publish(replacement).unwrap();
    assert!(weak.upgrade().is_some(), "the caller still owns the pinned parent");
    drop(pin);
    assert!(weak.upgrade().is_none());
    assert!(session.begin(Some("resp_2")).unwrap().parent.is_some());
}

#[test]
fn checkpoint_pins_share_budget_and_do_not_hold_an_execution_slot() {
    let group = group(1);
    let session = group.new_session().unwrap();
    assert!(session.pin_response("resp_1").unwrap().is_none());
    complete(&session, "resp_1");
    let charged = group.budget.used.load(Ordering::Acquire);
    assert!(charged > 0);
    let first = session.pin_response("resp_1").unwrap().unwrap();
    let second = session.pin_response("resp_1").unwrap().unwrap();
    assert!(Arc::ptr_eq(&first.checkpoint, &second.checkpoint));
    assert_eq!(group.budget.used.load(Ordering::Acquire), charged);
    let active = session.begin(None).unwrap();
    session.discard_cached_response("resp_1").unwrap();
    assert!(session.pin_response("resp_1").unwrap().is_none());
    drop(active);
    assert_eq!(group.budget.used.load(Ordering::Acquire), charged);
    drop(first);
    assert_eq!(group.budget.used.load(Ordering::Acquire), charged);
    drop(second);
    assert_eq!(group.budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn pinned_execution_checks_parent_and_owner_without_changing_ordinary_forks() {
    let group = group(2);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_source");
    let pin = source.pin_response("resp_source").unwrap().unwrap();
    assert!(target.pin_response("resp_source").unwrap().is_none());
    assert!(target.begin_pinned(Some("resp_source"), &pin).is_err());
    for previous in [None, Some("resp_other")] {
        assert!(source.begin_pinned(previous, &pin).is_err());
    }
    let ordinary_fork = target.begin(Some("resp_source")).unwrap();
    assert!(Arc::ptr_eq(ordinary_fork.parent.as_ref().unwrap(), &pin.checkpoint));
    let replacement = new_checkpoint(&ordinary_fork, "resp_branch");
    ordinary_fork.publish(replacement).unwrap();
    let continuation = source.begin_pinned(Some("resp_source"), &pin).unwrap();
    assert!(Arc::ptr_eq(continuation.parent.as_ref().unwrap(), &pin.checkpoint));
}

#[test]
fn checkpoint_pin_keeps_charge_but_cannot_reopen_a_closed_owner() {
    let group = group(1);
    let budget = Arc::clone(&group.budget);
    let session = group.new_session().unwrap();
    complete(&session, "resp_1");
    let pin = session.pin_response("resp_1").unwrap().unwrap();
    let charged = budget.used.load(Ordering::Acquire);
    drop(group);
    assert!(session.pin_response("resp_1").is_err());
    assert!(session.begin_pinned(Some("resp_1"), &pin).is_err());
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    let owner = pin.owner.clone();
    drop(session);
    assert!(owner.upgrade().is_none());
    drop(pin);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn group_session_cap_counts_idle_and_dropped_members() {
    let group = group(2);
    let first = group.new_session().unwrap();
    let second = group.new_session().unwrap();
    complete(&first, "resp_1");
    assert!(group.new_session().is_err());
    drop(second);
    assert!(group.new_session().is_err(), "the cap covers the entire group lifetime");
    assert!(first.begin(Some("resp_1")).unwrap().parent.is_some());
}

#[test]
fn grouped_members_inherit_item_and_byte_budgets() {
    for (items, bytes) in [(1, 10_000), (10, 1)] {
        let group = ResponseSessionGroup::new(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(items).unwrap(),
            NonZeroUsize::new(bytes).unwrap(),
            NonZeroUsize::new(100_000).unwrap(),
        );
        let member = group.new_session().unwrap();
        let lease = member.begin(None).unwrap();
        let input = Vec::from(&ResponsesInput::Text("private prompt".to_owned()));
        let input = input
            .iter()
            .cycle()
            .take(2)
            .cloned()
            .map(InOutItem::Input)
            .collect::<Vec<_>>();
        assert!(matches!(
            lease.checkpoint("resp_1".to_owned(), None, &ResponseMetadata::default(), &input, false),
            Err(ExecutorError::PayloadTooLarge(_))
        ));
    }
}

#[test]
fn groups_do_not_share_parent_lookup() {
    let first_group = group(1);
    let second_group = group(1);
    let source = first_group.new_session().unwrap();
    let other = second_group.new_session().unwrap();
    complete(&source, "resp_1");
    assert!(other.begin(Some("resp_1")).unwrap().parent.is_none());
}

#[test]
fn failed_fork_preserves_source_and_unrelated_destination_checkpoint() {
    let group = group(2);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_source");
    complete(&target, "resp_target");
    let fork = target.begin(Some("resp_source")).unwrap();
    assert!(fork.parent.is_some());
    drop(fork);
    assert!(source.begin(Some("resp_source")).unwrap().parent.is_some());
    assert!(target.begin(Some("resp_target")).unwrap().parent.is_some());
}

#[test]
fn fork_cannot_find_parent_evicted_before_execution_starts() {
    let group = group(2);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_source");
    drop(source.begin(Some("resp_source")).unwrap());
    assert!(target.begin(Some("resp_source")).unwrap().parent.is_none());
}

#[test]
fn dropped_group_invalidates_live_members_and_rejects_late_fork_publication() {
    let group = group(2);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_source");
    let fork = target.begin(Some("resp_source")).unwrap();
    let weak = Arc::downgrade(fork.parent.as_ref().unwrap());
    let checkpoint = new_checkpoint(&fork, "resp_fork");
    drop(group);
    assert!(source.begin(None).is_err());
    assert!(weak.upgrade().is_some(), "the active fork still pins its parent");
    assert!(fork.publish(checkpoint).is_err());
    assert!(weak.upgrade().is_none());
    assert!(target.begin(None).is_err());
}

#[test]
fn group_does_not_keep_dropped_members_or_their_checkpoints_alive() {
    let group = group(2);
    let source = group.new_session().unwrap();
    let target = group.new_session().unwrap();
    complete(&source, "resp_source");
    let member = Arc::downgrade(&source.state);
    let checkpoint = Arc::downgrade(source.state.lock().unwrap().latest.as_ref().unwrap());
    drop(source);
    assert!(member.upgrade().is_none());
    assert!(checkpoint.upgrade().is_none());
    assert!(target.begin(Some("resp_source")).unwrap().parent.is_none());
}

#[test]
fn session_rejects_concurrent_turns_and_releases_cancelled_lease() {
    let session = session(10, 10_000);
    let lease = session.begin(None).unwrap();
    assert!(session.begin(None).is_err());
    drop(lease);
    assert!(session.begin(None).is_ok());
}

#[tokio::test]
async fn idle_wait_is_not_cancellation_and_is_notified_by_lease_disposal() {
    let session = session(10, 10_000);
    session.wait_until_idle().await.unwrap();
    let lease = session.begin(None).unwrap();
    let waiting = session.wait_until_idle();
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    assert!(session.begin(None).is_err());
    drop(lease);
    waiting.await.unwrap();
    assert!(session.begin(None).is_ok());
}

#[tokio::test]
async fn successful_publication_wakes_all_idle_waiters() {
    let session = session(10, 10_000);
    let lease = session.begin(None).unwrap();
    let checkpoint = new_checkpoint(&lease, "resp_1");
    let first = session.wait_until_idle();
    let second = session.wait_until_idle();
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    lease.publish(checkpoint).unwrap();
    first.await.unwrap();
    second.await.unwrap();
    assert!(session.begin(Some("resp_1")).unwrap().parent.is_some());
}

#[test]
fn owner_drop_releases_checkpoint_and_rejects_late_publication() {
    let session = session(10, 10_000);
    complete(&session, "resp_1");
    let lease = session.begin(Some("resp_1")).unwrap();
    let weak = Arc::downgrade(lease.parent.as_ref().unwrap());
    let checkpoint = new_checkpoint(&lease, "resp_2");
    drop(session);
    assert!(lease.publish(checkpoint).is_err());
    assert!(weak.upgrade().is_none());
}

#[test]
fn successful_replacement_does_not_keep_ancestor_objects() {
    let session = session(10, 10_000);
    complete(&session, "resp_1");
    let lease = session.begin(Some("resp_1")).unwrap();
    let weak = Arc::downgrade(lease.parent.as_ref().unwrap());
    let checkpoint = new_checkpoint(&lease, "resp_2");
    assert_eq!(checkpoint.history.len(), 2);
    lease.publish(checkpoint).unwrap();
    assert!(weak.upgrade().is_none());
    let lease = session.begin(Some("resp_2")).unwrap();
    assert_eq!(lease.parent.as_ref().unwrap().history.len(), 2);
}

#[test]
fn failed_continuation_evicts_parent_but_unknown_id_does_not() {
    let session = session(10, 10_000);
    complete(&session, "resp_1");
    drop(session.begin(Some("unknown")).unwrap());
    let lease = session.begin(Some("resp_1")).unwrap();
    assert!(lease.parent.is_some());
    drop(lease);
    assert!(session.begin(Some("resp_1")).unwrap().parent.is_none());
}

#[test]
fn separate_sessions_have_no_shared_lookup() {
    let first = session(10, 10_000);
    let second = session(10, 10_000);
    complete(&first, "resp_1");
    assert!(second.begin(Some("resp_1")).unwrap().parent.is_none());
}

#[test]
fn explicit_discard_preserves_the_active_snapshot_and_execution_slot() {
    let session = session(10, 10_000);
    complete(&session, "resp_1");
    session.discard_cached_response("unrelated").unwrap();
    let lease = session.begin(Some("resp_1")).unwrap();
    assert!(lease.parent.is_some());
    session.discard_cached_response("resp_1").unwrap();
    assert!(lease.parent.is_some(), "the active snapshot remains pinned");
    assert!(session.begin(None).is_err(), "discard is not cancellation");
    drop(lease);
    assert!(session.begin(Some("resp_1")).unwrap().parent.is_none());
}

#[test]
fn item_budget_is_enforced_on_the_complete_history() {
    let session = session(1, 10_000);
    complete(&session, "resp_1");
    let lease = session.begin(Some("resp_1")).unwrap();
    let input = serde_json::from_value(json!({"type":"message", "role":"user", "content":"next"})).unwrap();
    assert!(matches!(
        lease.checkpoint(
            "resp_2".to_owned(),
            None,
            &ResponseMetadata::default(),
            &[InOutItem::Input(input)],
            false
        ),
        Err(ExecutorError::PayloadTooLarge(_))
    ));
}

#[test]
fn byte_budget_includes_metadata_and_exact_boundary() {
    let source = session(10, 10_000);
    let lease = source.begin(None).unwrap();
    let checkpoint = new_checkpoint(&lease, "resp_1");
    let size = serialized_size_up_to(&checkpoint, usize::MAX).unwrap().unwrap();
    let exact = session(10, size);
    complete(&exact, "resp_1");
    let too_small = session(10, size - 1);
    let lease = too_small.begin(None).unwrap();
    let metadata = ResponseMetadata {
        multi_agent_tree: None,
        effective_instructions: Some("large metadata".repeat(100)),
        ..ResponseMetadata::default()
    };
    assert!(matches!(
        lease.checkpoint("resp_1".to_owned(), None, &metadata, &[], false),
        Err(ExecutorError::PayloadTooLarge(_))
    ));
}

#[test]
fn canonical_items_and_sanitized_mcp_metadata_are_retained() {
    let session = session(10, 10_000);
    let lease = session.begin(None).unwrap();
    let metadata = ResponseMetadata {
        multi_agent_tree: None,
        effective_tools: Some(
            serde_json::from_value(json!([{
                "type":"mcp", "server_label":"counter", "server_url":"https://example.com/mcp",
                "headers":{"X-API-Key":"secret-header"}, "authorization":"secret-token", "require_approval":"never"
            }]))
            .unwrap(),
        ),
        ..ResponseMetadata::default()
    };
    let input = serde_json::from_value(json!({"type":"reasoning", "id":"rs_1",
        "content":[{"type":"reasoning_text", "text":"part one"},{"type":"reasoning_text", "text":"part two"}],
        "summary":[{"type":"summary_text", "text":"canonical summary"}], "encrypted_content":"opaque"
    }))
    .unwrap();
    let checkpoint = lease
        .checkpoint("resp_1".to_owned(), None, &metadata, &[InOutItem::Input(input)], false)
        .unwrap();
    let serialized = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(serialized["history"][0]["content"].as_array().unwrap().len(), 2);
    assert_eq!(serialized["history"][0]["encrypted_content"], "opaque");
    assert!(!serialized.to_string().contains("secret-header"));
    assert!(!serialized.to_string().contains("secret-token"));
    assert!(!format!("{checkpoint:?}").contains("canonical summary"));
}

#[test]
fn steering_requirements_use_pinned_parent_custom_tool_metadata() {
    let session = session(10, 10_000);
    let lease = session.begin(None).unwrap();
    let metadata = ResponseMetadata {
        effective_tools: Some(
            deserialize_from_value(json!([
                {"type":"custom", "name":"set_temperature_unit"}
            ]))
            .unwrap(),
        ),
        ..ResponseMetadata::default()
    };
    let history: Vec<InputItem> = deserialize_from_value(json!([
        {"type":"function_call", "call_id":"custom", "name":"set_temperature_unit", "arguments":"{}"},
        {"type":"function_call", "call_id":"weather", "name":"get_weather", "arguments":"{}"},
        {"type":"function_call", "call_id":"async_weather", "name":"get_weather", "arguments":"{}", "async":true},
        {"type":"custom_tool_call", "id":"ct_async", "call_id":"async_custom", "name":"set_temperature_unit", "input":"C", "async":true}
    ]))
    .unwrap();
    let items = history.into_iter().map(InOutItem::Input).collect::<Vec<_>>();
    let checkpoint = lease
        .checkpoint("resp_parent".to_owned(), None, &metadata, &items, false)
        .unwrap();
    lease.publish(checkpoint).unwrap();
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    let required = pin.required_steering_input().unwrap();
    assert_eq!(pin.history().len(), 4, "async calls remain in canonical history");
    assert_eq!(required.len(), 2, "only synchronous calls require outputs for steering");
    assert!(matches!(&required[0], SteeringRequiredInput::Custom { call_id }
        if call_id.as_str() == "custom"));
    assert!(matches!(&required[1], SteeringRequiredInput::Function { call_id, name }
        if call_id.as_str() == "weather" && name == "get_weather"));
}

#[test]
fn compaction_replacement_preserves_mcp_discovery_without_old_context() {
    let session = session(3, 10_000);
    let lease = session.begin(None).unwrap();
    let items = serde_json::from_value::<Vec<InputItem>>(json!([
        {"type":"message", "role":"user", "content":"private prompt"},
        {"type":"mcp_list_tools", "id":"mcp_1", "server_label":"counter", "tools":[]}
    ]))
    .unwrap()
    .into_iter()
    .map(InOutItem::Input)
    .collect::<Vec<_>>();
    let parent = lease
        .checkpoint("resp_1".to_owned(), None, &ResponseMetadata::default(), &items, false)
        .unwrap();
    lease.publish(parent).unwrap();
    let mut lease = session.begin(Some("resp_1")).unwrap();
    lease.mark_history_replaced();
    let compacted: Vec<InputItem> = serde_json::from_value(json!([
        {"type":"message", "role":"user", "id":"msg_kept", "status":"completed", "content":"retained user"},
        {"type":"compaction", "id":"cmp_1", "encrypted_content":"canonical summary"}
    ]))
    .unwrap();
    let items = compacted.into_iter().map(InOutItem::Input).collect::<Vec<_>>();
    let checkpoint = lease
        .checkpoint("resp_2".to_owned(), None, &ResponseMetadata::default(), &items, false)
        .unwrap();
    assert_eq!(checkpoint.history.len(), 3);
    assert!(matches!(checkpoint.history[0], InputItem::McpListTools(_)));
    assert!(matches!(checkpoint.history[2], InputItem::Compaction(_)));
    assert!(
        !serde_json::to_string(&checkpoint.history)
            .unwrap()
            .contains("private prompt")
    );
}

#[test]
fn recorded_rounds_are_not_duplicated_and_mcp_discovery_is_retained() {
    let session = session(10, 10_000);
    let mut lease = session.begin(None).unwrap();
    let message: OutputItem = serde_json::from_value(json!({
        "type":"message", "id":"msg_1", "role":"assistant", "status":"completed", "content":[]
    }))
    .unwrap();
    let discovery: OutputItem = serde_json::from_value(json!({
        "type":"mcp_list_tools", "id":"mcp_1", "server_label":"counter", "tools":[]
    }))
    .unwrap();
    assert!(lease.retains_output(0, &message));
    lease.mark_outputs_recorded(2);
    assert!(!lease.retains_output(1, &message));
    assert!(lease.retains_output(2, &message));
    lease.mark_history_replaced();
    lease.mark_outputs_recorded(4);
    assert!(!lease.retains_output(2, &message));
    assert!(lease.retains_output(4, &message));
    assert!(lease.retains_output(0, &discovery));
}
