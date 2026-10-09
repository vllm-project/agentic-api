use super::*;
use crate::events::{EventPayload, normalize_sse_line};
use crate::executor::upstream::tests::request_context;
use crate::types::agent::AgentIdentity;
use crate::types::io::OutputItem;
use crate::utils::common::serialize_to_string;
use futures::poll;
use serde_json::json;
use std::task::Poll;
use tokio::sync::mpsc;

fn item_added(output_index: u64, id: &str) -> EventFrame {
    let mut wire = WireEvent::new("response.output_item.added");
    wire.output_index = Some(output_index);
    wire.rest.insert("item".to_owned(), json!({"id": id}));
    EventFrame {
        event_type: SSEEventType::OutputItemAdded,
        payload: EventPayload::None,
        wire,
    }
}

fn indexless(marker: &str) -> EventFrame {
    let mut wire = WireEvent::new("response.custom");
    wire.rest.insert("marker".to_owned(), json!(marker));
    EventFrame {
        event_type: SSEEventType::Other,
        payload: EventPayload::None,
        wire,
    }
}

fn created(response_id: &str) -> EventFrame {
    normalize_sse_line(&format!(
        r#"data: {{"type":"response.created","response":{{"id":"{response_id}","status":"in_progress"}}}}"#
    ))
    .unwrap()
}

fn translation(frames: Vec<EventFrame>, defer_from_output_index: Option<u32>) -> Translation {
    Translation {
        frames,
        defer_from_output_index,
    }
}

fn wire_bytes(frame: &EventFrame) -> usize {
    serialize_to_string(&frame.wire).unwrap().len()
}

fn label(frame: &EventFrame) -> &str {
    frame
        .wire
        .rest
        .get("item")
        .and_then(|item| item["id"].as_str())
        .or_else(|| frame.wire.rest.get("marker").and_then(Value::as_str))
        .unwrap_or_default()
}

/// Everything enqueued so far, parsed back from its SSE content.
fn delivered(receiver: &mut mpsc::Receiver<StreamEvent>) -> Vec<EventFrame> {
    std::iter::from_fn(|| receiver.try_recv().ok())
        .map(|event| {
            event
                .into_frame()
                .content
                .lines()
                .find_map(normalize_sse_line)
                .expect("SSE data line")
        })
        .collect()
}

fn labels(frames: &[EventFrame]) -> Vec<&str> {
    frames.iter().map(label).collect()
}

fn indexes(frames: &[EventFrame]) -> Vec<Option<u64>> {
    frames.iter().map(|frame| frame.wire.output_index).collect()
}

fn sequence(frames: &[EventFrame]) -> Vec<Option<u64>> {
    frames.iter().map(EventFrame::sequence_number).collect()
}

/// Defer every frame behind a window opened at the round's first output.
async fn defer(relay: &mut StreamRelay, frames: Vec<EventFrame>) -> ExecutorResult<()> {
    relay
        .accept(
            translation(frames, Some(0)),
            &request_context(),
            &ToolRegistry::default(),
        )
        .await
}

