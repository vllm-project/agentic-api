//! Replay actual `OpenAI` and gateway `store:false` exchanges through the HTTP gateway.
use super::*;
use agentic_core::types::io::OutputItem;
use serde_json::{Value, json};
use std::collections::HashMap;

const DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../agentic-server-core/tests/cassettes/store_false"
);

fn terminal(turn: &Value) -> Value {
    if let Some(body) = turn["response"].get("body") {
        return body.clone();
    }
    sse_events(&recorded_sse(turn))
        .into_iter()
        .find(|event| event["type"] == "response.completed")
        .expect("recorded terminal event")["response"]
        .clone()
}

fn recorded_sse(turn: &Value) -> String {
    turn["response"]["sse"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect()
}

fn input_items(input: &Value) -> Vec<Value> {
    if let Some(text) = input.as_str() {
        vec![json!({"type":"message", "role":"user", "content":text})]
    } else {
        input.as_array().unwrap().clone()
    }
}

// Compare the ordered model context independently of generated item IDs and
// content-part framing. Call IDs, arguments and outputs remain significant.
fn history_content(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let kind = item["type"].as_str().unwrap_or("message");
            if kind == "message" {
                let text = item["content"].as_str().map_or_else(
                    || {
                        item["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|part| part["text"].as_str().unwrap())
                            .collect::<String>()
                    },
                    str::to_owned,
                );
                json!({"type":kind, "role":item["role"], "text":text})
            } else {
                let mut value = item.clone();
                value.as_object_mut().unwrap().remove("id");
                value.as_object_mut().unwrap().remove("status");
                value
            }
        })
        .collect()
}

fn public_output(output: &Value) -> Value {
    let typed: Vec<OutputItem> = serde_json::from_value(output.clone()).unwrap();
    let mut output = serde_json::to_value(typed).unwrap();
    for item in output.as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("id");
        if let Some(content) = item["content"].as_array_mut() {
            for part in content {
                // No log probabilities may be represented as absent or an empty array.
                if part["logprobs"].as_array().is_some_and(Vec::is_empty) {
                    part.as_object_mut().unwrap().remove("logprobs");
                }
            }
        }
    }
    output
}

fn assert_recorded_error(actual: &Value, turn: &Value, ids: &HashMap<String, String>) {
    let mut expected = terminal(turn);
    if let Some(message) = expected["error"]["message"].as_str() {
        let mut message = message.to_owned();
        for (recorded, local) in ids {
            message = message.replace(recorded, local);
        }
        expected["error"]["message"] = json!(message);
    }
    assert_eq!(*actual, expected, "recorded continuation error");
}

async fn response_body(response: reqwest::Response, stream: bool) -> Value {
    if stream {
        let events = sse_events(&response.text().await.unwrap());
        assert_eq!(events.last().unwrap()["type"], "response.completed");
        events.last().unwrap()["response"].clone()
    } else {
        response.json().await.unwrap()
    }
}

fn load_cassette(scenario: &str, provider: &str, mode: &str) -> Value {
    let path = format!("{DIR}/store-false-{provider}-{mode}.yaml");
    let mut cassette: Value = serde_yml::from_str(&std::fs::read_to_string(path).expect("recorded cassette")).unwrap();
    cassette["cases"]
        .as_object_mut()
        .expect("named cassette scenarios")
        .remove(scenario)
        .expect("recorded scenario")
}

