//! Execute public gateway recordings through the current core, with only inference mocked.
//! Expected output and history come from the captures, never from the current store.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::executor::error::ExecutorError;
use agentic_core::executor::{BoxStream, ExecuteRequest, compact_response};
use agentic_core::types::conversations::{ConversationResponse, CreateConversationRequest};
use agentic_core::types::io::{ItemLimit, ItemOrder, ItemPage, OutputItem};
use agentic_core::types::request_response::ResponsePayload;
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::support::replay::{IdPairs, compare_value, remap_error_message, remap_ids};
use crate::support::{Cassette, MockResponse, TestFixture, Turn, named_sse_events, streamed_sse_events, text_response};

fn inference_response(response: &Value, items: Vec<Value>, stream: bool) -> MockResponse {
    let mut response = response.clone();
    response["output"] = Value::Array(items);
    response["usage"] = Value::Null;
    for item in response["output"].as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("agent");
        assert_ne!(
            item["type"], "compaction",
            "the model must not supply gateway checkpoints"
        );
    }
    if !stream {
        return MockResponse::Json(response.to_string());
    }
    let mut started = response.clone();
    started["status"] = json!("in_progress");
    started["output"] = json!([]);
    let mut events = vec![
        json!({"type": "response.created", "response": started}),
        json!({"type": "response.in_progress", "response": started}),
    ];
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        events.push(json!({"type": "response.output_item.added", "output_index": index, "item": added}));
        events.push(json!({"type": "response.output_item.done", "output_index": index, "item": item}));
    }
    events.push(json!({"type": "response.completed", "response": response}));
    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse(body)
}

fn model_responses(turn: &Turn<Value>) -> Vec<MockResponse> {
    if turn.response.status_code != Some(200)
        || turn.request.method != "POST"
        || !turn.request.path.starts_with("/v1/responses")
    {
        return Vec::new();
    }
    let response = terminal(turn);
    let standalone = turn.request.path.ends_with("/compact");
    let stream = turn.request.body["stream"] == true;
    let mut inference = Vec::new();
    let mut items = Vec::new();
    for item in response["output"].as_array().unwrap() {
        if item["type"] != "compaction" {
            if !standalone {
                items.push(item.clone());
                // vLLM ends its inference at the async call; the gateway obtains the later answer in another round.
                if item["type"] == "function_call" && item["async"] == true {
                    inference.push(inference_response(&response, std::mem::take(&mut items), stream));
                }
            }
            continue;
        }
        if !items.is_empty() {
            inference.push(inference_response(&response, std::mem::take(&mut items), stream));
        }
        let content = item["encrypted_content"].as_str().unwrap();
        let carried = serde_json::from_str::<Value>(content).ok();
        let summary = carried
            .as_ref()
            .and_then(|value| value["summary"].as_str())
            .unwrap_or(content);
        inference.push(text_response(summary));
    }
    if !items.is_empty() {
        inference.push(inference_response(&response, items, stream));
    }
    inference
}

fn terminal(turn: &Turn<Value>) -> Value {
    turn.response.body.clone().unwrap_or_else(|| {
        let events = named_sse_events(turn.response.sse.as_ref().unwrap().iter().flat_map(|line| line.lines()));
        events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("recorded response completed")["response"]
            .clone()
    })
}

fn comparable(mut value: Value) -> Value {
    match &mut value {
        Value::Object(fields) => {
            // Gateway carried windows contain generated user IDs. Compare their structure and ID relationships,
            // including all retained text, rather than treating that JSON encoding as an opaque random string.
            if fields.get("type").and_then(Value::as_str) == Some("compaction")
                && let Some(content) = fields.get("encrypted_content").and_then(Value::as_str)
                && let Ok(carried) = serde_json::from_str::<Value>(content)
                && carried.get("summary").is_some()
            {
                fields.insert("encrypted_content".into(), carried);
            }
            if fields.get("type").and_then(Value::as_str) == Some("output_text") {
                fields.entry("logprobs").or_insert_with(|| json!([]));
            }
            if fields.get("type").and_then(Value::as_str) == Some("reasoning")
                && fields.get("encrypted_content") == Some(&Value::Null)
            {
                fields.remove("encrypted_content");
            }
            for value in fields.values_mut() {
                *value = comparable(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = comparable(std::mem::take(value));
            }
        }
        _ => {}
    }
    value
}

fn response_fields(response: &Value) -> Value {
    comparable(json!({
        "id": response["id"], "object": response["object"], "status": response["status"],
        "store": response["store"], "output": response["output"],
        "previous_response_id": response["previous_response_id"], "conversation": response["conversation"]
    }))
}

fn assert_stream(events: &[Value], response: &Value) {
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.in_progress");
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    for pair in events.windows(2) {
        assert!(pair[0]["sequence_number"].as_u64().unwrap() < pair[1]["sequence_number"].as_u64().unwrap());
    }
    let output = response["output"].as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .count(),
        output.len()
    );
    for (index, item) in output.iter().enumerate() {
        for kind in ["response.output_item.added", "response.output_item.done"] {
            let matches: Vec<_> = events
                .iter()
                .filter(|event| event["type"] == kind && event["output_index"] == json!(index))
                .collect();
            assert_eq!(matches.len(), 1, "one {kind} for output {index}");
            assert_eq!(matches[0]["item"]["id"], item["id"]);
            if kind == "response.output_item.done" {
                let done: OutputItem = serde_json::from_value(matches[0]["item"].clone()).unwrap();
                let terminal: OutputItem = serde_json::from_value(item.clone()).unwrap();
                assert_eq!(
                    serde_json::to_value(done).unwrap(),
                    serde_json::to_value(terminal).unwrap()
                );
            }
        }
    }
}