#[tokio::test]
async fn synthetic_gateway_items_share_public_indexes_with_upstream_items() {
    let (sender, mut receiver) = mpsc::channel(16);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    for (index, name, kind) in [
        (0, "root", "reasoning"),
        (1, "mcp", "mcp_call"),
        (2, "web", "web_search_call"),
    ] {
        let agent = if name == "root" {
            AgentIdentity::root()
        } else {
            AgentIdentity::root().child(name).unwrap()
        };
        let source = AgentRoundId { agent, round: 0 };
        let frame = EventFrame::synthetic(
            SSEEventType::OutputItemAdded,
            json!({"output_index":1,"item":{"id":name,"type":kind}})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        relay.accept_agent_frame(&source, frame).await.unwrap();
        let frame = delivered(&mut receiver).remove(0);
        assert_eq!(frame.wire.output_index, Some(index));
        assert_eq!(frame.wire.rest["item"]["agent"]["agent_name"], source.agent.as_str());
        assert_eq!(
            relay.projection.take_item(source.agent.as_str(), name),
            Some(usize::try_from(index).unwrap())
        );
        let frame = EventFrame::synthetic(
            SSEEventType::McpCallInProgress,
            json!({"output_index":1,"item_id":name}).as_object().unwrap().clone(),
        )
        .unwrap();
        relay.accept_agent_frame(&source, frame).await.unwrap();
        let frame = delivered(&mut receiver).remove(0);
        assert_eq!(frame.wire.output_index, Some(index));
        relay.finish_agent_source(&source);
    }
}

#[tokio::test]
async fn agent_items_complete_streamed_items_and_materialize_new_ones() {
    let (sender, mut receiver) = mpsc::channel(16);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    let agent = AgentIdentity::root().child("researcher").unwrap();
    let message = |id: &str| -> OutputItem {
        serde_json::from_value(json!({
            "type": "message", "id": id, "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "hi"}],
            "agent": {"agent_name": agent.as_str()}
        }))
        .unwrap()
    };
    let source = AgentRoundId {
        agent: agent.clone(),
        round: 0,
    };
    let added = EventFrame::synthetic(
        SSEEventType::OutputItemAdded,
        json!({"output_index": 0, "item": {"id": "msg_streamed", "type": "message"}})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    relay.accept_agent_frame(&source, added).await.unwrap();
    assert_eq!(delivered(&mut receiver).len(), 1);

    assert_eq!(relay.emit_agent_item(&message("msg_streamed")).await.unwrap(), 0);
    let completed = delivered(&mut receiver);
    assert_eq!(
        completed.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
        [SSEEventType::OutputItemDone],
        "a streamed item needs only its completion"
    );

    assert_eq!(relay.emit_agent_item(&message("msg_new")).await.unwrap(), 1);
    let materialized = delivered(&mut receiver);
    assert_eq!(
        materialized.iter().map(|frame| frame.event_type).collect::<Vec<_>>(),
        [
            SSEEventType::OutputItemAdded,
            SSEEventType::ContentPartAdded,
            SSEEventType::OutputTextDelta,
            SSEEventType::OutputTextDone,
            SSEEventType::ContentPartDone,
            SSEEventType::OutputItemDone,
        ]
    );
    assert!(materialized.iter().all(|frame| frame.wire.output_index == Some(1)));
    for frame in completed.iter().chain(&materialized) {
        let attribution = frame.wire.agent.as_ref().expect("attributed frame");
        assert_eq!(attribution.agent_name, agent.as_str());
    }
    assert_eq!(sequence(&materialized), (2..8).map(Some).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_detached_relay_reserves_agent_item_indexes_without_presenting() {
    let mut relay = StreamRelay::detached();
    let item = OutputItem::Compaction(crate::types::io::CompactionItem {
        agent: None,
        id: Some("cmp_1".to_owned()),
        encrypted_content: "summary".to_owned(),
    });
    assert_eq!(relay.emit_agent_item(&item).await.unwrap(), 0);
    assert_eq!(relay.emit_agent_item(&item).await.unwrap(), 1);
    assert_eq!(relay.upcoming_sequence_number(), 0);
}

#[tokio::test]
async fn release_presents_deferred_frames_in_output_order_with_indexless_frames_last() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(8);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    defer(
        &mut relay,
        vec![
            indexless("diagnostic"),
            item_added(3, "msg_3"),
            item_added(1, "msg_1"),
            item_added(3, "msg_3b"),
        ],
    )
    .await
    .unwrap();
    assert!(receiver.try_recv().is_err(), "an open window presents nothing it hides");

    relay.release_deferred(Release::All, &request).await.unwrap();
    let frames = delivered(&mut receiver);
    assert_eq!(labels(&frames), ["msg_1", "msg_3", "msg_3b", "diagnostic"]);
    assert_eq!(sequence(&frames), [Some(0), Some(1), Some(2), Some(3)]);
    assert!(!relay.has_deferred());
    assert_eq!(relay.deferred.bytes(), 0);
}

#[tokio::test]
async fn a_moving_window_releases_indexed_frames_and_holds_indexless_frames_until_it_closes() {
    let request = request_context();
    let registry = ToolRegistry::default();
    let (sender, mut receiver) = mpsc::channel(8);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    let frames = vec![
        item_added(0, "msg_0"),
        indexless("diagnostic"),
        item_added(2, "msg_2"),
        item_added(1, "msg_1"),
    ];
    relay
        .accept(translation(frames, Some(1)), &request, &registry)
        .await
        .unwrap();
    assert_eq!(labels(&delivered(&mut receiver)), ["msg_0"]);

    relay
        .accept(translation(Vec::new(), Some(2)), &request, &registry)
        .await
        .unwrap();
    assert_eq!(labels(&delivered(&mut receiver)), ["msg_1"]);
    assert_eq!(relay.deferred.len(), 2);

    relay
        .accept(translation(Vec::new(), None), &request, &registry)
        .await
        .unwrap();
    let frames = delivered(&mut receiver);
    assert_eq!(labels(&frames), ["msg_2", "diagnostic"]);
    assert_eq!(sequence(&frames), [Some(2), Some(3)]);
    assert!(!relay.has_deferred());
}

#[tokio::test]
async fn per_item_release_interleaves_upstream_frames_with_gateway_events() {
    // The engine's order after concurrent gateway execution: each item's gateway
    // lifecycle, then its withheld upstream frames, then whatever had no item.
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(8);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    relay.begin_round(5).unwrap();
    defer(
        &mut relay,
        vec![item_added(2, "msg_2"), indexless("diagnostic"), item_added(0, "msg_0")],
    )
    .await
    .unwrap();

    relay.release_deferred(Release::Below(1), &request).await.unwrap();
    relay.emit_local(&mut item_added(6, "ws_1")).await.unwrap();
    relay.release_deferred(Release::Below(2), &request).await.unwrap();
    relay.release_deferred(Release::Below(3), &request).await.unwrap();
    relay.release_deferred(Release::All, &request).await.unwrap();

    let frames = delivered(&mut receiver);
    assert_eq!(labels(&frames), ["msg_0", "ws_1", "msg_2", "diagnostic"]);
    assert_eq!(indexes(&frames), [Some(5), Some(6), Some(7), None]);
    assert_eq!(sequence(&frames), [Some(0), Some(1), Some(2), Some(3)]);
}

#[tokio::test]
async fn item_release_presents_frames_after_an_omitted_call_at_contiguous_indexes() {
    // Item 1 is a refused call the public output omits, so item 2 is public item 1.
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(8);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    relay.begin_round(4).unwrap();
    defer(
        &mut relay,
        vec![item_added(2, "msg_2"), indexless("diagnostic"), item_added(0, "msg_0")],
    )
    .await
    .unwrap();

    let item = |index, public_index| Release::Item { index, public_index };
    relay.release_deferred(item(0, 0), &request).await.unwrap();
    relay.release_deferred(item(2, 1), &request).await.unwrap();
    assert_eq!(relay.deferred.len(), 1, "index-less frames wait for the final release");
    relay.release_deferred(Release::All, &request).await.unwrap();

    let frames = delivered(&mut receiver);
    assert_eq!(labels(&frames), ["msg_0", "msg_2", "diagnostic"]);
    assert_eq!(indexes(&frames), [Some(4), Some(5), None]);
    assert_eq!(sequence(&frames), [Some(0), Some(1), Some(2)]);
}

#[tokio::test]
async fn a_round_cannot_begin_while_the_previous_round_holds_deferred_frames() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(4);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    defer(&mut relay, vec![item_added(0, "msg_0")]).await.unwrap();

    let error = relay.begin_round(1).unwrap_err();
    assert!(error.to_string().contains("left 1 deferred stream events unreleased"));

    relay.release_deferred(Release::All, &request).await.unwrap();
    relay.begin_round(1).unwrap();
    relay
        .emit_upstream(&mut item_added(0, "msg_1"), &request)
        .await
        .unwrap();
    assert_eq!(indexes(&delivered(&mut receiver)), [Some(0), Some(1)]);
}

#[tokio::test]
async fn slow_consumer_backpressure_never_spills_into_the_deferred_buffer() {
    let request = request_context();
    let registry = ToolRegistry::default();
    let (sender, mut receiver) = mpsc::channel(1);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    let frames = (0..3).map(|index| item_added(index, &format!("msg_{index}"))).collect();

    let mut accept = Box::pin(relay.accept(translation(frames, None), &request, &registry));
    let mut drained = Vec::new();
    assert!(poll!(accept.as_mut()).is_pending(), "a full queue stalls the relay");
    drained.push(receiver.try_recv().unwrap());
    assert!(poll!(accept.as_mut()).is_pending(), "one drained slot admits one frame");
    drained.push(receiver.try_recv().unwrap());
    assert!(matches!(poll!(accept.as_mut()), Poll::Ready(Ok(()))));
    drop(accept);
    drained.push(receiver.try_recv().unwrap());

    assert_eq!(
        drained
            .into_iter()
            .map(|event| event.into_frame().sequence_number)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(!relay.has_deferred());
    assert_eq!(relay.deferred.bytes(), 0);
}

#[tokio::test]
async fn a_one_entry_deferred_buffer_rejects_the_next_frame_and_keeps_the_first() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(1);
    let limits = RelayLimits {
        deferred_frames: 1,
        ..RelayLimits::default()
    };
    let mut relay = StreamRelay::client(sender, limits);
    let first = item_added(1, "msg_1");
    let first_bytes = wire_bytes(&first);
    defer(&mut relay, vec![first]).await.unwrap();

    let error = defer(&mut relay, vec![item_added(2, "msg_2")]).await.unwrap_err();
    assert!(error.to_string().contains("deferred stream exceeded 1 buffered events"));
    assert_eq!(relay.deferred.len(), 1);
    assert_eq!(relay.deferred.bytes(), first_bytes);

    relay.release_deferred(Release::All, &request).await.unwrap();
    let frames = delivered(&mut receiver);
    assert_eq!(labels(&frames), ["msg_1"]);
    assert_eq!(sequence(&frames), [Some(0)]);
}

#[tokio::test]
async fn deferred_bytes_bound_both_the_buffer_and_any_single_frame() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(4);
    let first = item_added(1, "msg_1");
    let first_bytes = wire_bytes(&first);
    let limit = first_bytes + first_bytes / 2;
    let limits = RelayLimits {
        deferred_bytes: limit,
        ..RelayLimits::default()
    };
    let mut relay = StreamRelay::client(sender, limits);
    defer(&mut relay, vec![first]).await.unwrap();

    let error = defer(&mut relay, vec![item_added(2, "msg_2")]).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("stream error: deferred stream exceeded {limit} buffered bytes")
    );
    let oversized = item_added(3, &"x".repeat(limit));
    let oversized_bytes = wire_bytes(&oversized);
    let error = defer(&mut relay, vec![oversized]).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "stream error: deferred stream event of {oversized_bytes} bytes exceeds the {limit} buffered-byte limit"
        )
    );
    assert_eq!(relay.deferred.len(), 1, "rejected frames leave the buffer as it was");
    assert_eq!(relay.deferred.bytes(), first_bytes);

    relay.release_deferred(Release::All, &request).await.unwrap();
    assert_eq!(labels(&delivered(&mut receiver)), ["msg_1"]);
    assert_eq!(relay.deferred.bytes(), 0);
}