async fn replay(scenario: &str, provider: &str, stream: bool) {
    let mode = if stream { "streaming" } else { "nonstreaming" };
    let cassette = load_cassette(scenario, provider, mode);
    let turns = cassette["turns"].as_array().unwrap();
    let inference_turns: Vec<_> = turns
        .iter()
        .filter(|turn| turn["request"]["path"] == "/v1/responses" && turn["response"]["status_code"] == 200)
        .collect();
    let (llm_url, requests, llm) = if stream {
        spawn_tool_search_sse_sequence(inference_turns.iter().map(|turn| recorded_sse(turn)).collect()).await
    } else {
        spawn_mock_vllm_json_capture_sequence(inference_turns.iter().map(|turn| terminal(turn)).collect()).await
    };
    let fixture = storage_backed_state(&llm_url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let mut ids = HashMap::<String, String>::new();
    let mut histories = HashMap::<String, Vec<Value>>::new();
    let mut expected_inputs = Vec::new();
    let mut stored_responses = 0;
    let mut stored_items = 0;
    for turn in turns {
        let mut body = turn["request"]["body"].clone();
        let path = turn["request"]["path"].as_str().unwrap();
        assert_eq!(turn["request"]["method"], "POST");
        let parent = body["previous_response_id"].as_str().map(str::to_owned);
        let conversation = body["conversation"].as_str().map(str::to_owned);
        for field in ["previous_response_id", "conversation"] {
            if let Some(recorded_id) = body[field].as_str() {
                body[field] = json!(ids.get(recorded_id).expect("earlier recorded response or conversation"));
            }
        }
        let response = client.post(format!("{url}{path}")).json(&body).send().await.unwrap();
        let status = response.status();
        assert_eq!(
            u64::from(status.as_u16()),
            turn["response"]["status_code"].as_u64().unwrap(),
            "{scenario}/{mode}/{}",
            turn["filename"]
        );
        if !status.is_success() {
            assert_recorded_error(&response.json().await.unwrap(), turn, &ids);
            continue;
        }
        let expected = terminal(turn);
        let actual = response_body(response, stream && path == "/v1/responses").await;
        ids.insert(
            expected["id"].as_str().unwrap().to_owned(),
            actual["id"].as_str().unwrap().to_owned(),
        );
        if path == "/v1/conversations" {
            histories.insert(expected["id"].as_str().unwrap().to_owned(), Vec::new());
            continue;
        }
        assert_eq!(actual["store"], expected["store"]);
        assert_eq!(actual["status"], expected["status"]);
        assert_eq!(
            public_output(&actual["output"]),
            public_output(&expected["output"]),
            "{scenario}/{mode}"
        );
        let original_input = input_items(&body["input"]);
        let mut history = parent
            .as_ref()
            .or(conversation.as_ref())
            .map_or_else(Vec::new, |id| histories.get(id).expect("stored parent history").clone());
        history.extend(original_input.iter().cloned());
        expected_inputs.push(history_content(&history));
        let output = expected["output"].as_array().unwrap();
        history.extend(output.iter().cloned());
        if body["store"] == true {
            stored_responses += 1;
            stored_items += i64::try_from(original_input.len() + output.len()).unwrap();
            histories.insert(expected["id"].as_str().unwrap().to_owned(), history.clone());
            if let Some(conversation) = conversation {
                histories.insert(conversation, history);
            }
        } else {
            assert_response_not_persisted(&fixture.pool, actual["id"].as_str().unwrap()).await;
        }
    }
    let captured = requests.lock().await;
    assert_eq!(
        captured.len(),
        expected_inputs.len(),
        "rejected continuations must not invoke inference"
    );
    for (request, expected) in captured.iter().zip(expected_inputs) {
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("conversation").is_none());
        assert_eq!(
            history_content(request["input"].as_array().unwrap()),
            expected,
            "normalized full history for {scenario}/{mode}"
        );
    }
    assert_eq!(stored_row_counts(&fixture.pool).await, (stored_responses, stored_items));
    gateway.abort();
    llm.abort();
}

#[tokio::test]
async fn recorded_store_false_json_cases_match_openai() {
    for scenario in [
        "manual-replay",
        "stored-parent",
        "unstored-parent",
        "conversation",
        "function-replay",
    ] {
        replay(scenario, "openai-gpt-4o", false).await;
    }
}