async fn finish(response: Either<ResponsePayload, BoxStream>) -> (Value, Vec<Value>) {
    match response {
        Either::Left(response) => (serde_json::to_value(response).unwrap(), Vec::new()),
        Either::Right(stream) => {
            let chunks = stream.collect::<Vec<_>>().await;
            let events = streamed_sse_events(&chunks);
            let response = events.last().expect("stream events")["response"].clone();
            assert_stream(&events, &response);
            (response, events)
        }
    }
}

async fn execute(
    fixture: &TestFixture,
    turn: &Turn<Value>,
    ids: &IdPairs,
) -> Result<(Value, Vec<Value>), Box<ExecutorError>> {
    let body = remap_ids(&turn.request.body, ids);
    let segments: Vec<_> = turn
        .request
        .path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let result = match (turn.request.method.as_str(), segments.as_slice()) {
        ("POST", ["v1", "responses"]) => {
            return Ok(finish(
                ExecuteRequest::new(serde_json::from_value(body).unwrap(), Arc::clone(&fixture.exec_ctx))
                    .run()
                    .await?,
            )
            .await);
        }
        ("POST", ["v1", "responses", "compact"]) => serde_json::to_value(
            compact_response(serde_json::from_value(body).unwrap(), &fixture.exec_ctx, None).await?,
        )
        .unwrap(),
        ("POST", ["v1", "conversations"]) => {
            let request: CreateConversationRequest = serde_json::from_value(body).unwrap();
            let data = fixture
                .exec_ctx
                .conv_handler
                .create(
                    "default_tenant",
                    &request.metadata.unwrap_or_default(),
                    &request.items.unwrap_or_default(),
                )
                .await?;
            serde_json::to_value(ConversationResponse::new(
                data.conversation_id,
                data.created_at,
                data.metadata,
            ))
            .unwrap()
        }
        ("GET", ["v1", "responses", id]) => {
            serde_json::to_value(fixture.exec_ctx.resp_handler.get_snapshot(&ids.forward[*id]).await?).unwrap()
        }
        ("GET", ["v1", resource, id, collection]) => {
            let query = remap_ids(&turn.request.query_params, ids);
            let page = ItemPage {
                limit: query["limit"].as_str().map_or_else(ItemLimit::default, |value| {
                    ItemLimit::try_from(value.parse::<i64>().unwrap()).unwrap()
                }),
                order: query["order"].as_str().map_or_else(ItemOrder::default, |value| {
                    serde_json::from_value(json!(value)).unwrap()
                }),
                after: query["after"].as_str(),
            };
            let items = match (*resource, *collection) {
                ("responses", "input_items") => {
                    fixture
                        .exec_ctx
                        .resp_handler
                        .list_input_items(&ids.forward[*id], page)
                        .await?
                }
                ("conversations", "items") => {
                    fixture
                        .exec_ctx
                        .conv_handler
                        .list_items("default_tenant", &ids.forward[*id], page)
                        .await?
                }
                _ => panic!("unsupported recorded GET: {}", turn.request.path),
            };
            serde_json::to_value(items).unwrap()
        }
        _ => panic!("unsupported recorded route: {}", turn.request.path),
    };
    Ok((result, Vec::new()))
}