#[tokio::test]
async fn a_completed_partial_release_refunds_only_the_frames_it_sent() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(1);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    relay.begin_round(5).unwrap();
    let retained = item_added(3, "msg_3");
    let retained_bytes = wire_bytes(&retained);
    defer(
        &mut relay,
        vec![item_added(2, "msg_2"), item_added(1, "msg_1"), retained],
    )
    .await
    .unwrap();

    let mut release = Box::pin(relay.release_deferred(Release::Below(3), &request));
    assert!(poll!(release.as_mut()).is_pending());
    assert_eq!(receiver.try_recv().unwrap().into_frame().sequence_number, 0);
    assert!(matches!(poll!(release.as_mut()), Poll::Ready(Ok(()))));
    drop(release);
    assert_eq!(receiver.try_recv().unwrap().into_frame().sequence_number, 1);
    assert_eq!(relay.deferred.len(), 1);
    assert_eq!(relay.deferred.bytes(), retained_bytes);

    relay.release_deferred(Release::All, &request).await.unwrap();
    let frames = delivered(&mut receiver);
    assert_eq!(indexes(&frames), [Some(8)]);
    assert_eq!(sequence(&frames), [Some(2)]);
    assert_eq!(relay.deferred.bytes(), 0);
}

#[tokio::test]
async fn cancelling_a_release_keeps_unsent_frames_and_their_bytes() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(1);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    let second = item_added(2, "msg_2");
    let second_bytes = wire_bytes(&second);
    defer(&mut relay, vec![item_added(1, "msg_1"), second]).await.unwrap();

    let mut release = Box::pin(relay.release_deferred(Release::All, &request));
    assert!(poll!(release.as_mut()).is_pending());
    assert_eq!(receiver.try_recv().unwrap().into_frame().sequence_number, 0);
    drop(release);

    assert_eq!(relay.deferred.len(), 1);
    assert_eq!(relay.deferred.next(Release::All).map(label), Some("msg_2"));
    assert_eq!(relay.deferred.bytes(), second_bytes);
}