/// Reuse the original two-turn mixed web/function/custom capture as inference
/// fixtures, replacing stored continuation with client-supplied public history.
#[tokio::test]
async fn recorded_store_false_mixed_tools_replay_full_history() {
    for stream in [false, true] {
        let cassette = common::load_mixed_gateway_capture(stream);
        let turns = cassette["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        let (llm_url, requests, llm) = if stream {
            spawn_tool_search_sse_sequence(turns.iter().map(recorded_sse).collect()).await
        } else {
            spawn_mock_vllm_json_capture_sequence(turns.iter().map(terminal).collect()).await
        };
        let fixture = storage_backed_state(&llm_url).await;
        let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
        let client = reqwest::Client::new();
        let mut history = Vec::new();
        for turn in turns {
            let mut body = turn["request"]["body"].clone();
            history.extend(input_items(&body["input"]));
            body["store"] = json!(false);
            body["input"] = json!(history);
            // Exercise provider extensions with unchanged recorded model responses.
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
            body["top_k"] = json!(20);
            body.as_object_mut().unwrap().remove("previous_response_id");
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let actual = response_body(response, stream).await;
            assert_eq!(actual["store"], false);
            common::assert_public_mixed_tools(&actual, &body["tools"], "stateless mixed tools");
            assert_eq!(
                public_output(&actual["output"]),
                public_output(&terminal(turn)["output"])
            );
            assert_response_not_persisted(&fixture.pool, actual["id"].as_str().unwrap()).await;
            history.extend(actual["output"].as_array().unwrap().iter().cloned());
        }
        assert_eq!(stored_row_counts(&fixture.pool).await, (0, 0));
        let captured = requests.lock().await;
        assert_eq!(captured.len(), 2);
        for request in captured.iter() {
            common::assert_lowered_mixed_upstream(request);
            assert!(request.get("previous_response_id").is_none());
            assert_eq!(request["chat_template_kwargs"], json!({"enable_thinking": false}));
            assert_eq!(request["top_k"], 20);
        }
        let input = captured[1]["input"].as_array().unwrap();
        for item in history.iter().filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            )
        }) {
            let call_id = &item["call_id"];
            assert!(
                input
                    .iter()
                    .any(|item| item["type"] == "function_call" && item["call_id"] == *call_id)
            );
            assert!(
                input
                    .iter()
                    .any(|normalized| normalized["type"] == "function_call_output"
                        && normalized["call_id"] == *call_id
                        && normalized["output"] == item["output"])
            );
        }
        gateway.abort();
        llm.abort();
    }
}

#[tokio::test]
async fn recorded_store_false_sse_cases_match_openai() {
    for scenario in [
        "manual-replay",
        "stored-parent",
        "unstored-parent",
        "conversation",
        "function-replay",
    ] {
        replay(scenario, "openai-gpt-4o", true).await;
    }
}

#[tokio::test]
async fn recorded_store_false_gateway_cases_preserve_reasoning_and_history() {
    for stream in [false, true] {
        for scenario in [
            "manual-replay",
            "stored-parent",
            "unstored-parent",
            "conversation",
            "function-replay",
        ] {
            replay(scenario, "gateway-Qwen-Qwen3.8-27B-FP8", stream).await;
        }
    }
}