fn compare(turn: &Turn<Value>, actual: Result<(Value, Vec<Value>), Box<ExecutorError>>, ids: &mut IdPairs) {
    let (status, actual, events) = match actual {
        Ok((body, events)) => (200, body, events),
        Err(error) => (
            error.http_status().as_u16(),
            serde_json::from_slice(&(*error).into_response_body()).unwrap(),
            Vec::new(),
        ),
    };
    assert_eq!(Some(status), turn.response.status_code, "{}: {actual}", turn.filename);
    assert_eq!(
        !events.is_empty(),
        turn.response.sse.is_some(),
        "{} transport",
        turn.filename
    );
    let mut expected = terminal(turn);
    if status >= 400 {
        remap_error_message(&mut expected, ids);
    }
    if status == 200 && turn.request.path.starts_with("/v1/responses") && turn.request.method == "POST" {
        compare_value(
            &response_fields(&expected),
            &response_fields(&actual),
            &turn.filename,
            ids,
            false,
        );
    } else {
        compare_value(&comparable(expected), &comparable(actual), &turn.filename, ids, false);
    }
}

async fn replay(scenario: &str) {
    let path = format!("{}/compaction-{scenario}-gateway.yaml", super::DIR);
    let recording: Cassette<Value> =
        serde_yaml::from_str(&std::fs::read_to_string(path).expect("gateway cassette required")).unwrap();
    let mut order: Vec<_> = (0..recording.turns.len()).collect();
    if scenario == "stale" {
        // Prepare the streamed turn before the side turn commits, then resume its inference afterward.
        order.swap(2, 3);
    }
    let responses = order
        .into_iter()
        .flat_map(|index| model_responses(&recording.turns[index]))
        .collect::<Vec<_>>();
    let inference_count = responses.len();
    let fixture = TestFixture::new_with_responses(responses).await;
    let mut ids = IdPairs::default();
    let mut index = 0;
    while index < recording.turns.len() {
        let turn = &recording.turns[index];
        if scenario == "stale" && index == 2 {
            let body = remap_ids(&turn.request.body, &ids);
            let pending = ExecuteRequest::new(serde_json::from_value(body).unwrap(), Arc::clone(&fixture.exec_ctx))
                .run()
                .await
                .unwrap();
            assert!(matches!(&pending, Either::Right(_)));
            let side = &recording.turns[index + 1];
            compare(side, execute(&fixture, side, &ids).await, &mut ids);
            compare(turn, Ok(finish(pending).await), &mut ids);
            index += 2;
        } else {
            compare(turn, execute(&fixture, turn, &ids).await, &mut ids);
            index += 1;
        }
    }
    let requests = fixture.request_bodies().await;
    assert_eq!(
        requests.len(),
        inference_count,
        "all recorded inference work must execute"
    );
    let summaries: Vec<_> = requests
        .iter()
        .filter(|request| {
            request["input"].as_array().unwrap().last().unwrap()["content"]
                .to_string()
                .contains(super::COMPACTION_PROMPT)
        })
        .collect();
    assert!(!summaries.is_empty(), "the current executor must actually summarize");
    for summary in &summaries {
        assert!(summary["tools"].is_null() || summary["tools"] == json!([]));
        assert_eq!(summary["store"], false);
    }
    if scenario == "stale" {
        assert!(
            !summaries[0]["input"].to_string().contains("VIOLET"),
            "compaction must use its original history snapshot"
        );
    }
    if scenario == "standalone" {
        let image_url = recording.turns[5].request.body["input"][0]["content"][1]["image_url"]
            .as_str()
            .unwrap();
        assert!(
            requests.last().unwrap()["input"].to_string().contains(image_url),
            "the third stored response must still receive the retained image, as in the OpenAI reference"
        );
    }
    // The last turn asks for the remembered word after the client sent a compaction back as input: only a stored
    // one stays history, as on OpenAI.
    let last_input = requests.last().unwrap()["input"].to_string();
    if scenario == "store-false" {
        assert!(
            !last_input.contains("APRICOT"),
            "an unstored checkpoint is used for its turn only"
        );
    } else if scenario.starts_with("in-turn-chain") {
        assert!(last_input.contains("APRICOT"), "a stored checkpoint stays history");
    }
}

async fn run(scenario: &str) {
    tokio::time::timeout(Duration::from_secs(30), replay(scenario))
        .await
        .expect("bounded gateway replay");
}

#[tokio::test]
async fn gateway_replays_stale_compaction() {
    run("stale").await;
}
#[tokio::test]
async fn gateway_replays_in_turn_chain() {
    run("in-turn-chain").await;
}
#[tokio::test]
async fn gateway_replays_streamed_chain() {
    run("in-turn-chain-stream").await;
}
#[tokio::test]
async fn gateway_replays_tool_loop() {
    run("tool-loop").await;
}
#[tokio::test]
async fn gateway_replays_standalone_compaction() {
    run("standalone").await;
}
#[tokio::test]
async fn gateway_replays_multi_agent_compaction() {
    run("multi-agent").await;
}
#[tokio::test]
async fn gateway_replays_unstored_compaction() {
    run("store-false").await;
}
