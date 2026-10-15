use agentic_core::events::{EventPayload, SSEEventType, normalize_sse_line};

#[test]
fn test_reasoning_delta() {
    let line = r#"data: {"type":"response.reasoning_summary_text.delta","delta":"Let me think","item_id":"rs_1","output_index":2,"summary_index":1,"sequence_number":3}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningSummaryTextDelta);
    if let EventPayload::ReasoningSummaryTextDelta {
        delta,
        item_id,
        output_index,
        summary_index,
    } = &frame.payload
    {
        assert_eq!(delta, "Let me think");
        assert_eq!(item_id, "rs_1");
        assert_eq!(*output_index, Some(2));
        assert_eq!(*summary_index, 1);
    } else {
        panic!("expected ReasoningSummaryTextDelta payload");
    }
}

#[test]
fn test_reasoning_done_reads_text_not_delta() {
    let line = r#"data: {"type":"response.reasoning_summary_text.done","text":"Full reasoning summary here","item_id":"rs_1","output_index":2,"summary_index":1,"sequence_number":5}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningSummaryTextDone);
    if let EventPayload::ReasoningSummaryTextDone {
        text,
        item_id,
        output_index,
        summary_index,
    } = &frame.payload
    {
        assert_eq!(text, "Full reasoning summary here");
        assert_eq!(item_id, "rs_1");
        assert_eq!(*output_index, Some(2));
        assert_eq!(*summary_index, 1);
    } else {
        panic!("expected ReasoningSummaryTextDone payload");
    }
}

#[test]
fn test_reasoning_text_delta() {
    let line = r#"data: {"type":"response.reasoning_text.delta","delta":"The user asks","item_id":"rs_1","output_index":0,"content_index":0,"sequence_number":4}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningTextDelta);
    if let EventPayload::ReasoningTextDelta {
        delta,
        item_id,
        output_index,
        content_index,
    } = &frame.payload
    {
        assert_eq!(delta, "The user asks");
        assert_eq!(item_id, "rs_1");
        assert_eq!(*output_index, Some(0));
        assert_eq!(*content_index, 0);
    } else {
        panic!("expected ReasoningTextDelta payload");
    }
}

#[test]
fn test_reasoning_text_done() {
    let line = r#"data: {"type":"response.reasoning_text.done","text":"The user asks about math.","item_id":"rs_1","output_index":0,"content_index":0,"sequence_number":10}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningTextDone);
    if let EventPayload::ReasoningTextDone {
        text,
        item_id,
        output_index,
        content_index,
    } = &frame.payload
    {
        assert_eq!(text, "The user asks about math.");
        assert_eq!(item_id, "rs_1");
        assert_eq!(*output_index, Some(0));
        assert_eq!(*content_index, 0);
    } else {
        panic!("expected ReasoningTextDone payload");
    }
}

#[test]
fn test_reasoning_part_added_classified() {
    let line = r#"data: {"type":"response.reasoning_part.added","content_index":0,"item_id":"rs_1","output_index":0,"part":{"text":"","type":"reasoning_text"},"sequence_number":3}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningPartAdded);
    assert!(matches!(frame.payload, EventPayload::Raw(_)));
}

#[test]
fn test_reasoning_part_done_classified() {
    let line = r#"data: {"type":"response.reasoning_part.done","content_index":0,"item_id":"rs_1","output_index":0,"part":{"text":"thinking...","type":"reasoning_text"},"sequence_number":80}"#;
    let frame = normalize_sse_line(line).unwrap();
    assert_eq!(frame.event_type, SSEEventType::ReasoningPartDone);
    assert!(matches!(frame.payload, EventPayload::Raw(_)));
}
