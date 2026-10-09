use super::*;
use crate::types::io::CompactionItem;
use serde_json::json;

fn item(value: Value) -> OutputItem {
    serde_json::from_value(value).expect("output item")
}

fn frames(item: &OutputItem, output_index: usize) -> Vec<Value> {
    materialized_item_frames(item, output_index)
        .map(|frame| serialize_to_value(&frame.expect("frame").wire).expect("wire event"))
        .collect()
}

fn types(frames: &[Value]) -> Vec<&str> {
    frames
        .iter()
        .map(|frame| frame["type"].as_str().expect("event type"))
        .collect()
}

#[test]
fn a_message_streams_each_part_and_only_output_text_has_text_events() {
    let message = item(json!({
        "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [
            {"type": "output_text", "text": "hello", "annotations": [{"type": "note"}]},
            {"type": "input_text", "text": "quoted", "source": "mail"}
        ]
    }));
    let frames = frames(&message, 3);

    assert_eq!(
        types(&frames),
        [
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.content_part.added",
            "response.content_part.done",
            "response.output_item.done",
        ]
    );
    assert!(frames.iter().all(|frame| frame["output_index"] == 3));
    assert_eq!(
        frames[0]["item"],
        json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": []})
    );
    assert_eq!(
        frames[1]["part"],
        json!({"type": "output_text", "text": "", "annotations": [{"type": "note"}]})
    );
    assert_eq!(frames[2]["delta"], "hello");
    assert_eq!(frames[3]["text"], "hello");
    assert_eq!(frames[4]["part"]["text"], "hello");
    assert_eq!(
        frames[5]["part"],
        json!({"type": "input_text", "text": "", "source": "mail"})
    );
    assert_eq!(frames[6]["part"]["text"], "quoted");
    for (frame, content_index) in frames[1..7].iter().zip([0, 0, 0, 0, 1, 1]) {
        assert_eq!(frame["item_id"], "msg_1");
        assert_eq!(frame["content_index"], content_index);
    }
    assert_eq!(frames[7]["item"], serialize_to_value(&message).unwrap());
}

#[test]
fn a_function_call_streams_its_arguments_once() {
    let call = item(json!({
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup",
        "arguments": "{\"q\":1}", "status": "completed"
    }));
    let frames = frames(&call, 0);

    assert_eq!(
        types(&frames),
        [
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
        ]
    );
    assert_eq!(frames[0]["item"]["arguments"], "");
    assert_eq!(frames[0]["item"]["status"], "in_progress");
    assert_eq!(frames[0]["item"]["name"], "lookup");
    assert_eq!(frames[1]["item_id"], "fc_1");
    assert_eq!(frames[1]["delta"], "{\"q\":1}");
    assert_eq!(frames[2]["name"], "lookup");
    assert_eq!(frames[2]["arguments"], "{\"q\":1}");
    assert_eq!(frames[3]["item"], serialize_to_value(&call).unwrap());
}

/// An async call keeps its public marker from its first event, as it does when streamed live.
#[test]
fn an_async_function_call_is_marked_from_its_first_event() {
    let call = item(json!({
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup",
        "arguments": "{}", "status": "completed", "async": true
    }));
    let frames = frames(&call, 0);

    assert_eq!(frames[0]["type"], "response.output_item.added");
    assert_eq!(frames[0]["item"]["async"], true);
    assert_eq!(frames[3]["item"]["async"], true);
}

#[test]
fn mcp_discovery_starts_without_tools_and_completes_with_them() {
    let list = item(json!({
        "type": "mcp_list_tools", "id": "mcpl_1", "server_label": "docs",
        "tools": [{"name": "search", "input_schema": {"type": "object"}}]
    }));
    let frames = frames(&list, 1);

    assert_eq!(
        types(&frames),
        [
            "response.output_item.added",
            "response.mcp_list_tools.in_progress",
            "response.mcp_list_tools.completed",
            "response.output_item.done",
        ]
    );
    assert_eq!(frames[0]["item"]["tools"], json!([]));
    assert_eq!(frames[1]["item_id"], "mcpl_1");
    assert_eq!(frames[2]["item_id"], "mcpl_1");
    assert_eq!(frames[3]["item"]["tools"][0]["name"], "search");
}

#[test]
fn other_items_are_added_and_done_whole() {
    let compaction = OutputItem::Compaction(CompactionItem {
        agent: None,
        id: Some("cmp_1".to_owned()),
        encrypted_content: "summary".to_owned(),
    });
    let frames = frames(&compaction, 2);

    assert_eq!(
        types(&frames),
        ["response.output_item.added", "response.output_item.done"]
    );
    let whole = serialize_to_value(&compaction).unwrap();
    assert_eq!(frames[0]["item"], whole);
    assert_eq!(frames[1]["item"], whole);
}

#[test]
fn the_done_frame_presents_the_whole_item() {
    let call = item(json!({
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup",
        "arguments": "{}", "status": "completed"
    }));
    let frame = item_done_frame(&call, 4).unwrap();

    assert_eq!(frame.event_type, SSEEventType::OutputItemDone);
    assert_eq!(frame.wire.output_index, Some(4));
    assert_eq!(frame.wire.rest["item"], serialize_to_value(&call).unwrap());
}