async fn replay_conversation_items(provider: &str, stream: bool) {
    let mode = if stream { "streaming" } else { "nonstreaming" };
    let cassette = load_cassette("conversation-items", provider, mode);
    let turns = cassette["turns"].as_array().unwrap();
    let inference: Vec<_> = turns
        .iter()
        .filter(|turn| turn["request"]["method"] == "POST" && turn["request"]["path"] == "/v1/responses")
        .collect();
    let (llm_url, requests, llm) = if stream {
        spawn_tool_search_sse_sequence(inference.iter().map(|turn| recorded_sse(turn)).collect()).await
    } else {
        spawn_mock_vllm_json_capture_sequence(inference.iter().map(|turn| terminal(turn)).collect()).await
    };
    let fixture = storage_backed_state(&llm_url).await;
    let (url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let mut ids = HashMap::<String, String>::new();
    let mut history = Vec::new();
    let mut expected_inputs = Vec::new();
    let mut stored_responses = 0;
    for turn in turns {
        let method = turn["request"]["method"].as_str().unwrap();
        let mut path = turn["request"]["path"].as_str().unwrap().to_owned();
        for (recorded, local) in &ids {
            path = path.replace(recorded, local);
        }
        let mut body = turn["request"]["body"].clone();
        if let Some(recorded) = body["conversation"].as_str() {
            body["conversation"] = json!(ids[recorded]);
        }
        let request = client
            .request(method.parse().unwrap(), format!("{url}{path}"))
            .bearer_auth("test-key");
        let request = if method == "GET" {
            request.query(&turn["request"]["query_params"])
        } else {
            request.json(&body)
        };
        let response = request.send().await.unwrap();
        let status = response.status();
        assert_eq!(
            u64::from(status.as_u16()),
            turn["response"]["status_code"],
            "{provider}/{mode}/{}",
            turn["filename"]
        );
        let actual = response_body(response, stream && method == "POST" && path == "/v1/responses").await;
        let expected = terminal(turn);
        if !status.is_success() {
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_recorded_error(&actual, turn, &ids);
        } else if method == "GET" {
            assert_eq!(
                history_content(actual["data"].as_array().unwrap()),
                history_content(expected["data"].as_array().unwrap()),
                "conversation items at {}",
                turn["filename"]
            );
            assert_eq!(
                history_content(actual["data"].as_array().unwrap()),
                history_content(&history)
            );
        } else {
            ids.insert(
                expected["id"].as_str().unwrap().to_owned(),
                actual["id"].as_str().unwrap().to_owned(),
            );
            if path == "/v1/responses" {
                assert_eq!(actual["store"], body["store"]);
                assert_eq!(actual["status"], "completed");
                assert_eq!(public_output(&actual["output"]), public_output(&expected["output"]));
                let input = input_items(&body["input"]);
                let mut context = history.clone();
                context.extend(input.iter().cloned());
                expected_inputs.push(history_content(&context));
                if body["store"] == true {
                    stored_responses += 1;
                    history.extend(input);
                    history.extend(expected["output"].as_array().unwrap().iter().cloned());
                } else {
                    assert_response_not_persisted(&fixture.pool, actual["id"].as_str().unwrap()).await;
                }
            }
        }
    }
    let captured = requests.lock().await;
    assert_eq!(captured.len(), expected_inputs.len());
    for (request, expected) in captured.iter().zip(expected_inputs) {
        assert_eq!(history_content(request["input"].as_array().unwrap()), expected);
    }
    assert_eq!(
        stored_row_counts(&fixture.pool).await,
        (stored_responses, i64::try_from(history.len()).unwrap())
    );
    gateway.abort();
    llm.abort();
}

#[tokio::test]
async fn recorded_store_false_conversation_items_match_openai() {
    for provider in ["openai-gpt-4o", "gateway-Qwen-Qwen3.8-27B-FP8"] {
        for stream in [false, true] {
            replay_conversation_items(provider, stream).await;
        }
    }
}

#[test]
fn recorded_store_false_provider_state_contracts_agree() {
    for mode in ["nonstreaming", "streaming"] {
        for scenario in [
            "manual-replay",
            "stored-parent",
            "unstored-parent",
            "conversation",
            "function-replay",
        ] {
            let reference = load_cassette(scenario, "openai-gpt-4o", mode);
            let gateway = load_cassette(scenario, "gateway-Qwen-Qwen3.8-27B-FP8", mode);
            let reference = reference["turns"].as_array().unwrap();
            let gateway = gateway["turns"].as_array().unwrap();
            assert_eq!(reference.len(), gateway.len());
            let mut ids = HashMap::new();
            for (expected, observed) in reference.iter().zip(gateway) {
                assert_eq!(observed["response"]["status_code"], expected["response"]["status_code"]);
                for field in ["store", "stream", "tools", "tool_choice"] {
                    assert_eq!(
                        observed["request"]["body"][field], expected["request"]["body"][field],
                        "{scenario}/{mode}/{field}"
                    );
                }
                let actual = terminal(observed);
                let expected_body = terminal(expected);
                if expected_body.get("error").is_some_and(|error| !error.is_null()) {
                    assert_recorded_error(&actual, expected, &ids);
                    continue;
                }
                ids.insert(
                    expected_body["id"].as_str().unwrap().to_owned(),
                    actual["id"].as_str().unwrap().to_owned(),
                );
                for field in ["object", "status", "store"] {
                    assert_eq!(actual[field], expected_body[field], "{scenario}/{mode}/{field}");
                }
                for field in ["previous_response_id", "conversation"] {
                    assert_eq!(
                        actual[field].is_null(),
                        expected_body[field].is_null(),
                        "{scenario}/{mode}/{field}"
                    );
                }
                if let Some(output) = actual["output"].as_array() {
                    let calls: Vec<_> = output.iter().filter(|item| item["type"] == "function_call").collect();
                    let expected_calls: Vec<_> = expected_body["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| item["type"] == "function_call")
                        .collect();
                    assert_eq!(calls.len(), expected_calls.len());
                    for (call, expected_call) in calls.iter().zip(expected_calls) {
                        assert_eq!(call["name"], expected_call["name"]);
                        assert_eq!(
                            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
                            serde_json::from_str::<Value>(expected_call["arguments"].as_str().unwrap()).unwrap()
                        );
                    }
                }
            }
        }
    }
}
