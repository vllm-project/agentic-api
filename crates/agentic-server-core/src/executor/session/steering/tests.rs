use super::super::tests::{complete, new_checkpoint, session};
use super::super::{ResponseCheckpointPin, ResponseSession};
use super::*;
use crate::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use crate::storage::{ConversationStore, ResponseStore};
use crate::types::io::ResponsesInput;
use crate::types::request_response::RequestPayload;
use crate::utils::common::{deserialize_from_value, serialize_to_value};
use serde_json::json;

fn steering_queue(entries: usize, bytes: usize) -> SteeringQueue {
    SteeringQueue::new(
        "resp_parent".to_owned(),
        NonZeroUsize::new(entries).unwrap(),
        NonZeroUsize::new(bytes).unwrap(),
    )
    .unwrap()
}

fn steering_text(text: &str) -> SteeringInput {
    SteeringInput::try_from(ResponsesInput::Text(text.to_owned())).unwrap()
}

fn execution_context() -> Arc<ExecutionContext> {
    Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        "http://127.0.0.1:1".to_owned(),
    ))
}

async fn prepare_successor(
    exec: &Arc<ExecutionContext>,
    session: &ResponseSession,
    pin: &ResponseCheckpointPin,
    queue: Option<&SteeringQueue>,
) -> PreparedRetainedResponse {
    let payload: RequestPayload = deserialize_from_value(json!({
        "model":"test-model", "store":false, "stream":true,
        "previous_response_id":pin.response_id(), "input":"continue"
    }))
    .unwrap();
    let builder = ExecuteRequest::new(payload, Arc::clone(exec))
        .with_session(session, Some(pin))
        .unwrap();
    let builder = match queue {
        Some(queue) => builder.with_steering(queue).unwrap(),
        None => builder,
    };
    builder.prepare_retained().await.unwrap()
}