#[tokio::test]
async fn cancelled_send_does_not_consume_lifecycle_or_sequence() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(1);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    relay
        .emit_upstream(&mut item_added(0, "msg_0"), &request)
        .await
        .unwrap();

    let mut pending_frame = created("upstream");
    let mut pending = Box::pin(relay.emit_upstream(&mut pending_frame, &request));
    assert!(poll!(pending.as_mut()).is_pending());
    drop(pending);
    assert_eq!(receiver.try_recv().unwrap().into_frame().sequence_number, 0);

    assert!(relay.emit_upstream(&mut created("upstream"), &request).await.unwrap());
    let delivered = receiver
        .try_recv()
        .expect("cancelled creation was not delivered")
        .into_frame();
    assert_eq!(delivered.sequence_number, 1);
    assert!(delivered.content.contains("resp_test"));
}

#[tokio::test]
async fn rejected_send_does_not_advance_sequence() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(1);
    let mut relay = StreamRelay::client(sender, RelayLimits::with_event_bytes(500 * 1024));
    let error = relay
        .emit_upstream(&mut item_added(0, &"x".repeat(1024 * 1024)), &request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stream event exceeded"));
    assert!(receiver.try_recv().is_err());
    relay
        .emit_upstream(&mut item_added(0, "msg_0"), &request)
        .await
        .unwrap();
    assert_eq!(receiver.try_recv().unwrap().into_frame().sequence_number, 0);
}

