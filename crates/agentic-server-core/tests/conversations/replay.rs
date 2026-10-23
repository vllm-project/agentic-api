//! Only model output is mocked. Management, execution, and persistence run locally.
//! Expected histories come from recorded requests/results, never from the gateway store.

use super::edge_replay::{execute_step, remap_ids};
use super::support::{MockResponse, TestFixture};
use super::{
    Cassette, IdPairs, RecordedResponse, Turn, Value, assert_conversation_object_shape, assert_sse_conversation_shapes,
    compare_value, lifecycle, result_body,
};
use agentic_core::executor::ExecuteRequest;
use either::Either;
use futures::StreamExt;
use std::sync::Arc;

#[path = "replay_history.rs"]
mod history;

fn normalize_recorded_defaults(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            // Older gateway recordings emitted null for absent opaque state.
            if fields.get("type").and_then(Value::as_str) == Some("reasoning")
                && fields.get("encrypted_content") == Some(&Value::Null)
            {
                fields.remove("encrypted_content");
            }
            // Missing or null logprobs now serialize as an empty collection.
            if fields.get("type").and_then(Value::as_str) == Some("output_text")
                && fields.get("logprobs").is_none_or(Value::is_null)
            {
                fields.insert("logprobs".into(), Value::Array(Vec::new()));
            }
            for value in fields.values_mut() {
                normalize_recorded_defaults(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(normalize_recorded_defaults),
        _ => {}
    }
}

fn response_fields(body: &Value) -> Value {
    let mut fields = serde_json::Map::new();
    for field in [
        "id",
        "object",
        "status",
        "output",
        "previous_response_id",
        "conversation",
    ] {
        fields.insert(field.to_owned(), body[field].clone());
    }
    let mut result = Value::Object(fields);
    normalize_recorded_defaults(&mut result);
    result
}

fn model_response(turn: &Turn) -> MockResponse {
    assert_eq!(turn.response.status_code, 200);
    if let Some(lines) = &turn.response.sse {
        let mut body = lines.join("");
        if !body.contains("data: [DONE]") {
            body.push_str("data: [DONE]\n\n");
        }
        MockResponse::Sse(body)
    } else {
        MockResponse::Json(turn.response.body.as_ref().expect("recorded model output").to_string())
    }
}

async fn execute(fixture: &TestFixture, turn: &Turn, ids: &IdPairs) -> RecordedResponse {
    if turn.request.path != "/v1/responses" {
        return match execute_step(&fixture.exec_ctx.conv_handler, turn, ids).await {
            Ok(body) => RecordedResponse {
                status_code: 200,
                body: Some(body),
                sse: None,
            },
            Err(error) => RecordedResponse {
                status_code: error.http_status().as_u16(),
                body: Some(serde_json::from_slice(&(*error).into_response_body()).unwrap()),
                sse: None,
            },
        };
    }
    let request = serde_json::from_value(remap_ids(&turn.request.body, ids)).unwrap();
    let result = ExecuteRequest::new(request, Arc::clone(&fixture.exec_ctx))
        .run()
        .await
        .expect("execute recorded Responses request");
    match result {
        Either::Left(payload) => RecordedResponse {
            status_code: 200,
            body: Some(serde_json::to_value(payload).unwrap()),
            sse: None,
        },
        Either::Right(mut stream) => {
            let mut body = String::new();
            // Drain the stream so terminal persistence completes before checking storage.
            while let Some(chunk) = stream.next().await {
                body.push_str(&chunk);
            }
            RecordedResponse {
                status_code: 200,
                body: None,
                sse: Some(body.lines().map(str::to_owned).collect()),
            }
        }
    }
}

pub(super) async fn run(name: &str, recording: &Cassette) {
    let responses = recording
        .turns
        .iter()
        .filter(|turn| turn.request.path == "/v1/responses")
        .map(model_response)
        .collect();
    let fixture = TestFixture::new_with_responses(responses).await;
    let mut ids = IdPairs::default();
    let mut histories = history::Histories::default();
    let mut model_requests = 0;
    for turn in &recording.turns {
        let location = format!("{name}/{}", turn.filename);
        let actual = execute(&fixture, turn, &ids).await;
        assert_eq!(actual.status_code, turn.response.status_code, "{location}");
        assert_eq!(actual.sse.is_some(), turn.response.sse.is_some(), "{location}");
        let mut expected_body = result_body(&turn.response);
        normalize_recorded_defaults(&mut expected_body);
        let actual_body = result_body(&actual);
        if turn.request.path == "/v1/responses" {
            let has_conversation = !turn.request.body["conversation"].is_null();
            assert_conversation_object_shape(&actual_body, has_conversation, &location);
            if actual.sse.is_some() {
                assert_sse_conversation_shapes(&actual, has_conversation, &location);
                assert_eq!(lifecycle(&turn.response), lifecycle(&actual), "{location}");
            }
            // Runtime metadata (usage, timings, model defaults) is not storage behavior.
            // Output text/content is exact: the recorded model output is deterministic.
            for item in expected_body["output"].as_array().unwrap() {
                let id = item["id"].as_str().expect("recorded output public ID");
                ids.compare(id, id, &location);
            }
            compare_value(
                &response_fields(&expected_body),
                &response_fields(&actual_body),
                &location,
                &mut ids,
                false,
            );
            model_requests += 1;
            let requests = fixture.request_bodies().await;
            assert_eq!(requests.len(), model_requests, "{location}: model request count");
            histories.response(
                turn,
                &expected_body,
                &requests.last().unwrap()["input"],
                &ids,
                &location,
            );
        } else {
            // Compare complete management results, including public IDs, page boundaries,
            // item content/order, first_id/last_id, and has_more.
            compare_value(&expected_body, &actual_body, &location, &mut ids, false);
            histories.management(turn, &expected_body);
        }
        // Recheck every prior snapshot after append/delete/branch operations, too.
        histories.assert_stored(&fixture, &ids, &location).await;
    }
    assert_eq!(fixture.request_bodies().await.len(), model_requests, "{name}");
}