#[tokio::test]
async fn steering_reservation_seals_the_matching_batch_without_refunding_capacity() {
    let exec = execution_context();
    let session = session(10, 10_000);
    complete(&session, "resp_parent");
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    let mut queue = steering_queue(3, 10_000);
    queue.try_accept(steering_text("FIRST_PRIVATE_STEER")).unwrap();
    queue.try_accept(steering_text("SECOND_PRIVATE_STEER")).unwrap();
    let ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>();
    let budget = Arc::clone(&queue.budget);
    let charged = budget.used.load(Ordering::Acquire);
    let prepared = prepare_successor(&exec, &session, &pin, Some(&queue)).await;
    let response_id = prepared.response_id().to_owned();
    let reservation = queue
        .reserve_successor(prepared)
        .unwrap_or_else(|_| panic!("matching batch"));
    assert_eq!(reservation.response_id(), response_id);
    assert_eq!(reservation.previous_response_id(), "resp_parent");
    assert_eq!(
        reservation
            .entries()
            .map(|(steer, _)| steer.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
    for ((_, input), expected) in reservation
        .entries()
        .zip(["FIRST_PRIVATE_STEER", "SECOND_PRIVATE_STEER"])
    {
        assert!(matches!(input.as_ref(), ResponsesInput::Text(text) if text == expected));
    }
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    assert!(matches!(session.begin(None), Err(ExecutorError::InvalidRequest(_))));
    assert!(!format!("{reservation:?}").contains("PRIVATE_STEER"));
    drop(reservation);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
    drop(session.begin(None).unwrap());
    assert!(session.pin_response("resp_parent").unwrap().is_none());
    assert_eq!(pin.history().len(), 1);
}

#[tokio::test]
async fn stale_steering_reservation_returns_both_owners_for_fresh_preparation() {
    let exec = execution_context();
    let session = session(10, 10_000);
    complete(&session, "resp_parent");
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    let mut queue = steering_queue(3, 10_000);
    queue.try_accept(steering_text("first")).unwrap();
    let prepared = prepare_successor(&exec, &session, &pin, Some(&queue)).await;
    let response_id = prepared.response_id().to_owned();
    queue.try_accept(steering_text("second")).unwrap();
    let ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>();
    let budget = Arc::clone(&queue.budget);
    let charged = budget.used.load(Ordering::Acquire);
    let rejected = queue.reserve_successor(prepared).expect_err("stale snapshot");
    let (queue, prepared) = *rejected;
    assert_eq!(prepared.response_id(), response_id);
    assert_eq!(
        queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>(),
        ids
    );
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    assert!(!prepared.matches_steering(&queue));
    assert!(matches!(session.begin(None), Err(ExecutorError::InvalidRequest(_))));
    drop(prepared);
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    let fresh = prepare_successor(&exec, &session, &pin, Some(&queue)).await;
    let reservation = queue
        .reserve_successor(fresh)
        .unwrap_or_else(|_| panic!("fresh snapshot"));
    assert_eq!(reservation.entries().len(), 2);
    drop(reservation);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
    drop(session.begin(None).unwrap());
}

#[tokio::test]
async fn unbound_preparation_cannot_reserve_accepted_steering() {
    let exec = execution_context();
    let session = session(10, 10_000);
    complete(&session, "resp_parent");
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    let mut queue = steering_queue(3, 10_000);
    let id = queue.try_accept(steering_text("retain me")).unwrap().id.clone();
    let prepared = prepare_successor(&exec, &session, &pin, None).await;
    let rejected = queue.reserve_successor(prepared).expect_err("unbound request");
    let (queue, prepared) = *rejected;
    assert_eq!(queue.entries().next().unwrap().0.id, id);
    assert_eq!(queue.len(), 1);
    drop(prepared);
    let bound = prepare_successor(&exec, &session, &pin, Some(&queue)).await;
    let reservation = queue
        .reserve_successor(bound)
        .unwrap_or_else(|_| panic!("bound request"));
    assert_eq!(reservation.entries().next().unwrap().0.id, id);
    drop(reservation);
}

#[tokio::test]
async fn unrelated_batch_cannot_claim_another_batches_prepared_successor() {
    let exec = execution_context();
    let session = session(10, 10_000);
    complete(&session, "resp_parent");
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    let mut unrelated = steering_queue(3, 10_000);
    let unrelated_id = unrelated.try_accept(steering_text("same content")).unwrap().id.clone();
    let mut queue = steering_queue(3, 10_000);
    let id = queue.try_accept(steering_text("same content")).unwrap().id.clone();
    let prepared = prepare_successor(&exec, &session, &pin, Some(&queue)).await;
    let (unrelated, prepared) = *unrelated.reserve_successor(prepared).expect_err("unrelated IDs");
    assert_eq!(unrelated.entries().next().unwrap().0.id, unrelated_id);
    assert!(prepared.matches_steering(&queue));
    let reservation = queue
        .reserve_successor(prepared)
        .unwrap_or_else(|_| panic!("original batch"));
    assert_eq!(reservation.entries().next().unwrap().0.id, id);
    drop(reservation);
    assert_eq!(unrelated.len(), 1);
}

#[tokio::test]
async fn publication_requires_its_exact_sink_and_never_recovers_committed_input() {
    use crate::events::SSEEventType;
    use crate::executor::gateway_accumulator::synthetic_event;
    let mut queue = steering_queue(3, 10_000);
    queue.try_accept(steering_text("private update")).unwrap();
    let budget = Arc::clone(&queue.budget);
    let charged = budget.used.load(Ordering::Acquire);
    let (sink, _stream) = ResponseEventSink::channel(4096);
    let publication = SteeringSuccessorPublication {
        response_id: "resp_next".into(),
        sink: sink.clone(),
        queue: Box::new(queue),
    };
    let (unrelated, _stream) = ResponseEventSink::channel(4096);
    let response = json!({"id":"resp_next", "previous_response_id":"resp_parent", "status":"in_progress"});
    let mut frame = synthetic_event(SSEEventType::ResponseCreated, [("response".into(), response.clone())]).unwrap();
    unrelated.emit_frame(&mut frame, 0).await.unwrap();
    let publication = publication.commit(&unrelated.created_receipt().unwrap()).err().unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    let mut frame = synthetic_event(SSEEventType::ResponseCreated, [("response".into(), response)]).unwrap();
    sink.emit_frame(&mut frame, 0).await.unwrap();
    sink.mark_failed();
    sink.mark_joined();
    let publication = publication.fail_before_creation().err().unwrap();
    publication
        .commit(&sink.created_receipt().unwrap())
        .unwrap_or_else(|_| panic!("matching publication"));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn failure_recovery_waits_for_join_and_retains_accepted_ids() {
    let mut queue = steering_queue(3, 10_000);
    let id = queue.try_accept(steering_text("é🙂")).unwrap().id.clone();
    let budget = Arc::clone(&queue.budget);
    let charged = budget.used.load(Ordering::Acquire);
    let (sink, _stream) = ResponseEventSink::channel(4096);
    let publication = SteeringSuccessorPublication {
        response_id: "resp_next".into(),
        sink: sink.clone(),
        queue: Box::new(queue),
    };
    sink.mark_failed();
    let publication = publication.fail_before_creation().err().unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    let (steer, input) = publication.queue.entries().next().unwrap();
    assert_eq!(steer.id, id);
    assert_eq!(steer.previous_response_id, "resp_parent");
    assert!(matches!(input.as_ref(), ResponsesInput::Text(text) if text == "é🙂"));
    sink.mark_joined();
    publication
        .fail_before_creation()
        .unwrap_or_else(|_| panic!("joined before creation"));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn steering_queue_retains_order_across_parent_publication_and_failed_execution() {
    let session = session(10, 10_000);
    let parent = session.begin(None).unwrap();
    let mut queue = steering_queue(3, 10_000);
    assert!(session.pin_response("resp_parent").unwrap().is_none());
    let first = queue.try_accept(steering_text("FIRST_PRIVATE_STEER")).unwrap().clone();
    let checkpoint = new_checkpoint(&parent, "resp_parent");
    parent.publish(checkpoint).unwrap();
    let pin = session.pin_response("resp_parent").unwrap().unwrap();
    drop(session.begin_pinned(Some("resp_parent"), &pin).unwrap());
    assert!(session.pin_response("resp_parent").unwrap().is_none());
    let second = queue.try_accept(steering_text("SECOND_PRIVATE_STEER")).unwrap().clone();
    assert_ne!(first.id, second.id);
    assert!(first.id.starts_with("steer_"));
    assert_eq!(queue.len(), 2);
    for ((steer, input), expected) in queue.entries().zip(["FIRST_PRIVATE_STEER", "SECOND_PRIVATE_STEER"]) {
        assert_eq!(steer.previous_response_id, "resp_parent");
        assert!(matches!(input.as_ref(), ResponsesInput::Text(text) if text == expected));
    }
    assert_eq!(queue.entries().next().unwrap().0.id, first.id);
    assert_eq!(queue.entries().last().unwrap().0.id, second.id);
    assert_eq!(pin.history().len(), 1);
    assert!(
        !serialize_to_value(&pin.history())
            .unwrap()
            .to_string()
            .contains("PRIVATE_STEER")
    );
    assert!(!format!("{queue:?}").contains("PRIVATE_STEER"));
}

#[test]
fn steering_count_refusal_preserves_input_existing_entries_and_byte_charges() {
    let mut queue = steering_queue(2, 10_000);
    assert!(queue.is_empty());
    queue.try_accept(steering_text("first")).unwrap();
    queue.try_accept(steering_text("second")).unwrap();
    let ids = queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>();
    let charged = queue.budget.used.load(Ordering::Acquire);
    let wire = json!([{"type":"message", "role":"user", "content":[{
        "type":"input_image", "image_url":"data:image/png;base64,preserve", "detail":"high",
        "extension":{"nested":["é🙂"]}
    }]}]);
    let input = deserialize_from_value(wire.clone()).unwrap();
    let refused = queue.try_accept(input).unwrap_err();
    assert_eq!(serialize_to_value(&refused).unwrap(), wire);
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.budget.used.load(Ordering::Acquire), charged);
    assert_eq!(
        queue.entries().map(|(steer, _)| steer.id.clone()).collect::<Vec<_>>(),
        ids
    );
}

#[test]
fn steering_byte_boundaries_reject_atomically_and_release_on_disposal() {
    let text = "é".repeat(128);
    let mut measured = steering_queue(2, 10_000);
    measured.try_accept(steering_text(&text)).unwrap();
    let exact_limit = measured.budget.used.load(Ordering::Acquire);
    drop(measured);

    let mut exact = steering_queue(2, exact_limit);
    let budget = Arc::clone(&exact.budget);
    exact.try_accept(steering_text(&text)).unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), exact_limit);
    assert!(exact.try_accept(steering_text("")).is_err());
    assert_eq!(exact.len(), 1);
    drop(exact);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
    let full_charge = budget.try_reserve(exact_limit).unwrap();
    drop(full_charge);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);

    let mut short = steering_queue(2, exact_limit - 1);
    let identity_bytes = short.budget.used.load(Ordering::Acquire);
    let refused = short.try_accept(steering_text(&text)).unwrap_err();
    assert!(matches!(refused.as_ref(), ResponsesInput::Text(value) if value == &text));
    assert!(short.is_empty());
    assert_eq!(short.budget.used.load(Ordering::Acquire), identity_bytes);
    short.try_accept(steering_text("")).unwrap();
}