#[tokio::test]
async fn local_and_upstream_events_share_numbering_but_not_id_rewriting() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(4);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    relay.begin_round(7).unwrap();
    relay.emit_local(&mut created("local")).await.unwrap();
    let mut progress = normalize_sse_line(
        r#"data: {"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}"#,
    )
    .unwrap();
    relay.emit_upstream(&mut progress, &request).await.unwrap();
    relay.emit_local(&mut item_added(7, "local_item")).await.unwrap();
    relay
        .emit_upstream(&mut item_added(1, "upstream_item"), &request)
        .await
        .unwrap();

    // A duplicate lifecycle event must not wait for a full client queue.
    let mut duplicate_frame = created("local");
    let mut duplicate = Box::pin(relay.emit_local(&mut duplicate_frame));
    assert!(matches!(poll!(duplicate.as_mut()), Poll::Ready(Ok(false))));
    drop(duplicate);

    let frames = delivered(&mut receiver);
    assert_eq!(sequence(&frames), [Some(0), Some(1), Some(2), Some(3)]);
    assert_eq!(frames[0].wire.rest["response"]["id"], "local");
    assert_eq!(frames[1].wire.rest["response"]["id"], "resp_test");
    assert_eq!(frames[2].wire.output_index, Some(7));
    assert_eq!(frames[3].wire.output_index, Some(8));
}

