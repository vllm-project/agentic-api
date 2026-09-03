use std::fs;
use std::path::{Path, PathBuf};

use agentic_core::executor::request::RequestContext;
use agentic_core::executor::{UpstreamBody, decode_upstream};
use agentic_core::types::request_response::RequestPayload;
use serde_json::json;

fn request_context() -> RequestContext {
    let request: RequestPayload = serde_json::from_value(json!({
        "model": "test-model",
        "input": "hi",
        "store": true,
        "stream": true
    }))
    .expect("valid request");
    RequestContext {
        original_request: request.clone(),
        enriched_request: request,
        new_input_items: Vec::new(),
        response_id: "resp_reserved".to_owned(),
        conversation_id: None,
        conversation_version: None,
    }
}

fn yaml_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())) {
            let path = entry.expect("cassette directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "yaml") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn response_streams(path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let document: serde_json::Value =
        serde_yaml::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));

    if let Some(sse) = document["sse"].as_array() {
        return vec![
            sse.iter()
                .map(|chunk| chunk.as_str().expect("SSE chunk string").to_owned())
                .collect(),
        ];
    }

    document["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|turn| turn["request"]["path"] == "/v1/responses")
        .filter_map(|turn| turn["response"]["sse"].as_array())
        .map(|sse| {
            sse.iter()
                .map(|chunk| chunk.as_str().expect("SSE chunk string").to_owned())
                .collect()
        })
        .collect()
}

fn is_responses_event_stream(stream: &str) -> bool {
    stream.lines().any(|line| {
        line.strip_prefix("data: ")
            .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .is_some_and(|event| event["type"] == "response.created")
    })
}

fn message_stream(terminal_ids: [&str; 2]) -> String {
    [
        json!({
            "type": "response.created",
            "response": {"id": "resp_upstream", "status": "in_progress"}
        }),
        json!({
            "type": "response.in_progress",
            "response": {"id": "resp_upstream", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"id": "msg_streamed_0", "type": "message", "role": "assistant", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"id": "msg_streamed_0", "type": "message", "role": "assistant", "status": "completed"}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"id": "msg_streamed_1", "type": "message", "role": "assistant", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {"id": "msg_streamed_1", "type": "message", "role": "assistant", "status": "completed"}
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_upstream",
                "status": "completed",
                "output": [
                    {"id": terminal_ids[0], "type": "message", "role": "assistant", "status": "completed"},
                    {"id": terminal_ids[1], "type": "message", "role": "assistant", "status": "completed"}
                ]
            }
        }),
    ]
    .map(|event| format!("data: {event}"))
    .join("\n")
}

#[test]
fn strict_relay_decoder_accepts_data_lines_without_a_space() {
    let stream = message_stream(["msg_terminal_0", "msg_terminal_1"]).replace("data: ", "data:");

    decode_upstream(&request_context(), UpstreamBody::Sse(&stream))
        .expect("SSE data fields may omit the optional space after the colon");
}

#[test]
fn strict_relay_decoder_rejects_duplicate_terminal_item_ids() {
    let stream = message_stream(["msg_terminal", "msg_terminal"]);

    let error = decode_upstream(&request_context(), UpstreamBody::Sse(&stream))
        .expect_err("duplicate terminal item ids must be rejected");
    assert!(error.to_string().contains("repeats output item 'msg_terminal'"));
}

#[test]
fn strict_relay_decoder_accepts_recorded_responses_streams() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes");
    let mut decoded = 0;

    for path in yaml_files(&root) {
        for (turn_index, chunks) in response_streams(&path).into_iter().enumerate() {
            let stream = chunks.join("\n");
            if !is_responses_event_stream(&stream) {
                continue;
            }
            decode_upstream(&request_context(), UpstreamBody::Sse(&stream)).unwrap_or_else(|error| {
                panic!(
                    "{} turn {} was rejected: {error}",
                    path.strip_prefix(&root).expect("cassette below root").display(),
                    turn_index + 1
                )
            });
            decoded += 1;
        }
    }

    assert!(decoded >= 50, "decoded only {decoded} recorded Responses SSE streams");
}