#[test]
fn steering_queue_charges_nested_media_and_empty_message_structure() {
    let mut queue = steering_queue(10, 1024);
    let identity_bytes = queue.budget.used.load(Ordering::Acquire);
    for wire in [
        json!([{"type":"message", "role":"user", "content":[{
            "type":"input_image", "image_url":"", "extension":{"nested":["x".repeat(4096)]}
        }]}]),
        serde_json::Value::Array(vec![json!({"type":"message", "role":"user", "content":""}); 100]),
    ] {
        let input = deserialize_from_value(wire.clone()).unwrap();
        let refused = queue.try_accept(input).unwrap_err();
        assert_eq!(serialize_to_value(&refused).unwrap(), wire);
        assert!(queue.is_empty());
        assert_eq!(queue.budget.used.load(Ordering::Acquire), identity_bytes);
    }
    queue.try_accept(steering_text("small retry")).unwrap();
}

#[test]
fn steering_queue_rejects_invalid_identity_or_insufficient_fixed_capacity() {
    let one = NonZeroUsize::new(1).unwrap();
    let capacity = NonZeroUsize::new(10_000).unwrap();
    assert!(matches!(
        SteeringQueue::new(String::new(), one, capacity),
        Err(ExecutorError::InvalidRequest(_))
    ));
    assert!(matches!(
        SteeringQueue::new("resp_parent".to_owned(), one, one),
        Err(ExecutorError::PayloadTooLarge(_))
    ));
}