#[tokio::test]
async fn closed_receiver_does_not_consume_presentation_state() {
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let mut relay = StreamRelay::client(sender, RelayLimits::default());
    let error = relay.emit_local(&mut created("local")).await.unwrap_err();
    assert!(error.to_string().contains("stream receiver closed"));

    let mut presentation = relay.into_presentation();
    assert_eq!(presentation.upcoming_sequence_number(), 0);
    assert!(
        presentation.process_event(&mut created("local"), 0),
        "the undelivered response.created can still be presented"
    );
}

#[tokio::test]
async fn a_detached_relay_neither_presents_nor_defers() {
    let request = request_context();
    let mut relay = StreamRelay::detached();
    assert!(!relay.is_live());
    relay
        .accept(
            translation(vec![item_added(0, "msg_0"), indexless("diagnostic")], Some(0)),
            &request,
            &ToolRegistry::default(),
        )
        .await
        .unwrap();
    assert!(!relay.has_deferred());
    assert!(!relay.emit_local(&mut item_added(0, "local")).await.unwrap());
    assert_eq!(relay.upcoming_sequence_number(), 0);
}

#[tokio::test]
async fn an_agent_sink_forwards_unstamped_frames_with_only_the_upstream_offset() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(4);
    let sink = AgentFrameSink {
        agent: AgentIdentity::root(),
        round: 2,
        sender,
    };
    let mut relay = StreamRelay::agent(sink, RelayLimits::default());
    relay.begin_round(3).unwrap();

    let forward = async {
        relay
            .emit_upstream(&mut item_added(1, "upstream"), &request)
            .await
            .unwrap();
        relay.emit_local(&mut item_added(1, "local")).await.unwrap();
    };
    let acknowledge = async {
        let mut forwarded = Vec::new();
        for _ in 0..2 {
            let agent_frame = receiver.recv().await.unwrap();
            forwarded.push((
                agent_frame.round,
                agent_frame.frame.wire.output_index,
                agent_frame.frame.sequence_number(),
            ));
            agent_frame.delivered.send(()).unwrap();
        }
        forwarded
    };
    let ((), forwarded) = tokio::join!(forward, acknowledge);
    assert_eq!(forwarded, [(2, Some(4), None), (2, Some(1), None)]);
}

#[tokio::test]
async fn a_response_sink_owns_the_sequence_for_both_origins() {
    let request = request_context();
    let (sender, mut receiver) = mpsc::channel(4);
    let sink = ResponseEventSink::new(sender, DEFAULT_MAX_STREAM_EVENT_BYTES);
    let mut relay = StreamRelay::response(sink, RelayLimits::default());
    relay.begin_round(2).unwrap();

    assert!(relay.emit_local(&mut created("local")).await.unwrap());
    assert!(!relay.emit_local(&mut created("local")).await.unwrap());
    relay
        .emit_upstream(&mut item_added(1, "upstream"), &request)
        .await
        .unwrap();

    let frames = delivered(&mut receiver);
    assert_eq!(sequence(&frames), [Some(0), Some(1)]);
    assert_eq!(indexes(&frames), [None, Some(3)]);
}
